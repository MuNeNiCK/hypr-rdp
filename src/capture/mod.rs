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
        let cw = client_size.width as u32;
        let ch = client_size.height as u32;

        // H.264 requires even dimensions
        let cw = cw & !1;
        let ch = ch & !1;

        let mut inner = self.inner.lock().await;
        if cw > 0
            && ch > 0
            && (cw != inner.resolution.0 || ch != inner.resolution.1)
            && !inner.resolution_fixed
            && inner.output.is_none()
        {
            tracing::info!(
                client_w = cw,
                client_h = ch,
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
                client_w = cw,
                client_h = ch,
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

        let (mut w, mut h) = monitor.dimensions();
        let desktop_scale = monitor.desktop_scale_factor();
        let device_scale = monitor.device_scale_factor();
        let physical = monitor.physical_dimensions();

        tracing::info!(
            w,
            h,
            ?desktop_scale,
            ?device_scale,
            ?physical,
            monitors = layout.monitors().len(),
            "Client requested DisplayControl layout"
        );

        if w == 0 || h == 0 || w > u16::MAX as u32 || h > u16::MAX as u32 {
            tracing::warn!(w, h, "Ignoring invalid DisplayControl dimensions");
            return;
        }

        w &= !1;
        h &= !1;

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
