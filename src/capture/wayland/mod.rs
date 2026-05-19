use std::io::ErrorKind;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::unix::io::OwnedFd;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use ironrdp_server::{DisplayUpdate, PixelFormat};
use tokio::sync::mpsc;

use super::frame::{FramePacer, FrameProcessor};
use super::CaptureMode;
use crate::egfx::{EgfxShared, H264RateControl};
use crate::input::SharedOutputLayout;
pub(crate) use output::{
    create_headless_output, list_stale_headless_outputs, output_info, wait_for_output,
    HeadlessOutputGuard,
};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{delegate_noop, Connection, Dispatch, Proxy, QueueHandle, WEnum};
use wayland_protocols::ext::image_capture_source::v1::client::ext_image_capture_source_v1;
use wayland_protocols::ext::image_capture_source::v1::client::ext_output_image_capture_source_manager_v1;
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1, ext_image_copy_capture_manager_v1,
    ext_image_copy_capture_session_v1,
};
#[cfg(feature = "vaapi")]
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1,
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1,
};

#[cfg(feature = "vaapi")]
mod dmabuf_capture;
mod ext;
mod output;
mod state;
mod wlr;

use state::AppState;

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

/// Verify that a named output exists in Hyprland monitors.
fn verify_output_exists(output_name: &str) -> Result<()> {
    output_info(output_name).map(|_| ())
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
    rate_control: H264RateControl,
    fps: u32,
    output_name: String,
    deferred_resize: Option<ironrdp_server::DesktopSize>,
    stop_flag: Arc<std::sync::atomic::AtomicBool>,
) -> Result<(CaptureInfo, std::thread::JoinHandle<()>)> {
    let (info_tx, info_rx) = tokio::sync::oneshot::channel();

    let handle = std::thread::Builder::new()
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
                rate_control,
                fps,
                output_name,
                deferred_resize,
                stop_flag,
            ) {
                tracing::error!("Capture thread error: {:#}", e);
            }
        })?;

    let info = info_rx.await.context("capture thread failed to start")??;
    Ok((info, handle))
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
    rate_control: H264RateControl,
    fps: u32,
    output_name: String,
    deferred_resize: Option<ironrdp_server::DesktopSize>,
    stop_flag: Arc<std::sync::atomic::AtomicBool>,
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
        rate_control,
        fps,
        output_name,
        deferred_resize,
        stop_flag,
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
    rate_control: H264RateControl,
    fps: u32,
    output_name: String,
    deferred_resize: Option<ironrdp_server::DesktopSize>,
    stop_flag: Arc<std::sync::atomic::AtomicBool>,
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

    let mut state = AppState::new(tx, output_name.clone(), stop_flag);

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
        CaptureMode::Ext => ext::capture_loop_ext(
            &conn,
            &mut event_queue,
            &mut state,
            &qh,
            &wl_output,
            &shm,
            &output_name,
            egfx_shared,
            info_tx,
            bitrate,
            quality,
            rate_control,
            fps,
            deferred_resize,
        ),
        CaptureMode::Wlr => {
            let screencopy_mgr = state
                .screencopy_manager
                .as_ref()
                .context("zwlr_screencopy_manager_v1 not available")?
                .clone();
            wlr::capture_loop_wlr(
                &conn,
                &mut event_queue,
                &mut state,
                &qh,
                &wl_output,
                &shm,
                &screencopy_mgr,
                &output_name,
                egfx_shared,
                info_tx,
                bitrate,
                quality,
                rate_control,
                fps,
                deferred_resize,
            )
        }
    }
}

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
            match guard.read() {
                Ok(_) => {}
                Err(e) if is_wayland_would_block(&e) => return Ok(()),
                Err(e) => return Err(anyhow::anyhow!("Wayland read: {}", e)),
            }
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

fn is_wayland_would_block(err: &wayland_client::backend::WaylandError) -> bool {
    match err {
        wayland_client::backend::WaylandError::Io(io) => {
            io.kind() == ErrorKind::WouldBlock || io.raw_os_error() == Some(libc::EAGAIN)
        }
        wayland_client::backend::WaylandError::Protocol(_) => false,
    }
}
