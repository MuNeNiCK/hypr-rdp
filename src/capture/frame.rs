use std::num::{NonZeroU16, NonZeroUsize};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use ironrdp_server::{BitmapUpdate, DesktopSize, DisplayUpdate, PixelFormat};
use tokio::sync::mpsc;

use crate::egfx::{encoder::Avc444FrameEncoding, EgfxFrameReadiness, EgfxShared, H264RateControl};

use super::damage::{
    clamp_damage_region, damage_area_pixels, merge_damage_region, FrameDiffDamageDetector,
};

/// Maximum consecutive encode failures before falling back to software encoder.
const MAX_ENCODE_FAILURES: u32 = 5;

pub(super) fn avc444_dimensions_supported(width: u32, height: u32) -> bool {
    width != 0 && height != 0 && width.is_multiple_of(4) && height.is_multiple_of(2)
}

/// Common frame processing: EGFX H.264/RFX encoding or bitmap fallback.
pub(super) struct FrameProcessor {
    egfx_shared: Option<Arc<EgfxShared>>,
    pub(super) h264_encoder: Option<crate::egfx::FrameEncoder>,
    egfx_handle: Option<ironrdp_server::GfxServerHandle>,
    pub(super) egfx_sender: Option<tokio::sync::mpsc::UnboundedSender<ironrdp_server::ServerEvent>>,
    pub(super) egfx_surface_id: Option<u16>,
    egfx_active: bool,
    egfx_ready: bool,
    pub(super) egfx_generation: u32,
    pub(super) egfx_codec: Option<EgfxCodec>,
    width: u32,
    height: u32,
    pixel_format: PixelFormat,
    stride: u32,
    bitrate: u32,
    quality: u8,
    rate_control: H264RateControl,
    fps: u32,
    /// Whether we've sent at least one frame (first frame always sent)
    pub(super) sent_first_frame: bool,
    /// Consecutive encode failure count for runtime VAAPI -> software fallback.
    encode_failures: u32,
    pub(super) pending_damage_regions: Vec<(i32, i32, i32, i32)>,
    damage_detector: FrameDiffDamageDetector,
    pub(super) stats: FrameStats,
    pending_initial_resize: Option<DesktopSize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum EgfxCodec {
    Avc420,
    Avc444,
}

enum EncodedEgfxFrame {
    Avc420(Vec<u8>),
    Avc444(crate::egfx::encoder::Avc444EncodedFrame),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EncodedFrameState {
    Sendable,
    Skipped,
    Invalid,
}

impl EncodedEgfxFrame {
    fn len(&self) -> usize {
        match self {
            Self::Avc420(data) => data.len(),
            Self::Avc444(frame) => frame.stream1.len() + frame.stream2.len(),
        }
    }

    fn state(&self) -> EncodedFrameState {
        match self {
            Self::Avc420(data) if data.is_empty() => EncodedFrameState::Skipped,
            Self::Avc420(data) if data.len() > 32 => EncodedFrameState::Sendable,
            Self::Avc420(_) => EncodedFrameState::Invalid,
            Self::Avc444(frame) => {
                let stream1_has_regions = !frame.stream1_regions.is_empty();
                let stream2_has_regions = !frame.stream2_regions.is_empty();
                let stream2_has_data = !frame.stream2.is_empty();

                if frame.stream1.is_empty()
                    && frame.stream2.is_empty()
                    && !stream1_has_regions
                    && !stream2_has_regions
                {
                    return EncodedFrameState::Skipped;
                }

                match frame.encoding {
                    Avc444FrameEncoding::Luma | Avc444FrameEncoding::Chroma
                        if stream1_has_regions && !stream2_has_regions && !stream2_has_data =>
                    {
                        EncodedFrameState::Sendable
                    }
                    Avc444FrameEncoding::LumaAndChroma
                        if stream1_has_regions && stream2_has_regions =>
                    {
                        EncodedFrameState::Sendable
                    }
                    _ => EncodedFrameState::Invalid,
                }
            }
        }
    }
}

pub(super) struct FrameStats {
    window_start: Instant,
    captured_frames: u32,
    sent_frames: u32,
    skipped_no_damage: u32,
    skipped_pacer: u32,
    skipped_encoder: u32,
    skipped_backpressure: u32,
    skipped_local_backpressure: u32,
    skipped_transport_unavailable: u32,
    skipped_transport_not_ready: u32,
    skipped_transport_no_channel: u32,
    skipped_transport_backpressure: u32,
    bytes: u64,
    encode_us_total: u128,
    send_us_total: u128,
    damage_pixels: u64,
}

fn egfx_perf_logging_enabled() -> bool {
    egfx_perf_logging_enabled_with(|name| std::env::var_os(name).is_some())
}

pub(super) fn egfx_perf_logging_enabled_with(mut is_set: impl FnMut(&str) -> bool) -> bool {
    is_set("HYPR_RDP_EGFX_PERF")
}

impl FrameStats {
    fn new() -> Self {
        Self {
            window_start: Instant::now(),
            captured_frames: 0,
            sent_frames: 0,
            skipped_no_damage: 0,
            skipped_pacer: 0,
            skipped_encoder: 0,
            skipped_backpressure: 0,
            skipped_local_backpressure: 0,
            skipped_transport_unavailable: 0,
            skipped_transport_not_ready: 0,
            skipped_transport_no_channel: 0,
            skipped_transport_backpressure: 0,
            bytes: 0,
            encode_us_total: 0,
            send_us_total: 0,
            damage_pixels: 0,
        }
    }

    pub(super) fn record_capture(&mut self, width: u32, height: u32) {
        self.captured_frames = self.captured_frames.saturating_add(1);
        self.maybe_log(width, height);
    }

    pub(super) fn record_no_damage_skip(&mut self, width: u32, height: u32) {
        self.skipped_no_damage = self.skipped_no_damage.saturating_add(1);
        self.maybe_log(width, height);
    }

    pub(super) fn record_pacer_skip(&mut self, width: u32, height: u32) {
        self.skipped_pacer = self.skipped_pacer.saturating_add(1);
        self.maybe_log(width, height);
    }

    pub(super) fn record_encoder_skip(&mut self, width: u32, height: u32) {
        self.skipped_encoder = self.skipped_encoder.saturating_add(1);
        self.maybe_log(width, height);
    }

    fn record_sent(
        &mut self,
        width: u32,
        height: u32,
        damage_regions: &[(i32, i32, i32, i32)],
        bytes: usize,
        encode_elapsed: Duration,
        send_elapsed: Duration,
    ) {
        self.sent_frames = self.sent_frames.saturating_add(1);
        self.bytes = self.bytes.saturating_add(bytes as u64);
        self.encode_us_total = self
            .encode_us_total
            .saturating_add(encode_elapsed.as_micros());
        self.send_us_total = self.send_us_total.saturating_add(send_elapsed.as_micros());
        self.damage_pixels =
            self.damage_pixels
                .saturating_add(damage_area_pixels(damage_regions, width, height));
        self.maybe_log(width, height);
    }

    pub(super) fn record_send_unavailable(
        &mut self,
        readiness: EgfxFrameReadiness,
        width: u32,
        height: u32,
    ) {
        self.skipped_backpressure = self.skipped_backpressure.saturating_add(1);
        match readiness {
            EgfxFrameReadiness::Ready => {}
            EgfxFrameReadiness::LocalBackpressure { .. } => {
                self.skipped_local_backpressure = self.skipped_local_backpressure.saturating_add(1);
            }
            EgfxFrameReadiness::TransportUnavailable => {
                self.skipped_transport_unavailable =
                    self.skipped_transport_unavailable.saturating_add(1);
            }
            EgfxFrameReadiness::TransportNotReady => {
                self.skipped_transport_not_ready =
                    self.skipped_transport_not_ready.saturating_add(1);
            }
            EgfxFrameReadiness::TransportNoChannel => {
                self.skipped_transport_no_channel =
                    self.skipped_transport_no_channel.saturating_add(1);
            }
            EgfxFrameReadiness::TransportBackpressure { .. } => {
                self.skipped_transport_backpressure =
                    self.skipped_transport_backpressure.saturating_add(1);
            }
        }
        self.maybe_log(width, height);
    }

