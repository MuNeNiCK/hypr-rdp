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

use crate::egfx::{EgfxShared, H264BackendPolicy, H264RateControl};
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
    h264_backend: H264BackendPolicy,
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

/// Fit a requested presentation size to the captured source and the H.264
/// policy limits, keeping both dimensions even. Aspect mismatches are
/// letterboxed by the presentation scaler, so the requested size is otherwise
/// kept as-is: reshaping it here announces a size the client did not ask for,
/// which clients answer with another resize request — the negotiation then
/// walks a shrinking staircase (728x408 → 724x408 → 724x406 → …) instead of
/// converging.
///
/// Mirroring cannot invent detail the captured output does not have. The
/// scaler magnifies the source by `min(width / source_width, height /
/// source_height)`, and only above 1 is every extra pixel invented: a HiDPI
/// client asking in physical pixels — a 1707x960 window on a 1.5-scaled 4K
/// screen requests 2560x1440 — spreads the configured bitrate over 1.78x the
/// pixels of a 1080p source and pays the encode time for them.
///
/// Dividing both sides by that factor keeps the requested aspect ratio, so a
/// presentation the client picked for letterboxing (a 1920x1200 window onto a
/// 3840x1080 output magnifies by 0.5 and is left alone) is unaffected. When
/// the request matches the source aspect the result is the source itself,
/// which is an identity geometry and keeps the zero-copy capture path;
/// otherwise the same letterboxing as before happens on a smaller canvas.
/// For any source with even dimensions of at least two pixels the fitted size
/// normalizes to itself, so a client repeating its request still converges.
fn normalize_presentation_size(requested_size: (u32, u32), source_size: (u32, u32)) -> (u32, u32) {
    let (source_w, source_h) = source_size;
    let (width, height) = (requested_size.0 & !1, requested_size.1 & !1);
    if width == 0 || height == 0 || source_w == 0 || source_h == 0 {
        return (0, 0);
    }

    // The encoder only emits even sizes, so the largest presentation that can
    // carry the source 1:1 is the source rounded down to even, floored at the
    // two-pixel minimum.
    let (fit_w, fit_h) = (source_w.max(2) & !1, source_h.max(2) & !1);
    let (fitted_width, fitted_height) = if width <= fit_w || height <= fit_h {
        (u64::from(width), u64::from(height))
    } else {
        let (requested_w, requested_h) = (u64::from(width), u64::from(height));
        let (fit_w, fit_h) = (u64::from(fit_w), u64::from(fit_h));
        if requested_w * fit_h <= requested_h * fit_w {
            // Width-limited: the fit lands on the source width exactly.
            (fit_w, requested_h * fit_w / requested_w)
        } else {
            // Height-limited. This axis only adds letterbox padding, and
            // AVC444 needs a width that is a multiple of four, so round to one
            // where there is room for it.
            let padded = requested_w * fit_h / requested_h;
            let padded = if padded >= 4 {
                padded & !3
            } else {
                padded & !1
            };
            (padded, fit_h)
        }
    };

    // Encoder limits last: the fit only shrinks, so clamping after it never
    // undoes the fit, while clamping first can land a pixel under the source
    // and lose the identity geometry.
    clamp_to_h264_software_limits(fitted_width as u32, fitted_height as u32)
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
    let requested = if physical_output && !resolution_fixed {
        source_size
    } else {
        configured_resolution
    };
    if !physical_output {
        return requested;
    }
    if resolution_fixed {
        // A pinned resolution is the operator's call, upscale included; only
        // the encoder limits still apply.
        return clamp_to_h264_software_limits(requested.0, requested.1);
    }
    normalize_presentation_size(requested, source_size)
}

