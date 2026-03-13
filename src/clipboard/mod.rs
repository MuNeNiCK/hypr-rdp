//! Clipboard sharing via wlr-data-control-v1 Wayland protocol and ironrdp-cliprdr.
//!
//! Uses the `zwlr_data_control_manager_v1` protocol natively (no external CLI tools).
//! A dedicated thread runs a Wayland event loop to monitor clipboard changes and
//! handle data transfer via pipe fds.
//!
//! Implementation follows wl-clipboard (wl-paste/wl-copy) patterns:
//! - Selection callback: receive → flush → close write end → read synchronously
//! - Source send callback: write data to fd synchronously, close fd

use std::collections::HashMap;
use std::fmt;
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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
use wayland_client::backend::ObjectId;
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{delegate_noop, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1, zwlr_data_control_manager_v1, zwlr_data_control_offer_v1,
    zwlr_data_control_source_v1,
};

const TEXT_MIME: &str = "text/plain;charset=utf-8";
const UTF8_MIME: &str = "UTF8_STRING";
const TEXT_PLAIN_MIME: &str = "text/plain";

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
        let clipboard_data = Arc::new(Mutex::new(None::<Vec<u8>>));
        let pending_write = Arc::new(Mutex::new(None::<Vec<u8>>));
        let suppress = Arc::new(AtomicBool::new(false));
        let running = Arc::new(AtomicBool::new(true));

        Box::new(HyprCliprdrBackend {
            event_sender: self.event_sender.clone(),
            remote_formats: Vec::new(),
            watcher_thread: None,
            suppress_watcher: suppress,
            clipboard_data,
            pending_write,
            running,
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
    pending_write: Arc<Mutex<Option<Vec<u8>>>>,
    running: Arc<AtomicBool>,
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
        let has_content = self
            .clipboard_data
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|d| !d.is_empty()))
            .unwrap_or(false);

        if has_content {
            let formats = vec![ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)];
            if let Some(ref sender) = self.event_sender {
                let _ = sender.send(ServerEvent::Clipboard(
                    ClipboardMessage::SendInitiateCopy(formats),
                ));
            }
        }
    }

    fn on_process_negotiated_capabilities(
        &mut self,
        _capabilities: ClipboardGeneralCapabilityFlags,
    ) {
    }

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        tracing::debug!(
            formats = available_formats.len(),
            "Clipboard: remote clipboard updated"
        );
        self.remote_formats = available_formats.to_vec();

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
        let response = if request.format == ClipboardFormatId::CF_UNICODETEXT {
            let data = self.clipboard_data.lock().ok().and_then(|g| g.clone());
            match data {
                Some(ref data) if !data.is_empty() => {
                    let text = String::from_utf8_lossy(data);
                    FormatDataResponse::new_unicode_string(&text).into_owned()
                }
                _ => FormatDataResponse::new_error().into_owned(),
            }
        } else {
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
            return;
        }

        let data = response.data();
        if data.is_empty() {
            return;
        }

        let utf8 = utf16le_to_utf8(data);
        if utf8.is_empty() {
            return;
        }

        tracing::debug!(len = utf8.len(), "Clipboard: received data from RDP client");
        self.suppress_watcher.store(true, Ordering::SeqCst);
        if let Ok(mut guard) = self.pending_write.lock() {
            *guard = Some(utf8.into_bytes());
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
        let pending_write = Arc::clone(&self.pending_write);
        let running = Arc::clone(&self.running);

        let handle = thread::Builder::new()
            .name("clipboard-watcher".into())
            .spawn(move || {
                if let Err(e) =
                    clipboard_thread(sender, suppress, clipboard_data, pending_write, running)
                {
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

// --- Wayland clipboard thread ---

fn clipboard_thread(
    event_sender: mpsc::UnboundedSender<ServerEvent>,
    suppress: Arc<AtomicBool>,
    clipboard_data: Arc<Mutex<Option<Vec<u8>>>>,
    pending_write: Arc<Mutex<Option<Vec<u8>>>>,
    running: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let conn = Connection::connect_to_env()
        .map_err(|e| anyhow::anyhow!("clipboard: failed to connect to Wayland: {}", e))?;
    let mut event_queue = conn.new_event_queue::<ClipState>();
    let qh = event_queue.handle();

    let display = conn.display();
    let _registry = display.get_registry(&qh, ());

    let mut state = ClipState::new(event_sender, suppress, clipboard_data, pending_write);

    event_queue
        .roundtrip(&mut state)
        .map_err(|e| anyhow::anyhow!("clipboard: Wayland roundtrip failed: {}", e))?;

    let manager = state
        .manager
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("zwlr_data_control_manager_v1 not available"))?
        .clone();

    let seat = state
        .seat
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("wl_seat not available"))?
        .clone();

    let device = manager.get_data_device(&seat, &qh, ());
    state.device = Some(device);

    tracing::info!("Clipboard: wlr-data-control-v1 device bound");

    let wayland_fd = conn.as_fd().as_raw_fd();

    loop {
        if !running.load(Ordering::Relaxed) {
            break;
        }

        // Dispatch all pending events
        loop {
            let n = event_queue
                .dispatch_pending(&mut state)
                .map_err(|e| anyhow::anyhow!("clipboard: dispatch_pending failed: {}", e))?;
            if n == 0 {
                break;
            }
        }
        conn.flush()
            .map_err(|e| anyhow::anyhow!("clipboard: flush failed: {}", e))?;

        // RDP → Wayland: pick up pending_write and set selection
        if let Some(data) = state.pending_write.lock().ok().and_then(|mut g| g.take()) {
            tracing::debug!(len = data.len(), "Clipboard: writing to Wayland selection");
            let source = manager.create_data_source(&qh, ());
            source.offer(TEXT_MIME.to_string());
            source.offer(UTF8_MIME.to_string());
            source.offer(TEXT_PLAIN_MIME.to_string());

            if let Ok(mut g) = state.source_data.lock() {
                *g = Some(data);
            }

            if let Some(dev) = state.device.as_ref() {
                dev.set_selection(Some(&source));
            }
            // Roundtrip processes any echo Selection event while suppress is true
            event_queue
                .roundtrip(&mut state)
                .map_err(|e| anyhow::anyhow!("clipboard: roundtrip failed: {}", e))?;
            // Clear suppress after roundtrip — echo event already handled
            state.suppress.store(false, Ordering::SeqCst);
        }

        // Poll Wayland fd with 100ms timeout
        let guard = match event_queue.prepare_read() {
            Some(g) => g,
            None => continue,
        };

        let mut pollfd = libc::pollfd {
            fd: wayland_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ret = unsafe { libc::poll(&mut pollfd, 1, 100) };
        if ret > 0 {
            guard
                .read()
                .map_err(|e| anyhow::anyhow!("clipboard: read failed: {}", e))?;
        } else {
            drop(guard);
        }
    }

    Ok(())
}

// --- Wayland state for clipboard thread ---

struct ClipState {
    event_sender: mpsc::UnboundedSender<ServerEvent>,
    suppress: Arc<AtomicBool>,
    clipboard_data: Arc<Mutex<Option<Vec<u8>>>>,
    pending_write: Arc<Mutex<Option<Vec<u8>>>>,
    manager: Option<zwlr_data_control_manager_v1::ZwlrDataControlManagerV1>,
    seat: Option<wl_seat::WlSeat>,
    device: Option<zwlr_data_control_device_v1::ZwlrDataControlDeviceV1>,
    offer_mimes: HashMap<ObjectId, Vec<String>>,
    source_data: Arc<Mutex<Option<Vec<u8>>>>,
}

impl ClipState {
    fn new(
        event_sender: mpsc::UnboundedSender<ServerEvent>,
        suppress: Arc<AtomicBool>,
        clipboard_data: Arc<Mutex<Option<Vec<u8>>>>,
        pending_write: Arc<Mutex<Option<Vec<u8>>>>,
    ) -> Self {
        Self {
            event_sender,
            suppress,
            clipboard_data,
            pending_write,
            manager: None,
            seat: None,
            device: None,
            offer_mimes: HashMap::new(),
            source_data: Arc::new(Mutex::new(None)),
        }
    }
}

// --- Registry dispatch ---

impl Dispatch<wl_registry::WlRegistry, ()> for ClipState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "zwlr_data_control_manager_v1" => {
                    state.manager = Some(registry.bind(name, version.min(2), qh, ()));
                }
                "wl_seat" => {
                    if state.seat.is_none() {
                        state.seat = Some(registry.bind(name, version.min(1), qh, ()));
                    }
                }
                _ => {}
            }
        }
    }
}