    fn maybe_log(&mut self, width: u32, height: u32) {
        let elapsed = self.window_start.elapsed();
        if elapsed < Duration::from_secs(1) {
            return;
        }

        let seconds = elapsed.as_secs_f64();
        let frames = self.sent_frames.max(1);
        let frame_pixels = u64::from(width) * u64::from(height);
        let avg_damage_pct = if frame_pixels == 0 || self.sent_frames == 0 {
            0.0
        } else {
            (self.damage_pixels as f64 * 100.0) / (frame_pixels as f64 * self.sent_frames as f64)
        };

        if egfx_perf_logging_enabled() {
            tracing::info!(
                target: "hypr_rdp::egfx_perf",
                captured_fps = self.captured_frames as f64 / seconds,
                fps = self.sent_frames as f64 / seconds,
                mbps = (self.bytes as f64 * 8.0) / seconds / 1_000_000.0,
                avg_encode_ms = self.encode_us_total as f64 / f64::from(frames) / 1000.0,
                avg_send_ms = self.send_us_total as f64 / f64::from(frames) / 1000.0,
                avg_damage_pct,
                skipped_no_damage = self.skipped_no_damage,
                skipped_pacer = self.skipped_pacer,
                skipped_encoder = self.skipped_encoder,
                skipped_backpressure = self.skipped_backpressure,
                skipped_local_backpressure = self.skipped_local_backpressure,
                skipped_transport_unavailable = self.skipped_transport_unavailable,
                skipped_transport_not_ready = self.skipped_transport_not_ready,
                skipped_transport_no_channel = self.skipped_transport_no_channel,
                skipped_transport_backpressure = self.skipped_transport_backpressure,
                "EGFX frame stats"
            );
        }

        *self = Self::new();
    }
}

/// Capture frame pacer using an absolute deadline while tolerating compositor
/// frame-time quantization.
pub(super) struct FramePacer {
    frame_interval: Duration,
    next_send_at: Option<Instant>,
}

impl FramePacer {
    const SEND_EARLY_FRACTION: f64 = 0.10;

    pub(super) fn new(target_fps: u32, now: Instant) -> Self {
        let frame_interval = Duration::from_secs_f64(1.0 / f64::from(target_fps.max(1)));
        Self {
            frame_interval,
            next_send_at: Some(now),
        }
    }

    pub(super) fn should_send(
        &mut self,
        now: Instant,
        sent_first_frame: bool,
        has_damage: bool,
    ) -> bool {
        if !sent_first_frame {
            self.next_send_at = Some(now + self.frame_interval);
            return true;
        }

        if !has_damage {
            return false;
        }

        let next_send_at = self.next_send_at.unwrap_or(now);
        let send_early = self.frame_interval.mul_f64(Self::SEND_EARLY_FRACTION);
        if now + send_early < next_send_at {
            return false;
        }

        let mut next = next_send_at + self.frame_interval;
        while next <= now {
            next += self.frame_interval;
        }
        self.next_send_at = Some(next);
        true
    }
}

impl FrameProcessor {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        egfx_shared: Option<Arc<EgfxShared>>,
        width: u32,
        height: u32,
        pixel_format: PixelFormat,
        stride: u32,
        bitrate: u32,
        quality: u8,
        rate_control: H264RateControl,
        fps: u32,
    ) -> Self {
        Self {
            egfx_shared,
            h264_encoder: None,
            egfx_handle: None,
            egfx_sender: None,
            egfx_surface_id: None,
            egfx_active: false,
            egfx_ready: false,
            egfx_generation: 0,
            egfx_codec: None,
            width,
            height,
            pixel_format,
            stride,
            bitrate,
            quality,
            rate_control,
            fps,
            sent_first_frame: false,
            encode_failures: 0,
            pending_damage_regions: Vec::new(),
            damage_detector: FrameDiffDamageDetector::new(),
            stats: FrameStats::new(),
            pending_initial_resize: None,
        }
    }

    pub(super) fn set_pending_initial_resize(&mut self, resize: Option<DesktopSize>) {
        self.pending_initial_resize = resize;
    }

    pub(super) fn has_pending_damage(&self) -> bool {
        !self.pending_damage_regions.is_empty()
    }

    fn metadata_qp(&self) -> u8 {
        match self.rate_control {
            H264RateControl::Vbr => 0,
            H264RateControl::Cqp => self.quality.min(51),
        }
    }

    fn handle_encoder_skip(
        encode_failures: &mut u32,
        h264_encoder: &mut Option<crate::egfx::FrameEncoder>,
        stats: &mut FrameStats,
        width: u32,
        height: u32,
    ) {
        *encode_failures = 0;
        if let Some(enc) = h264_encoder {
            enc.force_idr();
        }
        stats.record_encoder_skip(width, height);
        tracing::trace!("H.264 encoder skipped frame; forcing next frame to IDR");
    }

    pub(super) fn queue_damage(&mut self, damage_regions: &[(i32, i32, i32, i32)]) {
        for &(x, y, w, h) in damage_regions {
            let Some(region) = clamp_damage_region(x, y, w, h, self.width, self.height) else {
                continue;
            };
            merge_damage_region(&mut self.pending_damage_regions, region);
        }
    }

