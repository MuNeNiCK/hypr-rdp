mod damage;
#[cfg(feature = "vaapi")]
pub mod dmabuf;
mod frame;
mod scale;
mod wayland;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use ironrdp_displaycontrol::pdu::{DisplayControlMonitorLayout, MonitorOrientation};
use ironrdp_server::{DesktopSize, DisplayUpdate, RdpServerDisplay, RdpServerDisplayUpdates};
use tokio::sync::{mpsc, Mutex};

use crate::egfx::{EgfxShared, H264RateControl};
use crate::input::SharedOutputLayout;

pub(crate) use wayland::HeadlessOutputGuard;

const H264_SOFTWARE_MAX_LONG_DIMENSION: u32 = 3840;
const H264_SOFTWARE_MAX_SHORT_DIMENSION: u32 = 2160;
const DISPLAYCONTROL_MAX_PRESENTATION_AREA: u64 = 3840 * 2400;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    stop_flag: Arc<AtomicBool>,
    capture_handle: Option<std::thread::JoinHandle<()>>,
    headless_guard: Option<HeadlessOutputGuard>,
    pending_initial_resize: Option<DesktopSize>,
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
    wayland::wait_for_output_size(output_name, width, height, Duration::from_secs(5))
        .context("headless output did not reach requested size after resize")?;
    Ok(())
}

