#[cfg(feature = "vaapi")]
pub mod dmabuf;
mod wayland;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use ironrdp_displaycontrol::pdu::DisplayControlMonitorLayout;
use ironrdp_server::{DesktopSize, DisplayUpdate, RdpServerDisplay, RdpServerDisplayUpdates};
use tokio::sync::{mpsc, Mutex};

use crate::egfx::{EgfxShared, H264RateControl};
use crate::input::SharedOutputLayout;

pub(crate) use wayland::HeadlessOutputGuard;

const OPENH264_MAX_LONG_DIMENSION: u32 = 3840;
const OPENH264_MAX_SHORT_DIMENSION: u32 = 2160;

#[derive(Clone, Copy, Debug)]
pub enum CaptureMode {
    /// ext-image-copy-capture-v1
    Ext,
    /// wlr-screencopy-v1
    Wlr,
}

/// Inner state for display capture. Held behind Arc<Mutex<>> so that
/// server::run() can call shutdown() independently of RdpServer's drop.
struct HyprDisplayInner {
    width: u16,
    height: u16,
    resolution: (u32, u32),
    capture_mode: CaptureMode,
    output_name: String,
    egfx_shared: Option<Arc<EgfxShared>>,
    output_layout: Arc<SharedOutputLayout>,
    update_tx: mpsc::Sender<DisplayUpdate>,
    update_rx: Option<mpsc::Receiver<DisplayUpdate>>,
    bitrate: u32,
    quality: u8,
    rate_control: H264RateControl,
    fps: u32,
    output: Option<String>,
    resolution_fixed: bool,
    pending_resize: bool,
    deferred_resize: Option<(u32, u32)>,
    stop_flag: Arc<AtomicBool>,
    capture_handle: Option<std::thread::JoinHandle<()>>,
    headless_guard: Option<HeadlessOutputGuard>,
}

impl HyprDisplayInner {
    /// Explicit shutdown: stop capture thread → join → remove headless output.
    fn shutdown(&mut self) {
        self.stop_flag.store(true, Ordering::Release);
        if let Some(handle) = self.capture_handle.take() {
            let _ = handle.join();
        }
        // Thread exited, Wayland connection closed. Safe to remove output.
        drop(self.headless_guard.take());
    }
}

impl Drop for HyprDisplayInner {
    fn drop(&mut self) {
        // Safety net: if shutdown() was not called (e.g. early error in setup()),
        // ensure capture thread is joined before headless_guard drops.
        self.shutdown();
    }
}

/// Shared handle to HyprDisplayInner for explicit shutdown from server::run().
#[derive(Clone)]
pub struct HyprDisplayHandle {
    inner: Arc<Mutex<HyprDisplayInner>>,
}

impl HyprDisplayHandle {
    pub async fn shutdown(&self) {
        let mut inner = self.inner.lock().await;
        inner.shutdown();
    }
}

fn resize_headless_output(output_name: &str, width: u32, height: u32) -> Result<()> {
    let mode = format!("{}x{}@60", width, height);
    let rule = format!("{},{},-9999x0,1", output_name, mode);
    crate::hyprland::keyword_monitor(&rule).context("failed to resize headless output")?;
    wayland::wait_for_output(output_name, Duration::from_secs(5))
        .context("headless output not ready after resize")?;
    Ok(())
}

fn clamp_to_openh264_limits(width: u32, height: u32) -> (u32, u32) {
    let width = width & !1;
    let height = height & !1;
    if width == 0 || height == 0 {
        return (width, height);
    }

    let long = width.max(height);
    let short = width.min(height);
    if long <= OPENH264_MAX_LONG_DIMENSION && short <= OPENH264_MAX_SHORT_DIMENSION {
        return (width, height);
    }

    let scale_by_long = OPENH264_MAX_LONG_DIMENSION as f64 / long as f64;
    let scale_by_short = OPENH264_MAX_SHORT_DIMENSION as f64 / short as f64;
    let scale = scale_by_long.min(scale_by_short).min(1.0);

    let scaled_width = ((width as f64 * scale).floor() as u32).max(2) & !1;
    let scaled_height = ((height as f64 * scale).floor() as u32).max(2) & !1;
    (scaled_width, scaled_height)
}