    /// Process a captured frame. Returns true if the capture loop should continue.
    pub(super) fn process(&mut self, data: &[u8], tx: &mpsc::Sender<DisplayUpdate>) -> bool {
        let force_egfx_full_frame = self
            .egfx_shared
            .as_ref()
            .is_some_and(|shared| shared.full_frame_requested());

        // Skip frames with no damage (except the very first frame)
        if self.sent_first_frame && !self.has_pending_damage() && !force_egfx_full_frame {
            return true;
        }

        if let Some(size) = self.pending_initial_resize {
            let graphics_ready = self
                .egfx_shared
                .as_ref()
                .is_none_or(|shared| shared.is_ready());
            if graphics_ready {
                tracing::info!(
                    width = size.width,
                    height = size.height,
                    "Sending initial resize after graphics channel is ready"
                );
                self.pending_initial_resize = None;
                if tx.blocking_send(DisplayUpdate::Resize(size)).is_err() {
                    tracing::info!("Display update channel closed");
                }
                return false;
            }
        }

        let mut sent_via_egfx = false;
        if let Some(shared) = &self.egfx_shared {
            let egfx_ready = shared.is_ready();
            let avc_enabled = shared.is_avc_enabled();
            let ready = egfx_ready && avc_enabled;
            let codec = if shared.is_avc444_enabled()
                && avc444_dimensions_supported(self.width, self.height)
            {
                Some(EgfxCodec::Avc444)
            } else if avc_enabled {
                Some(EgfxCodec::Avc420)
            } else {
                None
            };
            let gen = shared.generation();

            if ready != self.egfx_ready {
                self.egfx_ready = ready;
                if !ready {
                    self.egfx_active = false;
                    self.egfx_handle = None;
                    self.egfx_sender = None;
                    self.egfx_surface_id = None;
                    self.h264_encoder = None;
                    self.egfx_codec = None;
                    self.sent_first_frame = false;
                    self.damage_detector.invalidate();
                    if !egfx_ready {
                        tracing::trace!("EGFX channel became unavailable");
                    }
                }
            }

            if gen != self.egfx_generation || (ready && codec != self.egfx_codec) {
                self.egfx_generation = gen;
                self.egfx_surface_id = None;
                self.h264_encoder = None;
                self.egfx_codec = None;
                self.sent_first_frame = false;
                self.damage_detector.invalidate();
                if ready {
                    let selected_codec = codec.unwrap_or(EgfxCodec::Avc420);
                    let encoder_result = match selected_codec {
                        EgfxCodec::Avc444 => crate::egfx::FrameEncoder::new_avc444_software_only(
                            self.width,
                            self.height,
                            self.bitrate,
                            self.fps,
                            self.quality,
                            self.rate_control,
                        ),
                        EgfxCodec::Avc420 => crate::egfx::FrameEncoder::new(
                            self.width,
                            self.height,
                            self.bitrate,
                            self.fps,
                            self.quality,
                            self.rate_control,
                        ),
                    };

                    match encoder_result {
                        Ok(enc) => {
                            tracing::info!(
                                width = self.width,
                                height = self.height,
                                backend = enc.backend_name(),
                                codec = ?selected_codec,
                                gen,
                                bitrate = self.bitrate,
                                "H.264 encoder initialized"
                            );
                            self.egfx_codec = Some(selected_codec);
                            self.h264_encoder = Some(enc);
                        }
                        Err(e) => tracing::warn!("Failed to initialize H.264 encoder: {:#}", e),
                    }
                }
            }

            let force_full_frame = if ready {
                shared.take_full_frame_request()
            } else {
                false
            };
            let frame_damage_regions = if force_full_frame {
                vec![(0, 0, self.width as i32, self.height as i32)]
            } else if self.sent_first_frame {
                self.damage_detector.detect(
                    data,
                    self.width,
                    self.height,
                    self.stride as usize,
                    &self.pending_damage_regions,
                )
            } else {
                self.damage_detector.detect(
                    data,
                    self.width,
                    self.height,
                    self.stride as usize,
                    &[(0, 0, self.width as i32, self.height as i32)],
                )
            };
            if self.sent_first_frame && frame_damage_regions.is_empty() {
                self.pending_damage_regions.clear();
                self.stats.record_no_damage_skip(self.width, self.height);
                return true;
            }

            if ready && !self.egfx_active {
                self.egfx_handle = shared.get_handle();
                self.egfx_sender = shared.get_event_sender();
                if self.h264_encoder.is_some()
                    && self.egfx_handle.is_some()
                    && self.egfx_sender.is_some()
                {
                    self.egfx_active = true;
                    tracing::trace!("EGFX transport ready, switching to H.264 encoding");
                }
            }

            if self.egfx_active {
                // Surface initialization (separate borrow scope)
                if self.egfx_surface_id.is_none() {
                    if let (Some(handle), Some(sender)) = (&self.egfx_handle, &self.egfx_sender) {
                        if let Some(sid) = shared.init_or_reuse_surface(
                            handle,
                            sender,
                            self.width as u16,
                            self.height as u16,
                        ) {
                            self.egfx_surface_id = Some(sid);
                        }
                    }
                }

                // Encode and send (encoder borrow released before fallback check)
                if let Some(sid) = self.egfx_surface_id {
                    if let Some(handle) = &self.egfx_handle {
                        let readiness = shared.frame_readiness(handle);
                        if !readiness.is_ready() {
                            tracing::trace!(
                                ?readiness,
                                reason = readiness.reason(),
                                "EGFX frame skipped before encode"
                            );
                            self.stats
                                .record_send_unavailable(readiness, self.width, self.height);
                            return true;
                        }
                    }

                    let encode_start = Instant::now();
                    let codec = self.egfx_codec.unwrap_or(EgfxCodec::Avc420);
                    if force_full_frame {
                        if let Some(enc) = &mut self.h264_encoder {
                            enc.force_idr();
                        }
                    }
                    let encode_result = self.h264_encoder.as_mut().map(|enc| match codec {
                        EgfxCodec::Avc420 => enc
                            .encode(data, self.stride as usize)
                            .map(EncodedEgfxFrame::Avc420),
                        EgfxCodec::Avc444 => enc
                            .encode_avc444(data, self.stride as usize, &frame_damage_regions)
                            .map(EncodedEgfxFrame::Avc444),
                    });
                    let encode_elapsed = encode_start.elapsed();
                    match encode_result {
                        Some(Ok(ref encoded)) if encoded.state() == EncodedFrameState::Sendable => {
                            self.encode_failures = 0;
                            if let (Some(handle), Some(sender)) =
                                (&self.egfx_handle, &self.egfx_sender)
                            {
                                let timestamp = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_millis()
                                    as u32;
                                let send_start = Instant::now();
                                sent_via_egfx = match encoded {
                                    EncodedEgfxFrame::Avc420(h264_data) => shared
                                        .send_tracked_avc420_frame_with_damage(
                                            handle,
                                            sender,
                                            sid,
                                            self.width as u16,
                                            self.height as u16,
                                            h264_data,
                                            &frame_damage_regions,
                                            timestamp,
                                            self.metadata_qp(),
                                        ),
                                    EncodedEgfxFrame::Avc444(frame) => shared
                                        .send_tracked_avc444_frame_with_damage(
                                            handle,
                                            sender,
                                            sid,
                                            frame.encoding,
                                            &frame.stream1,
                                            &frame.stream1_regions,
                                            (!frame.stream2_regions.is_empty())
                                                .then_some(&frame.stream2[..]),
                                            (!frame.stream2_regions.is_empty())
                                                .then_some(&frame.stream2_regions[..]),
                                            timestamp,
                                            self.width as u16,
                                            self.height as u16,
                                            self.metadata_qp(),
                                        ),
                                };
                                let send_elapsed = send_start.elapsed();
                                if !sent_via_egfx {
                                    if let Some(enc) = &mut self.h264_encoder {
                                        enc.force_idr();
                                    }
                                } else {
                                    if matches!(encoded, EncodedEgfxFrame::Avc444(_)) {
                                        if let Some(enc) = &mut self.h264_encoder {
                                            enc.commit_avc444_reference();
                                        }
                                    }
                                    self.damage_detector.update_reference_regions(
                                        data,
                                        self.width,
                                        self.height,
                                        self.stride as usize,
                                        &frame_damage_regions,
                                    );
                                    self.stats.record_sent(
                                        self.width,
                                        self.height,
                                        &frame_damage_regions,
                                        encoded.len(),
                                        encode_elapsed,
                                        send_elapsed,
                                    );
                                }
                            }
                        }
                        Some(Ok(ref encoded)) if encoded.state() == EncodedFrameState::Skipped => {
                            Self::handle_encoder_skip(
                                &mut self.encode_failures,
                                &mut self.h264_encoder,
                                &mut self.stats,
                                self.width,
                                self.height,
                            );
                        }
                        Some(Ok(_)) => {
                            self.encode_failures += 1;
                            tracing::trace!(
                                failures = self.encode_failures,
                                max = MAX_ENCODE_FAILURES,
                                "H.264 encode produced no usable output"
                            );
                            if let Some(enc) = &mut self.h264_encoder {
                                enc.force_idr();
                            }
                        }
                        Some(Err(e)) => {
                            self.encode_failures += 1;
                            tracing::warn!(
                                failures = self.encode_failures,
                                max = MAX_ENCODE_FAILURES,
                                "H.264 encode failed: {:#}",
                                e
                            );
                            if let Some(enc) = &mut self.h264_encoder {
                                enc.force_idr();
                            }
                        }
                        None => {}
                    }

                    if force_full_frame && !sent_via_egfx {
                        shared.request_full_frame();
                    }

                    // Dynamic fallback: VAAPI -> software after repeated failures
                    if self.encode_failures >= MAX_ENCODE_FAILURES
                        && self.h264_encoder.as_ref().is_some_and(|e| e.is_vaapi())
                    {
                        tracing::warn!(
                            "VA-API encode failed {} consecutive times, switching to software encoder",
                            self.encode_failures
                        );
                        match crate::egfx::FrameEncoder::new_software_only(
                            self.width,
                            self.height,
                            self.bitrate,
                            self.fps,
                            self.quality,
                            self.rate_control,
                        ) {
                            Ok(enc) => {
                                self.h264_encoder = Some(enc);
                                self.encode_failures = 0;
                                self.egfx_surface_id = None; // Force surface re-init
                            }
                            Err(e) => {
                                tracing::error!("Software encoder fallback failed: {:#}", e);
                                self.h264_encoder = None;
                                self.egfx_active = false;
                            }
                        }
                    }
                }
            }

            // RFX-over-EGFX is not available through the current server API.
            // AVC-disabled clients fall through to bitmap fallback below.
        }

        if sent_via_egfx {
            self.sent_first_frame = true;
            self.pending_damage_regions.clear();
        }

        // Send bitmaps only when EGFX is unavailable or negotiated without AVC.
        // If EGFX is configured but capability negotiation has not completed yet,
        // keep the damage pending so startup does not mix legacy bitmap updates
        // with the graphics pipeline activation sequence.
        let egfx_state = self
            .egfx_shared
            .as_ref()
            .map(|s| (s.is_ready(), s.is_avc_enabled()));
        let egfx_runtime_available =
            self.egfx_active && self.h264_encoder.is_some() && self.egfx_surface_id.is_some();
        let should_send_bitmap = match egfx_state {
            None => true,
            Some((false, _)) => false,
            Some((true, avc_enabled)) => !avc_enabled || !egfx_runtime_available,
        };

        if !sent_via_egfx && should_send_bitmap {
            self.sent_first_frame = true;
            let update = DisplayUpdate::Bitmap(BitmapUpdate {
                x: 0,
                y: 0,
                width: NonZeroU16::new(self.width as u16).expect("width is non-zero"),
                height: NonZeroU16::new(self.height as u16).expect("height is non-zero"),
                format: self.pixel_format,
                data: Bytes::copy_from_slice(data),
                stride: NonZeroUsize::new(self.stride as usize).expect("stride is non-zero"),
            });
            if tx.blocking_send(update).is_err() {
                tracing::info!("Display update channel closed");
                return false;
            }
            self.damage_detector
                .update_reference(data, self.height, self.stride as usize);
            self.pending_damage_regions.clear();
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egfx::encoder::{Avc444EncodedFrame, Avc444FrameEncoding};
    use crate::egfx::{EgfxCodecPolicy, EgfxShared, H264RateControl};
    use ironrdp_core::{Decode, ReadCursor};
    use ironrdp_dvc::pdu::{DrdynvcDataPdu, DrdynvcServerPdu};
    use ironrdp_egfx::pdu::{
        Avc444BitmapStream, Codec1Type, Encoding, FrameAcknowledgePdu, GfxPdu, QueueDepth,
        WireToSurface1Pdu,
    };
    use ironrdp_server::{DisplayUpdate, EgfxServerMessage, PixelFormat};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tokio::sync::mpsc;

    const TEST_CHANNEL_ID: u32 = 1007;

    fn avc444_frame(
        encoding: Avc444FrameEncoding,
        stream1: Vec<u8>,
        stream2: Vec<u8>,
        stream1_regions: Vec<(i32, i32, i32, i32)>,
        stream2_regions: Vec<(i32, i32, i32, i32)>,
    ) -> EncodedEgfxFrame {
        EncodedEgfxFrame::Avc444(Avc444EncodedFrame {
            encoding,
            stream1,
            stream2,
            stream1_regions,
            stream2_regions,
        })
    }

    #[test]
    fn encoded_frame_state_treats_empty_output_as_encoder_skip() {
        assert_eq!(
            EncodedEgfxFrame::Avc420(Vec::new()).state(),
            EncodedFrameState::Skipped
        );
        assert_eq!(
            avc444_frame(
                Avc444FrameEncoding::Luma,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new()
            )
            .state(),
            EncodedFrameState::Skipped
        );
    }

    #[test]
    fn encoded_frame_state_accepts_avc444_empty_h264_payload_with_regions() {
        assert_eq!(
            avc444_frame(
                Avc444FrameEncoding::Luma,
                Vec::new(),
                Vec::new(),
                vec![(0, 0, 16, 16)],
                Vec::new()
            )
            .state(),
            EncodedFrameState::Sendable
        );
        assert_eq!(
            avc444_frame(
                Avc444FrameEncoding::LumaAndChroma,
                Vec::new(),
                Vec::new(),
                vec![(0, 0, 16, 16)],
                vec![(16, 0, 16, 16)]
            )
            .state(),
            EncodedFrameState::Sendable
        );
    }

    #[test]
    fn frame_processor_forces_idr_after_unsent_encoder_skip() {
        let width = 64;
        let height = 64;
        let stride = width * 4;
        let mut processor = FrameProcessor::new(
            None,
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Cqp,
            30,
        );
        processor.h264_encoder = Some(
            crate::egfx::FrameEncoder::new_avc444_software_only(
                width as u32,
                height as u32,
                1_000_000,
                30,
                23,
                H264RateControl::Cqp,
            )
            .expect("AVC444 encoder initializes"),
        );
        processor.encode_failures = 2;

        FrameProcessor::handle_encoder_skip(
            &mut processor.encode_failures,
            &mut processor.h264_encoder,
            &mut processor.stats,
            processor.width,
            processor.height,
        );

        assert_eq!(processor.encode_failures, 0);
        assert_eq!(
            processor
                .h264_encoder
                .as_ref()
                .and_then(crate::egfx::FrameEncoder::force_idr_requests_for_test),
            Some(1)
        );
    }

    #[test]
    fn encoded_frame_state_rejects_partial_avc444_payloads() {
        assert_eq!(
            avc444_frame(
                Avc444FrameEncoding::Luma,
                vec![0x55; 64],
                Vec::new(),
                Vec::new(),
                Vec::new()
            )
            .state(),
            EncodedFrameState::Invalid
        );
        assert_eq!(
            avc444_frame(
                Avc444FrameEncoding::LumaAndChroma,
                vec![0x55; 64],
                vec![0xaa; 64],
                vec![(0, 0, 16, 16)],
                Vec::new()
            )
            .state(),
            EncodedFrameState::Invalid
        );
        assert_eq!(
            avc444_frame(
                Avc444FrameEncoding::Luma,
                Vec::new(),
                vec![0x55; 64],
                vec![(0, 0, 16, 16)],
                Vec::new()
            )
            .state(),
            EncodedFrameState::Invalid
        );
    }

    fn gradient_bgra_frame(width: usize, height: usize, stride: usize) -> Vec<u8> {
        let mut frame = vec![0; stride * height];
        for y in 0..height {
            for x in 0..width {
                let offset = y * stride + x * 4;
                frame[offset] = (x * 11 + y * 3) as u8;
                frame[offset + 1] = (x * 5 + y * 17) as u8;
                frame[offset + 2] = (x * 19 + y * 7) as u8;
                frame[offset + 3] = 255;
            }
        }
        frame
    }

    fn negotiate_egfx(
        width: u16,
        height: u16,
    ) -> (
        Arc<EgfxShared>,
        mpsc::UnboundedReceiver<ironrdp_server::ServerEvent>,
    ) {
        let (shared, event_rx) = negotiate_egfx_with_policy(width, height, EgfxCodecPolicy::Avc444);
        assert!(shared.is_avc444_enabled());
        (shared, event_rx)
    }

    fn negotiate_egfx_with_policy(
        width: u16,
        height: u16,
        codec_policy: EgfxCodecPolicy,
    ) -> (
        Arc<EgfxShared>,
        mpsc::UnboundedReceiver<ironrdp_server::ServerEvent>,
    ) {
        let shared = Arc::new(EgfxShared::with_codec_policy(
            crate::egfx::DEFAULT_MAX_FRAMES_IN_FLIGHT,
            codec_policy,
        ));
        shared.set_surface_size(width, height);
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let mut factory = crate::egfx::HyprGfxFactory::new(Arc::clone(&shared));
        ironrdp_server::ServerEventSender::set_sender(&mut factory, event_tx);
        let (mut bridge, _handle) =
            ironrdp_server::GfxServerFactory::build_server_with_handle(&factory)
                .expect("EGFX server builds");
        ironrdp_dvc::DvcProcessor::start(&mut bridge, TEST_CHANNEL_ID).expect("channel starts");

        let caps = ironrdp_egfx::pdu::GfxPdu::CapabilitiesAdvertise(
            ironrdp_egfx::pdu::CapabilitiesAdvertisePdu(vec![
                ironrdp_egfx::pdu::CapabilitySet::V10_7 {
                    flags: ironrdp_egfx::pdu::CapabilitiesV107Flags::empty(),
                },
            ]),
        );
        let caps = ironrdp_core::encode_vec(&caps).expect("capabilities encode");
        let _ = ironrdp_dvc::DvcProcessor::process(&mut bridge, TEST_CHANNEL_ID, &caps)
            .expect("capabilities process");

        assert!(shared.is_ready());
        assert!(shared.is_avc_enabled());

        (shared, event_rx)
    }

    fn negotiate_egfx_without_avc(
        width: u16,
        height: u16,
    ) -> (
        Arc<EgfxShared>,
        mpsc::UnboundedReceiver<ironrdp_server::ServerEvent>,
    ) {
        let shared = Arc::new(EgfxShared::with_codec_policy(
            crate::egfx::DEFAULT_MAX_FRAMES_IN_FLIGHT,
            EgfxCodecPolicy::Auto,
        ));
        shared.set_surface_size(width, height);
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let mut factory = crate::egfx::HyprGfxFactory::new(Arc::clone(&shared));
        ironrdp_server::ServerEventSender::set_sender(&mut factory, event_tx);
        let (mut bridge, _handle) =
            ironrdp_server::GfxServerFactory::build_server_with_handle(&factory)
                .expect("EGFX server builds");
        ironrdp_dvc::DvcProcessor::start(&mut bridge, TEST_CHANNEL_ID).expect("channel starts");

        let caps = ironrdp_egfx::pdu::GfxPdu::CapabilitiesAdvertise(
            ironrdp_egfx::pdu::CapabilitiesAdvertisePdu(vec![
                ironrdp_egfx::pdu::CapabilitySet::V8 {
                    flags: ironrdp_egfx::pdu::CapabilitiesV8Flags::empty(),
                },
            ]),
        );
        let caps = ironrdp_core::encode_vec(&caps).expect("capabilities encode");
        let _ = ironrdp_dvc::DvcProcessor::process(&mut bridge, TEST_CHANNEL_ID, &caps)
            .expect("capabilities process");

        assert!(shared.is_ready());
        assert!(!shared.is_avc_enabled());

        (shared, event_rx)
    }

    fn assert_bitmap_update(
        rx: &mut mpsc::Receiver<DisplayUpdate>,
        width: usize,
        height: usize,
        stride: usize,
        format: PixelFormat,
        expected_data: &[u8],
    ) {
        match rx.try_recv() {
            Ok(DisplayUpdate::Bitmap(update)) => {
                assert_eq!(update.x, 0);
                assert_eq!(update.y, 0);
                assert_eq!(update.width.get(), width as u16);
                assert_eq!(update.height.get(), height as u16);
                assert_eq!(update.stride.get(), stride);
                assert_eq!(update.format, format);
                assert_eq!(update.data.as_ref(), expected_data);
            }
            other => panic!("expected bitmap update, got {other:?}"),
        }
    }

    fn drain_gfx_pdus(
        event_rx: &mut mpsc::UnboundedReceiver<ironrdp_server::ServerEvent>,
    ) -> Vec<GfxPdu> {
        let mut pdus = Vec::new();
        let mut expected_fragment_len = 0usize;
        let mut fragments = Vec::new();

        while let Ok(event) = event_rx.try_recv() {
            let ironrdp_server::ServerEvent::Egfx(EgfxServerMessage::SendMessages { messages }) =
                event
            else {
                continue;
            };

            for message in messages {
                let encoded = message.encode_unframed_pdu().expect("DVC message encodes");
                let mut cursor = ReadCursor::new(&encoded);
                let dvc = DrdynvcServerPdu::decode(&mut cursor).expect("DVC message decodes");
                let DrdynvcServerPdu::Data(data) = dvc else {
                    continue;
                };

                let complete = match data {
                    DrdynvcDataPdu::DataFirst(data_first) => {
                        let total_len = data_first.length() as usize;
                        if total_len == data_first.data().len() {
                            Some(data_first.into_data())
                        } else {
                            expected_fragment_len = total_len;
                            fragments = data_first.into_data();
                            None
                        }
                    }
                    DrdynvcDataPdu::Data(mut data) => {
                        if expected_fragment_len == 0 {
                            Some(data.into_data())
                        } else {
                            fragments.append(data.data_mut());
                            if fragments.len() == expected_fragment_len {
                                expected_fragment_len = 0;
                                Some(std::mem::take(&mut fragments))
                            } else {
                                None
                            }
                        }
                    }
                };

                if let Some(gfx_bytes) = complete {
                    let gfx_bytes = if gfx_bytes.starts_with(&[0xe0, 0x04]) {
                        &gfx_bytes[2..]
                    } else {
                        &gfx_bytes
                    };
                    let mut cursor = ReadCursor::new(gfx_bytes);
                    pdus.push(GfxPdu::decode(&mut cursor).expect("GFX PDU decodes"));
                }
            }
        }

        pdus
    }

    fn assert_sendable_avc444_wire_to_surface(
        pdus: &[GfxPdu],
        expected_encoding: Encoding,
    ) -> &WireToSurface1Pdu {
        let wire = pdus
            .iter()
            .find_map(|pdu| match pdu {
                GfxPdu::WireToSurface1(wire) => Some(wire),
                _ => None,
            })
            .expect("AVC444 frame emits WireToSurface1");
        assert_eq!(wire.codec_id, Codec1Type::Avc444v2);

        let mut cursor = ReadCursor::new(&wire.bitmap_data);
        let bitmap = Avc444BitmapStream::decode(&mut cursor).expect("AVC444 payload decodes");
        assert_eq!(bitmap.encoding, expected_encoding);
        assert!(!bitmap.stream1.data.is_empty());
        assert!(!bitmap.stream1.rectangles.is_empty());

        if expected_encoding == Encoding::LUMA_AND_CHROMA {
            let stream2 = bitmap.stream2.expect("LC=0 carries stream2");
            assert!(!stream2.data.is_empty());
            assert!(!stream2.rectangles.is_empty());
        } else {
            assert!(bitmap.stream2.is_none());
        }

        wire
    }

    fn assert_initial_surface_setup_precedes_logical_frame(pdus: &[GfxPdu]) {
        assert_eq!(pdus.len(), 6);
        let surface_id = match &pdus[1] {
            GfxPdu::CreateSurface(create) => create.surface_id,
            other => panic!("expected CreateSurface second, got {other:?}"),
        };
        match &pdus[0] {
            GfxPdu::ResetGraphics(reset) => {
                assert_eq!(reset.width, 64);
                assert_eq!(reset.height, 64);
                assert!(reset.monitors.is_empty());
            }
            other => panic!("expected ResetGraphics first, got {other:?}"),
        }
        match &pdus[2] {
            GfxPdu::MapSurfaceToOutput(map) => assert_eq!(map.surface_id, surface_id),
            other => panic!("expected MapSurfaceToOutput third, got {other:?}"),
        }
        let start = match &pdus[3] {
            GfxPdu::StartFrame(start) => start,
            other => panic!("expected StartFrame after surface setup, got {other:?}"),
        };
        let wire = match &pdus[4] {
            GfxPdu::WireToSurface1(wire) => wire,
            other => panic!("expected WireToSurface1 inside logical frame, got {other:?}"),
        };
        let end = match &pdus[5] {
            GfxPdu::EndFrame(end) => end,
            other => panic!("expected EndFrame after WireToSurface1, got {other:?}"),
        };

        assert_eq!(wire.surface_id, surface_id);
        assert_eq!(end.frame_id, start.frame_id);
    }

    fn frame_id_from_pdus(pdus: &[GfxPdu]) -> u32 {
        match pdus.iter().find_map(|pdu| match pdu {
            GfxPdu::StartFrame(start) => Some(start.frame_id),
            _ => None,
        }) {
            Some(frame_id) => frame_id,
            None => panic!("expected StartFrame in PDU list"),
        }
    }

    fn ack_frame(
        bridge: &mut ironrdp_server::GfxDvcBridge,
        frame_id: u32,
        queue_depth: QueueDepth,
    ) {
        let ack = GfxPdu::FrameAcknowledge(FrameAcknowledgePdu {
            queue_depth,
            frame_id,
            total_frames_decoded: 1,
        });
        let ack = ironrdp_core::encode_vec(&ack).expect("frame ack encodes");
        let _ = ironrdp_dvc::DvcProcessor::process(bridge, TEST_CHANNEL_ID, &ack)
            .expect("frame ack processes");
    }

    #[test]
    fn egfx_perf_logging_is_opt_in() {
        assert!(!egfx_perf_logging_enabled_with(|_| false));
        assert!(egfx_perf_logging_enabled_with(
            |name| name == "HYPR_RDP_EGFX_PERF"
        ));
    }

    #[test]
    fn frame_stats_keeps_send_unavailable_reason_counters() {
        let mut stats = FrameStats::new();

        stats.record_send_unavailable(
            EgfxFrameReadiness::TransportBackpressure {
                in_flight: 1,
                client_queue_depth: 0,
            },
            64,
            64,
        );
        stats.record_send_unavailable(
            EgfxFrameReadiness::LocalBackpressure {
                in_flight: 2,
                max: 2,
                client_queue_depth: 0,
                ack_suspended: false,
            },
            64,
            64,
        );

        assert_eq!(stats.skipped_backpressure, 2);
        assert_eq!(stats.skipped_transport_backpressure, 1);
        assert_eq!(stats.skipped_local_backpressure, 1);
    }

    #[test]
    fn avc444_dimension_gate_matches_local_packing_constraints() {
        assert!(avc444_dimensions_supported(1920, 1200));
        assert!(!avc444_dimensions_supported(18, 16));
        assert!(!avc444_dimensions_supported(64, 15));
        assert!(!avc444_dimensions_supported(0, 64));
    }

    #[test]
    fn frame_pacer_keeps_30fps_on_quantized_60hz_events() {
        let start = Instant::now();
        let mut pacer = FramePacer::new(30, start);

        assert!(pacer.should_send(start, false, true));
        assert!(!pacer.should_send(start + Duration::from_millis(16), true, true));
        assert!(pacer.should_send(start + Duration::from_millis(32), true, true));
        assert!(!pacer.should_send(start + Duration::from_millis(48), true, true));
        assert!(pacer.should_send(start + Duration::from_millis(64), true, true));
    }

    #[test]
    fn frame_pacer_does_not_burst_after_idle() {
        let start = Instant::now();
        let mut pacer = FramePacer::new(30, start);

        assert!(pacer.should_send(start, false, true));
        assert!(!pacer.should_send(start + Duration::from_secs(1), true, false));
        assert!(pacer.should_send(start + Duration::from_secs(1), true, true));
        assert!(!pacer.should_send(start + Duration::from_secs(1), true, true));
    }

    #[test]
    fn frame_pacer_keeps_30fps_on_quantized_50hz_events() {
        let start = Instant::now();
        let mut pacer = FramePacer::new(30, start);

        let sends = (0..50)
            .filter(|i| pacer.should_send(start + Duration::from_millis(i * 20), *i > 0, true))
            .count();

        assert_eq!(sends, 30);
    }

    #[test]
    fn frame_processor_selects_avc420_when_avc444_dimensions_are_unsupported() {
        let width = 18;
        let height = 16;
        let stride = width * 4;
        let (shared, _event_rx) = negotiate_egfx(width as u16, height as u16);
        let (display_tx, mut display_rx) = mpsc::channel(4);
        let frame = gradient_bgra_frame(width, height, stride);

        let mut processor = FrameProcessor::new(
            Some(shared),
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Vbr,
            30,
        );
        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);

        assert!(processor.process(&frame, &display_tx));
        assert_eq!(processor.egfx_codec, Some(EgfxCodec::Avc420));
        assert!(processor.sent_first_frame);
        assert!(processor.pending_damage_regions.is_empty());
        assert!(display_rx.try_recv().is_err());
    }

    #[test]
    fn frame_processor_selects_avc420_when_policy_forces_avc420() {
        let width = 64;
        let height = 64;
        let stride = width * 4;
        let (shared, _event_rx) =
            negotiate_egfx_with_policy(width as u16, height as u16, EgfxCodecPolicy::Avc420);
        assert!(!shared.is_avc444_enabled());
        let (display_tx, mut display_rx) = mpsc::channel(4);
        let frame = gradient_bgra_frame(width, height, stride);

        let mut processor = FrameProcessor::new(
            Some(shared),
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Vbr,
            30,
        );
        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);

        assert!(processor.process(&frame, &display_tx));
        assert_eq!(processor.egfx_codec, Some(EgfxCodec::Avc420));
        assert!(processor.sent_first_frame);
        assert!(processor.pending_damage_regions.is_empty());
        assert!(display_rx.try_recv().is_err());
    }