fn clamp_to_h264_software_limits(width: u32, height: u32) -> (u32, u32) {
    let width = width & !1;
    let height = height & !1;
    if width == 0 || height == 0 {
        return (width, height);
    }

    let long = width.max(height);
    let short = width.min(height);
    if long <= H264_SOFTWARE_MAX_LONG_DIMENSION && short <= H264_SOFTWARE_MAX_SHORT_DIMENSION {
        return (width, height);
    }

    let scale_by_long = H264_SOFTWARE_MAX_LONG_DIMENSION as f64 / long as f64;
    let scale_by_short = H264_SOFTWARE_MAX_SHORT_DIMENSION as f64 / short as f64;
    let scale = scale_by_long.min(scale_by_short).min(1.0);

    let scaled_width = ((width as f64 * scale).floor() as u32).max(2) & !1;
    let scaled_height = ((height as f64 * scale).floor() as u32).max(2) & !1;
    (scaled_width, scaled_height)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResizeTarget {
    ManagedHeadlessOutput,
    PhysicalPresentation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ResizeDecision {
    target: ResizeTarget,
    width: u32,
    height: u32,
}

fn startup_presentation_size(
    physical_output: bool,
    resolution_fixed: bool,
    configured_resolution: (u32, u32),
    source_size: (u32, u32),
) -> (u32, u32) {
    if physical_output && !resolution_fixed {
        source_size
    } else {
        configured_resolution
    }
}

fn initial_size_resize_decision(
    physical_output: bool,
    resolution_fixed: bool,
    current_resolution: (u32, u32),
    requested_size: (u32, u32),
) -> Option<ResizeDecision> {
    if resolution_fixed {
        return None;
    }

    let (width, height) = requested_size;
    if width == 0 || height == 0 || (width, height) == current_resolution {
        return None;
    }

    Some(ResizeDecision {
        target: if physical_output {
            ResizeTarget::PhysicalPresentation
        } else {
            ResizeTarget::ManagedHeadlessOutput
        },
        width,
        height,
    })
}

fn display_control_resize_decision(
    layout: &DisplayControlMonitorLayout,
    physical_output: bool,
    resolution_fixed: bool,
    current_resolution: (u32, u32),
) -> Option<ResizeDecision> {
    if resolution_fixed {
        return None;
    }

    let (requested_w, requested_h) = if physical_output {
        physical_display_control_size(layout)?
    } else {
        headless_display_control_size(layout)?
    };
    let (width, height) = clamp_to_h264_software_limits(requested_w, requested_h);
    if width == 0 || height == 0 || (width, height) == current_resolution {
        return None;
    }

    Some(ResizeDecision {
        target: if physical_output {
            ResizeTarget::PhysicalPresentation
        } else {
            ResizeTarget::ManagedHeadlessOutput
        },
        width,
        height,
    })
}

fn headless_display_control_size(layout: &DisplayControlMonitorLayout) -> Option<(u32, u32)> {
    let monitor = layout
        .monitors()
        .iter()
        .find(|m| m.is_primary())
        .or_else(|| layout.monitors().first())?;
    Some(monitor.dimensions())
}

fn physical_display_control_size(layout: &DisplayControlMonitorLayout) -> Option<(u32, u32)> {
    let [monitor] = layout.monitors() else {
        return None;
    };
    if !monitor.is_primary() || monitor.position() != Some((0, 0)) {
        return None;
    }
    if matches!(
        monitor.orientation(),
        Some(
            MonitorOrientation::Portrait
                | MonitorOrientation::LandscapeFlipped
                | MonitorOrientation::PortraitFlipped
        )
    ) {
        return None;
    }

    let (width, height) = monitor.dimensions();
    if u64::from(width).saturating_mul(u64::from(height)) > DISPLAYCONTROL_MAX_PRESENTATION_AREA {
        return None;
    }

    Some((width, height))
}

fn apply_presentation_state(inner: &mut HyprDisplayInner, width: u32, height: u32) -> DesktopSize {
    inner.resolution = (width, height);
    inner.width = width as u16;
    inner.height = height as u16;
    let desktop_size = DesktopSize {
        width: inner.width,
        height: inner.height,
    };

    if let Some(shared) = &inner.egfx_shared {
        shared.set_surface_size(inner.width, inner.height);
        shared.prepare_for_resize(inner.width, inner.height);
    }
    if let Err(e) = inner
        .output_layout
        .update_from_output_with_presentation(&inner.output_name, (width, height))
    {
        tracing::warn!(
            "Failed to refresh input layout after presentation resize: {}",
            e
        );
    }

    desktop_size
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
        let configured_resolution = clamp_to_h264_software_limits(resolution.0, resolution.1);
        if configured_resolution != requested_resolution {
            tracing::warn!(
                requested_w = requested_resolution.0,
                requested_h = requested_resolution.1,
                applied_w = configured_resolution.0,
                applied_h = configured_resolution.1,
                "Configured resolution exceeds H.264 software encoder policy limit; clamping"
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
                let mode = format!("{}x{}@60", configured_resolution.0, configured_resolution.1);
                let rule = format!("{},{},-9999x0,1", existing, mode);
                crate::hyprland::keyword_monitor(&rule)
                    .context("failed to resize reused headless output")?;
                wayland::wait_for_output_size(
                    &existing,
                    configured_resolution.0,
                    configured_resolution.1,
                    Duration::from_secs(5),
                )?;
                (
                    existing.clone(),
                    Some(wayland::HeadlessOutputGuard::adopt(existing)),
                )
            } else {
                let (name, guard) = wayland::create_headless_output(
                    configured_resolution.0,
                    configured_resolution.1,
                )?;
                wayland::wait_for_output_size(
                    &name,
                    configured_resolution.0,
                    configured_resolution.1,
                    Duration::from_secs(5),
                )?;
                (name, Some(guard))
            }
        };

        let capture_info = wayland::output_info(&output_name)
            .context("failed to get initial output dimensions")?;
        let presentation_resolution = startup_presentation_size(
            output.is_some(),
            resolution_fixed,
            configured_resolution,
            (capture_info.width, capture_info.height),
        );
        output_layout
            .update_from_output_with_presentation(&output_name, presentation_resolution)
            .context("failed to initialize input layout for output")?;

        let stop_flag = Arc::new(AtomicBool::new(false));

        let protocol_name = match capture_mode {
            CaptureMode::Ext => "ext-image-copy-capture-v1",
            CaptureMode::Wlr => "wlr-screencopy-v1",
        };
        tracing::info!(
            width = capture_info.width,
            height = capture_info.height,
            presentation_w = presentation_resolution.0,
            presentation_h = presentation_resolution.1,
            "Display prepared via {}; capture will start on client connection",
            protocol_name
        );

        let inner = Arc::new(Mutex::new(HyprDisplayInner {
            width: presentation_resolution.0 as u16,
            height: presentation_resolution.1 as u16,
            resolution: presentation_resolution,
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
            stop_flag,
            capture_handle: None,
            headless_guard,
            pending_initial_resize: None,
        }));

        let dims = (
            presentation_resolution.0 as u16,
            presentation_resolution.1 as u16,
        );
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
        let (cw, ch) = clamp_to_h264_software_limits(requested_w, requested_h);
        if cw != (requested_w & !1) || ch != (requested_h & !1) {
            tracing::warn!(
                requested_w,
                requested_h,
                applied_w = cw,
                applied_h = ch,
                "Client requested size exceeds H.264 software encoder policy limit; clamping"
            );
        }

        let mut inner = self.inner.lock().await;
        if let Some(decision) = initial_size_resize_decision(
            inner.output.is_some(),
            inner.resolution_fixed,
            inner.resolution,
            (cw, ch),
        ) {
            match decision.target {
                ResizeTarget::ManagedHeadlessOutput => {
                    tracing::info!(
                        client_w = requested_w,
                        client_h = requested_h,
                        applied_w = decision.width,
                        applied_h = decision.height,
                        server_w = inner.width,
                        server_h = inner.height,
                        "Client requested initial size; resizing headless output"
                    );

                    if let Err(e) =
                        resize_headless_output(&inner.output_name, decision.width, decision.height)
                    {
                        tracing::warn!("Failed to apply client initial size: {}", e);
                    } else {
                        let desktop_size =
                            apply_presentation_state(&mut inner, decision.width, decision.height);
                        inner.pending_initial_resize = Some(desktop_size);
                    }
                }
                ResizeTarget::PhysicalPresentation => {
                    tracing::info!(
                        client_w = requested_w,
                        client_h = requested_h,
                        applied_w = decision.width,
                        applied_h = decision.height,
                        server_w = inner.width,
                        server_h = inner.height,
                        "Client requested initial size; updating physical-output presentation"
                    );
                    let desktop_size =
                        apply_presentation_state(&mut inner, decision.width, decision.height);
                    inner.pending_initial_resize = Some(desktop_size);
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

        let mut inner = self.inner.blocking_lock();
        let Some(decision) = display_control_resize_decision(
            &layout,
            inner.output.is_some(),
            inner.resolution_fixed,
            inner.resolution,
        ) else {
            tracing::trace!(
                resolution_fixed = inner.resolution_fixed,
                physical_output = inner.output.is_some(),
                "Ignoring DisplayControl layout for current output policy"
            );
            return;
        };

        if decision.width != (requested_w & !1) || decision.height != (requested_h & !1) {
            tracing::warn!(
                requested_w,
                requested_h,
                applied_w = decision.width,
                applied_h = decision.height,
                "DisplayControl size exceeds H.264 software encoder policy limit; clamping"
            );
        }

        match decision.target {
            ResizeTarget::ManagedHeadlessOutput => {
                tracing::info!(
                    w = decision.width,
                    h = decision.height,
                    "Client requested resize via DisplayControl"
                );

                if let Err(e) =
                    resize_headless_output(&inner.output_name, decision.width, decision.height)
                {
                    tracing::warn!("Failed to resize headless output: {}", e);
                    return;
                }
                let desktop_size =
                    apply_presentation_state(&mut inner, decision.width, decision.height);
                let _ = inner
                    .update_tx
                    .try_send(DisplayUpdate::Resize(desktop_size));
            }
            ResizeTarget::PhysicalPresentation => {
                tracing::info!(
                    w = decision.width,
                    h = decision.height,
                    "Client requested physical-output presentation resize via DisplayControl"
                );
                let desktop_size =
                    apply_presentation_state(&mut inner, decision.width, decision.height);
                let _ = inner
                    .update_tx
                    .try_send(DisplayUpdate::Resize(desktop_size));
            }
        }
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

        let (tx, rx) = mpsc::channel(128);
        inner.update_tx = tx.clone();

        let pending_initial_resize = inner.pending_initial_resize.take();

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
            pending_initial_resize,
            Arc::clone(&inner.stop_flag),
        )
        .await?;
        inner.capture_handle = Some(capture_handle);
        inner.output_name = capture_info.output_name;
        if let Some(snapshot) = inner.output_layout.snapshot() {
            let presentation = snapshot.presentation_geometry.presentation();
            inner.width = presentation.width as u16;
            inner.height = presentation.height as u16;
            inner.resolution = (presentation.width, presentation.height);
        } else {
            inner.width = capture_info.width as u16;
            inner.height = capture_info.height as u16;
            inner.resolution = (capture_info.width, capture_info.height);
        }

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
    fn h264_software_limit_keeps_supported_landscape_size() {
        assert_eq!(clamp_to_h264_software_limits(1920, 1200), (1920, 1200));
        assert_eq!(clamp_to_h264_software_limits(3840, 2160), (3840, 2160));
    }

    #[test]
    fn h264_software_limit_scales_ultrawide_client_size() {
        assert_eq!(clamp_to_h264_software_limits(5120, 1440), (3840, 1080));
    }

    #[test]
    fn h264_software_limit_scales_portrait_size() {
        assert_eq!(clamp_to_h264_software_limits(1440, 5120), (1080, 3840));
    }

    #[test]
    fn h264_software_limit_rounds_to_even_dimensions() {
        assert_eq!(clamp_to_h264_software_limits(5121, 1441), (3840, 1080));
    }
}

#[cfg(test)]
mod output_downscaling {
    use super::*;
    use ironrdp_displaycontrol::pdu::{
        DeviceScaleFactor, DisplayControlMonitorLayout, MonitorLayoutEntry, MonitorOrientation,
    };

    fn single_primary(width: u32, height: u32) -> DisplayControlMonitorLayout {
        DisplayControlMonitorLayout::new(&[MonitorLayoutEntry::new_primary(width, height).unwrap()])
            .unwrap()
    }

    #[test]
    fn physical_output_startup_uses_explicit_resolution_as_presentation() {
        assert_eq!(
            startup_presentation_size(true, true, (1920, 1080), (3840, 2160)),
            (1920, 1080)
        );
    }

    #[test]
    fn physical_output_startup_uses_source_size_when_resolution_is_omitted() {
        assert_eq!(
            startup_presentation_size(true, false, (1920, 1080), (3840, 2160)),
            (3840, 2160)
        );
    }

    #[test]
    fn headless_startup_keeps_configured_session_resolution() {
        assert_eq!(
            startup_presentation_size(false, false, (1920, 1080), (3840, 2160)),
            (1920, 1080)
        );
    }

    #[test]
    fn physical_output_initial_size_updates_presentation_only() {
        let decision =
            initial_size_resize_decision(true, false, (3840, 2160), (1600, 900)).unwrap();

        assert_eq!(decision.target, ResizeTarget::PhysicalPresentation);
        assert_eq!((decision.width, decision.height), (1600, 900));
    }

    #[test]
    fn physical_output_initial_size_uses_desktop_size_policy_not_displaycontrol_layout_policy() {
        let decision = initial_size_resize_decision(true, false, (3840, 2160), (100, 100)).unwrap();

        assert_eq!(decision.target, ResizeTarget::PhysicalPresentation);
        assert_eq!((decision.width, decision.height), (100, 100));
    }

    #[test]
    fn fixed_zero_and_unchanged_initial_size_requests_are_noops() {
        assert_eq!(
            initial_size_resize_decision(true, true, (1920, 1080), (1600, 900)),
            None
        );
        assert_eq!(
            initial_size_resize_decision(true, false, (1920, 1080), (0, 900)),
            None
        );
        assert_eq!(
            initial_size_resize_decision(true, false, (1920, 1080), (1920, 1080)),
            None
        );
    }

    #[test]
    fn physical_output_displaycontrol_accepts_single_primary_at_origin() {
        let decision =
            display_control_resize_decision(&single_primary(1280, 720), true, false, (1920, 1080))
                .unwrap();

        assert_eq!(decision.target, ResizeTarget::PhysicalPresentation);
        assert_eq!((decision.width, decision.height), (1280, 720));
    }

    #[test]
    fn physical_output_displaycontrol_ignores_physical_size_and_scale_fields() {
        let monitor = MonitorLayoutEntry::new_primary(1280, 720)
            .unwrap()
            .with_physical_dimensions(1000, 500)
            .unwrap()
            .with_desktop_scale_factor(150)
            .unwrap()
            .with_device_scale_factor(DeviceScaleFactor::Scale140Percent);
        let layout = DisplayControlMonitorLayout::new(&[monitor]).unwrap();

        let decision = display_control_resize_decision(&layout, true, false, (1920, 1080)).unwrap();

        assert_eq!(decision.target, ResizeTarget::PhysicalPresentation);
        assert_eq!((decision.width, decision.height), (1280, 720));
    }

    #[test]
    fn physical_output_displaycontrol_rejects_multi_monitor_layouts() {
        let monitors = [
            MonitorLayoutEntry::new_primary(1280, 720).unwrap(),
            MonitorLayoutEntry::new_secondary(1024, 768).unwrap(),
        ];
        let layout = DisplayControlMonitorLayout::new(&monitors).unwrap();

        assert_eq!(
            display_control_resize_decision(&layout, true, false, (1920, 1080)),
            None
        );
    }

    #[test]
    fn physical_output_displaycontrol_rejects_valid_rotated_orientation() {
        let monitor = MonitorLayoutEntry::new_primary(1280, 720)
            .unwrap()
            .with_orientation(MonitorOrientation::Portrait);
        let layout = DisplayControlMonitorLayout::new(&[monitor]).unwrap();

        assert_eq!(
            display_control_resize_decision(&layout, true, false, (1920, 1080)),
            None
        );
    }

    #[test]
    fn physical_output_displaycontrol_rejects_layouts_over_advertised_area_cap() {
        assert_eq!(
            display_control_resize_decision(&single_primary(8192, 2000), true, false, (1920, 1080)),
            None
        );
    }

    #[test]
    fn physical_output_displaycontrol_normalizes_odd_height_for_h264() {
        let decision =
            display_control_resize_decision(&single_primary(1280, 721), true, false, (1920, 1080))
                .unwrap();

        assert_eq!(decision.target, ResizeTarget::PhysicalPresentation);
        assert_eq!((decision.width, decision.height), (1280, 720));
    }

    #[test]
    fn physical_output_displaycontrol_fixed_and_unchanged_requests_are_noops() {
        assert_eq!(
            display_control_resize_decision(&single_primary(1280, 720), true, true, (1920, 1080)),
            None
        );
        assert_eq!(
            display_control_resize_decision(&single_primary(1920, 1080), true, false, (1920, 1080)),
            None
        );
    }
}

#[cfg(test)]
mod managed_headless_resize {
    use super::*;
    use ironrdp_displaycontrol::pdu::{DisplayControlMonitorLayout, MonitorLayoutEntry};

    fn single_primary(width: u32, height: u32) -> DisplayControlMonitorLayout {
        DisplayControlMonitorLayout::new(&[MonitorLayoutEntry::new_primary(width, height).unwrap()])
            .unwrap()
    }

    #[test]
    fn managed_headless_initial_size_still_targets_headless_output_resize() {
        let decision =
            initial_size_resize_decision(false, false, (1920, 1080), (1600, 900)).unwrap();

        assert_eq!(decision.target, ResizeTarget::ManagedHeadlessOutput);
        assert_eq!((decision.width, decision.height), (1600, 900));
    }

    #[test]
    fn managed_headless_displaycontrol_still_targets_headless_output_resize() {
        let decision =
            display_control_resize_decision(&single_primary(1600, 900), false, false, (1920, 1080))
                .unwrap();

        assert_eq!(decision.target, ResizeTarget::ManagedHeadlessOutput);
        assert_eq!((decision.width, decision.height), (1600, 900));
    }

    #[test]
    fn managed_headless_fixed_and_unchanged_resize_requests_remain_noops() {
        assert_eq!(
            initial_size_resize_decision(false, true, (1920, 1080), (1600, 900)),
            None
        );
        assert_eq!(
            display_control_resize_decision(&single_primary(1600, 900), false, true, (1920, 1080)),
            None
        );
        assert_eq!(
            display_control_resize_decision(
                &single_primary(1920, 1080),
                false,
                false,
                (1920, 1080)
            ),
            None
        );
    }
}
