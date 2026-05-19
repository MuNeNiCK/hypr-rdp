use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use ironrdp_cliprdr::backend::{ClipboardMessage, CliprdrBackend, CliprdrBackendFactory};
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags, FileContentsRequest,
    FileContentsResponse, FormatDataRequest, FormatDataResponse, LockDataId,
};
use ironrdp_core::impl_as_any;
use ironrdp_pdu::IntoOwned;
use ironrdp_server::{CliprdrServerFactory, ServerEvent, ServerEventSender};
use tokio::sync::mpsc;

use super::formats::{fix_bitfields_dib, utf16le_to_utf8, PendingWrite};
use super::wayland::clipboard_thread;

pub struct HyprCliprdrFactory {
    event_sender: Option<mpsc::UnboundedSender<ServerEvent>>,
}

impl HyprCliprdrFactory {
    pub fn new() -> Self {
        Self { event_sender: None }
    }
}

impl ServerEventSender for HyprCliprdrFactory {
    fn set_sender(&mut self, sender: mpsc::UnboundedSender<ServerEvent>) {
        self.event_sender = Some(sender);
    }
}

impl CliprdrBackendFactory for HyprCliprdrFactory {
    fn build_cliprdr_backend(&self) -> Box<dyn CliprdrBackend> {
        let clipboard_data = Arc::new(Mutex::new(None::<Vec<u8>>));
        let clipboard_image = Arc::new(Mutex::new(None::<Vec<u8>>));
        let pending_write = Arc::new(Mutex::new(None::<PendingWrite>));
        let suppress = Arc::new(AtomicBool::new(false));
        let running = Arc::new(AtomicBool::new(true));

        Box::new(HyprCliprdrBackend {
            event_sender: self.event_sender.clone(),
            remote_formats: Vec::new(),
            watcher_thread: None,
            suppress_watcher: suppress,
            clipboard_data,
            clipboard_image,
            pending_write,
            running,
            last_requested_format: None,
        })
    }
}

impl CliprdrServerFactory for HyprCliprdrFactory {}

struct HyprCliprdrBackend {
    event_sender: Option<mpsc::UnboundedSender<ServerEvent>>,
    remote_formats: Vec<ClipboardFormat>,
    watcher_thread: Option<thread::JoinHandle<()>>,
    suppress_watcher: Arc<AtomicBool>,
    clipboard_data: Arc<Mutex<Option<Vec<u8>>>>,
    clipboard_image: Arc<Mutex<Option<Vec<u8>>>>, // CF_DIB bytes
    pending_write: Arc<Mutex<Option<PendingWrite>>>,
    running: Arc<AtomicBool>,
    last_requested_format: Option<ClipboardFormatId>,
}

impl_as_any!(HyprCliprdrBackend);

impl fmt::Debug for HyprCliprdrBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HyprCliprdrBackend")
            .field("remote_formats", &self.remote_formats.len())
            .field("watching", &self.watcher_thread.is_some())
            .finish()
    }
}

impl Drop for HyprCliprdrBackend {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
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
        let mut formats = Vec::new();