    #[test]
    fn frame_processor_sends_bitmap_when_egfx_is_not_configured() {
        let width = 8;
        let height = 4;
        let stride = width * 4 + 8;
        let (display_tx, mut display_rx) = mpsc::channel(4);
        let frame = gradient_bgra_frame(width, height, stride);

        let mut processor = FrameProcessor::new(
            None,
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Vbr,
            30,
        );
        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);

        assert!(processor.process(&frame, &display_tx));
        assert!(processor.sent_first_frame);
        assert!(processor.pending_damage_regions.is_empty());
        assert_bitmap_update(
            &mut display_rx,
            width,
            height,
            stride,
            PixelFormat::BgrA32,
            &frame,
        );
    }

    #[test]
    fn frame_processor_waits_for_egfx_negotiation_before_bitmap_fallback() {
        let width = 16;
        let height = 16;
        let stride = width * 4;
        let shared = Arc::new(EgfxShared::with_codec_policy(
            crate::egfx::DEFAULT_MAX_FRAMES_IN_FLIGHT,
            EgfxCodecPolicy::Auto,
        ));
        shared.set_surface_size(width as u16, height as u16);
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let mut factory = crate::egfx::HyprGfxFactory::new(Arc::clone(&shared));
        ironrdp_server::ServerEventSender::set_sender(&mut factory, event_tx);
        let (_bridge, _handle) =
            ironrdp_server::GfxServerFactory::build_server_with_handle(&factory)
                .expect("EGFX server builds");
        assert!(!shared.is_ready());

        let (display_tx, mut display_rx) = mpsc::channel(4);
        let frame = gradient_bgra_frame(width, height, stride);
        let mut processor = FrameProcessor::new(
            Some(shared),
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Vbr,
            30,
        );
        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);

