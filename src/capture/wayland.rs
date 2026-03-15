use std::num::{NonZeroU16, NonZeroUsize};
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::unix::io::OwnedFd;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use ironrdp_server::{BitmapUpdate, DisplayUpdate, PixelFormat};
use tokio::sync::mpsc;

use super::CaptureMode;
use crate::egfx::EgfxShared;
use crate::input::SharedOutputLayout;
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{delegate_noop, Connection, Dispatch, Proxy, QueueHandle, WEnum};
use wayland_protocols::ext::image_capture_source::v1::client::ext_image_capture_source_v1;
use wayland_protocols::ext::image_capture_source::v1::client::ext_output_image_capture_source_manager_v1;
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1, ext_image_copy_capture_manager_v1,
    ext_image_copy_capture_session_v1,
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1,
};
#[cfg(feature = "vaapi")]
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1,
};

struct MmapRegion {
    ptr: *mut libc::c_void,
    len: usize,
}

impl MmapRegion {
    fn new(fd: std::os::unix::io::RawFd, len: usize) -> Result<Self> {
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            bail!("mmap failed");
        }
        Ok(Self { ptr, len })
    }

    fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr as *const u8, self.len) }
    }
}

impl Drop for MmapRegion {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr, self.len);
        }
    }
}

pub struct CaptureInfo {
    pub width: u32,
    pub height: u32,
    /// Name of the output being captured
    pub output_name: String,
}

pub(crate) struct HeadlessOutputGuard {
    name: Option<String>,
}

impl HeadlessOutputGuard {
    fn new(name: String) -> Self {
        Self { name: Some(name) }
    }
}

impl Drop for HeadlessOutputGuard {
    fn drop(&mut self) {
        if let Some(name) = self.name.take() {
            remove_headless_output(&name);
        }
    }
}

const HEADLESS_PREFIX: &str = "hypr-rdp";

/// List headless outputs created by hypr-rdp (name starts with "hypr-rdp-").
pub(crate) fn list_stale_headless_outputs() -> Result<Vec<String>> {
    let monitors = crate::hyprland::monitors()?;
    let arr = monitors.as_array().context("expected monitors array")?;
    Ok(arr
        .iter()
        .filter_map(|m| {
            let name = m["name"].as_str()?;
            name.starts_with(HEADLESS_PREFIX).then(|| name.to_string())
        })
        .collect())
}

/// Create a headless output in Hyprland at the given resolution.
/// Returns the output name and RAII guard that removes it on drop.
/// The guard is created immediately after the output appears so that
/// any subsequent failure (e.g., keyword_monitor) cleans up automatically.
pub(crate) fn create_headless_output(width: u32, height: u32) -> Result<(String, HeadlessOutputGuard)> {
    // Subscribe to events BEFORE creating the output to catch monitoradded.
    // The ensure_registered() roundtrip guarantees Hyprland has accept()'ed
    // our socket2 connection before we trigger the creation.
    let mut events = crate::hyprland::EventStream::connect()?;
    events.ensure_registered()?;

    crate::hyprland::output_create_headless(HEADLESS_PREFIX)
        .context("failed to create headless output")?;

    // Wait for monitoradded event — data is the output name.
    let name = loop {
        let candidate = events
            .wait_for("monitoradded", Duration::from_secs(5))
            .context("failed to detect new headless output")?;
        if candidate.starts_with(HEADLESS_PREFIX) {
            break candidate;
        }
        tracing::debug!(name = %candidate, "Ignoring unrelated monitoradded event");
    };

    // Guard created immediately — any failure below will clean up the output
    let guard = HeadlessOutputGuard::new(name.clone());

    // Set resolution
    let mode = format!("{}x{}@60", width, height);
    let rule = format!("{},{},-9999x0,1", name, mode);
    crate::hyprland::keyword_monitor(&rule)
        .context("failed to set headless output resolution")?;

    tracing::info!(name = %name, width, height, "Created headless output");
    Ok((name, guard))
}

/// Remove a headless output from Hyprland.
pub(crate) fn remove_headless_output(name: &str) {
    match crate::hyprland::output_remove(name) {
        Ok(()) => {
            tracing::info!(name, "Removed headless output");
        }
        Err(e) => {
            tracing::warn!(name, error = %e, "Failed to remove headless output");
        }
    }
}

/// Wait for a Hyprland output to be ready (has non-zero dimensions).
pub(crate) fn wait_for_output(output_name: &str, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    let poll_interval = Duration::from_millis(100);

    loop {
        if let Ok(monitors) = crate::hyprland::monitors() {
            if let Some(arr) = monitors.as_array() {
                let found = arr.iter().any(|m| {
                    m["name"].as_str() == Some(output_name)
                        && m["width"].as_i64().unwrap_or(0) > 0
                });
                if found {
                    return Ok(());
                }
            }
        }

        if start.elapsed() >= timeout {
            bail!(
                "timed out waiting for output '{}' after {}ms",
                output_name,
                timeout.as_millis()
            );
        }

        std::thread::sleep(poll_interval);
    }
}

/// Verify that a named output exists in Hyprland monitors.
fn verify_output_exists(output_name: &str) -> Result<()> {
    let monitors = crate::hyprland::monitors()?;
    let found = monitors
        .as_array()
        .context("expected monitors array")?
        .iter()
        .any(|m| m["name"].as_str() == Some(output_name));
    if !found {
        bail!("output '{}' not found in Hyprland monitors", output_name);
    }
    Ok(())
}

/// Start screen capture on a background thread.
/// Start capture on the given output name.
/// The caller is responsible for creating/managing the headless output.
#[allow(clippy::too_many_arguments)]
pub async fn start_capture(
    tx: mpsc::Sender<DisplayUpdate>,
    capture_mode: CaptureMode,
    egfx_shared: Option<Arc<EgfxShared>>,
    output_layout: Arc<SharedOutputLayout>,
    bitrate: u32,
    quality: u8,
    fps: u32,
    output_name: String,
    deferred_resize: Option<ironrdp_server::DesktopSize>,
) -> Result<CaptureInfo> {
    let (info_tx, info_rx) = tokio::sync::oneshot::channel();

    std::thread::Builder::new()
        .name("wayland-capture".into())
        .spawn(move || {
            if let Err(e) = capture_thread(
                tx,
                info_tx,
                capture_mode,
                egfx_shared,
                output_layout,
                bitrate,
                quality,
                fps,
                output_name,
                deferred_resize,
            ) {
                tracing::error!("Capture thread error: {:#}", e);
            }
        })?;

    info_rx.await.context("capture thread failed to start")?
}

#[allow(clippy::too_many_arguments)]
fn capture_thread(
    tx: mpsc::Sender<DisplayUpdate>,
    info_tx: tokio::sync::oneshot::Sender<Result<CaptureInfo>>,
    capture_mode: CaptureMode,
    egfx_shared: Option<Arc<EgfxShared>>,
    output_layout: Arc<SharedOutputLayout>,
    bitrate: u32,
    quality: u8,
    fps: u32,
    output_name: String,
    deferred_resize: Option<ironrdp_server::DesktopSize>,
) -> Result<()> {
    let mut info_tx = Some(info_tx);
    let result = capture_thread_inner(
        tx,
        &mut info_tx,
        capture_mode,
        egfx_shared,
        output_layout,
        bitrate,
        quality,
        fps,
        output_name,
        deferred_resize,
    );
    if let Err(err) = result {
        if let Some(tx) = info_tx.take() {
            let _ = tx.send(Err(anyhow::anyhow!("{:#}", err)));
        }
        return Err(err);
    }
    Ok(())
}

