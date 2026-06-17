use std::os::fd::{AsFd, AsRawFd};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use ironrdp_server::{DesktopSize, PixelFormat};
use wayland_client::protocol::{wl_output, wl_shm};
use wayland_client::{Connection, QueueHandle};
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1;

#[cfg(feature = "vaapi")]
use super::dmabuf_capture;
use super::state::AppState;
use super::{create_shm_fd, poll_dispatch, CaptureInfo, MmapRegion, POLL_TIMEOUT_MS};
use crate::capture::frame::{FramePacer, FrameProcessor};
#[cfg(feature = "vaapi")]
use crate::capture::scale::dmabuf_zero_copy_allowed;
use crate::capture::scale::{
    output_downscaling_generation_action, prepare_presentation_frame, presentation_frame_shape,
};
use crate::egfx::{EgfxShared, H264RateControl};
#[cfg(feature = "vaapi")]
use crate::input::OutputLayoutSnapshot;
use crate::input::SharedOutputLayout;

#[cfg(feature = "vaapi")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DmaBufCaptureErrorAction {
    FallBackToShm,
    RestartCapture,
}

#[cfg(feature = "vaapi")]
fn dmabuf_setup_allowed(egfx_available: bool, snapshot: Option<&OutputLayoutSnapshot>) -> bool {
    egfx_available && snapshot.is_some_and(dmabuf_zero_copy_allowed)
}

