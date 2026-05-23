use std::fmt;
use std::os::fd::{AsFd, AsRawFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use ironrdp_server::{DesktopSize, PixelFormat};
use wayland_client::protocol::{wl_output, wl_shm};
use wayland_client::{Connection, Proxy, QueueHandle};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1,
};

use super::state::AppState;
use super::{create_shm_fd, poll_dispatch, CaptureInfo, MmapRegion, POLL_TIMEOUT_MS};
use crate::capture::frame::{FramePacer, FrameProcessor};
use crate::egfx::{EgfxShared, H264RateControl};

const WLR_FRAME_STALL_TIMEOUT: Duration = Duration::from_secs(2);
const WLR_EMPTY_DAMAGE_FULL_SCAN_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug)]
pub(super) struct BufferParametersChanged;

impl fmt::Display for BufferParametersChanged {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WLR buffer parameters changed")
    }
}

impl std::error::Error for BufferParametersChanged {}

pub(super) fn is_buffer_parameters_changed(err: &anyhow::Error) -> bool {
    err.downcast_ref::<BufferParametersChanged>().is_some()
}

fn reset_wlr_frame_state(state: &mut AppState) {
    state.active_wlr_frame_id = None;
    state.frame_ready = false;
    state.frame_failed = false;
    state.damage_regions.clear();
    state.buffer_width = 0;
    state.buffer_height = 0;
    state.wlr_stride = 0;
    state.shm_format = None;
}

fn wlr_frame_wait_stalled(wait_started: Instant, now: Instant) -> bool {
    now.duration_since(wait_started) >= WLR_FRAME_STALL_TIMEOUT
}

fn promote_empty_damage_to_full_scan_if_due(
    damage_regions: &mut Vec<(i32, i32, i32, i32)>,
    sent_first_frame: bool,
    last_empty_damage_full_scan: &mut Instant,
    now: Instant,
    width: u32,
    height: u32,
) -> bool {
    if damage_regions.is_empty()
        && sent_first_frame
        && now.duration_since(*last_empty_damage_full_scan) >= WLR_EMPTY_DAMAGE_FULL_SCAN_INTERVAL
    {
        damage_regions.push((0, 0, width as i32, height as i32));
        *last_empty_damage_full_scan = now;
        true
    } else {
        false
    }
}