fn initial_size_resize_decision(
    physical_output: bool,
    resolution_fixed: bool,
    current_resolution: (u32, u32),
    requested_size: (u32, u32),
    source_size: Option<(u32, u32)>,
) -> Option<ResizeDecision> {
    if resolution_fixed {
        return None;
    }

    let (width, height) = if physical_output {
        normalize_presentation_size(requested_size, source_size?)
    } else {
        requested_size
    };
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
    source_size: Option<(u32, u32)>,
) -> Option<ResizeDecision> {
    if resolution_fixed {
        return None;
    }

    let (requested_w, requested_h) = if physical_output {
        physical_display_control_size(layout)?
    } else {
        headless_display_control_size(layout)?
    };
    let (width, height) = if physical_output {
        normalize_presentation_size((requested_w, requested_h), source_size?)
    } else {
        clamp_to_h264_software_limits(requested_w, requested_h)
    };
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

fn apply_presentation_state_with(
    inner: &mut HyprDisplayInner,
    width: u32,
    height: u32,
    refresh_layout: impl FnOnce(&SharedOutputLayout, &str, (u32, u32)) -> Result<()>,
) -> Option<DesktopSize> {
    if let Err(e) = refresh_layout(&inner.output_layout, &inner.output_name, (width, height)) {
        tracing::warn!(
            "Failed to refresh input layout after presentation resize: {}",
            e
        );
        return None;
    }

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

    Some(desktop_size)
}

fn apply_resize_decision_with(
    inner: &mut HyprDisplayInner,
    decision: ResizeDecision,
    mut resize_headless: impl FnMut(&str, u32, u32) -> Result<()>,
    mut refresh_layout: impl FnMut(&SharedOutputLayout, &str, (u32, u32)) -> Result<()>,
) -> Option<DesktopSize> {
    match decision.target {
        ResizeTarget::ManagedHeadlessOutput => {
            if let Err(e) = resize_headless(&inner.output_name, decision.width, decision.height) {
                tracing::warn!("Failed to resize headless output: {}", e);
                None
            } else {
                apply_presentation_state_with(
                    inner,
                    decision.width,
                    decision.height,
                    |layout, name, presentation| refresh_layout(layout, name, presentation),
                )
            }
        }
        ResizeTarget::PhysicalPresentation => apply_presentation_state_with(
            inner,
            decision.width,
            decision.height,
            |layout, name, presentation| refresh_layout(layout, name, presentation),
        ),
    }
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
        h264_backend: H264BackendPolicy,
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
        let requested_presentation_resolution = if output.is_some() && !resolution_fixed {
            (capture_info.width, capture_info.height)
        } else {
            configured_resolution
        };
        let presentation_resolution = startup_presentation_size(
            output.is_some(),
            resolution_fixed,
            configured_resolution,
            (capture_info.width, capture_info.height),
        );
        if output.is_some() && presentation_resolution != requested_presentation_resolution {
            tracing::info!(
                requested_w = requested_presentation_resolution.0,
                requested_h = requested_presentation_resolution.1,
                source_w = capture_info.width,
                source_h = capture_info.height,
                applied_w = presentation_resolution.0,
                applied_h = presentation_resolution.1,
                "Physical output presentation bounds clamped to encoder limits and captured source"
            );
        }
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
            h264_backend,
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

    async fn request_initial_size_with(
        &mut self,
        client_size: DesktopSize,
        mut resize_headless: impl FnMut(&str, u32, u32) -> Result<()>,
        mut refresh_layout: impl FnMut(&SharedOutputLayout, &str, (u32, u32)) -> Result<()>,
    ) -> DesktopSize {
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
        let source_size = inner
            .output_layout
            .snapshot()
            .map(|snapshot| (snapshot.output_w, snapshot.output_h));
        if let Some(decision) = initial_size_resize_decision(
            inner.output.is_some(),
            inner.resolution_fixed,
            inner.resolution,
            (cw, ch),
            source_size,
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

                    if let Some(desktop_size) = apply_resize_decision_with(
                        &mut inner,
                        decision,
                        &mut resize_headless,
                        &mut refresh_layout,
                    ) {
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
                    if let Some(desktop_size) = apply_resize_decision_with(
                        &mut inner,
                        decision,
                        &mut resize_headless,
                        &mut refresh_layout,
                    ) {
                        inner.pending_initial_resize = Some(desktop_size);
                    }
                }
            }
        } else if cw > 0 && ch > 0 && (cw != inner.resolution.0 || ch != inner.resolution.1) {
            let (source_w, source_h) = source_size.unwrap_or_default();
            tracing::info!(
                client_w = requested_w,
                client_h = requested_h,
                applied_w = inner.width,
                applied_h = inner.height,
                source_w,
                source_h,
                resolution_fixed = inner.resolution_fixed,
                "Client requested initial size; keeping the current presentation"
            );
        }

        DesktopSize {
            width: inner.width,
            height: inner.height,
        }
    }

    fn request_layout_with(
        &mut self,
        layout: DisplayControlMonitorLayout,
        mut resize_headless: impl FnMut(&str, u32, u32) -> Result<()>,
        mut refresh_layout: impl FnMut(&SharedOutputLayout, &str, (u32, u32)) -> Result<()>,
    ) {
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
        let source_size = inner
            .output_layout
            .snapshot()
            .map(|snapshot| (snapshot.output_w, snapshot.output_h));
        let Some(decision) = display_control_resize_decision(
            &layout,
            inner.output.is_some(),
            inner.resolution_fixed,
            inner.resolution,
            source_size,
        ) else {
            tracing::trace!(
                resolution_fixed = inner.resolution_fixed,
                physical_output = inner.output.is_some(),
                "Ignoring DisplayControl layout for current output policy"
            );
            return;
        };

        if decision.width != (requested_w & !1) || decision.height != (requested_h & !1) {
            match decision.target {
                ResizeTarget::PhysicalPresentation => {
                    tracing::info!(
                        requested_w,
                        requested_h,
                        applied_w = decision.width,
                        applied_h = decision.height,
                        source_w = source_size.map(|(w, _)| w).unwrap_or_default(),
                        source_h = source_size.map(|(_, h)| h).unwrap_or_default(),
                        "DisplayControl presentation bounds clamped to encoder limits and captured source"
                    );
                }
                ResizeTarget::ManagedHeadlessOutput => {
                    tracing::warn!(
                        requested_w,
                        requested_h,
                        applied_w = decision.width,
                        applied_h = decision.height,
                        "DisplayControl size exceeds H.264 software encoder policy limit; clamping"
                    );
                }
            }
        }

        match decision.target {
            ResizeTarget::ManagedHeadlessOutput => {
                tracing::info!(
                    w = decision.width,
                    h = decision.height,
                    "Client requested resize via DisplayControl"
                );

                if let Some(desktop_size) = apply_resize_decision_with(
                    &mut inner,
                    decision,
                    &mut resize_headless,
                    &mut refresh_layout,
                ) {
                    let _ = inner
                        .update_tx
                        .try_send(DisplayUpdate::Resize(desktop_size));
                }
            }
            ResizeTarget::PhysicalPresentation => {
                tracing::info!(
                    w = decision.width,
                    h = decision.height,
                    "Client requested physical-output presentation resize via DisplayControl"
                );
                if let Some(desktop_size) = apply_resize_decision_with(
                    &mut inner,
                    decision,
                    &mut resize_headless,
                    &mut refresh_layout,
                ) {
                    let _ = inner
                        .update_tx
                        .try_send(DisplayUpdate::Resize(desktop_size));
                }
            }
        }
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
        self.request_initial_size_with(
            client_size,
            resize_headless_output,
            SharedOutputLayout::update_from_output_with_presentation,
        )
        .await
    }

    fn request_layout(&mut self, layout: DisplayControlMonitorLayout) {
        self.request_layout_with(
            layout,
            resize_headless_output,
            SharedOutputLayout::update_from_output_with_presentation,
        );
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
        let capture_dead = Arc::new(tokio::sync::Notify::new());

        inner.stop_flag = Arc::new(AtomicBool::new(false));
        let (capture_info, capture_handle) = wayland::start_capture(
            tx,
            Arc::clone(&capture_dead),
            inner.capture_mode,
            inner.egfx_shared.clone(),
            Arc::clone(&inner.output_layout),
            inner.bitrate,
            inner.quality,
            inner.rate_control,
            inner.fps,
            inner.h264_backend,
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

        Ok(Box::new(HyprDisplayUpdates { rx, capture_dead }))
    }
}

struct HyprDisplayUpdates {
    rx: mpsc::Receiver<DisplayUpdate>,
    /// Signaled when the capture thread exits without a stop request. The
    /// display half keeps a live sender for resize delivery, so the update
    /// channel cannot close by itself; without this signal a dead capture
    /// freezes the session instead of disconnecting it.
    capture_dead: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl RdpServerDisplayUpdates for HyprDisplayUpdates {
    async fn next_update(&mut self) -> Result<Option<DisplayUpdate>> {
        tokio::select! {
            biased;
            update = self.rx.recv() => Ok(update),
            _ = self.capture_dead.notified() => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn next_update_delivers_buffered_updates_before_the_death_signal() {
        let (tx, rx) = mpsc::channel(4);
        let capture_dead = Arc::new(tokio::sync::Notify::new());
        let mut updates = HyprDisplayUpdates {
            rx,
            capture_dead: Arc::clone(&capture_dead),
        };

        tx.send(DisplayUpdate::Resize(DesktopSize {
            width: 800,
            height: 600,
        }))
        .await
        .unwrap();
        capture_dead.notify_one();

        assert!(matches!(
            updates.next_update().await.unwrap(),
            Some(DisplayUpdate::Resize(_))
        ));
        // With the queue drained, the stored death permit disconnects.
        assert!(updates.next_update().await.unwrap().is_none());
        drop(tx);
    }

    #[tokio::test]
    async fn next_update_disconnects_when_capture_dies_with_a_live_sender() {
        // The display half keeps a sender clone for resizes, so the channel
        // alone can never close; the death signal must end the session.
        let (tx, rx) = mpsc::channel::<DisplayUpdate>(4);
        let capture_dead = Arc::new(tokio::sync::Notify::new());
        let mut updates = HyprDisplayUpdates {
            rx,
            capture_dead: Arc::clone(&capture_dead),
        };

        capture_dead.notify_one();

        assert!(updates.next_update().await.unwrap().is_none());
        drop(tx);
    }

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
    use crate::egfx::{EgfxCodecPolicy, DEFAULT_MAX_FRAMES_IN_FLIGHT};
    use ironrdp_displaycontrol::pdu::{
        DeviceScaleFactor, DisplayControlMonitorLayout, MonitorLayoutEntry, MonitorOrientation,
    };

    fn single_primary(width: u32, height: u32) -> DisplayControlMonitorLayout {
        DisplayControlMonitorLayout::new(&[MonitorLayoutEntry::new_primary(width, height).unwrap()])
            .unwrap()
    }

    fn physical_display_for_callback_test(
        resolution: (u32, u32),
    ) -> (
        HyprDisplay,
        mpsc::Receiver<DisplayUpdate>,
        Arc<EgfxShared>,
        Arc<SharedOutputLayout>,
    ) {
        physical_display_for_callback_test_with_source(resolution, resolution)
    }

    fn physical_display_for_callback_test_with_source(
        source: (u32, u32),
        resolution: (u32, u32),
    ) -> (
        HyprDisplay,
        mpsc::Receiver<DisplayUpdate>,
        Arc<EgfxShared>,
        Arc<SharedOutputLayout>,
    ) {
        let (tx, rx) = mpsc::channel(4);
        let shared = Arc::new(EgfxShared::with_codec_policy(
            DEFAULT_MAX_FRAMES_IN_FLIGHT,
            EgfxCodecPolicy::Auto,
        ));
        shared.set_surface_size(resolution.0 as u16, resolution.1 as u16);
        let output_layout = Arc::new(SharedOutputLayout::new());
        output_layout
            .update_snapshot_for_test(
                "DP-1", source.0, source.1, source.0, source.1, 0, 0, resolution,
            )
            .expect("initial physical layout");
        let inner = HyprDisplayInner {
            width: resolution.0 as u16,
            height: resolution.1 as u16,
            resolution,
            capture_mode: CaptureMode::Ext,
            output_name: "DP-1".into(),
            egfx_shared: Some(Arc::clone(&shared)),
            output_layout: Arc::clone(&output_layout),
            update_tx: tx,
            update_rx: None,
            bitrate: 1_000_000,
            quality: 23,
            rate_control: H264RateControl::Vbr,
            h264_backend: H264BackendPolicy::Auto,
            fps: 30,
            output: Some("DP-1".into()),
            resolution_fixed: false,
            stop_flag: Arc::new(AtomicBool::new(false)),
            capture_handle: None,
            headless_guard: None,
            pending_initial_resize: None,
        };

        (
            HyprDisplay {
                inner: Arc::new(Mutex::new(inner)),
            },
            rx,
            shared,
            output_layout,
        )
    }

    fn refresh_physical_layout_for_test(
        layout: &SharedOutputLayout,
        output_name: &str,
        presentation: (u32, u32),
    ) -> Result<()> {
        let snapshot = layout.snapshot().expect("existing physical layout");
        layout.update_snapshot_for_test(
            output_name,
            snapshot.output_w,
            snapshot.output_h,
            snapshot.layout_extent_w,
            snapshot.layout_extent_h,
            snapshot.output_offset_x,
            snapshot.output_offset_y,
            presentation,
        )
    }

    #[test]
    fn presentation_fits_a_hidpi_request_to_the_source_instead_of_upscaling() {
        // A 1707x960 window on a 1.5-scaled 4K screen asks in physical pixels.
        assert_eq!(
            normalize_presentation_size((2560, 1440), (1920, 1080)),
            (1920, 1080)
        );
    }

    #[test]
    fn presentation_fit_lands_on_the_source_exactly() {
        // Landing a pixel short would stop the geometry from being an identity,
        // which costs the zero-copy capture path.
        for request in [(1984, 1116), (2944, 1656), (2560, 1440)] {
            assert_eq!(
                normalize_presentation_size(request, (1920, 1080)),
                (1920, 1080),
                "request {request:?}"
            );
        }
        assert_eq!(
            normalize_presentation_size((2656, 1494), (2560, 1440)),
            (2560, 1440)
        );
    }

    #[test]
    fn physical_output_startup_keeps_pinned_resolution_above_the_source() {
        // --resolution is an explicit operator choice, upscale included.
        assert_eq!(
            startup_presentation_size(true, true, (2560, 1440), (1920, 1080)),
            (2560, 1440)
        );
    }

    #[test]
    fn presentation_fit_keeps_the_requested_aspect_ratio() {
        // Magnification is min(3000/1920, 1200/1080) = 1.111: the width stays
        // proportional to the request instead of collapsing to the source.
        assert_eq!(
            normalize_presentation_size((3000, 1200), (1920, 1080)),
            (2700, 1080)
        );
    }

    #[test]
    fn presentation_fit_lands_on_the_source_when_the_height_limits_it() {
        // The other branch: a wide request whose height runs out first keeps
        // the source height exactly and pads the width.
        assert_eq!(
            normalize_presentation_size((3840, 1200), (1920, 1080)),
            (3456, 1080)
        );
    }

    #[test]
    fn presentation_fit_runs_before_the_encoder_limits() {
        // Clamping first would land two pixels under the source and lose the
        // identity geometry the zero-copy capture path needs.
        assert_eq!(
            normalize_presentation_size((3842, 2162), (3840, 2160)),
            (3840, 2160)
        );
    }

    #[test]
    fn presentation_fit_keeps_even_dimensions_for_the_encoder() {
        // A 1600x900 window at 125% scale asks for 2000x1125; the fit divides
        // that request down to 1921 columns, which H.264 cannot encode.
        assert_eq!(
            normalize_presentation_size((2000, 1125), (1920, 1080)),
            (1920, 1080)
        );
    }

    #[test]
    fn presentation_fit_never_falls_below_the_encoder_minimum() {
        // A source under the two-pixel minimum is still magnified: a zero
        // dimension reads as "no resize" everywhere downstream.
        assert_eq!(normalize_presentation_size((1920, 1080), (1, 1)), (2, 2));
        assert_eq!(normalize_presentation_size((640, 480), (2, 1)), (2, 2));
    }

    #[test]
    fn an_odd_request_is_evened_before_it_is_fitted() {
        // DisplayControl layouts carry odd sizes; fitting the raw request
        // instead of the evened one lands two rows off the source.
        assert_eq!(
            normalize_presentation_size((1924, 1085), (1920, 1080)),
            (1920, 1080)
        );
    }

    #[test]
    fn an_unknown_source_size_is_rejected_instead_of_fitted() {
        // A zero source size means "not known yet"; fitting to it would
        // announce the two-pixel encoder minimum as the presentation.
        assert_eq!(normalize_presentation_size((1920, 1080), (0, 0)), (0, 0));
        assert_eq!(normalize_presentation_size((1920, 1080), (1920, 0)), (0, 0));
        assert_eq!(
            initial_size_resize_decision(true, false, (1920, 1080), (1600, 900), Some((0, 0))),
            None
        );
    }

    #[test]
    fn presentation_at_or_below_the_source_is_left_alone() {
        assert_eq!(
            normalize_presentation_size((1920, 1080), (1920, 1080)),
            (1920, 1080)
        );
        assert_eq!(
            normalize_presentation_size((960, 540), (1920, 1080)),
            (960, 540)
        );
        // Letterboxing: the window is taller than the ultrawide source, which
        // magnifies by 0.5 and needs no fitting.
        assert_eq!(
            normalize_presentation_size((1920, 1200), (3840, 1080)),
            (1920, 1200)
        );
    }

    #[test]
    fn presentation_fit_converges_when_a_client_repeats_its_request() {
        // Clients answer a new size with another request; a fitted size must
        // survive re-normalization unchanged or the negotiation walks a
        // shrinking staircase.
        let source = (1920, 1080);
        for request in [(2560, 1440), (4000, 1200), (3000, 3000)] {
            let fitted = normalize_presentation_size(request, source);
            assert_eq!(normalize_presentation_size(fitted, source), fitted);
        }
    }

    #[test]
    fn physical_output_displaycontrol_fits_a_hidpi_request_to_the_source() {
        let decision = display_control_resize_decision(
            &single_primary(2560, 1440),
            true,
            false,
            (1280, 720),
            Some((1920, 1080)),
        )
        .unwrap();

        assert_eq!(decision.target, ResizeTarget::PhysicalPresentation);
        assert_eq!((decision.width, decision.height), (1920, 1080));
    }

    #[test]
    fn physical_output_startup_keeps_explicit_resolution_for_letterboxing() {
        assert_eq!(
            startup_presentation_size(true, true, (1920, 1200), (3840, 1080)),
            (1920, 1200)
        );
        assert_eq!(
            startup_presentation_size(true, true, (1600, 900), (3840, 2160)),
            (1600, 900)
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
        let decision = initial_size_resize_decision(
            true,
            false,
            (3840, 2160),
            (1600, 900),
            Some((3840, 2160)),
        )
        .unwrap();

        assert_eq!(decision.target, ResizeTarget::PhysicalPresentation);
        assert_eq!((decision.width, decision.height), (1600, 900));
    }

    #[test]
    fn physical_output_initial_size_keeps_client_size_for_letterboxing() {
        let decision = initial_size_resize_decision(
            true,
            false,
            (3840, 1080),
            (1920, 1200),
            Some((3840, 1080)),
        )
        .unwrap();

        assert_eq!(decision.target, ResizeTarget::PhysicalPresentation);
        assert_eq!((decision.width, decision.height), (1920, 1200));
    }

    #[test]
    fn physical_output_size_negotiation_reaches_fixed_point_immediately() {
        // Regression: reshaping the requested size to the source aspect made
        // the client re-request every applied size, walking a shrinking
        // staircase (728x408 → 724x408 → 724x406 → … → 704x396) with a full
        // capture and encoder restart on every step.
        let source = Some((3840, 2160));
        let decision =
            initial_size_resize_decision(true, false, (3840, 2160), (728, 408), source).unwrap();
        assert_eq!((decision.width, decision.height), (728, 408));

        // The client echoes the applied size back; the negotiation must stop.
        let echo = initial_size_resize_decision(
            true,
            false,
            (decision.width, decision.height),
            (decision.width, decision.height),
            source,
        );
        assert_eq!(echo, None);
    }

    #[tokio::test]
    async fn physical_output_initial_size_callback_updates_presentation_state() {
        let (mut display, _rx, shared, _layout) =
            physical_display_for_callback_test_with_source((3840, 1080), (3840, 1080));

        let size = display
            .request_initial_size_with(
                DesktopSize {
                    width: 1920,
                    height: 1200,
                },
                |_name, _width, _height| panic!("physical output must not resize headless output"),
                refresh_physical_layout_for_test,
            )
            .await;

        assert_eq!(
            size,
            DesktopSize {
                width: 1920,
                height: 1200
            }
        );
        assert_eq!(shared.get_surface_size(), (1920, 1200));

        let inner = display.inner.lock().await;
        assert_eq!(inner.resolution, (1920, 1200));
        assert_eq!(
            inner.pending_initial_resize,
            Some(DesktopSize {
                width: 1920,
                height: 1200
            })
        );
    }

    #[tokio::test]
    async fn physical_output_initial_size_callback_layout_failure_preserves_state() {
        let (mut display, _rx, shared, _layout) = physical_display_for_callback_test((3840, 2160));

        let size = display
            .request_initial_size_with(
                DesktopSize {
                    width: 1600,
                    height: 900,
                },
                |_name, _width, _height| panic!("physical output must not resize headless output"),
                |_layout, _name, _presentation| anyhow::bail!("layout refresh failed"),
            )
            .await;

        assert_eq!(
            size,
            DesktopSize {
                width: 3840,
                height: 2160
            }
        );
        assert_eq!(shared.get_surface_size(), (3840, 2160));

        let inner = display.inner.lock().await;
        assert_eq!(inner.resolution, (3840, 2160));
        assert_eq!(inner.pending_initial_resize, None);
    }

    #[test]
    fn physical_output_initial_size_uses_desktop_size_policy_not_displaycontrol_layout_policy() {
        let decision =
            initial_size_resize_decision(true, false, (3840, 2160), (100, 100), Some((3840, 2160)))
                .unwrap();

        assert_eq!(decision.target, ResizeTarget::PhysicalPresentation);
        assert_eq!((decision.width, decision.height), (100, 100));
    }

    #[test]
    fn fixed_zero_and_unchanged_initial_size_requests_are_noops() {
        assert_eq!(
            initial_size_resize_decision(true, true, (1920, 1080), (1600, 900), Some((1920, 1080))),
            None
        );
        assert_eq!(
            initial_size_resize_decision(true, false, (1920, 1080), (0, 900), Some((1920, 1080))),
            None
        );
        assert_eq!(
            initial_size_resize_decision(
                true,
                false,
                (1920, 1200),
                (1920, 1200),
                Some((3840, 1080))
            ),
            None
        );
    }

    #[test]
    fn physical_output_displaycontrol_accepts_single_primary_at_origin() {
        let decision = display_control_resize_decision(
            &single_primary(1280, 720),
            true,
            false,
            (1920, 1080),
            Some((1920, 1080)),
        )
        .unwrap();

        assert_eq!(decision.target, ResizeTarget::PhysicalPresentation);
        assert_eq!((decision.width, decision.height), (1280, 720));
    }

    #[test]
    fn physical_output_displaycontrol_keeps_monitor_size_for_letterboxing() {
        let decision = display_control_resize_decision(
            &single_primary(1920, 1200),
            true,
            false,
            (3840, 1080),
            Some((3840, 1080)),
        )
        .unwrap();

        assert_eq!(decision.target, ResizeTarget::PhysicalPresentation);
        assert_eq!((decision.width, decision.height), (1920, 1200));
    }

    #[test]
    fn physical_output_displaycontrol_callback_emits_presentation_resize() {
        let (mut display, mut rx, shared, _layout) =
            physical_display_for_callback_test((1920, 1080));

        display.request_layout_with(
            single_primary(1280, 720),
            |_name, _width, _height| panic!("physical output must not resize headless output"),
            refresh_physical_layout_for_test,
        );

        match rx.try_recv().expect("resize update") {
            DisplayUpdate::Resize(size) => {
                assert_eq!(
                    size,
                    DesktopSize {
                        width: 1280,
                        height: 720
                    }
                );
            }
            other => panic!("expected resize update, got {other:?}"),
        }
        assert_eq!(shared.get_surface_size(), (1280, 720));

        let inner = display.inner.blocking_lock();
        assert_eq!(inner.resolution, (1280, 720));
    }

    #[test]
    fn physical_output_displaycontrol_callback_layout_failure_emits_no_resize() {
        let (mut display, mut rx, shared, _layout) =
            physical_display_for_callback_test((1920, 1080));

        display.request_layout_with(
            single_primary(1280, 720),
            |_name, _width, _height| panic!("physical output must not resize headless output"),
            |_layout, _name, _presentation| anyhow::bail!("layout refresh failed"),
        );

        assert!(rx.try_recv().is_err());
        assert_eq!(shared.get_surface_size(), (1920, 1080));

        let inner = display.inner.blocking_lock();
        assert_eq!(inner.resolution, (1920, 1080));
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

        let decision =
            display_control_resize_decision(&layout, true, false, (1920, 1080), Some((1920, 1080)))
                .unwrap();

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
            display_control_resize_decision(&layout, true, false, (1920, 1080), Some((1920, 1080))),
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
            display_control_resize_decision(&layout, true, false, (1920, 1080), Some((1920, 1080))),
            None
        );
    }

    #[test]
    fn physical_output_displaycontrol_rejects_layouts_over_advertised_area_cap() {
        assert_eq!(
            display_control_resize_decision(
                &single_primary(8192, 2000),
                true,
                false,
                (1920, 1080),
                Some((1920, 1080))
            ),
            None
        );
    }

    #[test]
    fn physical_output_displaycontrol_normalizes_odd_height_for_h264() {
        let decision = display_control_resize_decision(
            &single_primary(1280, 721),
            true,
            false,
            (1920, 1080),
            Some((1920, 1080)),
        )
        .unwrap();

        assert_eq!(decision.target, ResizeTarget::PhysicalPresentation);
        assert_eq!((decision.width, decision.height), (1280, 720));
    }

    #[test]
    fn physical_output_displaycontrol_fixed_and_unchanged_requests_are_noops() {
        assert_eq!(
            display_control_resize_decision(
                &single_primary(1280, 720),
                true,
                true,
                (1920, 1080),
                Some((1920, 1080))
            ),
            None
        );
        assert_eq!(
            display_control_resize_decision(
                &single_primary(1920, 1080),
                true,
                false,
                (1920, 1080),
                Some((1920, 1080))
            ),
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

    fn headless_inner_for_resize_test_with_tx(
        resolution: (u32, u32),
        tx: mpsc::Sender<DisplayUpdate>,
    ) -> HyprDisplayInner {
        let output_layout = Arc::new(SharedOutputLayout::new());
        output_layout
            .update_snapshot_for_test(
                "HEADLESS-1",
                resolution.0,
                resolution.1,
                resolution.0,
                resolution.1,
                0,
                0,
                resolution,
            )
            .expect("initial headless layout");
        HyprDisplayInner {
            width: resolution.0 as u16,
            height: resolution.1 as u16,
            resolution,
            capture_mode: CaptureMode::Ext,
            output_name: "HEADLESS-1".into(),
            egfx_shared: None,
            output_layout,
            update_tx: tx,
            update_rx: None,
            bitrate: 1_000_000,
            quality: 23,
            rate_control: H264RateControl::Vbr,
            h264_backend: H264BackendPolicy::Auto,
            fps: 30,
            output: None,
            resolution_fixed: false,
            stop_flag: Arc::new(AtomicBool::new(false)),
            capture_handle: None,
            headless_guard: None,
            pending_initial_resize: None,
        }
    }

    fn headless_inner_for_resize_test(resolution: (u32, u32)) -> HyprDisplayInner {
        let (tx, _rx) = mpsc::channel(4);
        headless_inner_for_resize_test_with_tx(resolution, tx)
    }

    fn headless_display_for_callback_test(
        resolution: (u32, u32),
    ) -> (HyprDisplay, mpsc::Receiver<DisplayUpdate>) {
        let (tx, rx) = mpsc::channel(4);
        (
            HyprDisplay {
                inner: Arc::new(Mutex::new(headless_inner_for_resize_test_with_tx(
                    resolution, tx,
                ))),
            },
            rx,
        )
    }

    fn refresh_headless_layout_for_test(
        layout: &SharedOutputLayout,
        output_name: &str,
        presentation: (u32, u32),
    ) -> Result<()> {
        layout.update_snapshot_for_test(
            output_name,
            presentation.0,
            presentation.1,
            presentation.0,
            presentation.1,
            0,
            0,
            presentation,
        )
    }

    #[test]
    fn managed_headless_initial_size_still_targets_headless_output_resize() {
        let decision =
            initial_size_resize_decision(false, false, (1920, 1080), (1600, 900), None).unwrap();

        assert_eq!(decision.target, ResizeTarget::ManagedHeadlessOutput);
        assert_eq!((decision.width, decision.height), (1600, 900));
    }

    #[tokio::test]
    async fn managed_headless_initial_size_callback_resizes_headless_and_updates_pending_resize() {
        let (mut display, _rx) = headless_display_for_callback_test((1920, 1080));
        let mut called = None;

        let size = display
            .request_initial_size_with(
                DesktopSize {
                    width: 1600,
                    height: 900,
                },
                |name, width, height| {
                    called = Some((name.to_string(), width, height));
                    Ok(())
                },
                refresh_headless_layout_for_test,
            )
            .await;

        assert_eq!(called, Some(("HEADLESS-1".into(), 1600, 900)));
        assert_eq!(
            size,
            DesktopSize {
                width: 1600,
                height: 900
            }
        );

        let inner = display.inner.lock().await;
        assert_eq!(inner.resolution, (1600, 900));
        assert_eq!(
            inner.pending_initial_resize,
            Some(DesktopSize {
                width: 1600,
                height: 900
            })
        );
    }

    #[test]
    fn managed_headless_displaycontrol_still_targets_headless_output_resize() {
        let decision = display_control_resize_decision(
            &single_primary(1600, 900),
            false,
            false,
            (1920, 1080),
            None,
        )
        .unwrap();

        assert_eq!(decision.target, ResizeTarget::ManagedHeadlessOutput);
        assert_eq!((decision.width, decision.height), (1600, 900));
    }

    #[test]
    fn managed_headless_displaycontrol_callback_resizes_headless_and_emits_resize() {
        let (mut display, mut rx) = headless_display_for_callback_test((1920, 1080));
        let mut called = None;

        display.request_layout_with(
            single_primary(1600, 900),
            |name, width, height| {
                called = Some((name.to_string(), width, height));
                Ok(())
            },
            refresh_headless_layout_for_test,
        );

        assert_eq!(called, Some(("HEADLESS-1".into(), 1600, 900)));
        match rx.try_recv().expect("resize update") {
            DisplayUpdate::Resize(size) => {
                assert_eq!(
                    size,
                    DesktopSize {
                        width: 1600,
                        height: 900
                    }
                );
            }
            other => panic!("expected resize update, got {other:?}"),
        }

        let inner = display.inner.blocking_lock();
        assert_eq!(inner.resolution, (1600, 900));
    }

    #[test]
    fn managed_headless_displaycontrol_callback_failure_preserves_state_and_emits_no_resize() {
        let (mut display, mut rx) = headless_display_for_callback_test((1920, 1080));

        display.request_layout_with(
            single_primary(1600, 900),
            |_name, _width, _height| anyhow::bail!("resize failed"),
            |_layout, _output_name, _presentation| {
                panic!("layout refresh must not run after headless resize failure")
            },
        );

        assert!(rx.try_recv().is_err());
        let inner = display.inner.blocking_lock();
        assert_eq!(inner.resolution, (1920, 1080));
        assert_eq!((inner.width, inner.height), (1920, 1080));
    }

    #[test]
    fn managed_headless_displaycontrol_callback_layout_failure_preserves_rdp_state() {
        let (mut display, mut rx) = headless_display_for_callback_test((1920, 1080));
        let mut called = None;

        display.request_layout_with(
            single_primary(1600, 900),
            |name, width, height| {
                called = Some((name.to_string(), width, height));
                Ok(())
            },
            |_layout, _output_name, _presentation| anyhow::bail!("layout refresh failed"),
        );

        assert_eq!(called, Some(("HEADLESS-1".into(), 1600, 900)));
        assert!(rx.try_recv().is_err());
        let inner = display.inner.blocking_lock();
        assert_eq!(inner.resolution, (1920, 1080));
        assert_eq!((inner.width, inner.height), (1920, 1080));
    }

    #[test]
    fn managed_headless_resize_side_effects_run_only_after_headless_resize_succeeds() {
        let mut inner = headless_inner_for_resize_test((1920, 1080));
        let mut called = None;
        let decision = ResizeDecision {
            target: ResizeTarget::ManagedHeadlessOutput,
            width: 1600,
            height: 900,
        };

        let desktop_size = apply_resize_decision_with(
            &mut inner,
            decision,
            |name, width, height| {
                called = Some((name.to_string(), width, height));
                Ok(())
            },
            refresh_headless_layout_for_test,
        )
        .expect("resize applies");

        assert_eq!(called, Some(("HEADLESS-1".into(), 1600, 900)));
        assert_eq!(
            desktop_size,
            DesktopSize {
                width: 1600,
                height: 900
            }
        );
        assert_eq!(inner.resolution, (1600, 900));
        assert_eq!((inner.width, inner.height), (1600, 900));
    }

    #[test]
    fn managed_headless_resize_failure_preserves_existing_presentation_state() {
        let mut inner = headless_inner_for_resize_test((1920, 1080));
        let decision = ResizeDecision {
            target: ResizeTarget::ManagedHeadlessOutput,
            width: 1600,
            height: 900,
        };

        assert!(apply_resize_decision_with(
            &mut inner,
            decision,
            |_name, _width, _height| { anyhow::bail!("resize failed") },
            |_layout, _output_name, _presentation| {
                panic!("layout refresh must not run after headless resize failure")
            }
        )
        .is_none());

        assert_eq!(inner.resolution, (1920, 1080));
        assert_eq!((inner.width, inner.height), (1920, 1080));
    }

    #[test]
    fn managed_headless_fixed_and_unchanged_resize_requests_remain_noops() {
        assert_eq!(
            initial_size_resize_decision(false, true, (1920, 1080), (1600, 900), None),
            None
        );
        assert_eq!(
            display_control_resize_decision(
                &single_primary(1600, 900),
                false,
                true,
                (1920, 1080),
                None
            ),
            None
        );
        assert_eq!(
            display_control_resize_decision(
                &single_primary(1920, 1080),
                false,
                false,
                (1920, 1080),
                None
            ),
            None
        );
    }
}