fn create_shm_fd(size: usize) -> Result<OwnedFd> {
    let fd = unsafe { libc::memfd_create(c"hypr-rdp-shm".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        bail!("memfd_create failed");
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let ret = unsafe { libc::ftruncate(fd.as_raw_fd(), size as libc::off_t) };
    if ret < 0 {
        bail!("ftruncate failed");
    }
    Ok(fd)
}

#[allow(clippy::too_many_arguments)]
fn capture_thread_inner(
    tx: mpsc::Sender<DisplayUpdate>,
    info_tx: &mut Option<tokio::sync::oneshot::Sender<Result<CaptureInfo>>>,
    capture_mode: CaptureMode,
    egfx_shared: Option<Arc<EgfxShared>>,
    output_layout: Arc<SharedOutputLayout>,
    bitrate: u32,
    quality: u8,
    fps: u32,
    output_name: String,
    deferred_resize: Option<ironrdp_server::DesktopSize>,
) -> Result<()> {
    verify_output_exists(&output_name)?;

    output_layout
        .update_from_output(&output_name)
        .context("failed to refresh input layout for headless output")?;

    let conn = Connection::connect_to_env().context("failed to connect to Wayland display")?;
    let mut event_queue = conn.new_event_queue::<AppState>();
    let qh = event_queue.handle();

    let display = conn.display();
    let _registry = display.get_registry(&qh, ());

    let mut state = AppState::new(tx, output_name.clone());

    // First roundtrip: collect globals
    event_queue
        .roundtrip(&mut state)
        .context("Wayland roundtrip failed")?;

    // Second roundtrip: get output names
    event_queue
        .roundtrip(&mut state)
        .context("Wayland roundtrip (2nd) failed")?;

    let wl_output = state
        .target_output
        .as_ref()
        .context(format!(
            "output '{}' not found in Wayland globals",
            state.target_output_name
        ))?
        .clone();
    let shm = state.shm.as_ref().context("wl_shm not available")?.clone();

    match capture_mode {
        CaptureMode::Ext => capture_loop_ext(
            &conn, &mut event_queue, &mut state, &qh, &wl_output, &shm,
            &output_name, egfx_shared, info_tx, bitrate, quality, fps, deferred_resize,
        ),
        CaptureMode::Wlr => {
            let screencopy_mgr = state
                .screencopy_manager
                .as_ref()
                .context("zwlr_screencopy_manager_v1 not available")?
                .clone();
            capture_loop_wlr(
                &conn, &mut event_queue, &mut state, &qh, &wl_output, &shm,
                &screencopy_mgr, &output_name, egfx_shared, info_tx, bitrate, quality, fps,
                deferred_resize,
            )
        }
    }
}

/// Maximum consecutive encode failures before falling back to software encoder.
const MAX_ENCODE_FAILURES: u32 = 5;

/// Poll timeout (ms) for Wayland event dispatch. Controls shutdown responsiveness.
const POLL_TIMEOUT_MS: i32 = 100;

/// Poll-based Wayland event dispatch with timeout.
///
/// Dispatches any already-queued events, then polls the Wayland fd for new events
/// up to `timeout_ms`. This allows the capture thread to check shutdown conditions
/// periodically instead of blocking indefinitely in `blocking_dispatch`.
fn poll_dispatch(
    event_queue: &mut wayland_client::EventQueue<AppState>,
    state: &mut AppState,
    timeout_ms: i32,
) -> Result<()> {
    // Dispatch any already-queued events
    event_queue
        .dispatch_pending(state)
        .context("dispatch_pending failed")?;

    // Try to read more events from the Wayland socket
    if let Some(guard) = event_queue.prepare_read() {
        let fd = guard.connection_fd();
        let mut pollfd = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ret = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
        if ret > 0 {
            guard
                .read()
                .map_err(|e| anyhow::anyhow!("Wayland read: {}", e))?;
            event_queue
                .dispatch_pending(state)
                .context("dispatch_pending after read")?;
        } else if ret < 0 {
            let errno = std::io::Error::last_os_error();
            if errno.raw_os_error() != Some(libc::EINTR) {
                bail!("poll failed on Wayland fd: {}", errno);
            }
            // EINTR: interrupted by signal, retry next call
        }
        // ret == 0: timeout (guard dropped = cancel)
    }
    // prepare_read() returned None: events already dispatched above

    Ok(())
}

/// Common frame processing: EGFX H.264 encoding or bitmap fallback.
struct FrameProcessor {
    egfx_shared: Option<Arc<EgfxShared>>,
    h264_encoder: Option<crate::egfx::FrameEncoder>,
    egfx_handle: Option<ironrdp_server::GfxServerHandle>,
    egfx_sender: Option<tokio::sync::mpsc::UnboundedSender<ironrdp_server::ServerEvent>>,
    egfx_surface_id: Option<u16>,
    egfx_active: bool,
    egfx_ready: bool,
    egfx_generation: u32,
    width: u32,
    height: u32,
    pixel_format: PixelFormat,
    stride: u32,
    bitrate: u32,
    quality: u8,
    fps: u32,
    /// Whether we've sent at least one frame (first frame always sent)
    sent_first_frame: bool,
    deferred_resize: Option<ironrdp_server::DesktopSize>,
    /// Consecutive encode failure count for runtime VAAPI -> software fallback.
    encode_failures: u32,
}

impl FrameProcessor {
    #[allow(clippy::too_many_arguments)]
    fn new(
        egfx_shared: Option<Arc<EgfxShared>>,
        width: u32, height: u32,
        pixel_format: PixelFormat, stride: u32,
        bitrate: u32, quality: u8, fps: u32,
        deferred_resize: Option<ironrdp_server::DesktopSize>,
    ) -> Self {
        Self {
            egfx_shared, h264_encoder: None, egfx_handle: None,
            egfx_sender: None, egfx_surface_id: None,
            egfx_active: false,
            egfx_ready: false, egfx_generation: 0,
            width, height, pixel_format, stride,
            bitrate, quality, fps,
            sent_first_frame: false,
            deferred_resize,
            encode_failures: 0,
        }
    }

    /// Process a captured frame. Returns true if the capture loop should continue.
    /// `has_damage` indicates whether damage was reported; false means no change (skip frame).
    fn process(
        &mut self,
        data: &[u8],
        tx: &mpsc::Sender<DisplayUpdate>,
        has_damage: bool,
    ) -> bool {
        // Skip frames with no damage (except the very first frame)
        if self.sent_first_frame && !has_damage {
            return true;
        }

        let mut sent_via_egfx = false;
        if let Some(shared) = &self.egfx_shared {
            let ready = shared.is_ready();
            let gen = shared.generation();

            if ready != self.egfx_ready {
                self.egfx_ready = ready;
                if !ready {
                    self.egfx_active = false;
                    self.egfx_handle = None;
                    self.egfx_sender = None;
                    self.egfx_surface_id = None;
                    self.h264_encoder = None;
                    tracing::info!("EGFX channel became unavailable");
                }
            }

            if gen != self.egfx_generation {
                self.egfx_generation = gen;
                self.egfx_surface_id = None;
                self.h264_encoder = None;
                if ready {
                    match crate::egfx::FrameEncoder::new(self.width, self.height, self.bitrate, self.fps) {
                        Ok(enc) => {
                            tracing::info!(
                                width = self.width, height = self.height,
                                backend = enc.backend_name(), gen,
                                bitrate = self.bitrate,
                                "H.264 encoder initialized"
                            );
                            self.h264_encoder = Some(enc);
                        }
                        Err(e) => tracing::warn!("Failed to initialize H.264 encoder: {:#}", e),
                    }
                }
            }

            if ready && !self.egfx_active {
                self.egfx_handle = shared.get_handle();
                self.egfx_sender = shared.get_event_sender();
                if self.h264_encoder.is_some() && self.egfx_handle.is_some() && self.egfx_sender.is_some() {
                    self.egfx_active = true;
                    tracing::info!("EGFX transport ready, switching to H.264 encoding");
                }
            }

            if self.egfx_active {
                // Surface initialization (separate borrow scope)
                if self.egfx_surface_id.is_none() {
                    if let (Some(handle), Some(sender)) = (&self.egfx_handle, &self.egfx_sender) {
                        if let Some(sid) =
                            EgfxShared::init_surface(handle, sender, self.width as u16, self.height as u16)
                        {
                            self.egfx_surface_id = Some(sid);
                        }
                    }
                }

                // Encode and send (encoder borrow released before fallback check)
                if let Some(sid) = self.egfx_surface_id {
                    let encode_result = self.h264_encoder.as_mut().map(|enc| enc.encode(data, self.stride as usize));
                    match encode_result {
                        Some(Ok(ref h264_data)) if h264_data.len() > 32 => {
                            self.encode_failures = 0;
                            if let (Some(handle), Some(sender)) = (&self.egfx_handle, &self.egfx_sender) {
                                let timestamp = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_millis() as u32;
                                sent_via_egfx = EgfxShared::send_frame(
                                    handle, sender, sid,
                                    self.width as u16, self.height as u16,
                                    h264_data, timestamp, self.quality,
                                );
                            }
                        }
                        Some(Ok(_)) => { self.encode_failures = 0; }
                        Some(Err(e)) => {
                            self.encode_failures += 1;
                            tracing::warn!(
                                failures = self.encode_failures,
                                max = MAX_ENCODE_FAILURES,
                                "H.264 encode failed: {:#}", e
                            );
                            if let Some(enc) = &mut self.h264_encoder {
                                enc.force_idr();
                            }
                        }
                        None => {}
                    }

                    // Dynamic fallback: VAAPI -> software after repeated failures
                    if self.encode_failures >= MAX_ENCODE_FAILURES
                        && self.h264_encoder.as_ref().is_some_and(|e| e.is_vaapi())
                    {
                        tracing::warn!(
                            "VA-API encode failed {} consecutive times, switching to software encoder",
                            self.encode_failures
                        );
                        match crate::egfx::FrameEncoder::new_software_only(
                            self.width, self.height, self.bitrate, self.fps,
                        ) {
                            Ok(enc) => {
                                self.h264_encoder = Some(enc);
                                self.encode_failures = 0;
                                self.egfx_surface_id = None; // Force surface re-init
                            }
                            Err(e) => {
                                tracing::error!("Software encoder fallback failed: {:#}", e);
                                self.h264_encoder = None;
                                self.egfx_active = false;
                            }
                        }
                    }
                }
            }
        }

        if sent_via_egfx {
            self.sent_first_frame = true;
            if let Some(size) = self.deferred_resize.take() {
                tracing::info!(width = size.width, height = size.height, "Sending deferred resize");
                let _ = tx.blocking_send(DisplayUpdate::Resize(size));
            }
        }

        // Only send bitmaps when EGFX is not expected. Sending bitmaps
        // before EGFX causes a bitmap→EGFX transition that breaks rendering
        // on some RDP clients (first connection after server startup).
        if !sent_via_egfx && self.egfx_shared.is_none() {
            self.sent_first_frame = true;
            let update = DisplayUpdate::Bitmap(BitmapUpdate {
                x: 0, y: 0,
                width: NonZeroU16::new(self.width as u16).expect("width is non-zero"),
                height: NonZeroU16::new(self.height as u16).expect("height is non-zero"),
                format: self.pixel_format,
                data: Bytes::copy_from_slice(data),
                stride: NonZeroUsize::new(self.stride as usize).expect("stride is non-zero"),
            });
            if tx.blocking_send(update).is_err() {
                tracing::info!("Display update channel closed");
                return false;
            }
        }
        true
    }
}

#[allow(clippy::too_many_arguments)]
fn capture_loop_ext(
    conn: &Connection,
    event_queue: &mut wayland_client::EventQueue<AppState>,
    state: &mut AppState,
    qh: &QueueHandle<AppState>,
    output: &wl_output::WlOutput,
    shm: &wl_shm::WlShm,
    output_name: &str,
    egfx_shared: Option<Arc<EgfxShared>>,
    info_tx: &mut Option<tokio::sync::oneshot::Sender<Result<CaptureInfo>>>,
    bitrate: u32,
    quality: u8,
    fps: u32,
    deferred_resize: Option<ironrdp_server::DesktopSize>,
) -> Result<()> {
    let capture_mgr = state
        .capture_manager
        .as_ref()
        .context("ext_image_copy_capture_manager_v1 not available")?
        .clone();
    let source_mgr = state
        .source_manager
        .as_ref()
        .context("ext_output_image_capture_source_manager_v1 not available")?
        .clone();

    let source = source_mgr.create_source(output, qh, ());
    let session = capture_mgr.create_session(
        &source,
        ext_image_copy_capture_manager_v1::Options::empty(),
        qh, (),
    );
    state.session = Some(session.clone());

    event_queue.roundtrip(state).context("failed to get buffer constraints")?;
    event_queue.roundtrip(state).context("failed to get buffer constraints (2nd)")?;

    let width = state.buffer_width;
    let height = state.buffer_height;
    if width == 0 || height == 0 {
        bail!("invalid buffer dimensions: {}x{}", width, height);
    }

    // Try DMA-BUF path if available (vaapi feature + compositor supports it + EGFX expected)
    #[cfg(feature = "vaapi")]
    if egfx_shared.is_some() {
        if let Some(ref dmabuf_result) = try_setup_dmabuf(state, qh, width, height) {
            match dmabuf_result {
                Ok(dmabuf_ctx) => {
                    if let Some(tx) = info_tx.take() {
                        let _ = tx.send(Ok(CaptureInfo {
                            width,
                            height,
                            output_name: output_name.to_string(),
                        }));
                    }
                    match capture_loop_ext_dmabuf(
                        conn,
                        event_queue,
                        state,
                        qh,
                        &session,
                        width,
                        height,
                        dmabuf_ctx,
                        egfx_shared.clone(),
                        bitrate,
                        quality,
                        fps,
                        deferred_resize,
                    ) {
                        Ok(()) => return Ok(()),
                        Err(e) => {
                            tracing::warn!("DMA-BUF capture failed, falling back to SHM: {:#}", e);
                            // Fall through to SHM path
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("DMA-BUF setup failed, falling back to SHM: {:#}", e);
                }
            }
        }
    }

    // SHM fallback path
    let shm_format = state.shm_format.unwrap_or(wl_shm::Format::Xrgb8888);
    let stride = width * 4;
    let buf_size = (stride as usize)
        .checked_mul(height as usize)
        .context("SHM buffer size overflow")?;
    let buf_size_i32 = i32::try_from(buf_size).context("SHM buffer too large for wl_shm_pool")?;

    // Double-buffered SHM: overlap capture and encoding so a capture request
    // is always pending with the compositor, preventing missed presentations.
    let shm_fd_0 = create_shm_fd(buf_size)?;
    let pool_0 = shm.create_pool(shm_fd_0.as_fd(), buf_size_i32, qh, ());
    let buffer_0 = pool_0.create_buffer(0, width as i32, height as i32, stride as i32, shm_format, qh, ());
    let mmap_0 = MmapRegion::new(shm_fd_0.as_fd().as_raw_fd(), buf_size)?;

    let shm_fd_1 = create_shm_fd(buf_size)?;
    let pool_1 = shm.create_pool(shm_fd_1.as_fd(), buf_size_i32, qh, ());
    let buffer_1 = pool_1.create_buffer(0, width as i32, height as i32, stride as i32, shm_format, qh, ());
    let mmap_1 = MmapRegion::new(shm_fd_1.as_fd().as_raw_fd(), buf_size)?;

    if let Some(tx) = info_tx.take() {
        let _ = tx.send(Ok(CaptureInfo { width, height, output_name: output_name.to_string() }));
    }

    let pixel_format = match shm_format {
        wl_shm::Format::Argb8888 => PixelFormat::BgrA32,
        wl_shm::Format::Xrgb8888 => PixelFormat::BgrX32,
        wl_shm::Format::Xbgr8888 => PixelFormat::RgbX32,
        wl_shm::Format::Abgr8888 => PixelFormat::RgbA32,
        _ => PixelFormat::BgrA32,
    };

    tracing::info!(width, height, ?shm_format, output = %output_name, mode = "ext", fps, "Starting capture loop (double-buffered SHM)");

    let mut proc = FrameProcessor::new(egfx_shared, width, height, pixel_format, stride, bitrate, quality, fps, deferred_resize);
    let frame_interval = Duration::from_secs_f64(1.0 / fps as f64);
    let mut last_frame_time = Instant::now() - frame_interval;

    let buffers = [&buffer_0, &buffer_1];
    let mmaps = [&mmap_0, &mmap_1];
    let mut cap_idx: usize = 0;

    // Start initial capture into buffer 0
    let mut frame = session.create_frame(qh, ());
    state.frame_ready = false;
    state.frame_failed = false;
    state.damage_regions.clear();
    frame.attach_buffer(buffers[cap_idx]);
    frame.damage_buffer(0, 0, width as i32, height as i32);
    frame.capture();
    conn.flush().context("Wayland flush failed")?;

    loop {
        if state.tx.is_closed() { break; }
        if state.stopped { tracing::warn!("Capture session stopped by compositor"); break; }

        // Wait for current frame to complete (poll-based for responsive shutdown)
        while !state.frame_ready && !state.frame_failed {
            poll_dispatch(event_queue, state, POLL_TIMEOUT_MS)?;
            if state.tx.is_closed() || state.stopped { break; }
        }
        frame.destroy();

        // Shutdown interrupted the wait — exit cleanly
        if !state.frame_ready && !state.frame_failed { break; }

        // Save completed frame state before starting next capture
        let completed_failed = state.frame_failed;
        let completed_idx = cap_idx;
        let has_damage = !state.damage_regions.is_empty();

        // Start NEXT capture immediately into the other buffer.
        // This ensures a capture request is always pending with the compositor,
        // so screen changes during encoding are never missed.
        cap_idx = 1 - cap_idx;
        state.frame_ready = false;
        state.frame_failed = false;
        state.damage_regions.clear();
        frame = session.create_frame(qh, ());
        frame.attach_buffer(buffers[cap_idx]);
        frame.damage_buffer(0, 0, width as i32, height as i32);
        frame.capture();
        conn.flush().context("Wayland flush failed")?;

        // Process the completed frame while next capture is pending
        if completed_failed { continue; }

        let data = mmaps[completed_idx].as_slice();

        // Always enforce frame rate limit. Without this, compositor animations
        // (window open, cursor blink) flood the client with 60fps H.264 frames,
        // overwhelming the decoder and building up a decode queue that delays
        // all subsequent frames (including keystroke updates) by seconds.
        let elapsed = last_frame_time.elapsed();
        if !proc.sent_first_frame || (elapsed >= frame_interval && has_damage) {
            last_frame_time = Instant::now();
            if !proc.process(data, &state.tx, has_damage || !proc.sent_first_frame) { break; }
        }
    }

    Ok(())
}

/// Context for DMA-BUF capture (created during setup, passed to capture loop).
#[cfg(feature = "vaapi")]
struct DmaBufCaptureContext {
    /// GBM device (must outlive gbm_bos)
    #[allow(dead_code)]
    gbm_device: super::dmabuf::GbmDevice,
    /// GBM buffer objects (kept alive for DMA-BUF fd lifetime)
    #[allow(dead_code)]
    gbm_bos: Vec<super::dmabuf::GbmBo>,
    /// Wayland buffers backed by DMA-BUFs
    wl_buffers: Vec<wl_buffer::WlBuffer>,
    /// DMA-BUF info for each capture buffer (kept for reference)
    #[allow(dead_code)]
    dmabuf_infos: Vec<super::dmabuf::DmaBufInfo>,
    /// VPP converter (XRGB -> NV12)
    vpp: crate::egfx::vpp::VppConverter,
    /// NV12 output DMA-BUF info (for encoder import)
    nv12_info: super::dmabuf::DmaBufInfo,
    /// DRM device path
    drm_device_path: std::path::PathBuf,
}

/// Try to set up DMA-BUF capture. Returns None if compositor doesn't support DMA-BUF,
/// or Some(Err) if setup fails.
#[cfg(feature = "vaapi")]
fn try_setup_dmabuf(
    state: &AppState,
    qh: &QueueHandle<AppState>,
    width: u32,
    height: u32,
) -> Option<Result<DmaBufCaptureContext>> {
    use super::dmabuf::{DRM_FORMAT_ARGB8888, DRM_FORMAT_XRGB8888};

    let dev = state.dmabuf_device?;
    if state.dmabuf_formats.is_empty() {
        return None;
    }
    let linux_dmabuf = state.linux_dmabuf.as_ref()?;

    // Find a suitable format (prefer XRGB8888)
    let (chosen_format, chosen_modifiers) = {
        let mut best: Option<&(u32, Vec<u64>)> = None;
        for entry in &state.dmabuf_formats {
            if entry.0 == DRM_FORMAT_XRGB8888 {
                best = Some(entry);
                break;
            }
            if entry.0 == DRM_FORMAT_ARGB8888 && best.is_none() {
                best = Some(entry);
            }
        }
        match best {
            Some(entry) => (entry.0, &entry.1),
            None => {
                tracing::debug!("No XRGB8888/ARGB8888 format in DMA-BUF formats");
                return None;
            }
        }
    };

    tracing::info!(
        format = format!("0x{:08x}", chosen_format),
        num_modifiers = chosen_modifiers.len(),
        "Attempting DMA-BUF capture setup"
    );

    Some(setup_dmabuf_inner(
        dev,
        linux_dmabuf,
        qh,
        width,
        height,
        chosen_format,
        chosen_modifiers,
    ))
}

#[cfg(feature = "vaapi")]
fn setup_dmabuf_inner(
    dev: libc::dev_t,
    linux_dmabuf: &zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
    qh: &QueueHandle<AppState>,
    width: u32,
    height: u32,
    format: u32,
    modifiers: &[u64],
) -> Result<DmaBufCaptureContext> {
    use super::dmabuf::{drm_device_from_devt, open_drm_device, DRM_FORMAT_MOD_INVALID, GbmBo, GbmDevice};

    // Find DRM device path from dev_t
    let drm_device_path =
        drm_device_from_devt(dev).context("failed to find DRM device from dev_t")?;
    tracing::info!(device = %drm_device_path.display(), "DMA-BUF: found DRM device");

    // Open DRM device and create GBM device
    let drm_fd = open_drm_device(&drm_device_path)?;
    let gbm_device = GbmDevice::new(drm_fd)?;

    // Filter out invalid modifiers
    let valid_modifiers: Vec<u64> = modifiers
        .iter()
        .copied()
        .filter(|m| *m != DRM_FORMAT_MOD_INVALID)
        .collect();

    // Allocate 2 GBM buffer objects (double-buffered capture)
    let mut gbm_bos = Vec::with_capacity(2);
    let mut wl_buffers = Vec::with_capacity(2);
    let mut dmabuf_infos = Vec::with_capacity(2);

    for i in 0..2 {
        let mut bo = if !valid_modifiers.is_empty() {
            GbmBo::create_with_modifiers(&gbm_device, width, height, format, &valid_modifiers)
                .or_else(|_| GbmBo::create(&gbm_device, width, height, format))
        } else {
            GbmBo::create(&gbm_device, width, height, format)
        }
        .with_context(|| format!("failed to allocate GBM buffer {}", i))?;

        let info = bo.dmabuf_info(format, width, height)
            .with_context(|| format!("failed to get DMA-BUF info for buffer {}", i))?;

        // Create wl_buffer via linux-dmabuf
        let params = linux_dmabuf.create_params(qh, ());
        let fd = bo.fd()
            .with_context(|| format!("failed to get fd for buffer {}", i))?;
        let stride = bo.stride();
        let offset = bo.offset(0);
        let modifier = bo.modifier();
        let modifier_hi = (modifier >> 32) as u32;
        let modifier_lo = (modifier & 0xFFFFFFFF) as u32;

        // Add the plane to params.
        // SAFETY: fd is valid as long as the GBM bo is alive (which we keep in gbm_bos).
        params.add(
            unsafe { std::os::unix::io::BorrowedFd::borrow_raw(fd) },
            0,
            offset,
            stride,
            modifier_hi,
            modifier_lo,
        );

        let wl_buf = params.create_immed(width as i32, height as i32, format, zwp_linux_buffer_params_v1::Flags::empty(), qh, ());

        tracing::debug!(
            idx = i,
            fd,
            stride,
            offset,
            modifier = format!("0x{:016x}", modifier),
            "DMA-BUF: allocated capture buffer"
        );

        dmabuf_infos.push(info);
        wl_buffers.push(wl_buf);
        gbm_bos.push(bo);
    }

    // Create VPP converter
    let mut vpp = crate::egfx::vpp::VppConverter::new(&drm_device_path, width, height)?;

    // Import the two XRGB DMA-BUFs as VPP input surfaces
    for (i, info) in dmabuf_infos.iter().enumerate() {
        vpp.import_input_surface(info.fd, width, height, info.stride, info.modifier, format)
            .with_context(|| format!("failed to import VPP input surface {}", i))?;
    }

    // Export the NV12 output surface as a DMA-BUF
    let nv12_info = vpp.export_nv12_output()?;
    tracing::info!(
        nv12_fd = nv12_info.fd,
        nv12_stride = nv12_info.stride,
        "DMA-BUF: VPP NV12 output exported"
    );

    Ok(DmaBufCaptureContext {
        gbm_device,
        gbm_bos,
        wl_buffers,
        dmabuf_infos,
        vpp,
        nv12_info,
        drm_device_path,
    })
}

/// DMA-BUF capture loop for ext-image-copy-capture.
#[cfg(feature = "vaapi")]
#[allow(clippy::too_many_arguments)]
fn capture_loop_ext_dmabuf(
    conn: &Connection,
    event_queue: &mut wayland_client::EventQueue<AppState>,
    state: &mut AppState,
    qh: &QueueHandle<AppState>,
    session: &ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1,
    width: u32,
    height: u32,
    dmabuf_ctx: &DmaBufCaptureContext,
    egfx_shared: Option<Arc<EgfxShared>>,
    bitrate: u32,
    quality: u8,
    fps: u32,
    deferred_resize: Option<ironrdp_server::DesktopSize>,
) -> Result<()> {
    tracing::info!(
        width,
        height,
        device = %dmabuf_ctx.drm_device_path.display(),
        mode = "ext-dmabuf",
        fps,
        "Starting capture loop (zero-copy DMA-BUF)"
    );

    let frame_interval = Duration::from_secs_f64(1.0 / fps as f64);
    let mut last_frame_time = Instant::now() - frame_interval;
    let mut cap_idx: usize = 0;
    let mut sent_first_frame = false;
    let mut deferred_resize = deferred_resize;

    // EGFX state (mirrors FrameProcessor but for DMA-BUF path)
    let mut h264_encoder: Option<crate::egfx::FrameEncoder> = None;
    let mut egfx_handle: Option<ironrdp_server::GfxServerHandle> = None;
    let mut egfx_sender: Option<tokio::sync::mpsc::UnboundedSender<ironrdp_server::ServerEvent>> = None;
    let mut egfx_surface_id: Option<u16> = None;
    let mut egfx_active = false;
    let mut egfx_ready = false;
    let mut egfx_generation: u32 = 0;
    let mut encode_failures: u32 = 0;

    // Start initial capture
    let mut frame = session.create_frame(qh, ());
    state.frame_ready = false;
    state.frame_failed = false;
    state.damage_regions.clear();
    frame.attach_buffer(&dmabuf_ctx.wl_buffers[cap_idx]);
    frame.damage_buffer(0, 0, width as i32, height as i32);
    frame.capture();
    conn.flush().context("Wayland flush failed")?;

    loop {
        if state.tx.is_closed() { break; }
        if state.stopped { tracing::warn!("Capture session stopped by compositor"); break; }

        // Wait for current frame to complete (poll-based for responsive shutdown)
        while !state.frame_ready && !state.frame_failed {
            poll_dispatch(event_queue, state, POLL_TIMEOUT_MS)?;
            if state.tx.is_closed() || state.stopped { break; }
        }
        frame.destroy();

        // Shutdown interrupted the wait — exit cleanly
        if !state.frame_ready && !state.frame_failed { break; }

        let completed_failed = state.frame_failed;
        let completed_idx = cap_idx;
        let has_damage = !state.damage_regions.is_empty();

        // Start next capture immediately
        cap_idx = 1 - cap_idx;
        state.frame_ready = false;
        state.frame_failed = false;
        state.damage_regions.clear();
        frame = session.create_frame(qh, ());
        frame.attach_buffer(&dmabuf_ctx.wl_buffers[cap_idx]);
        frame.damage_buffer(0, 0, width as i32, height as i32);
        frame.capture();
        conn.flush().context("Wayland flush failed")?;

        if completed_failed { continue; }

        // Rate limit
        let elapsed = last_frame_time.elapsed();
        if !sent_first_frame || (elapsed >= frame_interval && has_damage) {
            // Process via DMA-BUF zero-copy pipeline
            last_frame_time = Instant::now();

            // Update EGFX state
            if let Some(shared) = &egfx_shared {
                let ready = shared.is_ready();
                let gen = shared.generation();

                if ready != egfx_ready {
                    egfx_ready = ready;
                    if !ready {
                        egfx_active = false;
                        egfx_handle = None;
                        egfx_sender = None;
                        egfx_surface_id = None;
                        h264_encoder = None;
                        encode_failures = 0;
                    }
                }

                if gen != egfx_generation {
                    egfx_generation = gen;
                    egfx_surface_id = None;
                    h264_encoder = None;
                    encode_failures = 0;
                    if ready {
                        match crate::egfx::FrameEncoder::new(width, height, bitrate, fps) {
                            Ok(enc) => {
                                tracing::info!(
                                    width, height,
                                    backend = enc.backend_name(),
                                    gen,
                                    "H.264 encoder initialized (DMA-BUF path)"
                                );
                                h264_encoder = Some(enc);
                            }
                            Err(e) => {
                                // DMA-BUF path requires a working encoder; bail to SHM fallback
                                frame.destroy();
                                bail!("H.264 encoder init failed in DMA-BUF mode, falling back to SHM: {:#}", e);
                            }
                        }
                    }
                }

                if ready && !egfx_active {
                    egfx_handle = shared.get_handle();
                    egfx_sender = shared.get_event_sender();
                    if h264_encoder.is_some() && egfx_handle.is_some() && egfx_sender.is_some() {
                        egfx_active = true;
                        tracing::info!("EGFX transport ready (DMA-BUF path)");
                    }
                }

                if egfx_active {
                    // Surface initialization (separate borrow scope)
                    if egfx_surface_id.is_none() {
                        if let (Some(handle), Some(sender)) = (&egfx_handle, &egfx_sender) {
                            egfx_surface_id = EgfxShared::init_surface(
                                handle, sender, width as u16, height as u16,
                            );
                        }
                    }

                    if let Some(sid) = egfx_surface_id {
                        // Zero-copy encode pipeline:
                        // 1. VPP: XRGB DMA-BUF -> NV12 (GPU)
                        // 2. Encoder: NV12 DMA-BUF -> H.264 (GPU)
                        let vpp_result = dmabuf_ctx.vpp.convert(completed_idx);
                        let encode_result = match vpp_result {
                            Ok(()) => {
                                let nv12 = &dmabuf_ctx.nv12_info;
                                h264_encoder.as_mut().map(|enc| enc.encode_dmabuf(
                                    nv12.fd, nv12.width, nv12.height,
                                    nv12.stride, nv12.offset, nv12.modifier,
                                    nv12.uv_stride, nv12.uv_offset,
                                ))
                            }
                            Err(e) => Some(Err(e)),
                        };

                        match encode_result {
                            Some(Ok(ref h264_data)) if h264_data.len() > 32 => {
                                encode_failures = 0;
                                if let (Some(handle), Some(sender)) = (&egfx_handle, &egfx_sender) {
                                    let timestamp = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_millis() as u32;
                                    let sent = EgfxShared::send_frame(
                                        handle, sender, sid,
                                        width as u16, height as u16,
                                        h264_data, timestamp, quality,
                                    );
                                    if sent {
                                        sent_first_frame = true;
                                        if let Some(size) = deferred_resize.take() {
                                            tracing::info!(
                                                width = size.width,
                                                height = size.height,
                                                "Sending deferred resize"
                                            );
                                            let _ = state
                                                .tx
                                                .blocking_send(DisplayUpdate::Resize(size));
                                        }
                                    }
                                }
                            }
                            Some(Ok(_)) => { encode_failures = 0; }
                            Some(Err(e)) => {
                                encode_failures += 1;
                                tracing::warn!(
                                    failures = encode_failures,
                                    max = MAX_ENCODE_FAILURES,
                                    "DMA-BUF encode pipeline failed: {:#}", e
                                );
                                if let Some(enc) = &mut h264_encoder {
                                    enc.force_idr();
                                }
                                if encode_failures >= MAX_ENCODE_FAILURES {
                                    // Destroy the in-flight frame before dropping DMA-BUF resources
                                    frame.destroy();
                                    bail!(
                                        "VA-API encode failed {} consecutive times in DMA-BUF mode, \
                                         falling back to SHM",
                                        encode_failures
                                    );
                                }
                            }
                            None => {}
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn capture_loop_wlr(
    conn: &Connection,
    event_queue: &mut wayland_client::EventQueue<AppState>,
    state: &mut AppState,
    qh: &QueueHandle<AppState>,
    output: &wl_output::WlOutput,
    shm: &wl_shm::WlShm,
    screencopy_mgr: &zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
    output_name: &str,
    egfx_shared: Option<Arc<EgfxShared>>,
    info_tx: &mut Option<tokio::sync::oneshot::Sender<Result<CaptureInfo>>>,
    bitrate: u32,
    quality: u8,
    fps: u32,
    deferred_resize: Option<ironrdp_server::DesktopSize>,
) -> Result<()> {
    // First capture to get buffer dimensions
    let probe = screencopy_mgr.capture_output(0, output, qh, ());
    state.buffer_width = 0;
    state.buffer_height = 0;
    state.frame_ready = false;
    state.frame_failed = false;
    conn.flush().context("Wayland flush failed")?;

    // Wait for buffer info events
    let probe_start = Instant::now();
    let probe_timeout = Duration::from_secs(5);
    loop {
        poll_dispatch(event_queue, state, POLL_TIMEOUT_MS)?;
        if state.buffer_width > 0 && state.buffer_height > 0 {
            break;
        }
        if probe_start.elapsed() >= probe_timeout {
            bail!("timed out waiting for wlr-screencopy buffer info after {}s", probe_timeout.as_secs());
        }
    }
    probe.destroy();

    let width = state.buffer_width;
    let height = state.buffer_height;
    let shm_format = state.shm_format.unwrap_or(wl_shm::Format::Xrgb8888);
    let stride = if state.wlr_stride > 0 { state.wlr_stride } else { width * 4 };
    let buf_size = (stride as usize)
        .checked_mul(height as usize)
        .context("SHM buffer size overflow")?;
    let buf_size_i32 = i32::try_from(buf_size).context("SHM buffer too large for wl_shm_pool")?;

    // Double-buffered SHM: overlap capture and encoding so a capture request
    // is always pending with the compositor, preventing missed presentations.
    let shm_fd_0 = create_shm_fd(buf_size)?;
    let pool_0 = shm.create_pool(shm_fd_0.as_fd(), buf_size_i32, qh, ());
    let buffer_0 = pool_0.create_buffer(0, width as i32, height as i32, stride as i32, shm_format, qh, ());
    let mmap_0 = MmapRegion::new(shm_fd_0.as_fd().as_raw_fd(), buf_size)?;

    let shm_fd_1 = create_shm_fd(buf_size)?;
    let pool_1 = shm.create_pool(shm_fd_1.as_fd(), buf_size_i32, qh, ());
    let buffer_1 = pool_1.create_buffer(0, width as i32, height as i32, stride as i32, shm_format, qh, ());
    let mmap_1 = MmapRegion::new(shm_fd_1.as_fd().as_raw_fd(), buf_size)?;

    if let Some(tx) = info_tx.take() {
        let _ = tx.send(Ok(CaptureInfo { width, height, output_name: output_name.to_string() }));
    }

    let pixel_format = match shm_format {
        wl_shm::Format::Argb8888 => PixelFormat::BgrA32,
        wl_shm::Format::Xrgb8888 => PixelFormat::BgrX32,
        wl_shm::Format::Xbgr8888 => PixelFormat::RgbX32,
        wl_shm::Format::Abgr8888 => PixelFormat::RgbA32,
        _ => PixelFormat::BgrA32,
    };

    tracing::info!(width, height, ?shm_format, stride, output = %output_name, mode = "wlr", fps, "Starting capture loop (double-buffered)");

    let mut proc = FrameProcessor::new(egfx_shared, width, height, pixel_format, stride, bitrate, quality, fps, deferred_resize);
    let frame_interval = Duration::from_secs_f64(1.0 / fps as f64);
    let mut last_frame_time = Instant::now() - frame_interval;

    let buffers = [&buffer_0, &buffer_1];
    let mmaps = [&mmap_0, &mmap_1];
    let mut cap_idx: usize = 0;

    // Start initial capture into buffer 0
    let mut frame = screencopy_mgr.capture_output(0, output, qh, ());
    state.frame_ready = false;
    state.frame_failed = false;
    state.damage_regions.clear();
    state.buffer_width = 0; // Reset so we wait for this frame's buffer events
    conn.flush().context("Wayland flush failed")?;
    let mut buffer_sent = false;
    while !state.frame_ready && !state.frame_failed {
        poll_dispatch(event_queue, state, POLL_TIMEOUT_MS)?;
        if !buffer_sent && state.buffer_width > 0 {
            frame.copy_with_damage(buffers[cap_idx]);
            conn.flush().context("Wayland flush failed")?;
            buffer_sent = true;
        }
        if state.tx.is_closed() || state.stopped { break; }
    }

    loop {
        // Save completed frame state
        let completed_failed = state.frame_failed;
        let completed_idx = cap_idx;
        let has_damage = !state.damage_regions.is_empty();
        frame.destroy();

        if state.tx.is_closed() { break; }
        if !state.frame_ready && !state.frame_failed { break; } // shutdown interrupted

        // Start NEXT capture immediately into the other buffer.
        cap_idx = 1 - cap_idx;
        state.frame_ready = false;
        state.frame_failed = false;
        state.damage_regions.clear();
        frame = screencopy_mgr.capture_output(0, output, qh, ());
        state.buffer_width = 0; // Reset so we wait for this frame's buffer events
        conn.flush().context("Wayland flush failed")?;
        buffer_sent = false;

        // Process the completed frame while waiting for next buffer info + capture
        if !completed_failed {
            let data = mmaps[completed_idx].as_slice();
            let elapsed = last_frame_time.elapsed();
            if !proc.sent_first_frame || (elapsed >= frame_interval && has_damage) {
                last_frame_time = Instant::now();
                if !proc.process(data, &state.tx, has_damage || !proc.sent_first_frame) { break; }
            }
        }

        // Wait for next frame to complete (poll-based for responsive shutdown)
        while !state.frame_ready && !state.frame_failed {
            poll_dispatch(event_queue, state, POLL_TIMEOUT_MS)?;
            if !buffer_sent && state.buffer_width > 0 {
                // Detect compositor buffer renegotiation (dimensions, stride, or format)
                let new_stride = if state.wlr_stride > 0 { state.wlr_stride } else { state.buffer_width * 4 };
                if state.buffer_width != width
                    || state.buffer_height != height
                    || new_stride != stride
                    || state.shm_format.unwrap_or(wl_shm::Format::Xrgb8888) != shm_format
                {
                    tracing::warn!(
                        old_w = width, old_h = height,
                        new_w = state.buffer_width, new_h = state.buffer_height,
                        "WLR: compositor changed buffer parameters, restarting capture"
                    );
                    frame.destroy();
                    bail!("WLR buffer parameters changed, restarting capture");
                }
                frame.copy_with_damage(buffers[cap_idx]);
                conn.flush().context("Wayland flush failed")?;
                buffer_sent = true;
            }
            if state.tx.is_closed() || state.stopped { break; }
        }
    }

    Ok(())
}

// --- Wayland state ---

struct AppState {
    tx: mpsc::Sender<DisplayUpdate>,
    target_output_name: String,
    // Globals
    shm: Option<wl_shm::WlShm>,
    target_output: Option<wl_output::WlOutput>,
    outputs: Vec<(u32, wl_output::WlOutput)>, // (name_id, output)
    output_names: Vec<(u32, String)>,         // (wl_output id, name)
    capture_manager: Option<ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1>,
    source_manager:
        Option<ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1>,
    screencopy_manager: Option<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1>,
    #[cfg(feature = "vaapi")]
    linux_dmabuf: Option<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1>,
    // Session state
    session: Option<ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1>,
    buffer_width: u32,
    buffer_height: u32,
    wlr_stride: u32,
    shm_format: Option<wl_shm::Format>,
    // DMA-BUF session state
    #[cfg(feature = "vaapi")]
    dmabuf_device: Option<libc::dev_t>,
    #[cfg(feature = "vaapi")]
    dmabuf_formats: Vec<(u32, Vec<u64>)>, // (drm_format, modifiers)
    // Frame state
    frame_ready: bool,
    frame_failed: bool,
    damage_regions: Vec<(i32, i32, i32, i32)>,
    stopped: bool,
}

impl AppState {
    fn new(tx: mpsc::Sender<DisplayUpdate>, target_output_name: String) -> Self {
        Self {
            tx,
            target_output_name,
            shm: None,
            target_output: None,
            outputs: Vec::new(),
            output_names: Vec::new(),
            capture_manager: None,
            source_manager: None,
            screencopy_manager: None,
            #[cfg(feature = "vaapi")]
            linux_dmabuf: None,
            session: None,
            buffer_width: 0,
            buffer_height: 0,
            wlr_stride: 0,
            shm_format: None,
            #[cfg(feature = "vaapi")]
            dmabuf_device: None,
            #[cfg(feature = "vaapi")]
            dmabuf_formats: Vec::new(),
            frame_ready: false,
            frame_failed: false,
            damage_regions: Vec::new(),
            stopped: false,
        }
    }
}

// --- Registry dispatch ---

impl Dispatch<wl_registry::WlRegistry, ()> for AppState {
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
                "wl_shm" => {
                    state.shm = Some(registry.bind(name, version.min(1), qh, ()));
                }
                "wl_output" => {
                    let output: wl_output::WlOutput = registry.bind(name, version.min(4), qh, ());
                    state.outputs.push((name, output));
                }
                "ext_image_copy_capture_manager_v1" => {
                    state.capture_manager = Some(registry.bind(name, version.min(1), qh, ()));
                }
                "ext_output_image_capture_source_manager_v1" => {
                    state.source_manager = Some(registry.bind(name, version.min(1), qh, ()));
                }
                "zwlr_screencopy_manager_v1" => {
                    state.screencopy_manager = Some(registry.bind(name, version.min(3), qh, ()));
                }
                #[cfg(feature = "vaapi")]
                "zwp_linux_dmabuf_v1" => {
                    state.linux_dmabuf = Some(registry.bind(name, version.min(4), qh, ()));
                }
                _ => {}
            }
        }
    }
}

// --- Output dispatch (to get output name) ---

impl Dispatch<wl_output::WlOutput, ()> for AppState {
    fn event(
        state: &mut Self,
        proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            // Find which output this proxy belongs to
            let proxy_id = proxy.id().protocol_id();
            state.output_names.push((proxy_id, name.clone()));
            if name == state.target_output_name {
                // Find the matching output in our stored list
                for (_, output) in &state.outputs {
                    if output.id().protocol_id() == proxy_id {
                        state.target_output = Some(output.clone());
                        tracing::info!(name = %name, "Matched target output");
                        break;
                    }
                }
            }
        }
    }
}

// --- Session dispatch ---

impl Dispatch<ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1, ()> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        _: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_image_copy_capture_session_v1::Event::BufferSize { width, height } => {
                state.buffer_width = width;
                state.buffer_height = height;
            }
            ext_image_copy_capture_session_v1::Event::ShmFormat {
                format: WEnum::Value(fmt),
            } => {
                match fmt {
                    wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888 => {
                        state.shm_format = Some(fmt);
                    }
                    _ => {
                        if state.shm_format.is_none() {
                            state.shm_format = Some(fmt);
                        }
                    }
                }
            }
            ext_image_copy_capture_session_v1::Event::Done => {}
            ext_image_copy_capture_session_v1::Event::Stopped => {
                tracing::warn!("Session stopped");
                state.stopped = true;
            }
            #[cfg(feature = "vaapi")]
            ext_image_copy_capture_session_v1::Event::DmabufDevice { device } => {
                // device is a Vec<u8> containing a dev_t value
                if device.len() >= std::mem::size_of::<libc::dev_t>() {
                    let dev = libc::dev_t::from_ne_bytes(
                        device[..std::mem::size_of::<libc::dev_t>()]
                            .try_into()
                            .unwrap(),
                    );
                    tracing::info!(dev, "Session: DMA-BUF device advertised");
                    state.dmabuf_device = Some(dev);
                }
            }
            #[cfg(feature = "vaapi")]
            ext_image_copy_capture_session_v1::Event::DmabufFormat { format, modifiers } => {
                // modifiers is a Vec<u8> containing an array of u64 values
                let mut mods = Vec::new();
                let chunk_size = std::mem::size_of::<u64>();
                let mut i = 0;
                while i + chunk_size <= modifiers.len() {
                    let m = u64::from_ne_bytes(
                        modifiers[i..i + chunk_size].try_into().unwrap(),
                    );
                    mods.push(m);
                    i += chunk_size;
                }
                tracing::debug!(
                    format = format!("0x{:08x}", format),
                    num_modifiers = mods.len(),
                    "Session: DMA-BUF format advertised"
                );
                state.dmabuf_formats.push((format, mods));
            }
            _ => {}
        }
    }
}

// --- Frame dispatch ---

impl Dispatch<ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1, ()> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        _: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_image_copy_capture_frame_v1::Event::Ready => {
                state.frame_ready = true;
            }
            ext_image_copy_capture_frame_v1::Event::Failed { .. } => {
                state.frame_failed = true;
            }
            ext_image_copy_capture_frame_v1::Event::Damage {
                x,
                y,
                width,
                height,
            } => {
                state.damage_regions.push((x, y, width, height));
            }
            _ => {}
        }
    }
}

// --- No-op dispatchers ---

delegate_noop!(AppState: ignore wl_shm::WlShm);
delegate_noop!(AppState: ignore wl_shm_pool::WlShmPool);
delegate_noop!(AppState: ignore wl_buffer::WlBuffer);
delegate_noop!(AppState: ignore ext_image_capture_source_v1::ExtImageCaptureSourceV1);
delegate_noop!(AppState: ignore ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1);
delegate_noop!(AppState: ignore ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1);
delegate_noop!(AppState: ignore zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1);
#[cfg(feature = "vaapi")]
delegate_noop!(AppState: ignore zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1);
#[cfg(feature = "vaapi")]
delegate_noop!(AppState: ignore zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1);

// --- wlr-screencopy frame dispatch ---

impl Dispatch<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, ()> for AppState {
    fn event(
        state: &mut Self,
        _proxy: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format: WEnum::Value(format),
                width,
                height,
                stride,
            } => {
                // Use the first suitable format
                if state.buffer_width == 0 {
                    state.buffer_width = width;
                    state.buffer_height = height;
                    state.wlr_stride = stride;
                    state.shm_format = Some(format);
                }
                // Prefer Xrgb8888
                if format == wl_shm::Format::Xrgb8888 || format == wl_shm::Format::Argb8888 {
                    state.buffer_width = width;
                    state.buffer_height = height;
                    state.wlr_stride = stride;
                    state.shm_format = Some(format);
                }
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => {
                state.frame_ready = true;
            }
            zwlr_screencopy_frame_v1::Event::Failed => {
                state.frame_failed = true;
            }
            zwlr_screencopy_frame_v1::Event::Damage { x, y, width, height } => {
                state.damage_regions.push((x as i32, y as i32, width as i32, height as i32));
            }
            _ => {}
        }
    }
}

impl Dispatch<wayland_client::protocol::wl_display::WlDisplay, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &wayland_client::protocol::wl_display::WlDisplay,
        _: wayland_client::protocol::wl_display::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<wayland_client::protocol::wl_callback::WlCallback, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &wayland_client::protocol::wl_callback::WlCallback,
        _: wayland_client::protocol::wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