/// RdpServerDisplay implementation that delegates to HyprDisplayInner.
pub struct HyprDisplay {
    inner: Arc<Mutex<HyprDisplayInner>>,
}

impl HyprDisplay {
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        resolution: (u32, u32),
        capture_mode: CaptureMode,
        egfx_shared: Arc<EgfxShared>,
        output_layout: Arc<SharedOutputLayout>,
        bitrate: u32,
        quality: u8,
        rate_control: H264RateControl,
        fps: u32,
        resolution_fixed: bool,
        output: Option<String>,
    ) -> Result<(Self, HyprDisplayHandle, (u16, u16))> {
        let (tx, rx) = mpsc::channel(128);
        let requested_resolution = resolution;
        let resolution = clamp_to_openh264_limits(resolution.0, resolution.1);
        if resolution != requested_resolution {
            tracing::warn!(
                requested_w = requested_resolution.0,
                requested_h = requested_resolution.1,
                applied_w = resolution.0,
                applied_h = resolution.1,
                "Configured resolution exceeds OpenH264 software encoder limit; clamping"
            );
        }

        // Create or verify output up front, but defer Wayland capture until a
        // client subscribes to display updates. This keeps idle memory bounded.
        let (output_name, headless_guard) = if let Some(ref name) = output {
            (name.clone(), None)
        } else {
            let stale = wayland::list_stale_headless_outputs().unwrap_or_default();
            if let Some(existing) = stale.into_iter().next() {
                tracing::info!(name = %existing, "Reusing headless output from previous session");
                let mode = format!("{}x{}@60", resolution.0, resolution.1);
                let rule = format!("{},{},-9999x0,1", existing, mode);
                crate::hyprland::keyword_monitor(&rule)
                    .context("failed to resize reused headless output")?;
                wayland::wait_for_output(&existing, Duration::from_secs(5))?;
                (
                    existing.clone(),
                    Some(wayland::HeadlessOutputGuard::adopt(existing)),
                )
            } else {
                let (name, guard) = wayland::create_headless_output(resolution.0, resolution.1)?;
                wayland::wait_for_output(&name, Duration::from_secs(5))?;
                (name, Some(guard))
            }
        };

        let capture_info = wayland::output_info(&output_name)
            .context("failed to get initial output dimensions")?;
        output_layout
            .update_from_output(&output_name)
            .context("failed to initialize input layout for output")?;

        let stop_flag = Arc::new(AtomicBool::new(false));

        let protocol_name = match capture_mode {
            CaptureMode::Ext => "ext-image-copy-capture-v1",
            CaptureMode::Wlr => "wlr-screencopy-v1",
        };
        tracing::info!(
            width = capture_info.width,
            height = capture_info.height,
            "Display prepared via {}; capture will start on client connection",
            protocol_name
        );

        let inner = Arc::new(Mutex::new(HyprDisplayInner {
            width: capture_info.width as u16,
            height: capture_info.height as u16,
            resolution,
            capture_mode,
            output_name: capture_info.output_name,
            egfx_shared: Some(egfx_shared),
            output_layout,
            update_tx: tx,
            update_rx: Some(rx),
            bitrate,
            quality,
            rate_control,
            fps,
            output,
            resolution_fixed,
            pending_resize: false,
            deferred_resize: None,
            stop_flag,
            capture_handle: None,
            headless_guard,
        }));

        let dims = (capture_info.width as u16, capture_info.height as u16);
        let handle = HyprDisplayHandle {
            inner: Arc::clone(&inner),
        };
        Ok((Self { inner }, handle, dims))
    }
}

#[async_trait]
impl RdpServerDisplay for HyprDisplay {
    async fn size(&mut self) -> DesktopSize {
        let inner = self.inner.lock().await;
        DesktopSize {
            width: inner.width,
            height: inner.height,
        }
    }