#[cfg(feature = "vaapi")]
fn dmabuf_capture_error_action(err: &anyhow::Error) -> DmaBufCaptureErrorAction {
    if dmabuf_capture::is_capture_session_geometry_changed(err) {
        DmaBufCaptureErrorAction::RestartCapture
    } else {
        DmaBufCaptureErrorAction::FallBackToShm
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn capture_loop_ext(
    conn: &Connection,
    event_queue: &mut wayland_client::EventQueue<AppState>,
    state: &mut AppState,
    qh: &QueueHandle<AppState>,
    output: &wl_output::WlOutput,
    shm: &wl_shm::WlShm,
    output_name: &str,
    output_layout: Arc<SharedOutputLayout>,
    egfx_shared: Option<Arc<EgfxShared>>,
    info_tx: &mut Option<tokio::sync::oneshot::Sender<Result<CaptureInfo>>>,
    bitrate: u32,
    quality: u8,
    rate_control: H264RateControl,
    fps: u32,
    pending_initial_resize: Option<DesktopSize>,
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
        qh,
        (),
    );
    state.session = Some(session.clone());

    event_queue
        .roundtrip(state)
        .context("failed to get buffer constraints")?;
    event_queue
        .roundtrip(state)
        .context("failed to get buffer constraints (2nd)")?;

    let width = state.buffer_width;
    let height = state.buffer_height;
    if width == 0 || height == 0 {
        bail!("invalid buffer dimensions: {}x{}", width, height);
    }

    // Try DMA-BUF path if available (vaapi feature + compositor supports it + EGFX expected)
    #[cfg(feature = "vaapi")]
    {
        let snapshot = output_layout.snapshot();
        if dmabuf_setup_allowed(egfx_shared.is_some(), snapshot.as_ref()) {
            if let Some(ref dmabuf_result) =
                dmabuf_capture::try_setup_dmabuf(state, qh, width, height)
            {
                match dmabuf_result {
                    Ok(dmabuf_ctx) => {
                        if let Some(tx) = info_tx.take() {
                            let _ = tx.send(Ok(CaptureInfo {
                                width,
                                height,
                                output_name: output_name.to_string(),
                            }));
                        }
                        match dmabuf_capture::capture_loop_ext_dmabuf(
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
                            rate_control,
                            fps,
                            pending_initial_resize,
                            Arc::clone(&output_layout),
                        ) {
                            Ok(()) => return Ok(()),
                            Err(e) => {
                                match dmabuf_capture_error_action(&e) {
                                    DmaBufCaptureErrorAction::RestartCapture => {
                                        tracing::warn!(
                                            "DMA-BUF source geometry changed, restarting EXT capture: {:#}",
                                            e
                                        );
                                        return Err(e);
                                    }
                                    DmaBufCaptureErrorAction::FallBackToShm => {
                                        tracing::warn!(
                                            "DMA-BUF capture failed, falling back to SHM: {:#}",
                                            e
                                        );
                                        // Fall through to SHM path
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("DMA-BUF setup failed, falling back to SHM: {:#}", e);
                    }
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
    let buffer_0 = pool_0.create_buffer(
        0,
        width as i32,
        height as i32,
        stride as i32,
        shm_format,
        qh,
        (),
    );
    let mmap_0 = MmapRegion::new(shm_fd_0.as_fd().as_raw_fd(), buf_size)?;

    let shm_fd_1 = create_shm_fd(buf_size)?;
    let pool_1 = shm.create_pool(shm_fd_1.as_fd(), buf_size_i32, qh, ());
    let buffer_1 = pool_1.create_buffer(
        0,
        width as i32,
        height as i32,
        stride as i32,
        shm_format,
        qh,
        (),
    );
    let mmap_1 = MmapRegion::new(shm_fd_1.as_fd().as_raw_fd(), buf_size)?;

    if let Some(tx) = info_tx.take() {
        let _ = tx.send(Ok(CaptureInfo {
            width,
            height,
            output_name: output_name.to_string(),
        }));
    }

    let pixel_format = match shm_format {
        wl_shm::Format::Argb8888 => PixelFormat::BgrA32,
        wl_shm::Format::Xrgb8888 => PixelFormat::BgrX32,
        wl_shm::Format::Xbgr8888 => PixelFormat::RgbX32,
        wl_shm::Format::Abgr8888 => PixelFormat::RgbA32,
        _ => PixelFormat::BgrA32,
    };

    tracing::info!(width, height, ?shm_format, output = %output_name, mode = "ext", fps, "Starting capture loop (double-buffered SHM)");

    let snapshot = output_layout
        .snapshot()
        .context("output layout not initialized for capture scaling")?;
    let (presentation_width, presentation_height, presentation_stride) =
        presentation_frame_shape(width, height, stride, &snapshot)?;
    let mut processor_generation = snapshot.geometry_generation;

    let mut proc = FrameProcessor::new(
        egfx_shared.clone(),
        presentation_width,
        presentation_height,
        pixel_format,
        presentation_stride,
        bitrate,
        quality,
        rate_control,
        fps,
    );
    proc.set_pending_initial_resize(pending_initial_resize);
    let mut frame_pacer = FramePacer::new(fps, Instant::now());

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
        if state.should_stop() {
            break;
        }

        // Wait for current frame to complete (poll-based for responsive shutdown)
        while !state.frame_ready && !state.frame_failed {
            poll_dispatch(event_queue, state, POLL_TIMEOUT_MS)?;
            if state.should_stop() {
                break;
            }
        }
        frame.destroy();

        // Shutdown interrupted the wait — exit cleanly
        if !state.frame_ready && !state.frame_failed {
            break;
        }

        // Save completed frame state before starting next capture
        let completed_failed = state.frame_failed;
        let completed_idx = cap_idx;
        let completed_damage_regions = state.damage_regions.clone();

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
        if completed_failed {
            continue;
        }
        let data = mmaps[completed_idx].as_slice();
        let snapshot = output_layout
            .snapshot()
            .context("output layout not initialized for capture scaling")?;
        let prepared = prepare_presentation_frame(
            data,
            width,
            height,
            stride,
            pixel_format,
            &completed_damage_regions,
            &snapshot,
        )?;
        let action = output_downscaling_generation_action(processor_generation, &prepared);
        if action.refresh_processor {
            processor_generation = action.next_generation;
            proc = FrameProcessor::new(
                egfx_shared.clone(),
                prepared.width,
                prepared.height,
                pixel_format,
                prepared.stride,
                bitrate,
                quality,
                rate_control,
                fps,
            );
        }
        proc.queue_damage(&action.damage_regions);
        proc.stats.record_capture(
            prepared.width,
            prepared.height,
            prepared.damage_regions.len(),
            false,
        );

        // Always enforce frame rate limit. Without this, compositor animations
        // (window open, cursor blink) flood the client with 60fps H.264 frames,
        // overwhelming the decoder and building up a decode queue that delays
        // all subsequent frames (including keystroke updates) by seconds.
        let has_pending_damage = proc.has_pending_damage();
        if !has_pending_damage && proc.sent_first_frame {
            proc.stats
                .record_no_damage_skip(prepared.width, prepared.height);
        } else if frame_pacer.should_send(
            Instant::now(),
            proc.sent_first_frame,
            has_pending_damage,
            proc.pacing_fps(),
        ) {
            if !proc.process(prepared.data.as_ref(), &state.tx) {
                break;
            }
        } else {
            proc.stats
                .record_pacer_skip(prepared.width, prepared.height);
        }
    }

    Ok(())
}

#[cfg(all(test, feature = "vaapi"))]
mod tests {
    use super::*;
    use crate::display::geometry::{PresentationGeometry, Size};

    fn snapshot(source: (u32, u32), presentation: (u32, u32)) -> OutputLayoutSnapshot {
        let source_size = Size::new(source.0, source.1).unwrap();
        let presentation_size = Size::new(presentation.0, presentation.1).unwrap();
        OutputLayoutSnapshot {
            output_name: "DP-1".into(),
            output_w: source.0,
            output_h: source.1,
            layout_extent_w: source.0,
            layout_extent_h: source.1,
            output_offset_x: 0,
            output_offset_y: 0,
            presentation_geometry: PresentationGeometry::new(source_size, presentation_size),
            geometry_generation: 0,
        }
    }

    #[test]
    fn ext_dmabuf_branch_skips_setup_when_presentation_geometry_is_scaled() {
        assert!(dmabuf_setup_allowed(
            true,
            Some(&snapshot((1920, 1080), (1920, 1080)))
        ));
        assert!(!dmabuf_setup_allowed(
            true,
            Some(&snapshot((3840, 2160), (1920, 1080)))
        ));
        assert!(!dmabuf_setup_allowed(
            false,
            Some(&snapshot((1920, 1080), (1920, 1080)))
        ));
        assert!(!dmabuf_setup_allowed(true, None));
    }

    #[test]
    fn ext_dmabuf_source_geometry_error_restarts_instead_of_shm_fallback() {
        let restart = anyhow::Error::new(dmabuf_capture::DmaBufCaptureSessionGeometryChanged);
        let fallback = anyhow::anyhow!("presentation geometry changed while using DMA-BUF");

        assert_eq!(
            dmabuf_capture_error_action(&restart),
            DmaBufCaptureErrorAction::RestartCapture
        );
        assert_eq!(
            dmabuf_capture_error_action(&fallback),
            DmaBufCaptureErrorAction::FallBackToShm
        );
    }
}
