use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ironrdp_cliprdr::backend::ClipboardMessage;
use ironrdp_cliprdr::pdu::{ClipboardFormat, ClipboardFormatId};
use ironrdp_server::ServerEvent;
use tokio::sync::mpsc;
use wayland_client::backend::ObjectId;
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{delegate_noop, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1, zwlr_data_control_manager_v1, zwlr_data_control_offer_v1,
    zwlr_data_control_source_v1,
};

use super::formats::{PendingWrite, IMAGE_PNG_MIME, TEXT_MIME, TEXT_PLAIN_MIME, UTF8_MIME};

pub(super) fn clipboard_thread(
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

                let text_mime = mimes
                    .iter()
                    .find(|m| {
                        m.as_str() == TEXT_MIME
                            || m.as_str() == UTF8_MIME
                            || m.as_str() == TEXT_PLAIN_MIME
                    })
                    .cloned();

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

delegate_noop!(ClipState: ignore wl_seat::WlSeat);
delegate_noop!(ClipState: ignore zwlr_data_control_manager_v1::ZwlrDataControlManagerV1);
