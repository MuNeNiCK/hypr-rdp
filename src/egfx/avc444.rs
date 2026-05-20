use anyhow::{bail, Result};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use super::h264::{
    annex_b_nal_types, avc444_h264_encoder_options, is_h264_keyframe, EncodedH264, H264Encoder,
};
use super::H264RateControl;

pub struct Avc444EncodedFrame {
    pub encoding: Avc444FrameEncoding,
    pub stream1: Vec<u8>,
    pub stream2: Vec<u8>,
    pub stream1_regions: Vec<(i32, i32, i32, i32)>,
    pub stream2_regions: Vec<(i32, i32, i32, i32)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Avc444FrameEncoding {
    LumaAndChroma,
    Luma,
    Chroma,
}

const DEFAULT_AVC444_MIN_CHROMA_INTERVAL: u32 = 0;

pub struct Avc444Encoder {
    encoder: H264Encoder,
    width: usize,
    height: usize,
    y444: Vec<u8>,
    main_u: Vec<u8>,
    main_v: Vec<u8>,
    aux_y: Vec<u8>,
    aux_u: Vec<u8>,
    aux_v: Vec<u8>,
    luma_reference: Option<Yuv420Reference>,
    chroma_reference: Option<Yuv420Reference>,
    last_chroma_encoded: bool,
    last_luma_reference_regions: Regions,
    last_chroma_reference_regions: Regions,
    frame_index: u64,
    frames_since_chroma: u32,
    force_chroma_on_next_frame: bool,
    perf_stats: Avc444PerfStats,
}

impl Avc444Encoder {
    pub fn new(
        width: u32,
        height: u32,
        bitrate: u32,
        fps: u32,
        qp: u8,
        rate_control: H264RateControl,
    ) -> Result<Self> {
        if width == 0 || height == 0 || !width.is_multiple_of(4) || !height.is_multiple_of(2) {
            bail!(
                "AVC444v2 dimensions must be non-zero, width must be divisible by 4, and height must be even: {}x{}",
                width,
                height
            );
        }

        let encoder = H264Encoder::new_with_options(
            width,
            height,
            bitrate,
            fps,
            qp,
            rate_control,
            avc444_h264_encoder_options(),
        )?;

        let w = width as usize;
        let h = height as usize;
        let y_len = w * h;
        let uv_len = (w / 2) * (h / 2);

        Ok(Self {
            encoder,
            width: w,
            height: h,
            y444: vec![0; y_len],
            main_u: vec![128; uv_len],
            main_v: vec![128; uv_len],
            aux_y: vec![128; y_len],
            aux_u: vec![128; uv_len],
            aux_v: vec![128; uv_len],
            luma_reference: None,
            chroma_reference: None,
            last_chroma_encoded: false,
            last_luma_reference_regions: Vec::new(),
            last_chroma_reference_regions: Vec::new(),
            frame_index: 0,
            frames_since_chroma: DEFAULT_AVC444_MIN_CHROMA_INTERVAL,
            force_chroma_on_next_frame: true,
            perf_stats: Avc444PerfStats::new(),
        })
    }

    pub fn encode(
        &mut self,
        bgra: &[u8],
        stride: usize,
        candidate_regions: &[(i32, i32, i32, i32)],
    ) -> Result<Avc444EncodedFrame> {
        anyhow::ensure!(
            bgra.len() >= self.height * stride,
            "BGRA buffer too small: {} < {}",
            bgra.len(),
            self.height * stride,
        );
        let total_start = Instant::now();
        let candidate_regions =
            align_avc444_v2_protocol_regions(self.width, self.height, candidate_regions);
        let force_full_frame = std::env::var_os("HYPR_RDP_AVC444_FULL_FRAME").is_some()
            || self.force_chroma_on_next_frame;
        if self.force_chroma_on_next_frame {
            self.force_chroma_on_next_frame = false;
        }
        let candidate_regions = if force_full_frame {
            vec![(0, 0, self.width as i32, self.height as i32)]
        } else {
            candidate_regions
        };
        bgra_to_avc444_v2_plane_regions(
            self.width,
            self.height,
            bgra,
            stride,
            &candidate_regions,
            &mut self.y444,
            &mut self.main_u,
            &mut self.main_v,
            &mut self.aux_y,
            &mut self.aux_u,
            &mut self.aux_v,
        );
        let convert_elapsed = total_start.elapsed();

        let mut luma_regions = detect_yuv420_regions(
            self.width,
            self.height,
            &self.y444,
            &self.main_u,
            &self.main_v,
            self.luma_reference.as_ref(),
            &candidate_regions,
        );
        let (detected_chroma_regions, detected_chroma_protocol_regions) =
            detect_avc444_v2_chroma_regions(
                self.width,
                self.height,
                &self.aux_y,
                &self.aux_u,
                &self.aux_v,
                self.chroma_reference.as_ref(),
                &candidate_regions,
            );
        if force_full_frame {
            luma_regions = vec![(0, 0, self.width as i32, self.height as i32)];
        }
        let detected_chroma_regions = if force_full_frame {
            vec![(0, 0, self.width as i32, self.height as i32)]
        } else {
            detected_chroma_regions
        };
        let detected_chroma_protocol_regions = if force_full_frame {
            vec![(0, 0, self.width as i32, self.height as i32)]
        } else {
            detected_chroma_protocol_regions
        };
        let chroma_changed = !detected_chroma_protocol_regions.is_empty();
        let encode_chroma = should_encode_avc444_v2_chroma(
            chroma_changed,
            force_full_frame,
            self.frames_since_chroma,
            avc444_min_chroma_interval(),
        );
        let (chroma_regions, chroma_protocol_regions) = if encode_chroma {
            (detected_chroma_regions, detected_chroma_protocol_regions)
        } else {
            (Vec::new(), Vec::new())
        };

        if luma_regions.is_empty() && chroma_protocol_regions.is_empty() {
            if chroma_changed {
                self.frames_since_chroma = self.frames_since_chroma.saturating_add(1);
            }
            self.last_chroma_encoded = false;
            self.last_luma_reference_regions.clear();
            self.last_chroma_reference_regions.clear();
            return Ok(Avc444EncodedFrame {
                encoding: Avc444FrameEncoding::Luma,
                stream1: Vec::new(),
                stream2: Vec::new(),
                stream1_regions: luma_regions,
                stream2_regions: chroma_protocol_regions,
            });
        }

        self.last_luma_reference_regions = luma_regions.clone();
        self.last_chroma_reference_regions = chroma_regions;

        let (
            encoding,
            stream1,
            stream2,
            stream1_regions,
            stream2_regions,
            stream1_encode_elapsed,
            stream2_encode_elapsed,
        ) = if !luma_regions.is_empty() {
            let stream1_encode_start = Instant::now();
            let luma = self
                .encoder
                .encode_yuv420_raw(&self.y444, &self.main_u, &self.main_v)?;
            let stream1_encode_elapsed = stream1_encode_start.elapsed();
            if luma.data.is_empty() {
                self.last_chroma_encoded = false;
                self.last_luma_reference_regions.clear();
                self.last_chroma_reference_regions.clear();
                return Ok(Avc444EncodedFrame {
                    encoding: Avc444FrameEncoding::Luma,
                    stream1: Vec::new(),
                    stream2: Vec::new(),
                    stream1_regions: Vec::new(),
                    stream2_regions: Vec::new(),
                });
            }

            if chroma_protocol_regions.is_empty() {
                self.frames_since_chroma = self.frames_since_chroma.saturating_add(1);
                self.last_chroma_encoded = false;
                self.debug_log_frame(&luma, &EncodedH264::empty(), &luma_regions, &[]);
                (
                    Avc444FrameEncoding::Luma,
                    luma,
                    EncodedH264::empty(),
                    luma_regions,
                    Vec::new(),
                    stream1_encode_elapsed,
                    Duration::ZERO,
                )
            } else {
                let stream2_encode_start = Instant::now();
                let chroma =
                    self.encoder
                        .encode_yuv420_raw(&self.aux_y, &self.aux_u, &self.aux_v)?;
                let stream2_encode_elapsed = stream2_encode_start.elapsed();
                self.last_chroma_encoded = !chroma.data.is_empty();
                if self.last_chroma_encoded {
                    self.frames_since_chroma = 0;
                } else {
                    self.frames_since_chroma = self.frames_since_chroma.saturating_add(1);
                }
                self.debug_log_frame(&luma, &chroma, &luma_regions, &chroma_protocol_regions);
                (
                    Avc444FrameEncoding::LumaAndChroma,
                    luma,
                    chroma,
                    luma_regions,
                    chroma_protocol_regions,
                    stream1_encode_elapsed,
                    stream2_encode_elapsed,
                )
            }
        } else {
            let stream1_encode_start = Instant::now();
            let chroma = self
                .encoder
                .encode_yuv420_raw(&self.aux_y, &self.aux_u, &self.aux_v)?;
            let stream1_encode_elapsed = stream1_encode_start.elapsed();
            self.last_chroma_encoded = !chroma.data.is_empty();
            if self.last_chroma_encoded {
                self.frames_since_chroma = 0;
            } else {
                self.frames_since_chroma = self.frames_since_chroma.saturating_add(1);
            }
            self.debug_log_frame(
                &EncodedH264::empty(),
                &chroma,
                &[],
                &chroma_protocol_regions,
            );
            (
                Avc444FrameEncoding::Chroma,
                chroma,
                EncodedH264::empty(),
                chroma_protocol_regions,
                Vec::new(),
                stream1_encode_elapsed,
                Duration::ZERO,
            )
        };

        let stream1_is_keyframe = is_h264_keyframe(stream1.frame_type);
        let stream2_is_keyframe = is_h264_keyframe(stream2.frame_type);

        let frame = if stream1.data.is_empty() {
            self.last_chroma_encoded = false;
            self.last_luma_reference_regions.clear();
            self.last_chroma_reference_regions.clear();
            Avc444EncodedFrame {
                encoding: Avc444FrameEncoding::Luma,
                stream1: Vec::new(),
                stream2: Vec::new(),
                stream1_regions: Vec::new(),
                stream2_regions: Vec::new(),
            }
        } else if encoding == Avc444FrameEncoding::LumaAndChroma && stream2.data.is_empty() {
            self.last_chroma_encoded = false;
            self.force_chroma_on_next_frame = true;
            self.last_chroma_reference_regions.clear();
            Avc444EncodedFrame {
                encoding: Avc444FrameEncoding::LumaAndChroma,
                stream1: Vec::new(),
                stream2: Vec::new(),
                stream1_regions: Vec::new(),
                stream2_regions: Vec::new(),
            }
        } else {
            let stream2_regions = if stream2.data.is_empty() {
                Vec::new()
            } else {
                stream2_regions
            };
            Avc444EncodedFrame {
                encoding,
                stream1: stream1.data,
                stream2: stream2.data,
                stream1_regions,
                stream2_regions,
            }
        };

        let total_elapsed = total_start.elapsed();
        self.perf_stats.record(
            frame.encoding,
            candidate_regions.len(),
            frame.stream1_regions.len(),
            frame.stream2_regions.len(),
            frame.stream1.len(),
            frame.stream2.len(),
            convert_elapsed,
            stream1_encode_elapsed,
            stream2_encode_elapsed,
            stream1_is_keyframe,
            stream2_is_keyframe,
            total_elapsed,
        );

        if std::env::var_os("HYPR_RDP_AVC444_TRACE").is_some() {
            tracing::trace!(
                frame = self.frame_index,
                convert_ms = convert_elapsed.as_secs_f64() * 1000.0,
                total_ms = total_elapsed.as_secs_f64() * 1000.0,
                candidate_regions = candidate_regions.len(),
                encoding = ?frame.encoding,
                stream1_bytes = frame.stream1.len(),
                stream2_bytes = frame.stream2.len(),
                "AVC444v2 encode timing"
            );
        }

        self.frame_index = self.frame_index.wrapping_add(1);
        Ok(frame)
    }

