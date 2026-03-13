//! Clipboard sharing via wl-copy/wl-paste and ironrdp-cliprdr.
//!
//! Uses the wl-copy/wl-paste CLI tools for Wayland clipboard access,
//! bridged to the RDP clipboard channel via ironrdp-cliprdr's CliprdrBackend trait.
//!
//! A `wl-paste --watch` subprocess monitors local clipboard changes and
//! sends `SendInitiateCopy` to notify the RDP client.

use std::fmt;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use ironrdp_cliprdr::backend::{CliprdrBackend, CliprdrBackendFactory, ClipboardMessage};
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags, FileContentsRequest,
    FileContentsResponse, FormatDataRequest, FormatDataResponse, LockDataId,
};
use ironrdp_core::impl_as_any;
use ironrdp_pdu::IntoOwned;
use ironrdp_server::{CliprdrServerFactory, ServerEvent, ServerEventSender};
use tokio::sync::mpsc;

pub struct HyprCliprdrFactory {
    event_sender: Option<mpsc::UnboundedSender<ServerEvent>>,
}

impl HyprCliprdrFactory {
    pub fn new() -> Self {
        Self {
            event_sender: None,
        }
    }
}

impl ServerEventSender for HyprCliprdrFactory {
    fn set_sender(&mut self, sender: mpsc::UnboundedSender<ServerEvent>) {
        self.event_sender = Some(sender);
    }
}

impl CliprdrBackendFactory for HyprCliprdrFactory {
    fn build_cliprdr_backend(&self) -> Box<dyn CliprdrBackend> {
        Box::new(HyprCliprdrBackend {
            event_sender: self.event_sender.clone(),
            remote_formats: Vec::new(),
            watcher_process: None,
            watcher_thread: None,
            suppress_watcher: Arc::new(AtomicBool::new(false)),
        })
    }
}

impl CliprdrServerFactory for HyprCliprdrFactory {}

struct HyprCliprdrBackend {
    event_sender: Option<mpsc::UnboundedSender<ServerEvent>>,
    remote_formats: Vec<ClipboardFormat>,
    watcher_process: Option<Child>,
    watcher_thread: Option<thread::JoinHandle<()>>,
    /// Shared flag to suppress watcher notifications during self-originated writes
    suppress_watcher: Arc<AtomicBool>,
}

impl_as_any!(HyprCliprdrBackend);

impl fmt::Debug for HyprCliprdrBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HyprCliprdrBackend")
            .field("remote_formats", &self.remote_formats.len())
            .field("watching", &self.watcher_process.is_some())
            .finish()
    }
}

impl Drop for HyprCliprdrBackend {
    fn drop(&mut self) {
        if let Some(mut child) = self.watcher_process.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(handle) = self.watcher_thread.take() {
            let _ = handle.join();
        }
    }
}

impl CliprdrBackend for HyprCliprdrBackend {
    fn temporary_directory(&self) -> &str {
        "/tmp"
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES
    }

    fn on_ready(&mut self) {
        tracing::info!("Clipboard channel ready");
        self.start_clipboard_watcher();
    }

    fn on_request_format_list(&mut self) {
        tracing::debug!("Clipboard: server requested format list");

        // Advertise local clipboard content to the RDP client
        let has_content = match read_wayland_clipboard() {
            Ok(data) => !data.is_empty(),
            Err(_) => false,
        };

        if has_content {
            let formats = vec![ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)];
            if let Some(ref sender) = self.event_sender {
                let _ = sender.send(ServerEvent::Clipboard(
                    ClipboardMessage::SendInitiateCopy(formats),
                ));
            }
        }
    }

    fn on_process_negotiated_capabilities(&mut self, capabilities: ClipboardGeneralCapabilityFlags) {
        tracing::debug!(?capabilities, "Clipboard: negotiated capabilities");
    }

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        tracing::info!(
            formats = available_formats.len(),
            "Clipboard: remote clipboard updated"
        );
        self.remote_formats = available_formats.to_vec();

        // Request text data from the remote client if available
        let has_unicode = available_formats
            .iter()
            .any(|f| f.id == ClipboardFormatId::CF_UNICODETEXT);

        if has_unicode {
            if let Some(ref sender) = self.event_sender {
                let _ = sender.send(ServerEvent::Clipboard(
                    ClipboardMessage::SendInitiatePaste(ClipboardFormatId::CF_UNICODETEXT),
                ));
            }
        }
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        tracing::debug!(?request, "Clipboard: format data requested");

        let response = if request.format == ClipboardFormatId::CF_UNICODETEXT {
            match read_wayland_clipboard() {
                Ok(data) if !data.is_empty() => {
                    let text = String::from_utf8_lossy(&data);
                    tracing::debug!(len = data.len(), "Clipboard: sending as CF_UNICODETEXT");
                    FormatDataResponse::new_unicode_string(&text).into_owned()
                }
                Ok(_) => FormatDataResponse::new_error().into_owned(),
                Err(e) => {
                    tracing::warn!("Failed to read Wayland clipboard: {:#}", e);
                    FormatDataResponse::new_error().into_owned()
                }
            }
        } else {
            tracing::debug!(format_id = request.format.value(), "Clipboard: unsupported format requested");
            FormatDataResponse::new_error().into_owned()
        };

        if let Some(ref sender) = self.event_sender {
            let _ = sender.send(ServerEvent::Clipboard(
                ClipboardMessage::SendFormatData(response),
            ));
        }
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        if response.is_error() {
            tracing::debug!("Clipboard: format data response is error");
            return;
        }

        let data = response.data();
        if data.is_empty() {
            return;
        }

        // Decode UTF-16LE from RDP client to UTF-8 for Wayland
        let utf8 = utf16le_to_utf8(data);
        if utf8.is_empty() {
            return;
        }

        // Suppress watcher to prevent echo loop: wl-copy triggers wl-paste --watch,
        // which would send SendInitiateCopy back to the RDP client
        self.suppress_watcher.store(true, Ordering::SeqCst);
        if let Err(e) = write_wayland_clipboard(utf8.as_bytes()) {
            tracing::warn!("Failed to write Wayland clipboard: {:#}", e);
        } else {
            tracing::debug!(len = utf8.len(), "Clipboard: wrote to Wayland clipboard");
        }
        // Small delay to ensure wl-paste --watch processes the change before we unsuppress
        std::thread::sleep(std::time::Duration::from_millis(100));
        self.suppress_watcher.store(false, Ordering::SeqCst);
    }

    fn on_file_contents_request(&mut self, request: FileContentsRequest) {
        tracing::debug!(?request, "Clipboard: file contents requested (not supported)");
    }

    fn on_file_contents_response(&mut self, response: FileContentsResponse<'_>) {
        tracing::debug!(
            stream_id = response.stream_id(),
            "Clipboard: file contents response (not supported)"
        );
    }

    fn on_lock(&mut self, data_id: LockDataId) {
        tracing::debug!(?data_id, "Clipboard: lock");
    }

    fn on_unlock(&mut self, data_id: LockDataId) {
        tracing::debug!(?data_id, "Clipboard: unlock");
    }
}