// --- Data control device dispatch ---

impl Dispatch<zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, ()> for ClipState {
    wayland_client::event_created_child!(ClipState, zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, [
        0 => (zwlr_data_control_offer_v1::ZwlrDataControlOfferV1, ()),
    ]);

    fn event(
        state: &mut Self,
        _proxy: &zwlr_data_control_device_v1::ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _: &(),
        conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_device_v1::Event::DataOffer { id } => {
                state.offer_mimes.insert(id.id(), Vec::new());
            }
            zwlr_data_control_device_v1::Event::Selection { id } => {
                if state.suppress.load(Ordering::SeqCst) {
                    if let Some(offer) = id {
                        state.offer_mimes.remove(&offer.id());
                        offer.destroy();
                    }
                    return;
                }

                let offer = match id {
                    Some(offer) => offer,
                    None => {
                        if let Ok(mut g) = state.clipboard_data.lock() {
                            *g = None;
                        }
                        return;
                    }
                };

                let offer_id = offer.id();
                let mimes = state.offer_mimes.remove(&offer_id);

                let mime = mimes.as_ref().and_then(|mimes| {
                    mimes
                        .iter()
                        .find(|m| {
                            m.as_str() == TEXT_MIME
                                || m.as_str() == UTF8_MIME
                                || m.as_str() == TEXT_PLAIN_MIME
                        })
                        .cloned()
                });

                let mime = match mime {
                    Some(m) => m,
                    None => {
                        offer.destroy();
                        return;
                    }
                };

                // pipe → receive → flush → close write end → poll → read
                let mut fds = [0i32; 2];
                if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
                    tracing::warn!("Clipboard: pipe() failed");
                    offer.destroy();
                    return;
                }
                let read_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
                let write_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };

