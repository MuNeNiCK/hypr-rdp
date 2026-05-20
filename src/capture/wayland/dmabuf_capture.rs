use std::sync::Arc;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use ironrdp_server::{DesktopSize, DisplayUpdate};
use wayland_client::protocol::wl_buffer;
use wayland_client::{Connection, QueueHandle};
use wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_session_v1;
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1,
};

use super::state::AppState;
use super::{poll_dispatch, POLL_TIMEOUT_MS};
use crate::capture::frame::FramePacer;
use crate::egfx::{EgfxShared, H264RateControl};

const MAX_ENCODE_FAILURES: u32 = 5;

/// Context for DMA-BUF capture (created during setup, passed to capture loop).
#[cfg(feature = "vaapi")]
pub(super) struct DmaBufCaptureContext {
    // Fields drop top-to-bottom; protocol/VA objects must release before GBM owners.
    wl_buffers: Vec<wl_buffer::WlBuffer>,
    vpp: crate::egfx::VppConverter,
    nv12_info: crate::egfx::VppDmaBufInfo,
    drm_device_path: std::path::PathBuf,
    #[allow(dead_code)]
    dmabuf_infos: Vec<crate::capture::dmabuf::DmaBufInfo>,
    #[allow(dead_code)]
    gbm_bos: Vec<crate::capture::dmabuf::GbmBo>,
    #[allow(dead_code)]
    gbm_device: crate::capture::dmabuf::GbmDevice,
}