    async fn request_initial_size(&mut self, client_size: DesktopSize) -> DesktopSize {
        let requested_w = client_size.width as u32;
        let requested_h = client_size.height as u32;

        // H.264 requires even dimensions
        let (cw, ch) = clamp_to_openh264_limits(requested_w, requested_h);
        if cw != (requested_w & !1) || ch != (requested_h & !1) {
            tracing::warn!(
                requested_w,
                requested_h,
                applied_w = cw,
                applied_h = ch,
                "Client requested size exceeds OpenH264 software encoder limit; clamping"
            );
        }

        let mut inner = self.inner.lock().await;
        if cw > 0
            && ch > 0
            && (cw != inner.resolution.0 || ch != inner.resolution.1)
            && !inner.resolution_fixed
            && inner.output.is_none()
        {
            tracing::info!(
                client_w = requested_w,
                client_h = requested_h,
                applied_w = cw,
                applied_h = ch,
                server_w = inner.width,
                server_h = inner.height,
                "Client requested initial size; resizing headless output"
            );

            if let Err(e) = resize_headless_output(&inner.output_name, cw, ch) {
                tracing::warn!("Failed to apply client initial size: {}", e);
            } else {
                inner.resolution = (cw, ch);
                inner.width = cw as u16;
                inner.height = ch as u16;
                inner.deferred_resize = Some((cw, ch));
                if let Some(shared) = &inner.egfx_shared {
                    shared.set_surface_size(inner.width, inner.height);
                }
                if let Err(e) = inner.output_layout.update_from_output(&inner.output_name) {
                    tracing::warn!("Failed to refresh input layout after initial resize: {}", e);
                }
            }
        } else if cw > 0 && ch > 0 && (cw != inner.resolution.0 || ch != inner.resolution.1) {
            tracing::info!(
                client_w = requested_w,
                client_h = requested_h,
                applied_w = cw,
                applied_h = ch,
                server_w = inner.width,
                server_h = inner.height,
                resolution_fixed = inner.resolution_fixed,
                "Client requested initial size (keeping configured server size)"
            );
        }

        DesktopSize {
            width: inner.width,
            height: inner.height,
        }
    }

    fn request_layout(&mut self, layout: DisplayControlMonitorLayout) {
        let monitor = match layout.monitors().iter().find(|m| m.is_primary()) {
            Some(m) => m,
            None => match layout.monitors().first() {
                Some(m) => m,
                None => return,
            },
        };

        let (requested_w, requested_h) = monitor.dimensions();
        let desktop_scale = monitor.desktop_scale_factor();
        let device_scale = monitor.device_scale_factor();
        let physical = monitor.physical_dimensions();

        tracing::info!(
            w = requested_w,
            h = requested_h,
            ?desktop_scale,
            ?device_scale,
            ?physical,
            monitors = layout.monitors().len(),
            "Client requested DisplayControl layout"
        );

        if requested_w == 0
            || requested_h == 0
            || requested_w > u16::MAX as u32
            || requested_h > u16::MAX as u32
        {
            tracing::warn!(
                w = requested_w,
                h = requested_h,
                "Ignoring invalid DisplayControl dimensions"
            );
            return;
        }

        let (w, h) = clamp_to_openh264_limits(requested_w, requested_h);
        if w != (requested_w & !1) || h != (requested_h & !1) {
            tracing::warn!(
                requested_w,
                requested_h,
                applied_w = w,
                applied_h = h,
                "DisplayControl size exceeds OpenH264 software encoder limit; clamping"
            );
        }

        if w == 0 || h == 0 {
            tracing::warn!("Dimensions too small after even-rounding, ignoring");
            return;
        }

        // request_layout is sync, so use try_lock
        let mut inner = match self.inner.try_lock() {
            Ok(g) => g,
            Err(_) => return,
        };

        if (w == inner.resolution.0 && h == inner.resolution.1)
            || inner.output.is_some()
            || inner.resolution_fixed
        {
            return;
        }

        tracing::info!(w, h, "Client requested resize via DisplayControl");

        if let Err(e) = resize_headless_output(&inner.output_name, w, h) {
            tracing::warn!("Failed to resize headless output: {}", e);
            return;
        }

        inner.resolution = (w, h);
        inner.width = w as u16;
        inner.height = h as u16;
        inner.pending_resize = false;

        if let Some(shared) = &inner.egfx_shared {
            shared.set_surface_size(inner.width, inner.height);
            shared.prepare_for_resize(inner.width, inner.height);
        }

        if let Err(e) = inner.output_layout.update_from_output(&inner.output_name) {
            tracing::warn!("Failed to refresh input layout after resize: {}", e);
        }

        let _ = inner.update_tx.try_send(DisplayUpdate::Resize(DesktopSize {
            width: w as u16,
            height: h as u16,
        }));
    }