fn start_wlr_frame(
    conn: &Connection,
    state: &mut AppState,
    qh: &QueueHandle<AppState>,
    output: &wl_output::WlOutput,
    screencopy_mgr: &zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
) -> Result<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1> {
    reset_wlr_frame_state(state);
    let frame = screencopy_mgr.capture_output(0, output, qh, ());
    state.active_wlr_frame_id = Some(frame.id().protocol_id());
    conn.flush().context("Wayland flush failed")?;
    Ok(frame)
}

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
    pending_initial_resize: Option<DesktopSize>,
) -> Result<()> {
    // First capture to get buffer dimensions
    let probe = screencopy_mgr.capture_output(0, output, qh, ());
    reset_wlr_frame_state(state);
    state.active_wlr_frame_id = Some(probe.id().protocol_id());
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
    state.active_wlr_frame_id = None;

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
    );
    proc.set_pending_initial_resize(pending_initial_resize);
    let mut frame_pacer = FramePacer::new(fps, Instant::now());
    let mut last_empty_damage_full_scan = Instant::now() - WLR_EMPTY_DAMAGE_FULL_SCAN_INTERVAL;

    let buffers = [&buffer_0, &buffer_1];
    let mmaps = [&mmap_0, &mmap_1];
    let mut cap_idx: usize = 0;

    // Start initial capture into buffer 0
    let mut frame = start_wlr_frame(conn, state, qh, output, screencopy_mgr)?;
    let mut wait_started = Instant::now();
    let mut buffer_sent = false;
    while !state.frame_ready && !state.frame_failed {
        poll_dispatch(event_queue, state, POLL_TIMEOUT_MS)?;
        if !buffer_sent && state.buffer_width > 0 {
            frame.copy_with_damage(buffers[cap_idx]);
            conn.flush().context("Wayland flush failed")?;
            buffer_sent = true;
            wait_started = Instant::now();
        }
        if state.should_stop() {
            break;
        }
        if wlr_frame_wait_stalled(wait_started, Instant::now()) {
            tracing::warn!(
                buffer_sent,
                timeout_ms = WLR_FRAME_STALL_TIMEOUT.as_millis(),
                "WLR initial frame stalled; restarting screencopy frame"
            );
            frame.destroy();
            frame = start_wlr_frame(conn, state, qh, output, screencopy_mgr)?;
            buffer_sent = false;
            wait_started = Instant::now();
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
        frame = start_wlr_frame(conn, state, qh, output, screencopy_mgr)?;
        buffer_sent = false;
        wait_started = Instant::now();

        // Process the completed frame while waiting for next buffer info + capture
        if !completed_failed {
            let mut damage_regions = completed_damage_regions;
            let promoted_full_scan = promote_empty_damage_to_full_scan_if_due(
                &mut damage_regions,
                proc.sent_first_frame,
                &mut last_empty_damage_full_scan,
                Instant::now(),
                width,
                height,
            );
            if promoted_full_scan {
                tracing::trace!(
                    interval_ms = WLR_EMPTY_DAMAGE_FULL_SCAN_INTERVAL.as_millis(),
                    "WLR returned empty damage; scheduling full-frame diff scan"
                );
            }

            proc.queue_damage(&damage_regions);
            let data = mmaps[completed_idx].as_slice();
            proc.stats
                .record_capture(width, height, damage_regions.len(), promoted_full_scan);
            let has_pending_damage = proc.has_pending_damage();
            if !has_pending_damage && proc.sent_first_frame {
                proc.stats.record_no_damage_skip(width, height);
            } else if frame_pacer.should_send(
                Instant::now(),
                proc.sent_first_frame,
                has_pending_damage,
                proc.pacing_fps(),
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
                    return Err(BufferParametersChanged.into());
                }
                frame.copy_with_damage(buffers[cap_idx]);
                conn.flush().context("Wayland flush failed")?;
                buffer_sent = true;
                wait_started = Instant::now();
            }
            if state.should_stop() {
                break;
            }
            if wlr_frame_wait_stalled(wait_started, Instant::now()) {
                tracing::warn!(
                    buffer_sent,
                    timeout_ms = WLR_FRAME_STALL_TIMEOUT.as_millis(),
                    "WLR frame stalled; restarting screencopy frame"
                );
                frame.destroy();
                frame = start_wlr_frame(conn, state, qh, output, screencopy_mgr)?;
                buffer_sent = false;
                wait_started = Instant::now();
            }
        }
    }

    state.active_wlr_frame_id = None;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        promote_empty_damage_to_full_scan_if_due, reset_wlr_frame_state, wlr_frame_wait_stalled,
        WLR_EMPTY_DAMAGE_FULL_SCAN_INTERVAL, WLR_FRAME_STALL_TIMEOUT,
    };
    use crate::capture::wayland::state::AppState;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[test]
    fn wlr_frame_wait_stall_threshold_is_mechanical() {
        let started = Instant::now();

        assert!(!wlr_frame_wait_stalled(
            started,
            started + WLR_FRAME_STALL_TIMEOUT - Duration::from_millis(1)
        ));
        assert!(wlr_frame_wait_stalled(
            started,
            started + WLR_FRAME_STALL_TIMEOUT
        ));
    }

    #[test]
    fn wlr_empty_damage_promotes_to_full_frame_diff_after_interval() {
        let mut damage = Vec::new();
        let started = Instant::now();
        let mut last_scan = started;

        assert!(!promote_empty_damage_to_full_scan_if_due(
            &mut damage,
            true,
            &mut last_scan,
            started + WLR_EMPTY_DAMAGE_FULL_SCAN_INTERVAL - Duration::from_millis(1),
            1920,
            1080,
        ));
        assert!(damage.is_empty());

        assert!(promote_empty_damage_to_full_scan_if_due(
            &mut damage,
            true,
            &mut last_scan,
            started + WLR_EMPTY_DAMAGE_FULL_SCAN_INTERVAL,
            1920,
            1080,
        ));
        assert_eq!(damage, vec![(0, 0, 1920, 1080)]);
        assert_eq!(last_scan, started + WLR_EMPTY_DAMAGE_FULL_SCAN_INTERVAL);
    }

    #[test]
    fn wlr_empty_damage_is_not_promoted_before_first_frame() {
        let mut damage = Vec::new();
        let started = Instant::now();
        let mut last_scan = started;

        assert!(!promote_empty_damage_to_full_scan_if_due(
            &mut damage,
            false,
            &mut last_scan,
            started + WLR_EMPTY_DAMAGE_FULL_SCAN_INTERVAL,
            1920,
            1080,
        ));
        assert!(damage.is_empty());
        assert_eq!(last_scan, started);
    }

    #[test]
    fn wlr_frame_state_reset_clears_active_frame_and_pending_events() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let mut state = AppState::new(tx, "DP-1".to_string(), Arc::new(AtomicBool::new(false)));
        state.active_wlr_frame_id = Some(42);
        state.frame_ready = true;
        state.frame_failed = true;
        state.damage_regions.push((1, 2, 3, 4));
        state.buffer_width = 640;
        state.buffer_height = 480;
        state.wlr_stride = 2560;
        state.shm_format = Some(wayland_client::protocol::wl_shm::Format::Xrgb8888);

        reset_wlr_frame_state(&mut state);

        assert_eq!(state.active_wlr_frame_id, None);
        assert!(!state.frame_ready);
        assert!(!state.frame_failed);
        assert!(state.damage_regions.is_empty());
        assert_eq!(state.buffer_width, 0);
        assert_eq!(state.buffer_height, 0);
        assert_eq!(state.wlr_stride, 0);
        assert_eq!(state.shm_format, None);
    }
}
