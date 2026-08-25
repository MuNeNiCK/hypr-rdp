use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ironrdp_cliprdr::pdu::{ClipboardFormat, ClipboardFormatId};
use ironrdp_server::ServerEvent;
use tokio::sync::mpsc;
use wayland_client::backend::ObjectId;
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1, zwlr_data_control_manager_v1, zwlr_data_control_offer_v1,
    zwlr_data_control_source_v1,
};

use super::backend::{announce_local_formats, ClipboardEchoCandidate};
use super::formats::{
    PendingWrite, IMAGE_PNG_MIME, MAX_CLIPBOARD_SIZE, TEXT_MIME, TEXT_PLAIN_MIME, UTF8_MIME,
};

const DATA_CONTROL_VERSION: u32 = 1;

fn data_control_manager_version(advertised_version: u32) -> u32 {
    advertised_version.min(DATA_CONTROL_VERSION)
}

pub(super) fn clipboard_thread(
    event_sender: mpsc::UnboundedSender<ServerEvent>,
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
        clipboard_data,
        clipboard_image,
        pending_write,
        echo_candidate,
    );

    let wayland_fd = conn.as_fd().as_raw_fd();
    let ready = dispatch_until(
        &conn,
        &mut event_queue,
        &mut state,
        wayland_fd,
        Instant::now() + COMPOSITOR_REPLY_TIMEOUT,
        |state| state.manager.is_some() && state.seat.is_some(),
    )?;

    // Say what was missing, not just that we waited. A compositor without
    // wlr-data-control answers its registry immediately and simply never
    // advertises the global, which is the common case here and is not a
    // compositor failing to answer.
    if !ready {
        let mut missing = Vec::new();
        if state.manager.is_none() {
            missing.push("zwlr_data_control_manager_v1");
        }
        if state.seat.is_none() {
            missing.push("wl_seat");
        }
        anyhow::bail!(
            "clipboard: {} not advertised within {:?}",
            missing.join(" and "),
            COMPOSITOR_REPLY_TIMEOUT
        );
    }

    let manager = state.manager.as_ref().expect("checked above").clone();

    let seat = state.seat.as_ref().expect("checked above").clone();

    let device = manager.get_data_device(&seat, &qh, ());
    state.device = Some(device);

    tracing::info!("Clipboard: wlr-data-control-v1 device bound");

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
        if let Some(pending) = state.take_pending_write() {
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
            // Push the request out and carry on. The echo it provokes is
            // handled by the poll below like any other event; waiting for it
            // here was a wait with no deadline, and a compositor that stopped
            // answering held this thread -- and the clipboard with it -- for as
            // long as it stayed stuck.
            conn.flush().map_err(|e| {
                anyhow::anyhow!("clipboard: flush after set_selection failed: {}", e)
            })?;
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
    /// Deadline until which a Selection event is our own echo and is swallowed
    /// rather than announced to the client as a local copy.
    ///
    /// Owned by this thread alone. It used to be an `AtomicBool` shared with
    /// the backend, which set it from the RDP thread and left this thread to
    /// clear it -- so a write that never reached the compositor left local
    /// copies silently suppressed for the rest of the session.
    suppress_echo_until: Option<Instant>,
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
        clipboard_data: Arc<Mutex<Option<Vec<u8>>>>,
        clipboard_image: Arc<Mutex<Option<Vec<u8>>>>,
        pending_write: Arc<Mutex<Option<PendingWrite>>>,
        echo_candidate: Arc<Mutex<Option<ClipboardEchoCandidate>>>,
    ) -> Self {
        Self {
            event_sender,
            suppress_echo_until: None,
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

    /// Take the write the RDP side left, arming echo suppression with it.
    ///
    /// One step rather than two calls, because a take that does not arm is the
    /// defect this state exists to prevent: everything below provokes Selection
    /// events that are our own, and one we fail to recognise is announced back
    /// to the client as somebody else's copy. Nothing arms when there is
    /// nothing to write.
    ///
    /// Armed before the destroy below, not just around `set_selection`:
    /// dropping a source we still hold the selection with may make the
    /// compositor clear the selection first, and that `selection(null)` is our
    /// own doing as much as the echo is. The protocol does not say either way,
    /// so the count of events one write provokes is not something to build on.
    fn take_pending_write(&mut self) -> Option<PendingWrite> {
        let pending = self.pending_write.lock().ok().and_then(|mut g| g.take())?;
        self.arm_echo_suppression();
        Some(pending)
    }

    /// Start swallowing Selection events, from before the calls that provoke
    /// them.
    fn arm_echo_suppression(&mut self) {
        self.suppress_echo_until = Some(Instant::now() + ECHO_SUPPRESSION_WINDOW);
    }

    /// Whether a Selection event arriving now is our own echo.
    ///
    /// Deliberately does not consume the suppression. One write may provoke
    /// more than one event -- destroying the previous source can clear the
    /// selection before our own echo arrives -- and consuming on the first
    /// would send the second
    /// into `read_offer_data` against our own data source. Only this thread
    /// answers that source's `send`, and it would be sitting in the read, so
    /// the transfer could not finish until the read timed out.
    fn echo_suppressed(&self) -> bool {
        self.suppress_echo_until
            .is_some_and(|deadline| Instant::now() < deadline)
    }

    /// Whether this Selection event is one our own write provoked -- and, if
    /// so, note that the compositor is answering.
    ///
    /// The two belong together. Checking without collapsing spends the whole
    /// backstop on every write; collapsing without checking would shorten a
    /// window that no event has justified shortening.
    fn selection_is_our_echo(&mut self) -> bool {
        if !self.echo_suppressed() {
            return false;
        }
        self.shrink_echo_suppression();
        true
    }

    /// Collapse the window to the grace period, because the compositor has
    /// just proved it is answering. Never extends a deadline.
    fn shrink_echo_suppression(&mut self) {
        let grace = Instant::now() + ECHO_SUPPRESSION_GRACE;
        if let Some(deadline) = self.suppress_echo_until {
            if grace < deadline {
                self.suppress_echo_until = Some(grace);
            }
        }
    }
}

/// Dispatch events until `ready` holds, or until the deadline passes.
///
/// `EventQueue::roundtrip` waits for the compositor with no deadline, and the
/// backend joins this thread on drop -- so a compositor that stops answering
/// takes the server down with it. This is the poll the thread loop already
/// runs, with a deadline on top.
fn dispatch_until(
    conn: &Connection,
    event_queue: &mut EventQueue<ClipState>,
    state: &mut ClipState,
    wayland_fd: std::os::fd::RawFd,
    deadline: Instant,
    ready: impl Fn(&ClipState) -> bool,
) -> anyhow::Result<bool> {
    loop {
        loop {
            let n = event_queue
                .dispatch_pending(state)
                .map_err(|e| anyhow::anyhow!("clipboard: dispatch_pending failed: {}", e))?;
            if n == 0 {
                break;
            }
        }
        if ready(state) {
            return Ok(true);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            // Not an error of its own: the caller knows what it was waiting
            // for and can say so, which a bare timeout cannot.
            return Ok(false);
        }
        conn.flush()
            .map_err(|e| anyhow::anyhow!("clipboard: flush failed: {}", e))?;
        let Some(guard) = event_queue.prepare_read() else {
            continue;
        };
        let mut pollfd = libc::pollfd {
            fd: wayland_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout_ms = remaining.as_millis().min(i32::MAX as u128) as libc::c_int;
        let ret = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
        if ret > 0 {
            guard
                .read()
                .map_err(|e| anyhow::anyhow!("clipboard: read failed: {}", e))?;
        } else {
            drop(guard);
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
                    // Primary-selection support starts at v2. Stay on v1 until
                    // this backend handles its offer lifecycle.
                    state.manager =
                        Some(registry.bind(name, data_control_manager_version(version), qh, ()));
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
                if state.selection_is_our_echo() {
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
            _ => {}
        }
    }
}

/// No-progress and overall deadlines for clipboard pipe transfers. The
/// watcher thread services these transfers inline, and the backend joins
/// that thread on drop — an unbounded read or write here therefore wedges
/// the whole server, so both directions must always terminate.
const PIPE_STALL_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a Selection event still counts as the echo of our own
/// `set_selection`, and how long we wait for the compositor to announce its
/// globals at startup.
///
/// Both are the same question -- how long before a compositor that has not
/// answered is not going to -- so they are the same number as the stalled-pipe
/// timeout above rather than a third one. A compositor answers in microseconds;
/// the window exists only so that one which never answers cannot suppress local
/// copies for the rest of the session.
const ECHO_SUPPRESSION_WINDOW: Duration = PIPE_STALL_TIMEOUT;

/// What is left of the window once the compositor has answered at all.
///
/// The rest of a write's burst follows its first event immediately, so there is
/// no reason to keep swallowing Selection events for the full backstop after
/// that. Without this the window is spent in full every time, and a client that
/// writes more often than the window is long keeps local copies suppressed for
/// as long as it cares to -- the very failure this is supposed to prevent, just
/// driven from the other side.
const ECHO_SUPPRESSION_GRACE: Duration = Duration::from_millis(100);
const COMPOSITOR_REPLY_TIMEOUT: Duration = PIPE_STALL_TIMEOUT;
const PIPE_TOTAL_TIMEOUT: Duration = Duration::from_secs(20);

fn pipe_cloexec() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }

    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

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
    let (read_fd, write_fd) = match pipe_cloexec() {
        Ok(fds) => fds,
        Err(e) => {
            tracing::warn!("Clipboard: failed to create pipe: {}", e);
            return None;
        }
    };

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

    fn clip_state() -> ClipState {
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        ClipState::new(
            event_tx,
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(None)),
        )
    }

    #[test]
    fn nothing_is_suppressed_until_we_write() {
        assert!(!clip_state().selection_is_our_echo());
    }

    #[test]
    fn arming_uses_the_window_the_constant_names() {
        let mut state = clip_state();
        let floor = Instant::now() + ECHO_SUPPRESSION_WINDOW;
        state.arm_echo_suppression();
        let ceiling = Instant::now() + ECHO_SUPPRESSION_WINDOW;

        let deadline = state.suppress_echo_until.expect("arming sets a deadline");
        assert!(
            deadline >= floor && deadline <= ceiling,
            "the deadline has to come from the constant, not from somewhere else"
        );
    }

    #[test]
    fn one_write_suppresses_every_event_it_provokes_not_just_the_first() {
        let mut state = clip_state();
        state.arm_echo_suppression();

        // Dropping the source we held the selection with may clear the
        // selection before our own set_selection echoes back, and the protocol
        // does not say how many events one write produces -- so the check must
        // not spend itself on the first one.
        assert!(state.selection_is_our_echo(), "the first event is ours");
        assert!(state.selection_is_our_echo(), "and so is the one behind it");
    }

    #[test]
    fn the_first_answer_collapses_the_window_to_the_grace() {
        let mut state = clip_state();
        state.arm_echo_suppression();

        // The compositor has answered, so the rest of the burst is already on
        // its way; holding the full backstop open past that is what lets a
        // write-happy client keep local copies suppressed indefinitely.
        assert!(state.selection_is_our_echo());

        let deadline = state.suppress_echo_until.expect("still armed");
        assert!(
            deadline <= Instant::now() + ECHO_SUPPRESSION_GRACE,
            "the window stayed open past the compositor's first answer"
        );
        assert!(
            ECHO_SUPPRESSION_GRACE * 4 < ECHO_SUPPRESSION_WINDOW,
            "a grace this close to the backstop collapses nothing"
        );
        // The backstop covers a compositor that has gone quiet. It has no
        // business outlasting the time we are already willing to sit on a
        // stalled pipe -- an oversized window here is indistinguishable from
        // the latch this change removed.
        assert!(
            ECHO_SUPPRESSION_WINDOW <= PIPE_STALL_TIMEOUT,
            "the backstop outgrew the budget it was derived from"
        );
        // And absolute ceilings, because the window is expressed relative to
        // PIPE_STALL_TIMEOUT: raising that one constant would widen it with
        // every test still green. COMPOSITOR_REPLY_TIMEOUT is an alias of the
        // same constant, so it needs no line of its own. These are the seconds
        // a user spends watching a clipboard do nothing.
        assert!(PIPE_STALL_TIMEOUT <= Duration::from_secs(5));
        assert!(PIPE_TOTAL_TIMEOUT <= Duration::from_secs(30));
        assert!(ECHO_SUPPRESSION_GRACE <= Duration::from_millis(200));
    }

    /// The property #31 asserted through `suppress_watcher` and this change
    /// moved onto the Wayland thread: a write from the RDP side arms echo
    /// suppression. It is here rather than in the thread loop because that is
    /// where the take now happens, and a take without an arm is the whole bug.
    #[test]
    fn taking_the_rdp_write_arms_echo_suppression() {
        for pending in [
            PendingWrite::Text(b"pasted from the client".to_vec()),
            PendingWrite::Image(vec![0u8; 16]),
        ] {
            let mut state = clip_state();
            assert!(state.take_pending_write().is_none());
            assert!(
                !state.echo_suppressed(),
                "nothing was written, so nothing may be swallowed"
            );

            *state.pending_write.lock().unwrap() = Some(pending);

            assert!(state.take_pending_write().is_some());
            assert!(
                state.echo_suppressed(),
                "the Selection events this write provokes are ours"
            );
            assert!(
                state.pending_write.lock().unwrap().is_none(),
                "the write is taken, not left for the next pass"
            );
        }
    }

    #[test]
    fn shrinking_never_extends_a_deadline() {
        let mut state = clip_state();
        // Any deadline inside the grace exercises this; take the largest one,
        // because everything up to it is wall-clock budget for reaching the
        // call below. A literal few milliseconds would test the same property
        // on a machine that never stalls and flake on a loaded CI runner.
        state.suppress_echo_until =
            Some(Instant::now() + ECHO_SUPPRESSION_GRACE - Duration::from_millis(1));
        let before = state.suppress_echo_until;

        assert!(state.selection_is_our_echo());

        assert_eq!(
            state.suppress_echo_until, before,
            "a deadline shorter than the grace must be left alone"
        );
    }

    #[test]
    fn a_lapsed_window_stops_swallowing_selections() {
        let mut state = clip_state();
        state.arm_echo_suppression();
        // A compositor that never answers must not silence local copies for the
        // rest of the session: past the deadline a Selection is somebody's copy
        // again, not our echo.
        state.suppress_echo_until = Some(Instant::now() - Duration::from_millis(1));

        assert!(!state.selection_is_our_echo());
    }

    fn pipe_pair() -> (OwnedFd, OwnedFd) {
        pipe_cloexec().unwrap()
    }

    #[test]
    fn clipboard_pipe_ends_are_close_on_exec() {
        let (read_fd, write_fd) = pipe_pair();

        for fd in [&read_fd, &write_fd] {
            let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
            assert_ne!(flags, -1);
            assert_ne!(flags & libc::FD_CLOEXEC, 0);
        }
    }

    #[test]
    fn transfer_timeout_enforces_total_duration_despite_recent_progress() {
        let now = std::time::Instant::now();

        assert!(transfer_timed_out(
            now - Duration::from_secs(2),
            now,
            Duration::from_secs(10),
            Duration::from_secs(1),
        ));
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

    #[test]
    fn data_control_manager_version_is_limited_to_used_v1_surface() {
        assert_eq!(data_control_manager_version(1), 1);
        assert_eq!(data_control_manager_version(2), 1);
        assert_eq!(data_control_manager_version(u32::MAX), 1);
    }
}