/// Try to set up DMA-BUF capture. Returns None if compositor doesn't support DMA-BUF,
/// or Some(Err) if setup fails.
#[cfg(feature = "vaapi")]
pub(super) fn try_setup_dmabuf(
    state: &AppState,
    qh: &QueueHandle<AppState>,
    width: u32,
    height: u32,
) -> Option<Result<DmaBufCaptureContext>> {
    use crate::capture::dmabuf::{DRM_FORMAT_ARGB8888, DRM_FORMAT_XRGB8888};

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
                tracing::trace!("No XRGB8888/ARGB8888 format in DMA-BUF formats");
                return None;
            }
        }
    };

    tracing::trace!(
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
    use crate::capture::dmabuf::{
        drm_device_from_devt, open_drm_device, GbmBo, GbmDevice, DRM_FORMAT_MOD_INVALID,
    };

    // Find DRM device path from dev_t
    let drm_device_path =
        drm_device_from_devt(dev).context("failed to find DRM device from dev_t")?;
    tracing::trace!(device = %drm_device_path.display(), "DMA-BUF: found DRM device");

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

        let info = bo
            .dmabuf_info(format, width, height)
            .with_context(|| format!("failed to get DMA-BUF info for buffer {}", i))?;

        // Create wl_buffer via linux-dmabuf
        let params = linux_dmabuf.create_params(qh, ());
        let fd = bo
            .fd()
            .with_context(|| format!("failed to get fd for buffer {}", i))?;
        let stride = bo.stride();
        let offset = bo.offset(0);
        let modifier = bo.modifier();
        let modifier_hi = (modifier >> 32) as u32;
        let modifier_lo = (modifier & 0xFFFFFFFF) as u32;

        // Add the plane to params.
        // SAFETY: `context.gbm_bos` owns the GBM BOs for the lifetime of these buffers.
        params.add(
            unsafe { std::os::unix::io::BorrowedFd::borrow_raw(fd) },
            0,
            offset,
            stride,
            modifier_hi,
            modifier_lo,
        );

        let wl_buf = params.create_immed(
            width as i32,
            height as i32,
            format,
            zwp_linux_buffer_params_v1::Flags::empty(),
            qh,
            (),
        );

        tracing::trace!(
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
    let mut vpp = crate::egfx::VppConverter::new(&drm_device_path, width, height)?;

    // Import the two XRGB DMA-BUFs as VPP input surfaces
    for (i, info) in dmabuf_infos.iter().enumerate() {
        vpp.import_input_surface(info.fd, width, height, info.stride, info.modifier, format)
            .with_context(|| format!("failed to import VPP input surface {}", i))?;
    }

    // Export the NV12 output surface as a DMA-BUF
    let nv12_info = vpp.export_nv12_output()?;
    tracing::trace!(
        nv12_fd = nv12_info.fd,
        nv12_stride = nv12_info.stride,
        "DMA-BUF: VPP NV12 output exported"
    );

    Ok(DmaBufCaptureContext {
        wl_buffers,
        vpp,
        nv12_info,
        drm_device_path,
        dmabuf_infos,
        gbm_bos,
        gbm_device,
    })
}

/// DMA-BUF capture loop for ext-image-copy-capture.
#[cfg(feature = "vaapi")]
#[allow(clippy::too_many_arguments)]
pub(super) fn capture_loop_ext_dmabuf(
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
    rate_control: H264RateControl,
    fps: u32,
    mut pending_initial_resize: Option<DesktopSize>,
) -> Result<()> {
    tracing::info!(
        width,
        height,
        device = %dmabuf_ctx.drm_device_path.display(),
        mode = "ext-dmabuf",
        fps,
        "Starting capture loop (zero-copy DMA-BUF)"
    );

    let mut frame_pacer = FramePacer::new(fps, Instant::now());
    let mut cap_idx: usize = 0;
    let mut sent_first_frame = false;

    // EGFX state (mirrors FrameProcessor but for DMA-BUF path)
    let mut h264_encoder: Option<crate::egfx::FrameEncoder> = None;
    let mut egfx_handle: Option<ironrdp_server::GfxServerHandle> = None;
    let mut egfx_sender: Option<tokio::sync::mpsc::UnboundedSender<ironrdp_server::ServerEvent>> =
        None;
    let mut egfx_surface_id: Option<u16> = None;
    let mut egfx_active = false;
    let mut egfx_ready = false;
    let mut egfx_generation: u32 = 0;
    let mut encode_failures: u32 = 0;
    let metadata_qp = match rate_control {
        H264RateControl::Vbr => 0,
        H264RateControl::Cqp => quality.min(51),
    };

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

        if completed_failed {
            continue;
        }

        // Rate limit
        if frame_pacer.should_send(Instant::now(), sent_first_frame, has_damage) {
            // Process via DMA-BUF zero-copy pipeline
            // Update EGFX state
            if let Some(shared) = &egfx_shared {
                if let Some(size) = pending_initial_resize {
                    if shared.is_ready() {
                        tracing::info!(
                            width = size.width,
                            height = size.height,
                            "Sending initial resize after graphics channel is ready"
                        );
                        pending_initial_resize = None;
                        let _ = state.tx.blocking_send(DisplayUpdate::Resize(size));
                        break;
                    }
                }

                let ready = shared.is_ready() && shared.is_avc_enabled();
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
                        match crate::egfx::FrameEncoder::new(
                            width,
                            height,
                            bitrate,
                            fps,
                            quality,
                            rate_control,
                        ) {
                            Ok(enc) => {
                                tracing::info!(
                                    width,
                                    height,
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
                        tracing::trace!("EGFX transport ready (DMA-BUF path)");
                    }
                }

                if egfx_active {
                    // Surface initialization (separate borrow scope)
                    if egfx_surface_id.is_none() {
                        if let (Some(handle), Some(sender)) = (&egfx_handle, &egfx_sender) {
                            egfx_surface_id = EgfxShared::init_surface(
                                handle,
                                sender,
                                width as u16,
                                height as u16,
                            );
                        }
                    }

                    if let Some(sid) = egfx_surface_id {
                        if let Some(handle) = &egfx_handle {
                            if !shared.can_send_frame(handle) {
                                tracing::trace!("EGFX frame skipped before DMA-BUF encode");
                                continue;
                            }
                        }

                        // Zero-copy encode pipeline:
                        // 1. VPP: XRGB DMA-BUF -> NV12 (GPU)
                        // 2. Encoder: NV12 DMA-BUF -> H.264 (GPU)
                        let vpp_result = dmabuf_ctx.vpp.convert(completed_idx);
                        let encode_result = match vpp_result {
                            Ok(()) => {
                                let nv12 = &dmabuf_ctx.nv12_info;
                                h264_encoder.as_mut().map(|enc| {
                                    enc.encode_dmabuf(
                                        nv12.fd,
                                        nv12.width,
                                        nv12.height,
                                        nv12.stride,
                                        nv12.offset,
                                        nv12.modifier,
                                        nv12.uv_stride,
                                        nv12.uv_offset,
                                    )
                                })
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
                                        .as_millis()
                                        as u32;
                                    let sent = shared.send_tracked_avc420_frame(
                                        handle,
                                        sender,
                                        sid,
                                        width as u16,
                                        height as u16,
                                        h264_data,
                                        timestamp,
                                        metadata_qp,
                                    );
                                    if sent {
                                        sent_first_frame = true;
                                    } else if let Some(enc) = &mut h264_encoder {
                                        enc.force_idr();
                                    }
                                }
                            }
                            Some(Ok(_)) => {
                                encode_failures = 0;
                            }
                            Some(Err(e)) => {
                                encode_failures += 1;
                                tracing::warn!(
                                    failures = encode_failures,
                                    max = MAX_ENCODE_FAILURES,
                                    "DMA-BUF encode pipeline failed: {:#}",
                                    e
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