        let has_text = self
            .clipboard_data
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|d| !d.is_empty()))
            .unwrap_or(false);
        if has_text {
            formats.push(ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT));
        }

        let has_image = self
            .clipboard_image
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|d| !d.is_empty()))
            .unwrap_or(false);
        if has_image {
            formats.push(ClipboardFormat::new(ClipboardFormatId::CF_DIB));
        }

        if !formats.is_empty() {
            if let Some(ref sender) = self.event_sender {
                let _ = sender.send(ServerEvent::Clipboard(ClipboardMessage::SendInitiateCopy(
                    formats,
                )));
            }
        }
    }

    fn on_process_negotiated_capabilities(
        &mut self,
        _capabilities: ClipboardGeneralCapabilityFlags,
    ) {
    }

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        tracing::trace!(
            formats = available_formats.len(),
            "Clipboard: remote clipboard updated"
        );
        self.remote_formats = available_formats.to_vec();

        let has_unicode = available_formats
            .iter()
            .any(|f| f.id == ClipboardFormatId::CF_UNICODETEXT);
        let has_dibv5 = available_formats
            .iter()
            .any(|f| f.id == ClipboardFormatId::CF_DIBV5);
        let has_dib = available_formats
            .iter()
            .any(|f| f.id == ClipboardFormatId::CF_DIB);

        // Prefer text over image; prefer CF_DIBV5 over CF_DIB (better BITFIELDS support)
        let format = if has_unicode {
            Some(ClipboardFormatId::CF_UNICODETEXT)
        } else if has_dibv5 {
            Some(ClipboardFormatId::CF_DIBV5)
        } else if has_dib {
            Some(ClipboardFormatId::CF_DIB)
        } else {
            None
        };

        if let Some(fmt) = format {
            self.last_requested_format = Some(fmt);
            if let Some(ref sender) = self.event_sender {
                let _ = sender.send(ServerEvent::Clipboard(ClipboardMessage::SendInitiatePaste(
                    fmt,
                )));
            }
        }
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        let response = if request.format == ClipboardFormatId::CF_UNICODETEXT {
            let data = self.clipboard_data.lock().ok().and_then(|g| g.clone());
            match data {
                Some(ref data) if !data.is_empty() => {
                    let text = String::from_utf8_lossy(data);
                    FormatDataResponse::new_unicode_string(&text).into_owned()
                }
                _ => FormatDataResponse::new_error().into_owned(),
            }
        } else if request.format == ClipboardFormatId::CF_DIB {
            let data = self.clipboard_image.lock().ok().and_then(|g| g.clone());
            match data {
                Some(dib_data) if !dib_data.is_empty() => {
                    FormatDataResponse::new_data(dib_data).into_owned()
                }
                _ => FormatDataResponse::new_error().into_owned(),
            }
        } else {
            FormatDataResponse::new_error().into_owned()
        };

        if let Some(ref sender) = self.event_sender {
            let _ = sender.send(ServerEvent::Clipboard(ClipboardMessage::SendFormatData(
                response,
            )));
        }
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        // 100 MB limit to prevent memory exhaustion from malicious clients
        const MAX_CLIPBOARD_SIZE: usize = 100 * 1024 * 1024;

        if response.is_error() {
            return;
        }

        let data = response.data();
        if data.is_empty() {
            return;
        }

        if data.len() > MAX_CLIPBOARD_SIZE {
            tracing::warn!(
                size = data.len(),
                max = MAX_CLIPBOARD_SIZE,
                "Clipboard data too large, ignoring"
            );
            return;
        }

        let requested_format = self.last_requested_format.take();

        if requested_format == Some(ClipboardFormatId::CF_DIBV5) {
            // Convert CF_DIBV5 to PNG for Wayland
            match ironrdp_cliprdr_format::bitmap::dibv5_to_png(data) {
                Ok(png_data) => {
                    tracing::trace!(len = png_data.len(), "Clipboard: converted DIBV5 to PNG");
                    self.suppress_watcher.store(true, Ordering::SeqCst);
                    if let Ok(mut guard) = self.pending_write.lock() {
                        *guard = Some(PendingWrite::Image(png_data));
                    }
                }
                Err(e) => {
                    tracing::warn!("Clipboard: failed to convert DIBV5 to PNG: {}", e);
                }
            }
        } else if requested_format == Some(ClipboardFormatId::CF_DIB) {
            // Convert CF_DIB to PNG for Wayland
            // Try standard dib_to_png first, then fix BITFIELDS if needed
            let png_result = ironrdp_cliprdr_format::bitmap::dib_to_png(data).or_else(|_| {
                // Windows often sends 32-bit BGRA with BI_BITFIELDS compression.
                // dib_to_png doesn't handle this, so strip the color masks and
                // convert to BI_RGB for a second attempt.
                let fixed = fix_bitfields_dib(data).ok_or_else(|| {
                    ironrdp_cliprdr_format::bitmap::BitmapError::Unsupported("cannot fix BITFIELDS")
                })?;
                ironrdp_cliprdr_format::bitmap::dib_to_png(&fixed)
            });
            match png_result {
                Ok(png_data) => {
                    tracing::trace!(len = png_data.len(), "Clipboard: converted DIB to PNG");
                    self.suppress_watcher.store(true, Ordering::SeqCst);
                    if let Ok(mut guard) = self.pending_write.lock() {
                        *guard = Some(PendingWrite::Image(png_data));
                    }
                }
                Err(e) => {
                    tracing::warn!("Clipboard: failed to convert DIB to PNG: {}", e);
                }
            }
        } else {
            let utf8 = utf16le_to_utf8(data);
            if utf8.is_empty() {
                return;
            }

            tracing::trace!(len = utf8.len(), "Clipboard: received text from RDP client");
            self.suppress_watcher.store(true, Ordering::SeqCst);
            if let Ok(mut guard) = self.pending_write.lock() {
                *guard = Some(PendingWrite::Text(utf8.into_bytes()));
            }
        }
    }

    fn on_file_contents_request(&mut self, _request: FileContentsRequest) {}

    fn on_file_contents_response(&mut self, _response: FileContentsResponse<'_>) {}

    fn on_lock(&mut self, _data_id: LockDataId) {}

    fn on_unlock(&mut self, _data_id: LockDataId) {}
}

impl HyprCliprdrBackend {
    fn start_clipboard_watcher(&mut self) {
        let sender = match self.event_sender.clone() {
            Some(s) => s,
            None => return,
        };

        let suppress = Arc::clone(&self.suppress_watcher);
        let clipboard_data = Arc::clone(&self.clipboard_data);
        let clipboard_image = Arc::clone(&self.clipboard_image);
        let pending_write = Arc::clone(&self.pending_write);
        let running = Arc::clone(&self.running);

        let handle = thread::Builder::new()
            .name("clipboard-watcher".into())
            .spawn(move || {
                if let Err(e) = clipboard_thread(
                    sender,
                    suppress,
                    clipboard_data,
                    clipboard_image,
                    pending_write,
                    running,
                ) {
                    tracing::error!("Clipboard thread error: {:#}", e);
                }
            })
            .ok();

        if let Some(h) = handle {
            self.watcher_thread = Some(h);
            tracing::info!("Clipboard: watching via wlr-data-control-v1");
        }
    }
}