    async fn updates(&mut self) -> Result<Box<dyn RdpServerDisplayUpdates>> {
        // Extract stop_flag and handle before joining, to avoid holding
        // the Mutex during a blocking join() call.
        let (stop_flag, handle) = {
            let mut inner = self.inner.lock().await;
            drop(inner.update_rx.take());
            (Arc::clone(&inner.stop_flag), inner.capture_handle.take())
        };
        stop_flag.store(true, Ordering::Release);
        if let Some(handle) = handle {
            let _ = tokio::task::spawn_blocking(move || handle.join()).await;
        }

        let mut inner = self.inner.lock().await;

        if let Some(ref shared) = inner.egfx_shared {
            if inner.pending_resize {
                shared.prepare_for_resize(inner.width, inner.height);
                inner.pending_resize = false;
            } else {
                shared.reset_for_new_client();
            }
        }

        let (tx, rx) = mpsc::channel(128);
        inner.update_tx = tx.clone();

        let deferred = if let Some((w, h)) = inner.deferred_resize.take() {
            inner.resolution = (w, h);
            inner.width = w as u16;
            inner.height = h as u16;
            inner.pending_resize = true;

            if inner.headless_guard.is_some() {
                if let Err(e) = resize_headless_output(&inner.output_name, w, h) {
                    tracing::warn!("Failed to resize headless output: {}", e);
                }
            }

            Some(DesktopSize {
                width: w as u16,
                height: h as u16,
            })
        } else {
            None
        };

        inner.stop_flag = Arc::new(AtomicBool::new(false));
        let (capture_info, capture_handle) = wayland::start_capture(
            tx,
            inner.capture_mode,
            inner.egfx_shared.clone(),
            Arc::clone(&inner.output_layout),
            inner.bitrate,
            inner.quality,
            inner.rate_control,
            inner.fps,
            inner.output_name.clone(),
            deferred,
            Arc::clone(&inner.stop_flag),
        )
        .await?;
        inner.capture_handle = Some(capture_handle);
        inner.width = capture_info.width as u16;
        inner.height = capture_info.height as u16;
        inner.output_name = capture_info.output_name;

        Ok(Box::new(HyprDisplayUpdates { rx }))
    }
}

struct HyprDisplayUpdates {
    rx: mpsc::Receiver<DisplayUpdate>,
}

#[async_trait]
impl RdpServerDisplayUpdates for HyprDisplayUpdates {
    async fn next_update(&mut self) -> Result<Option<DisplayUpdate>> {
        Ok(self.rx.recv().await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openh264_limit_keeps_supported_landscape_size() {
        assert_eq!(clamp_to_openh264_limits(1920, 1200), (1920, 1200));
        assert_eq!(clamp_to_openh264_limits(3840, 2160), (3840, 2160));
    }

    #[test]
    fn openh264_limit_scales_ultrawide_client_size() {
        assert_eq!(clamp_to_openh264_limits(5120, 1440), (3840, 1080));
    }

    #[test]
    fn openh264_limit_scales_portrait_size() {
        assert_eq!(clamp_to_openh264_limits(1440, 5120), (1080, 3840));
    }

    #[test]
    fn openh264_limit_rounds_to_even_dimensions() {
        assert_eq!(clamp_to_openh264_limits(5121, 1441), (3840, 1080));
    }
}
