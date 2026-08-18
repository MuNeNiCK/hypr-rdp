use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

use super::backend::{announce_local_formats, ClipboardEchoCandidate};
use super::formats::{
    PendingWrite, IMAGE_PNG_MIME, MAX_CLIPBOARD_SIZE, TEXT_MIME, TEXT_PLAIN_MIME, UTF8_MIME,
};

pub(super) fn clipboard_thread(
    event_sender: mpsc::UnboundedSender<ServerEvent>,
    suppress: Arc<AtomicBool>,
    clipboard_data: Arc<Mutex<Option<Vec<u8>>>>,
    clipboard_image: Arc<Mutex<Option<Vec<u8>>>>,
    pending_write: Arc<Mutex<Option<PendingWrite>>>,
    echo_candidate: Arc<Mutex<Option<ClipboardEchoCandidate>>>,
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
        echo_candidate,
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
    echo_candidate: Arc<Mutex<Option<ClipboardEchoCandidate>>>,
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
        echo_candidate: Arc<Mutex<Option<ClipboardEchoCandidate>>>,
    ) -> Self {
        Self {
            event_sender,
            suppress,
            clipboard_data,
            clipboard_image,
            pending_write,
            echo_candidate,
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
                "wl_seat" if state.seat.is_none() => {
                    state.seat = Some(registry.bind(name, version.min(1), qh, ()));
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

                if let Ok(mut candidate) = state.echo_candidate.lock() {
                    *candidate = None;
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
                    announce_local_formats(
                        &state.event_sender,
                        &state.echo_candidate,
                        &state.clipboard_data,
                        &state.clipboard_image,
                        formats,
                    );
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

/// No-progress and overall deadlines for clipboard pipe transfers. The
/// watcher thread services these transfers inline, and the backend joins
/// that thread on drop — an unbounded read or write here therefore wedges
/// the whole server, so both directions must always terminate.
const PIPE_STALL_TIMEOUT: Duration = Duration::from_secs(2);
const PIPE_TOTAL_TIMEOUT: Duration = Duration::from_secs(20);

fn set_nonblocking(fd: std::os::fd::RawFd) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn poll_fd(fd: std::os::fd::RawFd, events: libc::c_short) -> std::io::Result<bool> {
    let mut pollfd = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    match unsafe { libc::poll(&mut pollfd, 1, 100) } {
        0 => Ok(false),
        n if n > 0 => Ok(true),
        _ => {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                Ok(false)
            } else {
                Err(error)
            }
        }
    }
}

fn transfer_timed_out(
    started: std::time::Instant,
    last_progress: std::time::Instant,
    stall: Duration,
    total: Duration,
) -> bool {
    started.elapsed() >= total || last_progress.elapsed() >= stall
}

/// Read up to `max_size` bytes from a pipe without unbounded blocking: the
/// fd is switched to non-blocking and the loop aborts when no byte arrives
/// within `stall` or the transfer exceeds `total`. Returns Ok(None) when
/// the payload exceeds `max_size`.
fn read_pipe_bounded(
    fd: &OwnedFd,
    max_size: usize,
    stall: Duration,
    total: Duration,
) -> std::io::Result<Option<Vec<u8>>> {
    use std::io::Read;

    set_nonblocking(fd.as_raw_fd())?;
    let started = std::time::Instant::now();
    let mut last_progress = started;
    let mut data = Vec::new();
    let mut chunk = [0u8; 65536];
    let mut file = std::fs::File::from(fd.try_clone()?);
    loop {
        if transfer_timed_out(started, last_progress, stall, total) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "clipboard transfer stalled",
            ));
        }
        if !poll_fd(fd.as_raw_fd(), libc::POLLIN)? {
            continue;
        }
        match file.read(&mut chunk) {
            Ok(0) => return Ok(Some(data)),
            Ok(n) => {
                if data.len().saturating_add(n) > max_size {
                    return Ok(None);
                }
                data.extend_from_slice(&chunk[..n]);
                last_progress = std::time::Instant::now();
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::Interrupted =>
            {
                continue;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Write all of `data` into a pipe without unbounded blocking, with the
/// same deadline discipline as [`read_pipe_bounded`].
fn write_pipe_bounded(
    fd: &OwnedFd,
    data: &[u8],
    stall: Duration,
    total: Duration,
) -> std::io::Result<()> {
    use std::io::Write;

    set_nonblocking(fd.as_raw_fd())?;
    let started = std::time::Instant::now();
    let mut last_progress = started;
    let mut written = 0;
    let mut file = std::fs::File::from(fd.try_clone()?);
    while written < data.len() {
        if transfer_timed_out(started, last_progress, stall, total) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "clipboard transfer stalled",
            ));
        }
        if !poll_fd(fd.as_raw_fd(), libc::POLLOUT)? {
            continue;
        }
        match file.write(&data[written..]) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "clipboard peer stopped reading",
                ))
            }
            Ok(n) => {
                written += n;
                last_progress = std::time::Instant::now();
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::Interrupted =>
            {
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Read data from a clipboard offer via pipe.
fn read_offer_data(
    offer: &zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
    mime: &str,
    conn: &Connection,
) -> Option<Vec<u8>> {
    let mut fds = [0i32; 2];
    // O_CLOEXEC: without it the write end leaks into children spawned
    // elsewhere in the process (session hooks), and the read side never
    // sees EOF while such a child lives.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        tracing::warn!("Clipboard: pipe2() failed");
        return None;
    }
    let read_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };

    offer.receive(mime.to_string(), write_fd.as_fd());
    let _ = conn.flush();
    drop(write_fd);

    match read_pipe_bounded(
        &read_fd,
        MAX_CLIPBOARD_SIZE,
        PIPE_STALL_TIMEOUT,
        PIPE_TOTAL_TIMEOUT,
    ) {
        Ok(Some(data)) => Some(data),
        Ok(None) => {
            tracing::warn!(
                max = MAX_CLIPBOARD_SIZE,
                "Clipboard: offer data too large, ignoring"
            );
            None
        }
        Err(e) => {
            tracing::warn!(
                "Clipboard: failed to read offer data (mime={}): {}",
                mime,
                e
            );
            None
        }
    }
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
                        write_source_data(&fd, &data);
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

fn write_source_data(fd: &OwnedFd, data: &[u8]) -> bool {
    match write_pipe_bounded(fd, data, PIPE_STALL_TIMEOUT, PIPE_TOTAL_TIMEOUT) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!("Clipboard: failed to write source data: {}", e);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};

    use super::*;

    const FAST_STALL: Duration = Duration::from_millis(300);
    const FAST_TOTAL: Duration = Duration::from_secs(3);

    fn pipe_pair() -> (OwnedFd, OwnedFd) {
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        (unsafe { OwnedFd::from_raw_fd(fds[0]) }, unsafe {
            OwnedFd::from_raw_fd(fds[1])
        })
    }

    #[test]
    fn pipe_read_returns_data_when_the_writer_closes() {
        let (read_fd, write_fd) = pipe_pair();
        let writer = std::thread::spawn(move || {
            let mut file = std::fs::File::from(write_fd);
            file.write_all(b"clipboard payload").unwrap();
        });

        let data = read_pipe_bounded(&read_fd, 1024, FAST_STALL, FAST_TOTAL)
            .unwrap()
            .unwrap();

        assert_eq!(data, b"clipboard payload");
        writer.join().unwrap();
    }

    #[test]
    fn pipe_read_times_out_when_the_writer_stalls_mid_transfer() {
        // A source that writes part of the payload and never closes used to
        // block read_to_end forever and wedge the watcher thread.
        let (read_fd, write_fd) = pipe_pair();
        let mut file = std::fs::File::from(write_fd.try_clone().unwrap());
        file.write_all(b"partial").unwrap();

        let error = read_pipe_bounded(&read_fd, 1024, FAST_STALL, FAST_TOTAL).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        drop(write_fd);
    }

    #[test]
    fn pipe_read_rejects_oversized_payloads() {
        let (read_fd, write_fd) = pipe_pair();
        let writer = std::thread::spawn(move || {
            let mut file = std::fs::File::from(write_fd);
            file.write_all(&[0u8; 2048]).unwrap();
        });

        assert!(read_pipe_bounded(&read_fd, 1024, FAST_STALL, FAST_TOTAL)
            .unwrap()
            .is_none());
        writer.join().unwrap();
    }

    #[test]
    fn pipe_read_survives_a_slow_but_progressing_writer() {
        let (read_fd, write_fd) = pipe_pair();
        let writer = std::thread::spawn(move || {
            let mut file = std::fs::File::from(write_fd);
            for chunk in [b"slow".as_slice(), b"-", b"drip"] {
                file.write_all(chunk).unwrap();
                std::thread::sleep(Duration::from_millis(100));
            }
        });

        let data = read_pipe_bounded(&read_fd, 1024, FAST_STALL, FAST_TOTAL)
            .unwrap()
            .unwrap();

        assert_eq!(data, b"slow-drip");
        writer.join().unwrap();
    }

    #[test]
    fn pipe_write_completes_when_the_reader_drains() {
        let (read_fd, write_fd) = pipe_pair();
        let payload = vec![7u8; 256 * 1024]; // larger than the default pipe buffer
        let reader = std::thread::spawn(move || {
            let mut file = std::fs::File::from(read_fd);
            let mut out = Vec::new();
            file.read_to_end(&mut out).unwrap();
            out
        });

        write_pipe_bounded(&write_fd, &payload, FAST_STALL, FAST_TOTAL).unwrap();
        drop(write_fd);

        assert_eq!(reader.join().unwrap(), payload);
    }

    #[test]
    fn pipe_write_times_out_when_the_reader_stops_reading() {
        // A paste target that stops draining its pipe used to block
        // write_all forever once the payload exceeded the pipe buffer.
        let (read_fd, write_fd) = pipe_pair();
        let payload = vec![7u8; 256 * 1024];

        let error = write_pipe_bounded(&write_fd, &payload, FAST_STALL, FAST_TOTAL).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        drop(read_fd);
    }

    #[test]
    fn pipe_write_fails_fast_when_the_reader_is_gone() {
        let (read_fd, write_fd) = pipe_pair();
        drop(read_fd);

        assert!(write_pipe_bounded(&write_fd, b"clipboard", FAST_STALL, FAST_TOTAL).is_err());
    }

    #[test]
    fn source_data_writer_reports_both_outcomes() {
        let (read_fd, write_fd) = pipe_pair();
        let reader = std::thread::spawn(move || {
            let mut file = std::fs::File::from(read_fd);
            let mut out = Vec::new();
            file.read_to_end(&mut out).unwrap();
            out
        });
        assert!(write_source_data(&write_fd, b"clipboard"));
        drop(write_fd);
        assert_eq!(reader.join().unwrap(), b"clipboard");

        let (read_fd, write_fd) = pipe_pair();
        drop(read_fd);
        assert!(!write_source_data(&write_fd, b"clipboard"));
    }
}