    pub fn commit_reference(&mut self) {
        update_yuv420_reference_regions(
            &mut self.luma_reference,
            self.width,
            self.height,
            &self.y444,
            &self.main_u,
            &self.main_v,
            &self.last_luma_reference_regions,
        );
        if self.last_chroma_encoded {
            update_yuv420_reference_regions(
                &mut self.chroma_reference,
                self.width,
                self.height,
                &self.aux_y,
                &self.aux_u,
                &self.aux_v,
                &self.last_chroma_reference_regions,
            );
        }
    }

    #[cfg(test)]
    pub(crate) fn luma_reference_y_for_test(&self) -> Option<&[u8]> {
        self.luma_reference
            .as_ref()
            .map(|reference| reference.y.as_slice())
    }

    #[cfg(test)]
    pub(crate) fn last_reference_regions_for_test(&self) -> (&[Region], &[Region]) {
        (
            &self.last_luma_reference_regions,
            &self.last_chroma_reference_regions,
        )
    }

    pub fn force_idr(&mut self) {
        self.encoder.force_idr();
        self.force_chroma_on_next_frame = true;
    }

    fn debug_log_frame(
        &self,
        luma: &EncodedH264,
        chroma: &EncodedH264,
        luma_regions: &[Region],
        chroma_regions: &[Region],
    ) {
        if self.frame_index >= 8 && std::env::var_os("HYPR_RDP_AVC444_TRACE").is_none() {
            return;
        }

        tracing::trace!(
            frame = self.frame_index,
            luma_bytes = luma.data.len(),
            chroma_bytes = chroma.data.len(),
            luma_frame_type = ?luma.frame_type,
            chroma_frame_type = ?chroma.frame_type,
            luma_nals = ?annex_b_nal_types(&luma.data),
            chroma_nals = ?annex_b_nal_types(&chroma.data),
            luma_regions = luma_regions.len(),
            chroma_regions = chroma_regions.len(),
            "AVC444v2 encoded frame"
        );
    }
}

struct Avc444PerfStats {
    window_start: Instant,
    frames: u64,
    luma_and_chroma: u64,
    luma_only: u64,
    chroma_only: u64,
    stream1_bytes: u64,
    stream2_bytes: u64,
    candidate_regions: u64,
    stream1_regions: u64,
    stream2_regions: u64,
    convert_us_total: u128,
    stream1_encode_us_total: u128,
    stream2_encode_us_total: u128,
    total_us_total: u128,
    stream1_keyframes: u64,
    stream2_keyframes: u64,
}

impl Avc444PerfStats {
    fn new() -> Self {
        Self {
            window_start: Instant::now(),
            frames: 0,
            luma_and_chroma: 0,
            luma_only: 0,
            chroma_only: 0,
            stream1_bytes: 0,
            stream2_bytes: 0,
            candidate_regions: 0,
            stream1_regions: 0,
            stream2_regions: 0,
            convert_us_total: 0,
            stream1_encode_us_total: 0,
            stream2_encode_us_total: 0,
            total_us_total: 0,
            stream1_keyframes: 0,
            stream2_keyframes: 0,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn record(
        &mut self,
        encoding: Avc444FrameEncoding,
        candidate_regions: usize,
        stream1_regions: usize,
        stream2_regions: usize,
        stream1_bytes: usize,
        stream2_bytes: usize,
        convert_elapsed: Duration,
        stream1_encode_elapsed: Duration,
        stream2_encode_elapsed: Duration,
        stream1_is_keyframe: bool,
        stream2_is_keyframe: bool,
        total_elapsed: Duration,
    ) {
        if !avc444_perf_logging_enabled() {
            return;
        }

        self.frames = self.frames.saturating_add(1);
        match encoding {
            Avc444FrameEncoding::LumaAndChroma => {
                self.luma_and_chroma = self.luma_and_chroma.saturating_add(1);
            }
            Avc444FrameEncoding::Luma => {
                self.luma_only = self.luma_only.saturating_add(1);
            }
            Avc444FrameEncoding::Chroma => {
                self.chroma_only = self.chroma_only.saturating_add(1);
            }
        }
        self.stream1_bytes = self.stream1_bytes.saturating_add(stream1_bytes as u64);
        self.stream2_bytes = self.stream2_bytes.saturating_add(stream2_bytes as u64);
        self.candidate_regions = self
            .candidate_regions
            .saturating_add(candidate_regions as u64);
        self.stream1_regions = self.stream1_regions.saturating_add(stream1_regions as u64);
        self.stream2_regions = self.stream2_regions.saturating_add(stream2_regions as u64);
        self.convert_us_total = self
            .convert_us_total
            .saturating_add(convert_elapsed.as_micros());
        self.stream1_encode_us_total = self
            .stream1_encode_us_total
            .saturating_add(stream1_encode_elapsed.as_micros());
        self.stream2_encode_us_total = self
            .stream2_encode_us_total
            .saturating_add(stream2_encode_elapsed.as_micros());
        self.total_us_total = self
            .total_us_total
            .saturating_add(total_elapsed.as_micros());
        if stream1_is_keyframe {
            self.stream1_keyframes = self.stream1_keyframes.saturating_add(1);
        }
        if stream2_is_keyframe {
            self.stream2_keyframes = self.stream2_keyframes.saturating_add(1);
        }

        let elapsed = self.window_start.elapsed();
        if elapsed < Duration::from_secs(1) {
            return;
        }

        let frames = self.frames.max(1);
        let seconds = elapsed.as_secs_f64();
        tracing::info!(
            target: "hypr_rdp::avc444_perf",
            fps = self.frames as f64 / seconds,
            avg_convert_ms = self.convert_us_total as f64 / frames as f64 / 1000.0,
            avg_stream1_encode_ms =
                self.stream1_encode_us_total as f64 / frames as f64 / 1000.0,
            avg_stream2_encode_ms =
                self.stream2_encode_us_total as f64 / frames as f64 / 1000.0,
            avg_total_ms = self.total_us_total as f64 / frames as f64 / 1000.0,
            avg_stream1_kb = self.stream1_bytes as f64 / frames as f64 / 1024.0,
            avg_stream2_kb = self.stream2_bytes as f64 / frames as f64 / 1024.0,
            avg_candidate_regions = self.candidate_regions as f64 / frames as f64,
            avg_stream1_regions = self.stream1_regions as f64 / frames as f64,
            avg_stream2_regions = self.stream2_regions as f64 / frames as f64,
            luma_and_chroma = self.luma_and_chroma,
            luma_only = self.luma_only,
            chroma_only = self.chroma_only,
            stream1_keyframes = self.stream1_keyframes,
            stream2_keyframes = self.stream2_keyframes,
            "AVC444v2 perf"
        );

        *self = Self::new();
    }
}

fn avc444_perf_logging_enabled() -> bool {
    avc444_perf_logging_enabled_with(|name| std::env::var_os(name).is_some())
}

fn avc444_perf_logging_enabled_with(mut is_set: impl FnMut(&str) -> bool) -> bool {
    is_set("HYPR_RDP_AVC444_PERF")
}

fn avc444_min_chroma_interval() -> u32 {
    std::env::var("HYPR_RDP_AVC444_MIN_CHROMA_INTERVAL")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .map(|value| value.min(120))
        .unwrap_or(DEFAULT_AVC444_MIN_CHROMA_INTERVAL)
}

fn should_encode_avc444_v2_chroma(
    chroma_changed: bool,
    force_full_frame: bool,
    frames_since_chroma: u32,
    min_chroma_interval: u32,
) -> bool {
    chroma_changed && (force_full_frame || frames_since_chroma >= min_chroma_interval)
}

struct Yuv420Reference {
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

type Region = (i32, i32, i32, i32);
type Regions = Vec<Region>;

fn copy_yuv420_reference(
    reference: &mut Option<Yuv420Reference>,
    width: usize,
    height: usize,
    y: &[u8],
    u: &[u8],
    v: &[u8],
) {
    let y_len = width * height;
    let uv_len = (width / 2) * (height / 2);
    let reference = reference.get_or_insert_with(|| Yuv420Reference {
        y: vec![0; y_len],
        u: vec![0; uv_len],
        v: vec![0; uv_len],
    });
    reference.y[..y_len].copy_from_slice(&y[..y_len]);
    reference.u[..uv_len].copy_from_slice(&u[..uv_len]);
    reference.v[..uv_len].copy_from_slice(&v[..uv_len]);
}

fn update_yuv420_reference_regions(
    reference: &mut Option<Yuv420Reference>,
    width: usize,
    height: usize,
    y: &[u8],
    u: &[u8],
    v: &[u8],
    regions: &[(i32, i32, i32, i32)],
) {
    if reference.is_none() {
        copy_yuv420_reference(reference, width, height, y, u, v);
        return;
    }

    let Some(reference) = reference.as_mut() else {
        return;
    };

    for &(x, y_pos, w, h) in regions {
        let Some((left, top, right, bottom)) = clamp_region(x, y_pos, w, h, width, height) else {
            continue;
        };

        copy_plane_region(width, y, &mut reference.y, left, top, right, bottom);

        let chroma_width = width / 2;
        let chroma_left = left / 2;
        let chroma_top = top / 2;
        let chroma_right = right.div_ceil(2);
        let chroma_bottom = bottom.div_ceil(2);
        copy_plane_region(
            chroma_width,
            u,
            &mut reference.u,
            chroma_left,
            chroma_top,
            chroma_right,
            chroma_bottom,
        );
        copy_plane_region(
            chroma_width,
            v,
            &mut reference.v,
            chroma_left,
            chroma_top,
            chroma_right,
            chroma_bottom,
        );
    }
}

fn copy_plane_region(
    stride: usize,
    src: &[u8],
    dst: &mut [u8],
    left: usize,
    top: usize,
    right: usize,
    bottom: usize,
) {
    let width = right.saturating_sub(left);
    if width == 0 {
        return;
    }

    for row in top..bottom {
        let start = row * stride + left;
        let end = start + width;
        dst[start..end].copy_from_slice(&src[start..end]);
    }
}

#[allow(clippy::too_many_arguments)]
fn bgra_to_avc444_v2_plane_regions(
    width: usize,
    height: usize,
    bgra: &[u8],
    bgra_stride: usize,
    regions: &[(i32, i32, i32, i32)],
    y444: &mut [u8],
    main_u: &mut [u8],
    main_v: &mut [u8],
    aux_y: &mut [u8],
    aux_u: &mut [u8],
    aux_v: &mut [u8],
) {
    let chroma_w = width / 2;
    let quarter_w = width / 4;
    let tables = &*BGRA_TO_YUV_TABLES;

    for &(x, y, w, h) in regions {
        let Some((left, top, right, bottom)) = clamp_region(x, y, w, h, width, height) else {
            continue;
        };
        let left = align_down(left as i32, 4) as usize;
        let top = align_down(top as i32, 2) as usize;
        let right = align_up(right as i32, 4).clamp(0, width as i32) as usize;
        let bottom = align_up(bottom as i32, 2).clamp(0, height as i32) as usize;

        for cy in (top / 2)..(bottom / 2) {
            let even_y = cy * 2;
            let odd_y = even_y + 1;
            let even_row = &bgra[even_y * bgra_stride..(even_y + 1) * bgra_stride];
            let odd_row = &bgra[odd_y * bgra_stride..(odd_y + 1) * bgra_stride];
            let y_even_base = even_y * width;
            let y_odd_base = odd_y * width;
            let main_base = cy * chroma_w;

            for cx in (left / 2)..(right / 2) {
                let x = cx * 2;
                let (ya, ua, va) = bgra_row_pixel_to_yuv(tables, even_row, x);
                let (yb, ub, vb) = bgra_row_pixel_to_yuv(tables, even_row, x + 1);
                let (yc, uc, vc) = bgra_row_pixel_to_yuv(tables, odd_row, x);
                let (yd, ud, vd) = bgra_row_pixel_to_yuv(tables, odd_row, x + 1);

                y444[y_even_base + x] = ya;
                y444[y_even_base + x + 1] = yb;
                y444[y_odd_base + x] = yc;
                y444[y_odd_base + x + 1] = yd;

                let dst = main_base + cx;
                main_u[dst] = avg4_floor(ua, ub, uc, ud);
                main_v[dst] = avg4_floor(va, vb, vc, vd);

                let aux_y_even = y_even_base + cx;
                let aux_y_odd = y_odd_base + cx;
                aux_y[aux_y_even] = ub;
                aux_y[aux_y_even + chroma_w] = vb;
                aux_y[aux_y_odd] = ud;
                aux_y[aux_y_odd + chroma_w] = vd;

                let aux_x = cx / 2;
                if cx.is_multiple_of(2) {
                    aux_u[main_base + aux_x] = uc;
                    aux_u[main_base + aux_x + quarter_w] = vc;
                } else {
                    aux_v[main_base + aux_x] = uc;
                    aux_v[main_base + aux_x + quarter_w] = vc;
                }
            }
        }
    }
}

#[inline(always)]
fn bgra_row_pixel_to_yuv(tables: &BgraToYuvTables, row: &[u8], x: usize) -> (u8, u8, u8) {
    let offset = x * 4;
    let b = row[offset] as usize;
    let g = row[offset + 1] as usize;
    let r = row[offset + 2] as usize;

    bgra_components_to_yuv(tables, r, g, b)
}

#[cfg(test)]
fn bgra_pixel_to_yuv(
    tables: &BgraToYuvTables,
    bgra: &[u8],
    stride: usize,
    x: usize,
    y: usize,
) -> (u8, u8, u8) {
    let offset = y * stride + x * 4;
    let b = bgra[offset] as usize;
    let g = bgra[offset + 1] as usize;
    let r = bgra[offset + 2] as usize;

    bgra_components_to_yuv(tables, r, g, b)
}

#[inline(always)]
fn bgra_components_to_yuv(tables: &BgraToYuvTables, r: usize, g: usize, b: usize) -> (u8, u8, u8) {
    let y = ((tables.y_r[r] + tables.y_g[g] + tables.y_b[b]) >> 8).clamp(0, 255) as u8;
    let u = (((tables.u_r[r] + tables.u_g[g] + tables.u_b[b]) >> 8) + 128).clamp(0, 255) as u8;
    let v = (((tables.v_r[r] + tables.v_g[g] + tables.v_b[b]) >> 8) + 128).clamp(0, 255) as u8;

    (y, u, v)
}

static BGRA_TO_YUV_TABLES: LazyLock<BgraToYuvTables> = LazyLock::new(BgraToYuvTables::new);

struct BgraToYuvTables {
    y_r: [i32; 256],
    y_g: [i32; 256],
    y_b: [i32; 256],
    u_r: [i32; 256],
    u_g: [i32; 256],
    u_b: [i32; 256],
    v_r: [i32; 256],
    v_g: [i32; 256],
    v_b: [i32; 256],
}

impl BgraToYuvTables {
    fn new() -> Self {
        let mut tables = Self {
            y_r: [0; 256],
            y_g: [0; 256],
            y_b: [0; 256],
            u_r: [0; 256],
            u_g: [0; 256],
            u_b: [0; 256],
            v_r: [0; 256],
            v_g: [0; 256],
            v_b: [0; 256],
        };

        for value in 0..256 {
            let c = value as i32;
            tables.y_r[value] = 54 * c;
            tables.y_g[value] = 183 * c;
            tables.y_b[value] = 18 * c;
            tables.u_r[value] = -29 * c;
            tables.u_g[value] = -99 * c;
            tables.u_b[value] = 128 * c;
            tables.v_r[value] = 128 * c;
            tables.v_g[value] = -116 * c;
            tables.v_b[value] = -12 * c;
        }

        tables
    }
}

fn avg4_floor(a: u8, b: u8, c: u8, d: u8) -> u8 {
    ((u32::from(a) + u32::from(b) + u32::from(c) + u32::from(d)) / 4) as u8
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn pack_avc444_v2_planes(
    width: usize,
    height: usize,
    u444: &[u8],
    v444: &[u8],
    main_u: &mut [u8],
    main_v: &mut [u8],
    aux_y: &mut [u8],
    aux_u: &mut [u8],
    aux_v: &mut [u8],
) {
    let chroma_w = width / 2;
    let chroma_h = height / 2;
    let quarter_w = width / 4;

    for cy in 0..chroma_h {
        for cx in 0..chroma_w {
            let x = cx * 2;
            let y = cy * 2;
            let dst = cy * chroma_w + cx;
            main_u[dst] = avg_2x2_floor(u444, width, x, y);
            main_v[dst] = avg_2x2_floor(v444, width, x, y);
        }
    }

    aux_y.fill(128);
    aux_u.fill(128);
    aux_v.fill(128);

    for y in 0..height {
        for x in (1..width).step_by(2) {
            let dst_x = x / 2;
            let src = y * width + x;
            let dst = y * width + dst_x;
            aux_y[dst] = u444[src];
            aux_y[dst + chroma_w] = v444[src];
        }
    }

    for cy in 0..chroma_h {
        let src_y = cy * 2 + 1;
        for x in (0..width).step_by(4) {
            let dst_x = x / 4;
            let src = src_y * width + x;
            let dst = cy * chroma_w + dst_x;
            aux_u[dst] = u444[src];
            aux_u[dst + quarter_w] = v444[src];

            if x + 2 < width {
                let src = src + 2;
                aux_v[dst] = u444[src];
                aux_v[dst + quarter_w] = v444[src];
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
fn bgra_to_avc444_v2_planes(
    width: usize,
    height: usize,
    bgra: &[u8],
    bgra_stride: usize,
    y444: &mut [u8],
    main_u: &mut [u8],
    main_v: &mut [u8],
    aux_y: &mut [u8],
    aux_u: &mut [u8],
    aux_v: &mut [u8],
) {
    aux_y.fill(128);
    aux_u.fill(128);
    aux_v.fill(128);
    bgra_to_avc444_v2_plane_regions(
        width,
        height,
        bgra,
        bgra_stride,
        &[(0, 0, width as i32, height as i32)],
        y444,
        main_u,
        main_v,
        aux_y,
        aux_u,
        aux_v,
    );
}

#[cfg(test)]
fn avg_2x2_floor(plane: &[u8], stride: usize, x: usize, y: usize) -> u8 {
    let a = u32::from(plane[y * stride + x]);
    let b = u32::from(plane[y * stride + x + 1]);
    let c = u32::from(plane[(y + 1) * stride + x]);
    let d = u32::from(plane[(y + 1) * stride + x + 1]);
    ((a + b + c + d) / 4) as u8
}

fn detect_yuv420_regions(
    width: usize,
    height: usize,
    y: &[u8],
    u: &[u8],
    v: &[u8],
    reference: Option<&Yuv420Reference>,
    candidate_regions: &[(i32, i32, i32, i32)],
) -> Vec<(i32, i32, i32, i32)> {
    let Some(reference) = reference else {
        return vec![(0, 0, width as i32, height as i32)];
    };
    if candidate_regions.is_empty() {
        return Vec::new();
    }

    let mut regions = Vec::new();
    for &(x, y_pos, w, h) in candidate_regions {
        let Some((left, top, right, bottom)) = clamp_region(x, y_pos, w, h, width, height) else {
            continue;
        };
        let mut tile_y = top;
        while tile_y < bottom {
            let tile_bottom = (tile_y + 64).min(bottom);
            let mut tile_x = left;
            while tile_x < right {
                let tile_right = (tile_x + 64).min(right);
                if yuv420_tile_changed(
                    width,
                    y,
                    u,
                    v,
                    reference,
                    tile_x,
                    tile_y,
                    tile_right,
                    tile_bottom,
                ) {
                    merge_region(
                        &mut regions,
                        (
                            tile_x as i32,
                            tile_y as i32,
                            (tile_right - tile_x) as i32,
                            (tile_bottom - tile_y) as i32,
                        ),
                    );
                }
                tile_x += 64;
            }
            tile_y += 64;
        }
    }
    regions
}

fn detect_avc444_v2_chroma_regions(
    width: usize,
    height: usize,
    y: &[u8],
    u: &[u8],
    v: &[u8],
    reference: Option<&Yuv420Reference>,
    protocol_candidates: &[(i32, i32, i32, i32)],
) -> (Regions, Regions) {
    if reference.is_none() {
        return (
            vec![(0, 0, width as i32, height as i32)],
            vec![(0, 0, width as i32, height as i32)],
        );
    }

    let mut packed_regions = Vec::new();
    let mut protocol_regions = Vec::new();

    for &candidate in protocol_candidates {
        let packed_candidates = avc444_v2_chroma_packed_candidates(width, height, candidate);
        if packed_candidates.is_empty() {
            continue;
        }

        let changed = detect_yuv420_regions(width, height, y, u, v, reference, &packed_candidates);
        if changed.is_empty() {
            continue;
        }

        for region in changed {
            merge_region(&mut packed_regions, region);
        }
        if let Some((left, top, right, bottom)) = clamp_region(
            candidate.0,
            candidate.1,
            candidate.2,
            candidate.3,
            width,
            height,
        ) {
            merge_region(
                &mut protocol_regions,
                (
                    left as i32,
                    top as i32,
                    (right - left) as i32,
                    (bottom - top) as i32,
                ),
            );
        }
    }

    (packed_regions, protocol_regions)
}

fn align_avc444_v2_protocol_regions(
    width: usize,
    height: usize,
    regions: &[(i32, i32, i32, i32)],
) -> Vec<(i32, i32, i32, i32)> {
    align_regions(width, height, regions, 4, 2)
}

fn align_regions(
    width: usize,
    height: usize,
    regions: &[(i32, i32, i32, i32)],
    x_alignment: i32,
    y_alignment: i32,
) -> Vec<(i32, i32, i32, i32)> {
    let mut aligned = Vec::new();
    let Ok(width) = i32::try_from(width) else {
        return aligned;
    };
    let Ok(height) = i32::try_from(height) else {
        return aligned;
    };

    for &(x, y, w, h) in regions {
        if w <= 0 || h <= 0 {
            continue;
        }

        let left = align_down(x.clamp(0, width), x_alignment);
        let top = align_down(y.clamp(0, height), y_alignment);
        let right = align_up(x.saturating_add(w).clamp(0, width), x_alignment).clamp(0, width);
        let bottom = align_up(y.saturating_add(h).clamp(0, height), y_alignment).clamp(0, height);

        if right > left && bottom > top {
            merge_region(&mut aligned, (left, top, right - left, bottom - top));
        }
    }

    aligned
}

fn align_down(value: i32, alignment: i32) -> i32 {
    value - value.rem_euclid(alignment)
}

fn align_up(value: i32, alignment: i32) -> i32 {
    let rem = value.rem_euclid(alignment);
    if rem == 0 {
        value
    } else {
        value.saturating_add(alignment - rem)
    }
}

fn avc444_v2_chroma_packed_candidates(
    width: usize,
    height: usize,
    region: (i32, i32, i32, i32),
) -> Vec<(i32, i32, i32, i32)> {
    let Some((left, top, right, bottom)) =
        clamp_region(region.0, region.1, region.2, region.3, width, height)
    else {
        return Vec::new();
    };

    let packed_left = left / 2;
    let packed_right = right.div_ceil(2);
    let packed_width = packed_right.saturating_sub(packed_left);
    if packed_width == 0 {
        return Vec::new();
    }

    let half_width = width / 2;
    vec![
        (
            packed_left as i32,
            top as i32,
            packed_width as i32,
            (bottom - top) as i32,
        ),
        (
            (half_width + packed_left) as i32,
            top as i32,
            packed_width as i32,
            (bottom - top) as i32,
        ),
    ]
}

fn clamp_region(
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    width: usize,
    height: usize,
) -> Option<(usize, usize, usize, usize)> {
    if w <= 0 || h <= 0 {
        return None;
    }
    let width = i32::try_from(width).ok()?;
    let height = i32::try_from(height).ok()?;
    let left = x.clamp(0, width);
    let top = y.clamp(0, height);
    let right = x.saturating_add(w).clamp(0, width);
    let bottom = y.saturating_add(h).clamp(0, height);
    (right > left && bottom > top).then_some((
        left as usize,
        top as usize,
        right as usize,
        bottom as usize,
    ))
}

#[allow(clippy::too_many_arguments)]
fn yuv420_tile_changed(
    width: usize,
    y: &[u8],
    u: &[u8],
    v: &[u8],
    reference: &Yuv420Reference,
    left: usize,
    top: usize,
    right: usize,
    bottom: usize,
) -> bool {
    for row in top..bottom {
        let start = row * width + left;
        let end = row * width + right;
        if y[start..end] != reference.y[start..end] {
            return true;
        }
    }

    let chroma_width = width / 2;
    let chroma_left = left / 2;
    let chroma_right = right.div_ceil(2);
    let chroma_top = top / 2;
    let chroma_bottom = bottom.div_ceil(2);
    for row in chroma_top..chroma_bottom {
        let start = row * chroma_width + chroma_left;
        let end = row * chroma_width + chroma_right;
        if u[start..end] != reference.u[start..end] || v[start..end] != reference.v[start..end] {
            return true;
        }
    }

    false
}

fn merge_region(regions: &mut Vec<(i32, i32, i32, i32)>, region: (i32, i32, i32, i32)) {
    let mut merged = region;
    let mut index = 0;
    while index < regions.len() {
        if regions_overlap_or_touch(regions[index], merged) {
            merged = union_region(regions[index], merged);
            regions.swap_remove(index);
        } else {
            index += 1;
        }
    }
    regions.push(merged);
}

fn regions_overlap_or_touch(a: (i32, i32, i32, i32), b: (i32, i32, i32, i32)) -> bool {
    let a_right = a.0.saturating_add(a.2);
    let a_bottom = a.1.saturating_add(a.3);
    let b_right = b.0.saturating_add(b.2);
    let b_bottom = b.1.saturating_add(b.3);
    a.0 <= b_right && b.0 <= a_right && a.1 <= b_bottom && b.1 <= a_bottom
}

fn union_region(a: (i32, i32, i32, i32), b: (i32, i32, i32, i32)) -> (i32, i32, i32, i32) {
    let left = a.0.min(b.0);
    let top = a.1.min(b.1);
    let right = a.0.saturating_add(a.2).max(b.0.saturating_add(b.2));
    let bottom = a.1.saturating_add(a.3).max(b.1.saturating_add(b.3));
    (left, top, right - left, bottom - top)
}

#[cfg(test)]
mod tests {
    use super::*;
    use openh264::encoder::UsageType;

    #[test]
    fn avc444_v2_encoder_options_require_non_empty_payloads() {
        let options = avc444_h264_encoder_options();

        assert!(matches!(
            options.usage_type,
            UsageType::ScreenContentRealTime
        ));
        assert!(options.scene_change_detect);
        assert!(!options.long_term_reference);
        assert!(!options.frame_skip);
    }

    #[test]
    fn avc444_v2_chroma_encoding_follows_change_detection_by_default() {
        assert!(should_encode_avc444_v2_chroma(
            true,
            false,
            0,
            DEFAULT_AVC444_MIN_CHROMA_INTERVAL
        ));
        assert!(!should_encode_avc444_v2_chroma(
            false,
            false,
            0,
            DEFAULT_AVC444_MIN_CHROMA_INTERVAL
        ));
    }

    #[test]
    fn avc444_v2_chroma_encoding_can_be_rate_limited() {
        assert!(should_encode_avc444_v2_chroma(true, true, 0, 10));
        assert!(!should_encode_avc444_v2_chroma(false, true, 10, 10));
        assert!(!should_encode_avc444_v2_chroma(true, false, 9, 10));
        assert!(should_encode_avc444_v2_chroma(true, false, 10, 10));
        assert!(should_encode_avc444_v2_chroma(true, false, 0, 0));
    }

    #[test]
    fn bgra_to_yuv_uses_bt709_full_range_reference_points() {
        let tables = &*BGRA_TO_YUV_TABLES;
        let colors: [(u8, u8, u8); 9] = [
            (0, 0, 0),
            (255, 255, 255),
            (128, 128, 128),
            (255, 0, 0),
            (0, 255, 0),
            (0, 0, 255),
            (255, 255, 0),
            (0, 255, 255),
            (255, 0, 255),
        ];

        for (r, g, b) in colors {
            let actual =
                bgra_components_to_yuv(tables, usize::from(r), usize::from(g), usize::from(b));
            let expected = bt709_full_range_reference_yuv(r, g, b);

            assert_yuv_close(actual, expected, (r, g, b));
        }
    }

    fn bt709_full_range_reference_yuv(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
        let r = f64::from(r);
        let g = f64::from(g);
        let b = f64::from(b);
        let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        let u = -0.114_572 * r - 0.385_428 * g + 0.5 * b + 128.0;
        let v = 0.5 * r - 0.454_153 * g - 0.045_847 * b + 128.0;

        (
            y.round().clamp(0.0, 255.0) as u8,
            u.round().clamp(0.0, 255.0) as u8,
            v.round().clamp(0.0, 255.0) as u8,
        )
    }

    fn assert_yuv_close(actual: (u8, u8, u8), expected: (u8, u8, u8), rgb: (u8, u8, u8)) {
        for (channel, actual, expected) in [
            ("Y", actual.0, expected.0),
            ("U", actual.1, expected.1),
            ("V", actual.2, expected.2),
        ] {
            let diff = actual.abs_diff(expected);
            assert!(
                diff <= 2,
                "{channel} differs for RGB {rgb:?}: actual={actual} expected={expected}"
            );
        }
    }

    #[test]
    fn avc444_perf_logging_is_opt_in() {
        assert!(!avc444_perf_logging_enabled_with(|_| false));
        assert!(avc444_perf_logging_enabled_with(
            |name| name == "HYPR_RDP_AVC444_PERF"
        ));
    }

    #[test]
    fn initial_avc444_v2_frame_requires_luma_and_chroma() {
        let width = 16;
        let height = 16;
        let y = vec![0; width * height];
        let u = vec![128; (width / 2) * (height / 2)];
        let v = vec![128; (width / 2) * (height / 2)];
        let candidates = vec![(4, 4, 4, 4)];

        let luma_regions = detect_yuv420_regions(width, height, &y, &u, &v, None, &candidates);
        let (chroma_regions, chroma_protocol_regions) =
            detect_avc444_v2_chroma_regions(width, height, &y, &u, &v, None, &candidates);

        assert_eq!(luma_regions, vec![(0, 0, 16, 16)]);
        assert_eq!(chroma_regions, vec![(0, 0, 16, 16)]);
        assert_eq!(chroma_protocol_regions, vec![(0, 0, 16, 16)]);
        assert!(should_encode_avc444_v2_chroma(
            !chroma_protocol_regions.is_empty(),
            false,
            DEFAULT_AVC444_MIN_CHROMA_INTERVAL,
            DEFAULT_AVC444_MIN_CHROMA_INTERVAL
        ));
    }

    #[test]
    fn chroma_only_avc444_v2_frame_uses_chroma_as_stream1() {
        let frame = Avc444EncodedFrame {
            encoding: Avc444FrameEncoding::Chroma,
            stream1: vec![1, 2, 3, 4],
            stream2: Vec::new(),
            stream1_regions: vec![(8, 4, 4, 2)],
            stream2_regions: Vec::new(),
        };

        assert_eq!(frame.encoding, Avc444FrameEncoding::Chroma);
        assert_eq!(frame.stream1, [1, 2, 3, 4]);
        assert!(frame.stream2.is_empty());
        assert_eq!(frame.stream1_regions, vec![(8, 4, 4, 2)]);
        assert!(frame.stream2_regions.is_empty());
    }

    #[test]
    fn avc444_v2_encoder_first_frame_shape_uses_luma_and_chroma_when_available() {
        let width = 16;
        let height = 16;
        let stride = width * 4;
        let mut bgra = vec![0; stride * height];
        for y in 0..height {
            for x in 0..width {
                let offset = y * stride + x * 4;
                bgra[offset] = (x * 11 + y * 3) as u8;
                bgra[offset + 1] = (x * 5 + y * 17) as u8;
                bgra[offset + 2] = (x * 19 + y * 7) as u8;
                bgra[offset + 3] = 255;
            }
        }

        let mut encoder = match Avc444Encoder::new(
            width as u32,
            height as u32,
            1_000_000,
            30,
            23,
            H264RateControl::Cqp,
        ) {
            Ok(encoder) => encoder,
            Err(error) if format!("{error:#}").contains("libopenh264") => return,
            Err(error) => panic!("AVC444v2 encoder initialization failed: {error:#}"),
        };

        let frame = encoder
            .encode(&bgra, stride, &[(0, 0, width as i32, height as i32)])
            .expect("AVC444v2 first frame encodes");

        assert_eq!(frame.encoding, Avc444FrameEncoding::LumaAndChroma);
        assert!(!frame.stream1.is_empty());
        assert!(!frame.stream2.is_empty());
        assert_eq!(
            frame.stream1_regions,
            vec![(0, 0, width as i32, height as i32)]
        );
        assert_eq!(
            frame.stream2_regions,
            vec![(0, 0, width as i32, height as i32)]
        );
    }

    #[test]
    fn avc444_v2_vbr_first_frame_produces_sendable_luma_and_chroma_payloads() {
        let width = 16;
        let height = 16;
        let stride = width * 4;
        let mut bgra = vec![0; stride * height];
        for y in 0..height {
            for x in 0..width {
                let offset = y * stride + x * 4;
                bgra[offset] = (x * 11 + y * 3) as u8;
                bgra[offset + 1] = (x * 5 + y * 17) as u8;
                bgra[offset + 2] = (x * 19 + y * 7) as u8;
                bgra[offset + 3] = 255;
            }
        }

        let mut encoder = match Avc444Encoder::new(
            width as u32,
            height as u32,
            1_000_000,
            30,
            23,
            H264RateControl::Vbr,
        ) {
            Ok(encoder) => encoder,
            Err(error) if format!("{error:#}").contains("libopenh264") => return,
            Err(error) => panic!("AVC444v2 encoder initialization failed: {error:#}"),
        };

        let frame = encoder
            .encode(&bgra, stride, &[(0, 0, width as i32, height as i32)])
            .expect("VBR first frame encodes");

        assert_eq!(frame.encoding, Avc444FrameEncoding::LumaAndChroma);
        assert!(!frame.stream1.is_empty());
        assert!(!frame.stream2.is_empty());
        assert_eq!(
            frame.stream1_regions,
            vec![(0, 0, width as i32, height as i32)]
        );
        assert_eq!(
            frame.stream2_regions,
            vec![(0, 0, width as i32, height as i32)]
        );
    }

    fn write_bgra_pixel(bgra: &mut [u8], stride: usize, x: usize, y: usize, r: u8, g: u8, b: u8) {
        let offset = y * stride + x * 4;
        bgra[offset] = b;
        bgra[offset + 1] = g;
        bgra[offset + 2] = r;
        bgra[offset + 3] = 255;
    }

    fn gradient_bgra_frame(width: usize, height: usize, stride: usize) -> Vec<u8> {
        let mut bgra = vec![0; stride * height];
        for y in 0..height {
            for x in 0..width {
                let offset = y * stride + x * 4;
                bgra[offset] = (x * 11 + y * 3) as u8;
                bgra[offset + 1] = (x * 5 + y * 17) as u8;
                bgra[offset + 2] = (x * 19 + y * 7) as u8;
                bgra[offset + 3] = 255;
            }
        }
        bgra
    }

    fn new_test_avc444_encoder(
        width: usize,
        height: usize,
    ) -> std::result::Result<Avc444Encoder, String> {
        Avc444Encoder::new(
            width as u32,
            height as u32,
            1_000_000,
            30,
            23,
            H264RateControl::Cqp,
        )
        .map_err(|error| format!("{error:#}"))
    }

    #[test]
    fn avc444_force_idr_after_empty_output_recovers_with_full_lc0_and_stream1_idr() {
        let width = 16;
        let height = 16;
        let stride = width * 4;
        let bgra = gradient_bgra_frame(width, height, stride);
        let mut encoder = match new_test_avc444_encoder(width, height) {
            Ok(encoder) => encoder,
            Err(error) if error.contains("libopenh264") => return,
            Err(error) => panic!("AVC444v2 encoder initialization failed: {error}"),
        };

        let first = encoder
            .encode(&bgra, stride, &[(0, 0, width as i32, height as i32)])
            .expect("first frame encodes");
        assert_eq!(first.encoding, Avc444FrameEncoding::LumaAndChroma);
        encoder.commit_reference();

        let empty = encoder
            .encode(&bgra, stride, &[(0, 0, width as i32, height as i32)])
            .expect("unchanged frame encodes as no-op");
        assert!(empty.stream1.is_empty());
        assert!(empty.stream2.is_empty());
        assert!(empty.stream1_regions.is_empty());
        assert!(empty.stream2_regions.is_empty());

        encoder.force_idr();
        let recovered = encoder
            .encode(&bgra, stride, &[(0, 0, 4, 2)])
            .expect("forced recovery frame encodes");
        assert_eq!(recovered.encoding, Avc444FrameEncoding::LumaAndChroma);
        assert_eq!(
            recovered.stream1_regions,
            vec![(0, 0, width as i32, height as i32)]
        );
        assert_eq!(
            recovered.stream2_regions,
            vec![(0, 0, width as i32, height as i32)]
        );
        assert!(annex_b_nal_types(&recovered.stream1).contains(&5));
        assert!(!recovered.stream2.is_empty());
    }

    #[test]
    fn avc444_force_idr_after_chroma_only_role_switch_recovers_with_full_lc0() {
        let width = 16;
        let height = 16;
        let stride = width * 4;
        let tables = &*BGRA_TO_YUV_TABLES;
        let color_a = (0u8, 0u8, 187u8);
        let color_b = (0u8, 17u8, 17u8);
        let yuv_a = bgra_components_to_yuv(
            tables,
            usize::from(color_a.0),
            usize::from(color_a.1),
            usize::from(color_a.2),
        );
        let yuv_b = bgra_components_to_yuv(
            tables,
            usize::from(color_b.0),
            usize::from(color_b.1),
            usize::from(color_b.2),
        );
        assert_eq!(yuv_a.0, yuv_b.0);
        assert_ne!(yuv_a.1, yuv_b.1);

        let mut first = vec![0; stride * height];
        for y in 0..height {
            for x in 0..width {
                write_bgra_pixel(&mut first, stride, x, y, 128, 128, 128);
            }
        }
        let mut second = first.clone();
        let mut third = second.clone();
        write_bgra_pixel(&mut first, stride, 0, 0, color_a.0, color_a.1, color_a.2);
        write_bgra_pixel(&mut first, stride, 1, 0, color_b.0, color_b.1, color_b.2);
        write_bgra_pixel(&mut second, stride, 0, 0, color_b.0, color_b.1, color_b.2);
        write_bgra_pixel(&mut second, stride, 1, 0, color_a.0, color_a.1, color_a.2);
        write_bgra_pixel(&mut third, stride, 2, 0, color_a.0, color_a.1, color_a.2);

        let mut encoder = match new_test_avc444_encoder(width, height) {
            Ok(encoder) => encoder,
            Err(error) if error.contains("libopenh264") => return,
            Err(error) => panic!("AVC444v2 encoder initialization failed: {error}"),
        };

        let first_frame = encoder
            .encode(&first, stride, &[(0, 0, width as i32, height as i32)])
            .expect("first frame encodes");
        assert_eq!(first_frame.encoding, Avc444FrameEncoding::LumaAndChroma);
        encoder.commit_reference();

        let second_frame = encoder
            .encode(&second, stride, &[(0, 0, 4, 2)])
            .expect("chroma-only frame encodes");
        assert_eq!(second_frame.encoding, Avc444FrameEncoding::Chroma);

        encoder.force_idr();
        let recovered = encoder
            .encode(&third, stride, &[(0, 0, 4, 2)])
            .expect("forced recovery frame encodes");
        assert_eq!(recovered.encoding, Avc444FrameEncoding::LumaAndChroma);
        assert_eq!(
            recovered.stream1_regions,
            vec![(0, 0, width as i32, height as i32)]
        );
        assert_eq!(
            recovered.stream2_regions,
            vec![(0, 0, width as i32, height as i32)]
        );
        assert!(annex_b_nal_types(&recovered.stream1).contains(&5));
        assert!(!recovered.stream2.is_empty());
    }

    #[test]
    fn avc444_v2_encoder_chroma_only_change_uses_chroma_stream_role() {
        let width = 16;
        let height = 16;
        let stride = width * 4;
        let tables = &*BGRA_TO_YUV_TABLES;
        let color_a = (0u8, 0u8, 187u8);
        let color_b = (0u8, 17u8, 17u8);
        let yuv_a = bgra_components_to_yuv(
            tables,
            usize::from(color_a.0),
            usize::from(color_a.1),
            usize::from(color_a.2),
        );
        let yuv_b = bgra_components_to_yuv(
            tables,
            usize::from(color_b.0),
            usize::from(color_b.1),
            usize::from(color_b.2),
        );
        assert_eq!(yuv_a.0, yuv_b.0);
        assert_ne!(yuv_a.1, yuv_b.1);

        let mut first = vec![0; stride * height];
        for y in 0..height {
            for x in 0..width {
                write_bgra_pixel(&mut first, stride, x, y, 128, 128, 128);
            }
        }
        let mut second = first.clone();

        write_bgra_pixel(&mut first, stride, 0, 0, color_a.0, color_a.1, color_a.2);
        write_bgra_pixel(&mut first, stride, 1, 0, color_b.0, color_b.1, color_b.2);
        write_bgra_pixel(&mut second, stride, 0, 0, color_b.0, color_b.1, color_b.2);
        write_bgra_pixel(&mut second, stride, 1, 0, color_a.0, color_a.1, color_a.2);

        let mut encoder = match Avc444Encoder::new(
            width as u32,
            height as u32,
            1_000_000,
            30,
            23,
            H264RateControl::Cqp,
        ) {
            Ok(encoder) => encoder,
            Err(error) if format!("{error:#}").contains("libopenh264") => return,
            Err(error) => panic!("AVC444v2 encoder initialization failed: {error:#}"),
        };

        let first_frame = encoder
            .encode(&first, stride, &[(0, 0, width as i32, height as i32)])
            .expect("first frame encodes");
        assert_eq!(first_frame.encoding, Avc444FrameEncoding::LumaAndChroma);
        assert!(!first_frame.stream1.is_empty());
        assert!(!first_frame.stream2.is_empty());
        encoder.commit_reference();

        let second_frame = encoder
            .encode(&second, stride, &[(0, 0, 4, 2)])
            .expect("second frame encodes");

        assert_eq!(second_frame.encoding, Avc444FrameEncoding::Chroma);
        assert!(!second_frame.stream1.is_empty());
        assert!(second_frame.stream2.is_empty());
        assert_eq!(second_frame.stream1_regions, vec![(0, 0, 4, 2)]);
        assert!(second_frame.stream2_regions.is_empty());
    }

    #[test]
    fn reference_update_keeps_untransmitted_chroma_state_unchanged() {
        let width = 8;
        let height = 4;
        let mut reference = Some(Yuv420Reference {
            y: vec![1; width * height],
            u: vec![2; (width / 2) * (height / 2)],
            v: vec![3; (width / 2) * (height / 2)],
        });
        let y = vec![9; width * height];
        let u = vec![10; (width / 2) * (height / 2)];
        let v = vec![11; (width / 2) * (height / 2)];

        update_yuv420_reference_regions(&mut reference, width, height, &y, &u, &v, &[]);
        let reference = reference.expect("reference remains initialized");

        assert_eq!(reference.y, vec![1; width * height]);
        assert_eq!(reference.u, vec![2; (width / 2) * (height / 2)]);
        assert_eq!(reference.v, vec![3; (width / 2) * (height / 2)]);
    }

    #[test]
    fn avc444_v2_packing_matches_protocol_plane_layout() {
        let width = 4;
        let height = 4;
        let u444: Vec<u8> = (0..16).map(|v| v as u8).collect();
        let v444: Vec<u8> = (100..116).map(|v| v as u8).collect();
        let mut main_u = vec![0; 4];
        let mut main_v = vec![0; 4];
        let mut aux_y = vec![0; 16];
        let mut aux_u = vec![0; 4];
        let mut aux_v = vec![0; 4];

        pack_avc444_v2_planes(
            width,
            height,
            &u444,
            &v444,
            &mut main_u,
            &mut main_v,
            &mut aux_y,
            &mut aux_u,
            &mut aux_v,
        );

        assert_eq!(main_u, vec![2, 4, 10, 12]);
        assert_eq!(main_v, vec![102, 104, 110, 112]);
        assert_eq!(&aux_y[0..4], &[1, 3, 101, 103]);
        assert_eq!(&aux_y[4..8], &[5, 7, 105, 107]);
        assert_eq!(&aux_y[8..12], &[9, 11, 109, 111]);
        assert_eq!(&aux_y[12..16], &[13, 15, 113, 115]);
        assert_eq!(aux_u, vec![4, 104, 12, 112]);
        assert_eq!(aux_v, vec![6, 106, 14, 114]);
    }

    #[test]
    fn avc444_v2_bgra_path_matches_plane_packing() {
        let width = 8;
        let height = 4;
        let stride = width * 4;
        let mut bgra = vec![0; stride * height];
        for y in 0..height {
            for x in 0..width {
                let offset = y * stride + x * 4;
                bgra[offset] = (x * 17 + y * 3) as u8;
                bgra[offset + 1] = (x * 5 + y * 29) as u8;
                bgra[offset + 2] = (x * 11 + y * 7) as u8;
                bgra[offset + 3] = 255;
            }
        }

        let mut y444 = vec![0; width * height];
        let mut u444 = vec![0; width * height];
        let mut v444 = vec![0; width * height];
        let tables = &*BGRA_TO_YUV_TABLES;
        for y in 0..height {
            for x in 0..width {
                let (yy, uu, vv) = bgra_pixel_to_yuv(tables, &bgra, stride, x, y);
                let offset = y * width + x;
                y444[offset] = yy;
                u444[offset] = uu;
                v444[offset] = vv;
            }
        }

        let uv_len = (width / 2) * (height / 2);
        let mut expected_main_u = vec![0; uv_len];
        let mut expected_main_v = vec![0; uv_len];
        let mut expected_aux_y = vec![0; width * height];
        let mut expected_aux_u = vec![0; uv_len];
        let mut expected_aux_v = vec![0; uv_len];
        pack_avc444_v2_planes(
            width,
            height,
            &u444,
            &v444,
            &mut expected_main_u,
            &mut expected_main_v,
            &mut expected_aux_y,
            &mut expected_aux_u,
            &mut expected_aux_v,
        );

        let mut actual_y = vec![0; width * height];
        let mut actual_main_u = vec![0; uv_len];
        let mut actual_main_v = vec![0; uv_len];
        let mut actual_aux_y = vec![0; width * height];
        let mut actual_aux_u = vec![0; uv_len];
        let mut actual_aux_v = vec![0; uv_len];
        bgra_to_avc444_v2_planes(
            width,
            height,
            &bgra,
            stride,
            &mut actual_y,
            &mut actual_main_u,
            &mut actual_main_v,
            &mut actual_aux_y,
            &mut actual_aux_u,
            &mut actual_aux_v,
        );

        assert_eq!(actual_y, y444);
        assert_eq!(actual_main_u, expected_main_u);
        assert_eq!(actual_main_v, expected_main_v);
        assert_eq!(actual_aux_y, expected_aux_y);
        assert_eq!(actual_aux_u, expected_aux_u);
        assert_eq!(actual_aux_v, expected_aux_v);
    }

    #[test]
    fn avc444_v2_bgra_path_ignores_row_padding() {
        let width = 8;
        let height = 4;
        let tight_stride = width * 4;
        let padded_stride = tight_stride + 12;
        let tight = gradient_bgra_frame(width, height, tight_stride);
        let mut padded = vec![0xee; padded_stride * height];
        for y in 0..height {
            let tight_row = y * tight_stride;
            let padded_row = y * padded_stride;
            padded[padded_row..padded_row + tight_stride]
                .copy_from_slice(&tight[tight_row..tight_row + tight_stride]);
        }

        let y_len = width * height;
        let uv_len = (width / 2) * (height / 2);
        let mut tight_y = vec![0; y_len];
        let mut tight_main_u = vec![0; uv_len];
        let mut tight_main_v = vec![0; uv_len];
        let mut tight_aux_y = vec![0; y_len];
        let mut tight_aux_u = vec![0; uv_len];
        let mut tight_aux_v = vec![0; uv_len];
        bgra_to_avc444_v2_planes(
            width,
            height,
            &tight,
            tight_stride,
            &mut tight_y,
            &mut tight_main_u,
            &mut tight_main_v,
            &mut tight_aux_y,
            &mut tight_aux_u,
            &mut tight_aux_v,
        );

        let mut padded_y = vec![0; y_len];
        let mut padded_main_u = vec![0; uv_len];
        let mut padded_main_v = vec![0; uv_len];
        let mut padded_aux_y = vec![0; y_len];
        let mut padded_aux_u = vec![0; uv_len];
        let mut padded_aux_v = vec![0; uv_len];
        bgra_to_avc444_v2_planes(
            width,
            height,
            &padded,
            padded_stride,
            &mut padded_y,
            &mut padded_main_u,
            &mut padded_main_v,
            &mut padded_aux_y,
            &mut padded_aux_u,
            &mut padded_aux_v,
        );

        assert_eq!(padded_y, tight_y);
        assert_eq!(padded_main_u, tight_main_u);
        assert_eq!(padded_main_v, tight_main_v);
        assert_eq!(padded_aux_y, tight_aux_y);
        assert_eq!(padded_aux_u, tight_aux_u);
        assert_eq!(padded_aux_v, tight_aux_v);
    }

    #[test]
    fn avc444_v2_bgra_region_path_preserves_unchanged_planes() {
        let width = 8;
        let height = 4;
        let stride = width * 4;
        let mut bgra = vec![0; stride * height];
        for y in 0..height {
            for x in 0..width {
                let offset = y * stride + x * 4;
                bgra[offset] = (x * 13 + y * 7) as u8;
                bgra[offset + 1] = (x * 19 + y * 11) as u8;
                bgra[offset + 2] = (x * 23 + y * 17) as u8;
                bgra[offset + 3] = 255;
            }
        }

        let y_len = width * height;
        let uv_len = (width / 2) * (height / 2);
        let mut expected_y = vec![0; y_len];
        let mut expected_main_u = vec![0; uv_len];
        let mut expected_main_v = vec![0; uv_len];
        let mut expected_aux_y = vec![0; y_len];
        let mut expected_aux_u = vec![0; uv_len];
        let mut expected_aux_v = vec![0; uv_len];
        bgra_to_avc444_v2_planes(
            width,
            height,
            &bgra,
            stride,
            &mut expected_y,
            &mut expected_main_u,
            &mut expected_main_v,
            &mut expected_aux_y,
            &mut expected_aux_u,
            &mut expected_aux_v,
        );

        let mut actual_y = vec![11; y_len];
        let mut actual_main_u = vec![22; uv_len];
        let mut actual_main_v = vec![33; uv_len];
        let mut actual_aux_y = vec![44; y_len];
        let mut actual_aux_u = vec![55; uv_len];
        let mut actual_aux_v = vec![66; uv_len];
        bgra_to_avc444_v2_plane_regions(
            width,
            height,
            &bgra,
            stride,
            &[(4, 2, 1, 1)],
            &mut actual_y,
            &mut actual_main_u,
            &mut actual_main_v,
            &mut actual_aux_y,
            &mut actual_aux_u,
            &mut actual_aux_v,
        );

        for y in 0..height {
            for x in 0..width {
                let offset = y * width + x;
                if (4..8).contains(&x) && (2..4).contains(&y) {
                    assert_eq!(actual_y[offset], expected_y[offset]);
                } else {
                    assert_eq!(actual_y[offset], 11);
                }

                let aux_updated =
                    (2..4).contains(&y) && ((2..4).contains(&x) || (6..8).contains(&x));
                if aux_updated {
                    assert_eq!(actual_aux_y[offset], expected_aux_y[offset]);
                } else {
                    assert_eq!(actual_aux_y[offset], 44);
                }
            }
        }

        for cy in 0..(height / 2) {
            for cx in 0..(width / 2) {
                let offset = cy * (width / 2) + cx;
                if cy == 1 && (2..4).contains(&cx) {
                    assert_eq!(actual_main_u[offset], expected_main_u[offset]);
                    assert_eq!(actual_main_v[offset], expected_main_v[offset]);
                } else {
                    assert_eq!(actual_main_u[offset], 22);
                    assert_eq!(actual_main_v[offset], 33);
                }

                if cy == 1 && (cx == 1 || cx == 3) {
                    assert_eq!(actual_aux_u[offset], expected_aux_u[offset]);
                    assert_eq!(actual_aux_v[offset], expected_aux_v[offset]);
                } else {
                    assert_eq!(actual_aux_u[offset], 55);
                    assert_eq!(actual_aux_v[offset], 66);
                }
            }
        }
    }

    #[test]
    fn yuv420_region_detection_uses_candidate_area() {
        let width = 128;
        let height = 64;
        let y = vec![0; width * height];
        let u = vec![128; (width / 2) * (height / 2)];
        let v = vec![128; (width / 2) * (height / 2)];
        let reference = Yuv420Reference {
            y: y.clone(),
            u: u.clone(),
            v: v.clone(),
        };

        let mut changed_y = y;
        changed_y[10 * width + 90] = 1;
        let regions = detect_yuv420_regions(
            width,
            height,
            &changed_y,
            &u,
            &v,
            Some(&reference),
            &[(64, 0, 64, 64)],
        );

        assert_eq!(regions, vec![(64, 0, 64, 64)]);
    }

    #[test]
    fn yuv420_region_detection_checks_chroma_planes() {
        let width = 128;
        let height = 64;
        let y = vec![0; width * height];
        let u = vec![128; (width / 2) * (height / 2)];
        let v = vec![128; (width / 2) * (height / 2)];
        let reference = Yuv420Reference {
            y: y.clone(),
            u: u.clone(),
            v: v.clone(),
        };

        let mut changed_u = u;
        changed_u[5 * (width / 2) + 45] = 127;
        let regions = detect_yuv420_regions(
            width,
            height,
            &y,
            &changed_u,
            &v,
            Some(&reference),
            &[(64, 0, 64, 64)],
        );

        assert_eq!(regions, vec![(64, 0, 64, 64)]);
    }

    #[test]
    fn avc444_v2_protocol_regions_are_aligned_for_chroma_decode() {
        let regions = align_avc444_v2_protocol_regions(1920, 1200, &[(101, 105, 17, 9)]);

        assert_eq!(regions, vec![(100, 104, 20, 10)]);
    }

    #[test]
    fn avc444_v2_chroma_detection_uses_packed_coordinates_but_returns_protocol_regions() {
        let width = 128;
        let height = 128;
        let y = vec![128; width * height];
        let u = vec![128; (width / 2) * (height / 2)];
        let v = vec![128; (width / 2) * (height / 2)];
        let reference = Yuv420Reference {
            y: y.clone(),
            u: u.clone(),
            v: v.clone(),
        };

        let mut changed_y = y;
        changed_y[100 * width + 55] = 127;
        let candidates = align_avc444_v2_protocol_regions(width, height, &[(110, 100, 1, 1)]);
        let (packed_regions, protocol_regions) = detect_avc444_v2_chroma_regions(
            width,
            height,
            &changed_y,
            &u,
            &v,
            Some(&reference),
            &candidates,
        );

        assert_eq!(packed_regions, vec![(54, 100, 2, 2)]);
        assert_eq!(protocol_regions, vec![(108, 100, 4, 2)]);
    }
}