impl HyprCliprdrBackend {
    /// Start a `wl-paste --watch` subprocess to monitor local clipboard changes.
    /// Each time the clipboard changes, sends `SendInitiateCopy` to notify the RDP client.
    fn start_clipboard_watcher(&mut self) {
        let sender = match self.event_sender.clone() {
            Some(s) => s,
            None => return,
        };

        // `wl-paste --watch` runs the given command each time the clipboard changes.
        // We use `echo changed` as a fixed sentinel — one line per clipboard change,
        // regardless of clipboard content (avoids raw binary/multiline issues with `cat`).
        let mut child = match Command::new("wl-paste")
            .args(["--watch", "echo", "changed"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                tracing::warn!("Clipboard: failed to start wl-paste --watch: {}", e);
                return;
            }
        };

        let stdout = child.stdout.take().unwrap();
        self.watcher_process = Some(child);

        let suppress = Arc::clone(&self.suppress_watcher);
        let handle = thread::Builder::new()
            .name("clipboard-watcher".into())
            .spawn(move || {
                let reader = BufReader::new(stdout);
                // Each "changed\n" line = one clipboard change event
                for _line in reader.lines() {
                    // Skip notifications triggered by our own wl-copy writes
                    if suppress.load(Ordering::SeqCst) {
                        tracing::debug!("Clipboard: suppressing self-originated change");
                        continue;
                    }
                    let formats = vec![ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)];
                    if sender
                        .send(ServerEvent::Clipboard(
                            ClipboardMessage::SendInitiateCopy(formats),
                        ))
                        .is_err()
                    {
                        break; // Channel closed, session ended
                    }
                    tracing::debug!("Clipboard: local clipboard changed, notified RDP client");
                }
                tracing::debug!("Clipboard watcher thread exiting");
            })
            .ok();

        if let Some(h) = handle {
            self.watcher_thread = Some(h);
            tracing::info!("Clipboard: watching local clipboard changes via wl-paste --watch");
        }
    }
}

/// Read the current Wayland clipboard content using wl-paste.
fn read_wayland_clipboard() -> anyhow::Result<Vec<u8>> {
    let output = Command::new("wl-paste")
        .args(["--no-newline"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // "Nothing is copied" is a normal condition, not an error
        if stderr.contains("Nothing is copied") {
            return Ok(Vec::new());
        }
        anyhow::bail!("wl-paste failed: {}", stderr);
    }

    Ok(output.stdout)
}

/// Write data to the Wayland clipboard using wl-copy.
fn write_wayland_clipboard(data: &[u8]) -> anyhow::Result<()> {
    let mut child = Command::new("wl-copy")
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    if let Some(stdin) = child.stdin.take() {
        use std::io::Write;
        let mut stdin = stdin;
        stdin.write_all(data)?;
    }

    let status = child.wait()?;
    if !status.success() {
        anyhow::bail!("wl-copy failed with status {}", status);
    }

    Ok(())
}

/// Decode UTF-16LE bytes (from RDP) to a UTF-8 string.
/// Strips trailing null terminators.
fn utf16le_to_utf8(data: &[u8]) -> String {
    let u16s: Vec<u16> = data
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();

    // Strip trailing null terminators
    let end = u16s.iter().position(|&c| c == 0).unwrap_or(u16s.len());
    String::from_utf16_lossy(&u16s[..end])
}
