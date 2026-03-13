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

pub struct CaptureInfo {
    pub width: u32,
    pub height: u32,
    /// Name of the output being captured
    pub output_name: String,
}

struct HeadlessOutputGuard {
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

fn list_headless_outputs() -> Result<Vec<String>> {
    let monitors = crate::hyprland::monitors()?;
    let arr = monitors.as_array().context("expected monitors array")?;
    Ok(arr
        .iter()
        .filter_map(|m| {
            let name = m["name"].as_str()?;
            name.starts_with("HEADLESS-").then(|| name.to_string())
        })
        .collect())
}

/// Create a headless output in Hyprland at the given resolution.
/// Returns the output name (e.g. "HEADLESS-1").
fn create_headless_output(width: u32, height: u32) -> Result<String> {
    // Subscribe to events BEFORE creating the output to catch monitoradded.
    // The ensure_registered() roundtrip guarantees Hyprland has accept()'ed
    // our socket2 connection before we trigger the creation.
    let mut events = crate::hyprland::EventStream::connect()?;
    events.ensure_registered()?;

    crate::hyprland::output_create_headless()
        .context("failed to create headless output")?;

    // Wait for monitoradded event — data is the output name
    let name = events
        .wait_for("monitoradded", Duration::from_secs(5))
        .context("failed to detect new headless output")?;

    // Set resolution
    let mode = format!("{}x{}@60", width, height);
    let rule = format!("{},{},-9999x0,1", name, mode);
    crate::hyprland::keyword_monitor(&rule)
        .context("failed to set headless output resolution")?;

    tracing::info!(name = %name, width, height, "Created headless output");
    Ok(name)
}

/// Remove a headless output from Hyprland.
fn remove_headless_output(name: &str) {
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
fn wait_for_output(output_name: &str, timeout: Duration) -> Result<()> {
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
/// If `output` is Some, captures that output directly; otherwise creates a headless output.
#[allow(clippy::too_many_arguments)]
pub async fn start_capture(
    tx: mpsc::Sender<DisplayUpdate>,
    target_resolution: (u32, u32),
    capture_mode: CaptureMode,
    egfx_shared: Option<Arc<EgfxShared>>,
    output_layout: Arc<SharedOutputLayout>,
    bitrate: u32,
    quality: u8,
    fps: u32,
    output: Option<String>,
    deferred_resize: Option<ironrdp_server::DesktopSize>,
) -> Result<CaptureInfo> {
    let (info_tx, info_rx) = tokio::sync::oneshot::channel();

    std::thread::Builder::new()
        .name("wayland-capture".into())
        .spawn(move || {
            if let Err(e) = capture_thread(
                tx,
                info_tx,
                target_resolution,
                capture_mode,
                egfx_shared,
                output_layout,
                bitrate,
                quality,
                fps,
                output,
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
    target_resolution: (u32, u32),
    capture_mode: CaptureMode,
    egfx_shared: Option<Arc<EgfxShared>>,
    output_layout: Arc<SharedOutputLayout>,
    bitrate: u32,
    quality: u8,
    fps: u32,
    output: Option<String>,
    deferred_resize: Option<ironrdp_server::DesktopSize>,
) -> Result<()> {
    let mut info_tx = Some(info_tx);
    let result = capture_thread_inner(
        tx,
        &mut info_tx,
        target_resolution,
        capture_mode,
        egfx_shared,
        output_layout,
        bitrate,
        quality,
        fps,
        output,
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
    target_resolution: (u32, u32),
    capture_mode: CaptureMode,
    egfx_shared: Option<Arc<EgfxShared>>,
    output_layout: Arc<SharedOutputLayout>,
    bitrate: u32,
    quality: u8,
    fps: u32,
    output: Option<String>,
    deferred_resize: Option<ironrdp_server::DesktopSize>,
) -> Result<()> {
    let (target_w, target_h) = target_resolution;

    // Determine output to capture
    let (output_name, _output_guard) = if let Some(ref name) = output {
        // Capture a real output directly
        verify_output_exists(name)?;
        tracing::info!(output = %name, "Capturing existing output");
        (name.clone(), None)
    } else {
        // Create headless output
        // Clean up stale headless outputs from previous crashed sessions
        for stale in list_headless_outputs().unwrap_or_default() {
            tracing::warn!(name = %stale, "Removing stale headless output from previous session");
            remove_headless_output(&stale);
        }

        let name = create_headless_output(target_w, target_h)?;
        let guard = HeadlessOutputGuard::new(name.clone());

        // Poll until output is ready (replaces fixed 500ms sleep)
        wait_for_output(&name, Duration::from_secs(5))?;

        (name, Some(guard))
    };

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
        self.sent_first_frame = true;

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
                if let (Some(handle), Some(sender), Some(encoder)) =
                    (&self.egfx_handle, &self.egfx_sender, &mut self.h264_encoder)
                {
                    if self.egfx_surface_id.is_none() {
                        if let Some(sid) =
                            EgfxShared::init_surface(handle, sender, self.width as u16, self.height as u16)
                        {
                            self.egfx_surface_id = Some(sid);
                        }
                    } else if let Some(sid) = self.egfx_surface_id {
                        match encoder.encode(data) {
                            Ok(ref h264_data) if h264_data.len() > 32 => {
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
                            Ok(_) => {}
                            Err(e) => tracing::warn!("H.264 encode failed: {:#}", e),
                        }
                    }
                }
            }
        }

        if sent_via_egfx {
            if let Some(size) = self.deferred_resize.take() {
                tracing::info!(width = size.width, height = size.height, "Sending deferred resize");
                let _ = tx.blocking_send(DisplayUpdate::Resize(size));
            }
        }

        // Only send bitmaps when EGFX is not expected. Sending bitmaps
        // before EGFX causes a bitmap→EGFX transition that breaks rendering
        // on some RDP clients (first connection after server startup).
        if !sent_via_egfx && self.egfx_shared.is_none() {
            let update = DisplayUpdate::Bitmap(BitmapUpdate {
                x: 0, y: 0,
                width: NonZeroU16::new(self.width as u16).unwrap(),
                height: NonZeroU16::new(self.height as u16).unwrap(),
                format: self.pixel_format,
                data: Bytes::copy_from_slice(data),
                stride: NonZeroUsize::new(self.stride as usize).unwrap(),
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

    let shm_format = state.shm_format.unwrap_or(wl_shm::Format::Xrgb8888);
    let stride = width * 4;
    let buf_size = (stride * height) as usize;

    // Double-buffered SHM: overlap capture and encoding so a capture request
    // is always pending with the compositor, preventing missed presentations.
    let shm_fd_0 = create_shm_fd(buf_size)?;
    let pool_0 = shm.create_pool(shm_fd_0.as_fd(), buf_size as i32, qh, ());
    let buffer_0 = pool_0.create_buffer(0, width as i32, height as i32, stride as i32, shm_format, qh, ());
    let mmap_0 = unsafe {
        libc::mmap(std::ptr::null_mut(), buf_size, libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED, shm_fd_0.as_fd().as_raw_fd(), 0)
    };
    if mmap_0 == libc::MAP_FAILED { bail!("mmap failed for buffer 0"); }

    let shm_fd_1 = create_shm_fd(buf_size)?;
    let pool_1 = shm.create_pool(shm_fd_1.as_fd(), buf_size as i32, qh, ());
    let buffer_1 = pool_1.create_buffer(0, width as i32, height as i32, stride as i32, shm_format, qh, ());
    let mmap_1 = unsafe {
        libc::mmap(std::ptr::null_mut(), buf_size, libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED, shm_fd_1.as_fd().as_raw_fd(), 0)
    };
    if mmap_1 == libc::MAP_FAILED {
        unsafe { libc::munmap(mmap_0, buf_size); }
        bail!("mmap failed for buffer 1");
    }

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

    tracing::info!(width, height, ?shm_format, output = %output_name, mode = "ext", fps, "Starting capture loop (double-buffered)");

    let mut proc = FrameProcessor::new(egfx_shared, width, height, pixel_format, stride, bitrate, quality, fps, deferred_resize);
    let frame_interval = Duration::from_secs_f64(1.0 / fps as f64);
    let mut last_frame_time = Instant::now() - frame_interval;

    let buffers = [&buffer_0, &buffer_1];
    let mmaps = [mmap_0, mmap_1];
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

        // Wait for current frame to complete
        loop {
            event_queue.blocking_dispatch(state).context("Wayland dispatch failed")?;
            if state.frame_ready || state.frame_failed { break; }
        }
        frame.destroy();

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

        let data = unsafe { std::slice::from_raw_parts(mmaps[completed_idx] as *const u8, buf_size) };

        // Always enforce frame rate limit. Without this, compositor animations
        // (window open, cursor blink) flood the client with 60fps H.264 frames,
        // overwhelming the decoder and building up a decode queue that delays
        // all subsequent frames (including keystroke updates) by seconds.
        let elapsed = last_frame_time.elapsed();
        if (elapsed >= frame_interval || !proc.sent_first_frame) && has_damage {
            last_frame_time = Instant::now();
            if !proc.process(data, &state.tx, has_damage) { break; }
        }
    }

    unsafe {
        libc::munmap(mmap_0, buf_size);
        libc::munmap(mmap_1, buf_size);
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

    // Wait for buffer info events
    loop {
        event_queue.blocking_dispatch(state).context("Wayland dispatch (wlr probe) failed")?;
        // buffer events arrive before buffer_done; we get ready/failed only after copy()
        // Just wait until we have dimensions
        if state.buffer_width > 0 && state.buffer_height > 0 {
            break;
        }
    }
    probe.destroy();

    let width = state.buffer_width;
    let height = state.buffer_height;
    let shm_format = state.shm_format.unwrap_or(wl_shm::Format::Xrgb8888);
    let stride = if state.wlr_stride > 0 { state.wlr_stride } else { width * 4 };
    let buf_size = (stride * height) as usize;

    // Double-buffered SHM: overlap capture and encoding so a capture request
    // is always pending with the compositor, preventing missed presentations.
    let shm_fd_0 = create_shm_fd(buf_size)?;
    let pool_0 = shm.create_pool(shm_fd_0.as_fd(), buf_size as i32, qh, ());
    let buffer_0 = pool_0.create_buffer(0, width as i32, height as i32, stride as i32, shm_format, qh, ());
    let mmap_0 = unsafe {
        libc::mmap(std::ptr::null_mut(), buf_size, libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED, shm_fd_0.as_fd().as_raw_fd(), 0)
    };
    if mmap_0 == libc::MAP_FAILED { bail!("mmap failed for buffer 0"); }

    let shm_fd_1 = create_shm_fd(buf_size)?;
    let pool_1 = shm.create_pool(shm_fd_1.as_fd(), buf_size as i32, qh, ());
    let buffer_1 = pool_1.create_buffer(0, width as i32, height as i32, stride as i32, shm_format, qh, ());
    let mmap_1 = unsafe {
        libc::mmap(std::ptr::null_mut(), buf_size, libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED, shm_fd_1.as_fd().as_raw_fd(), 0)
    };
    if mmap_1 == libc::MAP_FAILED {
        unsafe { libc::munmap(mmap_0, buf_size); }
        bail!("mmap failed for buffer 1");
    }

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
    let mmaps = [mmap_0, mmap_1];
    let mut cap_idx: usize = 0;

    // Start initial capture into buffer 0
    let mut frame = screencopy_mgr.capture_output(0, output, qh, ());
    state.frame_ready = false;
    state.frame_failed = false;
    state.damage_regions.clear();
    let mut buffer_sent = false;
    loop {
        event_queue.blocking_dispatch(state).context("Wayland dispatch failed")?;
        if !buffer_sent && state.buffer_width > 0 {
            frame.copy_with_damage(buffers[cap_idx]);
            conn.flush().context("Wayland flush failed")?;
            buffer_sent = true;
        }
        if state.frame_ready || state.frame_failed { break; }
    }

    loop {
        // Save completed frame state
        let completed_failed = state.frame_failed;
        let completed_idx = cap_idx;
        let has_damage = !state.damage_regions.is_empty();
        frame.destroy();

        if state.tx.is_closed() { break; }

        // Start NEXT capture immediately into the other buffer.
        cap_idx = 1 - cap_idx;
        state.frame_ready = false;
        state.frame_failed = false;
        state.damage_regions.clear();
        frame = screencopy_mgr.capture_output(0, output, qh, ());
        buffer_sent = false;

        // Process the completed frame while waiting for next buffer info + capture
        if !completed_failed {
            let data = unsafe { std::slice::from_raw_parts(mmaps[completed_idx] as *const u8, buf_size) };
            let elapsed = last_frame_time.elapsed();
            if (elapsed >= frame_interval || !proc.sent_first_frame) && has_damage {
                last_frame_time = Instant::now();
                if !proc.process(data, &state.tx, has_damage) { break; }
            }
        }

        // Wait for next frame to complete (send buffer when ready)
        loop {
            event_queue.blocking_dispatch(state).context("Wayland dispatch failed")?;
            if !buffer_sent && state.buffer_width > 0 {
                frame.copy_with_damage(buffers[cap_idx]);
                conn.flush().context("Wayland flush failed")?;
                buffer_sent = true;
            }
            if state.frame_ready || state.frame_failed { break; }
        }
    }

    unsafe {
        libc::munmap(mmap_0, buf_size);
        libc::munmap(mmap_1, buf_size);
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
    // Session state
    session: Option<ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1>,
    buffer_width: u32,
    buffer_height: u32,
    wlr_stride: u32,
    shm_format: Option<wl_shm::Format>,
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
            session: None,
            buffer_width: 0,
            buffer_height: 0,
            wlr_stride: 0,
            shm_format: None,
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

#[cfg(test)]
mod tests {
    use super::parse_headless_outputs;

    #[test]
    fn parses_headless_output_names() {
        let json = br#"
            [
                {"name":"DP-1"},
                {"name":"HEADLESS-2"},
                {"name":"HEADLESS-3"}
            ]
        "#;

        let names = parse_headless_outputs(json).unwrap();

        assert_eq!(names, vec!["HEADLESS-2", "HEADLESS-3"]);
    }

    #[test]
    fn ignores_non_headless_outputs() {
        let json = br#"
            [
                {"name":"DP-1"},
                {"name":"HDMI-A-1"}
            ]
        "#;

        let names = parse_headless_outputs(json).unwrap();

        assert!(names.is_empty());
    }
}