                offer.receive(mime, write_fd.as_fd());
                let _ = conn.flush();
                drop(write_fd);

                // Poll with timeout to avoid blocking the event loop indefinitely
                let mut pollfd = libc::pollfd {
                    fd: read_fd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                let poll_ret = unsafe { libc::poll(&mut pollfd, 1, 2000) };
                if poll_ret <= 0 {
                    tracing::warn!("Clipboard: timeout waiting for offer data");
                    offer.destroy();
                    return;
                }

                let mut data = Vec::new();
                let mut file = std::fs::File::from(read_fd);
                if let Err(e) = file.read_to_end(&mut data) {
                    tracing::warn!("Clipboard: failed to read offer data: {}", e);
                    offer.destroy();
                    return;
                }

                offer.destroy();

                if data.is_empty() {
                    return;
                }

                tracing::debug!(len = data.len(), "Clipboard: read clipboard data");

                if let Ok(mut g) = state.clipboard_data.lock() {
                    *g = Some(data);
                }

                let formats = vec![ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)];
                let _ = state.event_sender.send(ServerEvent::Clipboard(
                    ClipboardMessage::SendInitiateCopy(formats),
                ));
            }
            zwlr_data_control_device_v1::Event::Finished => {
                tracing::warn!("Clipboard: data control device finished");
                state.device = None;
            }
            zwlr_data_control_device_v1::Event::PrimarySelection { .. } => {}
            _ => {}
        }
    }
}

// --- Data control offer dispatch ---

impl Dispatch<zwlr_data_control_offer_v1::ZwlrDataControlOfferV1, ()> for ClipState {
    fn event(
        state: &mut Self,
        proxy: &zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        _: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            if let Some(mimes) = state.offer_mimes.get_mut(&proxy.id()) {
                mimes.push(mime_type);
            }
        }
    }
}

// --- Data control source dispatch ---

impl Dispatch<zwlr_data_control_source_v1::ZwlrDataControlSourceV1, ()> for ClipState {
    fn event(
        state: &mut Self,
        _proxy: &zwlr_data_control_source_v1::ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        _: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {
                if mime_type == TEXT_MIME
                    || mime_type == UTF8_MIME
                    || mime_type == TEXT_PLAIN_MIME
                {
                    if let Some(data) = state.source_data.lock().ok().and_then(|g| g.clone()) {
                        let mut file = std::fs::File::from(fd);
                        let _ = file.write_all(&data);
                    }
                }
            }
            zwlr_data_control_source_v1::Event::Cancelled => {}
            _ => {}
        }
    }
}

// --- No-op dispatchers ---

delegate_noop!(ClipState: ignore wl_seat::WlSeat);
delegate_noop!(ClipState: ignore zwlr_data_control_manager_v1::ZwlrDataControlManagerV1);

// --- Utilities ---

fn utf16le_to_utf8(data: &[u8]) -> String {
    let u16s: Vec<u16> = data
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();

    let end = u16s.iter().position(|&c| c == 0).unwrap_or(u16s.len());
    String::from_utf16_lossy(&u16s[..end])
}
