use std::os::fd::{AsFd, AsRawFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use ironrdp_server::PixelFormat;
use wayland_client::protocol::{wl_output, wl_shm};
use wayland_client::{Connection, QueueHandle};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1;

use super::state::AppState;
use super::{create_shm_fd, poll_dispatch, CaptureInfo, MmapRegion, POLL_TIMEOUT_MS};
use crate::capture::frame::{FramePacer, FrameProcessor};
use crate::egfx::{EgfxShared, H264RateControl};

#[allow(clippy::too_many_arguments)]
pub(super) fn capture_loop_wlr(
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
    rate_control: H264RateControl,
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
            bail!(
                "timed out waiting for wlr-screencopy buffer info after {}s",
                probe_timeout.as_secs()
            );
        }
    }
    probe.destroy();

    let width = state.buffer_width;
    let height = state.buffer_height;
    let shm_format = state.shm_format.unwrap_or(wl_shm::Format::Xrgb8888);
    let stride = if state.wlr_stride > 0 {
        state.wlr_stride
    } else {
        width * 4
    };
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

    tracing::info!(width, height, ?shm_format, stride, output = %output_name, mode = "wlr", fps, "Starting capture loop (double-buffered)");

    let mut proc = FrameProcessor::new(
        egfx_shared,
        width,
        height,
        pixel_format,
        stride,
        bitrate,
        quality,
        rate_control,
        fps,
        deferred_resize,
    );
    let mut frame_pacer = FramePacer::new(fps, Instant::now());

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
        if state.should_stop() {
            break;
        }
    }

    loop {
        // Save completed frame state
        let completed_failed = state.frame_failed;
        let completed_idx = cap_idx;
        let completed_damage_regions = state.damage_regions.clone();
        frame.destroy();

        if state.should_stop() {
            break;
        }
        if !state.frame_ready && !state.frame_failed {
            break;
        } // shutdown interrupted

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
            proc.queue_damage(&completed_damage_regions);
            let data = mmaps[completed_idx].as_slice();
            proc.stats.record_capture(width, height);
            let has_pending_damage = proc.has_pending_damage();
            if !has_pending_damage && proc.sent_first_frame {
                proc.stats.record_no_damage_skip(width, height);
            } else if frame_pacer.should_send(
                Instant::now(),
                proc.sent_first_frame,
                has_pending_damage,
            ) {
                if !proc.process(data, &state.tx) {
                    break;
                }
            } else {
                proc.stats.record_pacer_skip(width, height);
            }
        }

        // Wait for next frame to complete (poll-based for responsive shutdown)
        while !state.frame_ready && !state.frame_failed {
            poll_dispatch(event_queue, state, POLL_TIMEOUT_MS)?;
            if !buffer_sent && state.buffer_width > 0 {
                // Detect compositor buffer renegotiation (dimensions, stride, or format)
                let new_stride = if state.wlr_stride > 0 {
                    state.wlr_stride
                } else {
                    state.buffer_width * 4
                };
                if state.buffer_width != width
                    || state.buffer_height != height
                    || new_stride != stride
                    || state.shm_format.unwrap_or(wl_shm::Format::Xrgb8888) != shm_format
                {
                    tracing::warn!(
                        old_w = width,
                        old_h = height,
                        new_w = state.buffer_width,
                        new_h = state.buffer_height,
                        "WLR: compositor changed buffer parameters, restarting capture"
                    );
                    frame.destroy();
                    bail!("WLR buffer parameters changed, restarting capture");
                }
                frame.copy_with_damage(buffers[cap_idx]);
                conn.flush().context("Wayland flush failed")?;
                buffer_sent = true;
            }
            if state.should_stop() {
                break;
            }
        }
    }

    Ok(())
}
