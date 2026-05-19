//! Clipboard sharing via wlr-data-control-v1 Wayland protocol and ironrdp-cliprdr.
//!
//! Uses the `zwlr_data_control_manager_v1` protocol natively (no external CLI tools).
//! A dedicated thread runs a Wayland event loop to monitor clipboard changes and
//! handle data transfer via pipe fds.
//!
//! Supports text (CF_UNICODETEXT) and images (CF_DIB via PNG conversion).

use std::collections::HashMap;
use std::fmt;
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
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
const IMAGE_PNG_MIME: &str = "image/png";

/// Data pending write to Wayland clipboard (from RDP client).
enum PendingWrite {
    Text(Vec<u8>),
    Image(Vec<u8>), // PNG bytes
}

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
            // Default: treat as unicode text
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

// --- Wayland clipboard thread ---

fn clipboard_thread(
    event_sender: mpsc::UnboundedSender<ServerEvent>,
    suppress: Arc<AtomicBool>,
    clipboard_data: Arc<Mutex<Option<Vec<u8>>>>,
    clipboard_image: Arc<Mutex<Option<Vec<u8>>>>,
    pending_write: Arc<Mutex<Option<PendingWrite>>>,
    running: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let conn = Connection::connect_to_env()
        .map_err(|e| anyhow::anyhow!("clipboard: failed to connect to Wayland: {}", e))?;
    let mut event_queue = conn.new_event_queue::<ClipState>();
    let qh = event_queue.handle();

    let display = conn.display();
    let _registry = display.get_registry(&qh, ());

    let mut state = ClipState::new(
        event_sender,
        suppress,
        clipboard_data,
        clipboard_image,
        pending_write,
    );

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
        if let Some(pending) = state.pending_write.lock().ok().and_then(|mut g| g.take()) {
            // Destroy previous source to prevent protocol object leak
            if let Some(old) = state.active_source.take() {
                old.destroy();
            }
            let source = manager.create_data_source(&qh, ());
            match &pending {
                PendingWrite::Text(data) => {
                    tracing::trace!(len = data.len(), "Clipboard: writing text to Wayland");
                    source.offer(TEXT_MIME.to_string());
                    source.offer(UTF8_MIME.to_string());
                    source.offer(TEXT_PLAIN_MIME.to_string());
                    if let Ok(mut g) = state.source_data.lock() {
                        *g = Some(data.clone());
                    }
                    if let Ok(mut g) = state.source_mime.lock() {
                        *g = SourceType::Text;
                    }
                }
                PendingWrite::Image(data) => {
                    tracing::trace!(len = data.len(), "Clipboard: writing image to Wayland");
                    source.offer(IMAGE_PNG_MIME.to_string());
                    if let Ok(mut g) = state.source_data.lock() {
                        *g = Some(data.clone());
                    }
                    if let Ok(mut g) = state.source_mime.lock() {
                        *g = SourceType::Image;
                    }
                }
            }

            if let Some(dev) = state.device.as_ref() {
                dev.set_selection(Some(&source));
            }
            state.active_source = Some(source);
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

#[derive(Clone, Copy)]
enum SourceType {
    Text,
    Image,
}

struct ClipState {
    event_sender: mpsc::UnboundedSender<ServerEvent>,
    suppress: Arc<AtomicBool>,
    clipboard_data: Arc<Mutex<Option<Vec<u8>>>>,
    clipboard_image: Arc<Mutex<Option<Vec<u8>>>>,
    pending_write: Arc<Mutex<Option<PendingWrite>>>,
    manager: Option<zwlr_data_control_manager_v1::ZwlrDataControlManagerV1>,
    seat: Option<wl_seat::WlSeat>,
    device: Option<zwlr_data_control_device_v1::ZwlrDataControlDeviceV1>,
    offer_mimes: HashMap<ObjectId, Vec<String>>,
    source_data: Arc<Mutex<Option<Vec<u8>>>>,
    source_mime: Arc<Mutex<SourceType>>,
    /// Currently active data source; destroyed when replaced to avoid protocol object leak.
    active_source: Option<zwlr_data_control_source_v1::ZwlrDataControlSourceV1>,
}

impl ClipState {
    fn new(
        event_sender: mpsc::UnboundedSender<ServerEvent>,
        suppress: Arc<AtomicBool>,
        clipboard_data: Arc<Mutex<Option<Vec<u8>>>>,
        clipboard_image: Arc<Mutex<Option<Vec<u8>>>>,
        pending_write: Arc<Mutex<Option<PendingWrite>>>,
    ) -> Self {
        Self {
            event_sender,
            suppress,
            clipboard_data,
            clipboard_image,
            pending_write,
            manager: None,
            seat: None,
            device: None,
            offer_mimes: HashMap::new(),
            source_data: Arc::new(Mutex::new(None)),
            source_mime: Arc::new(Mutex::new(SourceType::Text)),
            active_source: None,
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
                        if let Ok(mut g) = state.clipboard_image.lock() {
                            *g = None;
                        }
                        return;
                    }
                };

                let offer_id = offer.id();
                let mimes = state.offer_mimes.remove(&offer_id);
                let mimes = match mimes {
                    Some(m) => m,
                    None => {
                        offer.destroy();
                        return;
                    }
                };

                // Check for text MIME
                let text_mime = mimes
                    .iter()
                    .find(|m| {
                        m.as_str() == TEXT_MIME
                            || m.as_str() == UTF8_MIME
                            || m.as_str() == TEXT_PLAIN_MIME
                    })
                    .cloned();

                // Check for image MIME
                let image_mime = mimes.iter().find(|m| m.as_str() == IMAGE_PNG_MIME).cloned();

                if text_mime.is_none() && image_mime.is_none() {
                    // No supported MIME — clear stale caches
                    if let Ok(mut g) = state.clipboard_data.lock() {
                        *g = None;
                    }
                    if let Ok(mut g) = state.clipboard_image.lock() {
                        *g = None;
                    }
                    offer.destroy();
                    return;
                }

                let mut formats = Vec::new();

                // Clear stale caches for formats NOT present in the new selection.
                // Without this, switching image→text or text→image leaves the
                // previous format cached and re-advertisable to the RDP client.
                if text_mime.is_none() {
                    if let Ok(mut g) = state.clipboard_data.lock() {
                        *g = None;
                    }
                }
                if image_mime.is_none() {
                    if let Ok(mut g) = state.clipboard_image.lock() {
                        *g = None;
                    }
                }

                // Read text content if available
                if let Some(ref mime) = text_mime {
                    if let Some(data) = read_offer_data(&offer, mime, conn) {
                        if !data.is_empty() {
                            tracing::trace!(len = data.len(), "Clipboard: read text data");
                            if let Ok(mut g) = state.clipboard_data.lock() {
                                *g = Some(data);
                            }
                            formats.push(ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT));
                        }
                    }
                }

                // Read image content if available
                if let Some(ref mime) = image_mime {
                    if let Some(png_data) = read_offer_data(&offer, mime, conn) {
                        if !png_data.is_empty() {
                            // Convert PNG to CF_DIB for RDP clients
                            match ironrdp_cliprdr_format::bitmap::png_to_cf_dib(&png_data) {
                                Ok(dib_data) => {
                                    tracing::trace!(
                                        png_len = png_data.len(),
                                        dib_len = dib_data.len(),
                                        "Clipboard: converted PNG to CF_DIB"
                                    );
                                    if let Ok(mut g) = state.clipboard_image.lock() {
                                        *g = Some(dib_data);
                                    }
                                    formats.push(ClipboardFormat::new(ClipboardFormatId::CF_DIB));
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        "Clipboard: PNG to DIB conversion failed: {}",
                                        e
                                    );
                                }
                            }
                        }
                    }
                }

                offer.destroy();

                if !formats.is_empty() {
                    let _ = state.event_sender.send(ServerEvent::Clipboard(
                        ClipboardMessage::SendInitiateCopy(formats),
                    ));
                }
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

/// Read data from a clipboard offer via pipe.
fn read_offer_data(
    offer: &zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
    mime: &str,
    conn: &Connection,
) -> Option<Vec<u8>> {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        tracing::warn!("Clipboard: pipe() failed");
        return None;
    }
    let read_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };

    offer.receive(mime.to_string(), write_fd.as_fd());
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
        tracing::warn!("Clipboard: timeout waiting for offer data (mime={})", mime);
        return None;
    }

    let mut data = Vec::new();
    let mut file = std::fs::File::from(read_fd);
    if let Err(e) = file.read_to_end(&mut data) {
        tracing::warn!("Clipboard: failed to read offer data: {}", e);
        return None;
    }

    Some(data)
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
        proxy: &zwlr_data_control_source_v1::ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        _: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {
                let is_text_mime = mime_type == TEXT_MIME
                    || mime_type == UTF8_MIME
                    || mime_type == TEXT_PLAIN_MIME;
                let is_image_mime = mime_type == IMAGE_PNG_MIME;

                let source_type = state.source_mime.lock().ok().map(|g| *g);

                let should_send = match source_type {
                    Some(SourceType::Text) => is_text_mime,
                    Some(SourceType::Image) => is_image_mime,
                    None => is_text_mime,
                };

                if should_send {
                    if let Some(data) = state.source_data.lock().ok().and_then(|g| g.clone()) {
                        let mut file = std::fs::File::from(fd);
                        let _ = file.write_all(&data);
                    }
                }
            }
            zwlr_data_control_source_v1::Event::Cancelled => {
                // Protocol requires destroying the source after cancellation
                proxy.destroy();
                if state
                    .active_source
                    .as_ref()
                    .is_some_and(|s| s.id() == proxy.id())
                {
                    state.active_source = None;
                }
            }
            _ => {}
        }
    }
}