        assert!(processor.process(&frame, &display_tx));
        assert!(!processor.sent_first_frame);
        assert!(processor.has_pending_damage());
        assert!(display_rx.try_recv().is_err());
    }

    #[test]
    fn frame_processor_delays_initial_resize_until_egfx_channel_ready() {
        let width = 16;
        let height = 16;
        let stride = width * 4;
        let shared = Arc::new(EgfxShared::with_codec_policy(
            crate::egfx::DEFAULT_MAX_FRAMES_IN_FLIGHT,
            EgfxCodecPolicy::Auto,
        ));
        shared.set_surface_size(width as u16, height as u16);
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let mut factory = crate::egfx::HyprGfxFactory::new(Arc::clone(&shared));
        ironrdp_server::ServerEventSender::set_sender(&mut factory, event_tx);
        let (_bridge, _handle) =
            ironrdp_server::GfxServerFactory::build_server_with_handle(&factory)
                .expect("EGFX server builds");
        assert!(!shared.is_ready());

        let (display_tx, mut display_rx) = mpsc::channel(4);
        let frame = gradient_bgra_frame(width, height, stride);
        let mut processor = FrameProcessor::new(
            Some(shared),
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Vbr,
            30,
        );
        processor.set_pending_initial_resize(Some(DesktopSize {
            width: width as u16,
            height: height as u16,
        }));
        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);

        assert!(processor.process(&frame, &display_tx));
        assert!(!processor.sent_first_frame);
        assert!(processor.has_pending_damage());
        assert!(display_rx.try_recv().is_err());
    }

    #[test]
    fn frame_processor_sends_initial_resize_before_first_egfx_frame_when_ready() {
        let width = 16;
        let height = 16;
        let stride = width * 4;
        let (shared, mut event_rx) = negotiate_egfx(width as u16, height as u16);
        let (display_tx, mut display_rx) = mpsc::channel(4);
        let frame = gradient_bgra_frame(width, height, stride);

        let mut processor = FrameProcessor::new(
            Some(shared),
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Vbr,
            30,
        );
        processor.set_pending_initial_resize(Some(DesktopSize {
            width: width as u16,
            height: height as u16,
        }));
        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);

        assert!(!processor.process(&frame, &display_tx));
        assert!(!processor.sent_first_frame);
        assert!(processor.has_pending_damage());
        assert!(drain_gfx_pdus(&mut event_rx).is_empty());
        match display_rx.try_recv().expect("resize update is emitted") {
            DisplayUpdate::Resize(size) => {
                assert_eq!(size.width, width as u16);
                assert_eq!(size.height, height as u16);
            }
            _ => panic!("expected resize update before first EGFX frame"),
        }
    }

    #[test]
    fn frame_processor_sends_bitmap_when_egfx_client_has_no_avc() {
        let width = 16;
        let height = 16;
        let stride = width * 4;
        let (shared, mut event_rx) = negotiate_egfx_without_avc(width as u16, height as u16);
        let (display_tx, mut display_rx) = mpsc::channel(4);
        let frame = gradient_bgra_frame(width, height, stride);

        let mut processor = FrameProcessor::new(
            Some(shared),
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Vbr,
            30,
        );
        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);

        assert!(processor.process(&frame, &display_tx));
        assert!(processor.sent_first_frame);
        assert!(processor.pending_damage_regions.is_empty());
        assert!(drain_gfx_pdus(&mut event_rx).is_empty());
        assert_bitmap_update(
            &mut display_rx,
            width,
            height,
            stride,
            PixelFormat::BgrA32,
            &frame,
        );
    }

    #[test]
    fn bitmap_fallback_preserves_alpha_rgb_order_and_padding() {
        let width = 3;
        let height = 2;
        let stride = width * 4 + 5;
        let mut frame = vec![0xee; stride * height];
        let pixels = [
            [0x10, 0x20, 0x30, 0x40],
            [0x50, 0x60, 0x70, 0x80],
            [0x90, 0xa0, 0xb0, 0xc0],
            [0x01, 0x02, 0x03, 0x04],
            [0x05, 0x06, 0x07, 0x08],
            [0x09, 0x0a, 0x0b, 0x0c],
        ];
        for (index, pixel) in pixels.iter().enumerate() {
            let row = index / width;
            let x = index % width;
            let offset = row * stride + x * 4;
            frame[offset..offset + 4].copy_from_slice(pixel);
        }

        let (display_tx, mut display_rx) = mpsc::channel(4);
        let mut processor = FrameProcessor::new(
            None,
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Vbr,
            30,
        );
        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);

        assert!(processor.process(&frame, &display_tx));
        assert_bitmap_update(
            &mut display_rx,
            width,
            height,
            stride,
            PixelFormat::BgrA32,
            &frame,
        );
    }

    #[test]
    fn frame_processor_sends_bitmap_when_negotiated_avc_encoder_is_unavailable() {
        let width = 17;
        let height = 16;
        let stride = width * 4;
        let (shared, _event_rx) = negotiate_egfx(width as u16, height as u16);
        let (display_tx, mut display_rx) = mpsc::channel(4);
        let frame = gradient_bgra_frame(width, height, stride);

        let mut processor = FrameProcessor::new(
            Some(shared),
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Vbr,
            30,
        );
        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);

        assert!(processor.process(&frame, &display_tx));
        assert!(processor.h264_encoder.is_none());
        assert!(!processor.egfx_active);
        assert!(processor.sent_first_frame);
        assert!(processor.pending_damage_regions.is_empty());
        assert_bitmap_update(
            &mut display_rx,
            width,
            height,
            stride,
            PixelFormat::BgrA32,
            &frame,
        );
    }

    #[test]
    fn frame_processor_does_not_emit_bitmap_for_invalid_egfx_encode_input() {
        let width = 64;
        let height = 64;
        let stride = width * 4;
        let (shared, mut event_rx) =
            negotiate_egfx_with_policy(width as u16, height as u16, EgfxCodecPolicy::Auto);
        let (display_tx, mut display_rx) = mpsc::channel(4);
        let short_frame = vec![0x7f; stride * (height - 1)];

        let mut processor = FrameProcessor::new(
            Some(shared),
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Vbr,
            30,
        );
        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);

        assert!(processor.process(&short_frame, &display_tx));
        assert_eq!(processor.encode_failures, 1);
        assert!(!processor.sent_first_frame);
        assert!(processor.has_pending_damage());
        assert!(display_rx.try_recv().is_err());
        let setup_pdus = drain_gfx_pdus(&mut event_rx);
        assert!(setup_pdus
            .iter()
            .all(|pdu| !matches!(pdu, GfxPdu::WireToSurface1(_))));
    }

    #[test]
    fn frame_processor_emits_avc444_events_for_initial_and_followup_damage() {
        let width = 64;
        let height = 64;
        let stride = width * 4;
        let (shared, mut event_rx) = negotiate_egfx(width as u16, height as u16);
        let (display_tx, mut display_rx) = mpsc::channel(4);
        let first = gradient_bgra_frame(width, height, stride);
        let mut second = first.clone();
        second[(16 * stride) + (16 * 4)] ^= 0x7f;
        second[(17 * stride) + (17 * 4) + 1] ^= 0x3f;

        let mut processor = FrameProcessor::new(
            Some(shared),
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Cqp,
            30,
        );
        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);

        assert!(processor.process(&first, &display_tx));
        assert_eq!(processor.egfx_codec, Some(EgfxCodec::Avc444));
        assert!(processor.sent_first_frame);
        assert!(processor.egfx_surface_id.is_some());
        assert!(!processor.has_pending_damage());
        assert!(display_rx.try_recv().is_err());

        let initial_pdus = drain_gfx_pdus(&mut event_rx);
        assert_initial_surface_setup_precedes_logical_frame(&initial_pdus);
        assert_sendable_avc444_wire_to_surface(&initial_pdus, Encoding::LUMA_AND_CHROMA);

        processor.queue_damage(&[(16, 16, 2, 2)]);
        assert!(processor.process(&second, &display_tx));
        assert!(processor.sent_first_frame);
        assert!(!processor.has_pending_damage());
        assert!(display_rx.try_recv().is_err());

        let followup_pdus = drain_gfx_pdus(&mut event_rx);
        assert_sendable_avc444_wire_to_surface(&followup_pdus, Encoding::LUMA_AND_CHROMA);
    }

    #[test]
    fn frame_processor_sends_full_frame_after_repeated_capabilities_refresh_request() {
        let width = 64;
        let height = 64;
        let stride = width * 4;
        let (shared, mut event_rx) = negotiate_egfx(width as u16, height as u16);
        let (display_tx, mut display_rx) = mpsc::channel(4);
        let frame = gradient_bgra_frame(width, height, stride);

        let mut processor = FrameProcessor::new(
            Some(Arc::clone(&shared)),
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Cqp,
            30,
        );
        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);

        assert!(processor.process(&frame, &display_tx));
        assert!(processor.sent_first_frame);
        assert!(!processor.has_pending_damage());
        assert!(display_rx.try_recv().is_err());
        let initial_pdus = drain_gfx_pdus(&mut event_rx);
        assert_sendable_avc444_wire_to_surface(&initial_pdus, Encoding::LUMA_AND_CHROMA);

        shared.request_full_frame();
        assert!(processor.process(&frame, &display_tx));
        assert!(!shared.full_frame_requested());
        assert!(!processor.has_pending_damage());
        assert!(display_rx.try_recv().is_err());

        let refresh_pdus = drain_gfx_pdus(&mut event_rx);
        assert_sendable_avc444_wire_to_surface(&refresh_pdus, Encoding::LUMA_AND_CHROMA);
        let (last_luma_regions, last_chroma_regions) = processor
            .h264_encoder
            .as_ref()
            .and_then(crate::egfx::FrameEncoder::avc444_last_reference_regions_for_test)
            .expect("AVC444 region state exists");
        let full_frame = [(0, 0, width as i32, height as i32)];
        assert_eq!(last_luma_regions, full_frame);
        assert_eq!(last_chroma_regions, full_frame);
    }

    #[test]
    fn frame_processor_recovers_avc444_with_full_luma_and_chroma_after_send_failure() {
        let width = 64;
        let height = 64;
        let stride = width * 4;
        let (shared, event_rx) = negotiate_egfx(width as u16, height as u16);
        let (display_tx, mut display_rx) = mpsc::channel(4);
        let first = gradient_bgra_frame(width, height, stride);
        let mut second = first.clone();
        second[4] = 0x40;

        let mut processor = FrameProcessor::new(
            Some(shared),
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Cqp,
            30,
        );
        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);

        assert!(processor.process(&first, &display_tx));
        assert_eq!(processor.egfx_codec, Some(EgfxCodec::Avc444));
        assert!(processor.sent_first_frame);
        let committed_before = processor
            .h264_encoder
            .as_ref()
            .and_then(crate::egfx::FrameEncoder::avc444_luma_reference_y_for_test)
            .expect("AVC444 reference committed after successful send")
            .to_vec();

        drop(event_rx);
        processor.queue_damage(&[(0, 0, 4, 2)]);
        assert!(processor.process(&second, &display_tx));

        let committed_after = processor
            .h264_encoder
            .as_ref()
            .and_then(crate::egfx::FrameEncoder::avc444_luma_reference_y_for_test)
            .expect("AVC444 reference remains available")
            .to_vec();
        assert_eq!(committed_after, committed_before);
        assert!(processor.has_pending_damage());
        assert!(display_rx.try_recv().is_err());

        let (recovery_tx, mut recovery_rx) = mpsc::unbounded_channel();
        processor.egfx_sender = Some(recovery_tx);
        assert!(processor.process(&second, &display_tx));

        let committed_recovered = processor
            .h264_encoder
            .as_ref()
            .and_then(crate::egfx::FrameEncoder::avc444_luma_reference_y_for_test)
            .expect("AVC444 reference committed after recovery send")
            .to_vec();
        assert_ne!(committed_recovered, committed_before);
        let (last_luma_regions, last_chroma_regions) = processor
            .h264_encoder
            .as_ref()
            .and_then(crate::egfx::FrameEncoder::avc444_last_reference_regions_for_test)
            .expect("AVC444 region state exists");
        let full_frame = [(0, 0, width as i32, height as i32)];
        assert_eq!(last_luma_regions, full_frame);
        assert_eq!(last_chroma_regions, full_frame);
        assert!(!processor.has_pending_damage());
        assert!(display_rx.try_recv().is_err());
        assert!(recovery_rx.try_recv().is_ok());
    }

    #[test]
    fn frame_processor_backpressure_preserves_avc444_reference_and_pending_damage() {
        let width = 64;
        let height = 64;
        let stride = width * 4;
        let shared = Arc::new(EgfxShared::with_codec_policy(1, EgfxCodecPolicy::Avc444));
        shared.set_surface_size(width as u16, height as u16);
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let mut factory = crate::egfx::HyprGfxFactory::new(Arc::clone(&shared));
        ironrdp_server::ServerEventSender::set_sender(&mut factory, event_tx);
        let (mut bridge, handle) =
            ironrdp_server::GfxServerFactory::build_server_with_handle(&factory)
                .expect("EGFX server builds");
        ironrdp_dvc::DvcProcessor::start(&mut bridge, TEST_CHANNEL_ID).expect("channel starts");
        let caps = ironrdp_egfx::pdu::GfxPdu::CapabilitiesAdvertise(
            ironrdp_egfx::pdu::CapabilitiesAdvertisePdu(vec![
                ironrdp_egfx::pdu::CapabilitySet::V10_7 {
                    flags: ironrdp_egfx::pdu::CapabilitiesV107Flags::empty(),
                },
            ]),
        );
        let caps = ironrdp_core::encode_vec(&caps).expect("capabilities encode");
        let _ = ironrdp_dvc::DvcProcessor::process(&mut bridge, TEST_CHANNEL_ID, &caps)
            .expect("capabilities process");
        assert!(shared.is_ready());
        assert!(shared.is_avc444_enabled());

        let (display_tx, mut display_rx) = mpsc::channel(4);
        let first = gradient_bgra_frame(width, height, stride);
        let mut second = first.clone();
        second[16 * stride + 16 * 4] ^= 0x7f;
        second[17 * stride + 17 * 4 + 1] ^= 0x3f;

        let mut processor = FrameProcessor::new(
            Some(Arc::clone(&shared)),
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Cqp,
            30,
        );
        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);

        assert!(processor.process(&first, &display_tx));
        assert!(!shared.can_send_frame(&handle));
        assert!(!processor.has_pending_damage());
        assert!(display_rx.try_recv().is_err());
        let initial_pdus = drain_gfx_pdus(&mut event_rx);
        let frame_id = frame_id_from_pdus(&initial_pdus);
        let committed_before = processor
            .h264_encoder
            .as_ref()
            .and_then(crate::egfx::FrameEncoder::avc444_luma_reference_y_for_test)
            .expect("AVC444 reference committed after initial send")
            .to_vec();

        processor.queue_damage(&[(16, 16, 2, 2)]);
        assert!(processor.process(&second, &display_tx));

        assert_eq!(processor.stats.skipped_backpressure, 1);
        assert!(processor.has_pending_damage());
        assert!(!shared.can_send_frame(&handle));
        assert!(drain_gfx_pdus(&mut event_rx).is_empty());
        let committed_after_backpressure = processor
            .h264_encoder
            .as_ref()
            .and_then(crate::egfx::FrameEncoder::avc444_luma_reference_y_for_test)
            .expect("AVC444 reference remains available after backpressure")
            .to_vec();
        assert_eq!(committed_after_backpressure, committed_before);

        ack_frame(&mut bridge, frame_id, QueueDepth::AvailableBytes(1));
        assert!(shared.can_send_frame(&handle));
        assert!(processor.process(&second, &display_tx));
        assert!(!processor.has_pending_damage());
        let recovered_pdus = drain_gfx_pdus(&mut event_rx);
        assert_sendable_avc444_wire_to_surface(&recovered_pdus, Encoding::LUMA_AND_CHROMA);
        let committed_after_recovery = processor
            .h264_encoder
            .as_ref()
            .and_then(crate::egfx::FrameEncoder::avc444_luma_reference_y_for_test)
            .expect("AVC444 reference committed after backpressure recovery")
            .to_vec();
        assert_ne!(committed_after_recovery, committed_before);
        assert!(display_rx.try_recv().is_err());
    }

    #[test]
    fn frame_processor_recreates_avc444_state_after_egfx_generation_bump() {
        let width = 64;
        let height = 64;
        let stride = width * 4;
        let (shared, mut event_rx) = negotiate_egfx(width as u16, height as u16);
        let (display_tx, mut display_rx) = mpsc::channel(4);
        let first = gradient_bgra_frame(width, height, stride);
        let mut second = first.clone();
        second[8] = 0x7f;

        let mut processor = FrameProcessor::new(
            Some(Arc::clone(&shared)),
            width as u32,
            height as u32,
            PixelFormat::BgrA32,
            stride as u32,
            1_000_000,
            23,
            H264RateControl::Cqp,
            30,
        );
        processor.queue_damage(&[(0, 0, width as i32, height as i32)]);

        assert!(processor.process(&first, &display_tx));
        assert_eq!(processor.egfx_codec, Some(EgfxCodec::Avc444));
        assert!(processor.sent_first_frame);
        assert!(processor.egfx_surface_id.is_some());
        let initial_pdus = drain_gfx_pdus(&mut event_rx);
        let initial_wire =
            assert_sendable_avc444_wire_to_surface(&initial_pdus, Encoding::LUMA_AND_CHROMA);
        let old_surface_id = initial_wire.surface_id;
        let first_generation = processor.egfx_generation;

        shared.prepare_for_resize(width as u16, height as u16);
        let resize_pdus = drain_gfx_pdus(&mut event_rx);
        assert!(resize_pdus.iter().any(|pdu| matches!(
            pdu,
            GfxPdu::DeleteSurface(delete) if delete.surface_id == old_surface_id
        )));
        assert!(resize_pdus
            .iter()
            .all(|pdu| !matches!(pdu, GfxPdu::WireToSurface1(_))));
        processor.queue_damage(&[(0, 0, 4, 2)]);
        assert!(processor.process(&second, &display_tx));

        assert!(processor.egfx_generation > first_generation);
        assert_eq!(processor.egfx_generation, shared.generation());
        assert_eq!(processor.egfx_codec, Some(EgfxCodec::Avc444));
        assert!(processor.sent_first_frame);
        assert!(processor.egfx_surface_id.is_some());
        let resized_pdus = drain_gfx_pdus(&mut event_rx);
        let new_surface_id = resized_pdus
            .iter()
            .find_map(|pdu| match pdu {
                GfxPdu::CreateSurface(create) => Some(create.surface_id),
                _ => None,
            })
            .expect("resize reinitializes surface before sending frame");
        assert_ne!(new_surface_id, old_surface_id);
        assert!(resized_pdus.iter().any(|pdu| matches!(
            pdu,
            GfxPdu::MapSurfaceToOutput(map) if map.surface_id == new_surface_id
        )));
        let resized_wire =
            assert_sendable_avc444_wire_to_surface(&resized_pdus, Encoding::LUMA_AND_CHROMA);
        assert_eq!(resized_wire.surface_id, new_surface_id);
        assert!(!processor.has_pending_damage());
        let (last_luma_regions, last_chroma_regions) = processor
            .h264_encoder
            .as_ref()
            .and_then(crate::egfx::FrameEncoder::avc444_last_reference_regions_for_test)
            .expect("AVC444 region state exists after re-generation");
        let full_frame = [(0, 0, width as i32, height as i32)];
        assert_eq!(last_luma_regions, full_frame);
        assert_eq!(last_chroma_regions, full_frame);
        assert!(display_rx.try_recv().is_err());
    }
}