// --- No-op dispatchers ---

delegate_noop!(ClipState: ignore wl_seat::WlSeat);
delegate_noop!(ClipState: ignore zwlr_data_control_manager_v1::ZwlrDataControlManagerV1);

// --- Utilities ---

/// Fix a CF_DIB with BI_BITFIELDS compression (common on Windows for 32-bit BGRA).
///
/// BITMAPINFOHEADER (40 bytes) + 3 DWORD color masks (12 bytes) + pixel data
/// → BITMAPINFOHEADER (40 bytes, compression=BI_RGB) + pixel data
fn fix_bitfields_dib(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 52 {
        return None;
    }
    let header_size = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    if header_size != 40 {
        return None;
    }
    let bit_count = u16::from_le_bytes([data[14], data[15]]);
    if bit_count != 32 {
        return None;
    }
    let compression = u32::from_le_bytes([data[16], data[17], data[18], data[19]]);
    if compression != 3 {
        // Not BI_BITFIELDS
        return None;
    }

    // Reconstruct as BI_RGB: copy header with compression=0, skip 12 bytes of masks
    let mut fixed = Vec::with_capacity(data.len() - 12);
    fixed.extend_from_slice(&data[..16]); // header up to compression field
    fixed.extend_from_slice(&0u32.to_le_bytes()); // compression = BI_RGB (0)
    fixed.extend_from_slice(&data[20..40]); // rest of header
    fixed.extend_from_slice(&data[52..]); // pixel data (skip 12 bytes of color masks)
    Some(fixed)
}

fn utf16le_to_utf8(data: &[u8]) -> String {
    let u16s: Vec<u16> = data
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();

    let end = u16s.iter().position(|&c| c == 0).unwrap_or(u16s.len());
    String::from_utf16_lossy(&u16s[..end])
}
