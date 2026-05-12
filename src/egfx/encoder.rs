use anyhow::{bail, Context, Result};
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, QpRange, RateControlMode, UsageType,
    VuiConfig,
};
use openh264::formats::YUVSource;
use openh264::OpenH264API;
use yuv::{
    bgra_to_yuv420, bgra_to_yuv444, BufferStoreMut, YuvConversionMode, YuvPlanarImageMut, YuvRange,
    YuvStandardMatrix,
};

use super::H264RateControl;

/// H.264 encoder wrapping OpenH264 for screen capture encoding.
///
/// Pre-allocates YUV planes and uses BT.709 full-range conversion. Caches
/// SPS/PPS from IDR frames and prepends them to P-frames for Windows MFT
/// decoder compatibility.
pub struct H264Encoder {
    encoder: Encoder,
    width: usize,
    height: usize,
    // Pre-allocated YUV planes
    y_buf: Vec<u8>,
    u_buf: Vec<u8>,
    v_buf: Vec<u8>,
    /// Cached SPS/PPS NAL units from the last IDR frame (Annex B format)
    cached_sps_pps: Option<Vec<u8>>,
}

impl H264Encoder {
    pub fn new(
        width: u32,
        height: u32,
        bitrate: u32,
        fps: u32,
        qp: u8,
        rate_control: H264RateControl,
    ) -> Result<Self> {
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            bail!("dimensions must be non-zero and even: {}x{}", width, height);
        }

        let api = unsafe {
            OpenH264API::from_blob_path_unchecked("libopenh264.so")
                .context("failed to load libopenh264.so (install openh264 package)")?
        };

        let mut config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(bitrate))
            .max_frame_rate(FrameRate::from_hz(fps as f32))
            .usage_type(UsageType::ScreenContentRealTime)
            .complexity(Complexity::Medium)
            .adaptive_quantization(false)
            .background_detection(false)
            .vui(VuiConfig::bt709_full());
        config = match rate_control {
            H264RateControl::Vbr => config
                .rate_control_mode(RateControlMode::Bitrate)
                .qp(QpRange::new(0, 51))
                .skip_frames(true),
            H264RateControl::Cqp => config
                .rate_control_mode(RateControlMode::Off)
                .qp(QpRange::new(qp.min(51), qp.min(51)))
                .skip_frames(false),
        };

        let encoder =
            Encoder::with_api_config(api, config).context("failed to create OpenH264 encoder")?;

        let w = width as usize;
        let h = height as usize;

        Ok(Self {
            encoder,
            width: w,
            height: h,
            y_buf: vec![0u8; w * h],
            u_buf: vec![0u8; (w / 2) * (h / 2)],
            v_buf: vec![0u8; (w / 2) * (h / 2)],
            cached_sps_pps: None,
        })
    }

    /// Encode a BGRA frame to H.264 NAL units (Annex B format).
    ///
    /// SPS/PPS from IDR frames are cached and prepended to P-frames.
    /// `stride` is the byte stride of each row in the BGRA buffer.
    pub fn encode(&mut self, bgra: &[u8], stride: usize) -> Result<Vec<u8>> {
        anyhow::ensure!(
            bgra.len() >= self.height * stride,
            "BGRA buffer too small: {} < {}",
            bgra.len(),
            self.height * stride,
        );
        self.bgra_to_yuv420(bgra, stride)?;

        let yuv = YuvRef {
            y: &self.y_buf,
            u: &self.u_buf,
            v: &self.v_buf,
            width: self.width,
            height: self.height,
        };

        encode_yuv_source(&mut self.encoder, &mut self.cached_sps_pps, &yuv)
    }

    pub fn encode_yuv420(&mut self, y: &[u8], u: &[u8], v: &[u8]) -> Result<Vec<u8>> {
        let y_len = self.width * self.height;
        let uv_len = (self.width / 2) * (self.height / 2);
        anyhow::ensure!(y.len() >= y_len, "Y plane too small");
        anyhow::ensure!(u.len() >= uv_len, "U plane too small");
        anyhow::ensure!(v.len() >= uv_len, "V plane too small");

        let yuv = YuvRef {
            y: &y[..y_len],
            u: &u[..uv_len],
            v: &v[..uv_len],
            width: self.width,
            height: self.height,
        };

        encode_yuv_source(&mut self.encoder, &mut self.cached_sps_pps, &yuv)
    }

    /// Force the next encoded frame to be an IDR frame.
    pub fn force_idr(&mut self) {
        self.encoder.force_intra_frame();
    }

    /// Convert BGRA pixels to YUV420P planes (BT.709 full range).
    fn bgra_to_yuv420(&mut self, bgra: &[u8], stride: usize) -> Result<()> {
        let mut yuv = YuvPlanarImageMut {
            y_plane: BufferStoreMut::Borrowed(&mut self.y_buf),
            y_stride: self.width as u32,
            u_plane: BufferStoreMut::Borrowed(&mut self.u_buf),
            u_stride: (self.width / 2) as u32,
            v_plane: BufferStoreMut::Borrowed(&mut self.v_buf),
            v_stride: (self.width / 2) as u32,
            width: self.width as u32,
            height: self.height as u32,
        };

        bgra_to_yuv420(
            &mut yuv,
            bgra,
            stride as u32,
            YuvRange::Full,
            YuvStandardMatrix::Bt709,
            YuvConversionMode::Balanced,
        )
        .context("BGRA to YUV420 conversion failed")
    }
}

pub struct Avc444EncodedFrame {
    pub luma: Vec<u8>,
    pub chroma: Vec<u8>,
    pub luma_regions: Vec<(i32, i32, i32, i32)>,
    pub chroma_regions: Vec<(i32, i32, i32, i32)>,
}

pub struct Avc444Encoder {
    luma_encoder: H264Encoder,
    chroma_encoder: H264Encoder,
    width: usize,
    height: usize,
    y444: Vec<u8>,
    u444: Vec<u8>,
    v444: Vec<u8>,
    main_u: Vec<u8>,
    main_v: Vec<u8>,
    aux_y: Vec<u8>,
    aux_u: Vec<u8>,
    aux_v: Vec<u8>,
    encoded_y: Vec<u8>,
    encoded_main_u: Vec<u8>,
    encoded_main_v: Vec<u8>,
    encoded_aux_y: Vec<u8>,
    encoded_aux_u: Vec<u8>,
    encoded_aux_v: Vec<u8>,
    luma_reference: Option<Yuv420Reference>,
    chroma_reference: Option<Yuv420Reference>,
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
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            bail!("dimensions must be non-zero and even: {}x{}", width, height);
        }

        let luma_bitrate = bitrate.saturating_mul(7).saturating_div(10).max(1_000_000);
        let chroma_bitrate = bitrate.saturating_sub(luma_bitrate).max(1_000_000);
        let luma_encoder = H264Encoder::new(width, height, luma_bitrate, fps, qp, rate_control)?;
        let chroma_encoder =
            H264Encoder::new(width, height, chroma_bitrate, fps, qp, rate_control)?;

        let w = width as usize;
        let h = height as usize;
        let y_len = w * h;
        let uv_len = (w / 2) * (h / 2);

        Ok(Self {
            luma_encoder,
            chroma_encoder,
            width: w,
            height: h,
            y444: vec![0; y_len],
            u444: vec![0; y_len],
            v444: vec![0; y_len],
            main_u: vec![128; uv_len],
            main_v: vec![128; uv_len],
            aux_y: vec![128; y_len],
            aux_u: vec![128; uv_len],
            aux_v: vec![128; uv_len],
            encoded_y: vec![0; y_len],
            encoded_main_u: vec![128; uv_len],
            encoded_main_v: vec![128; uv_len],
            encoded_aux_y: vec![128; y_len],
            encoded_aux_u: vec![128; uv_len],
            encoded_aux_v: vec![128; uv_len],
            luma_reference: None,
            chroma_reference: None,
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
        self.bgra_to_yuv444(bgra, stride)?;
        pack_avc444_planes(
            self.width,
            self.height,
            &self.u444,
            &self.v444,
            &mut self.main_u,
            &mut self.main_v,
            &mut self.aux_y,
            &mut self.aux_u,
            &mut self.aux_v,
        );

        let luma_regions = detect_yuv420_regions(
            self.width,
            self.height,
            &self.y444,
            &self.main_u,
            &self.main_v,
            self.luma_reference.as_ref(),
            candidate_regions,
        );
        let chroma_candidates =
            align_avc444_v1_chroma_regions(self.width, self.height, candidate_regions);
        let chroma_regions = detect_yuv420_regions(
            self.width,
            self.height,
            &self.aux_y,
            &self.aux_u,
            &self.aux_v,
            self.chroma_reference.as_ref(),
            &chroma_candidates,
        );

        prepare_yuv420_encode_planes(
            self.width,
            self.height,
            &self.y444,
            &self.main_u,
            &self.main_v,
            self.luma_reference.as_ref(),
            &luma_regions,
            &mut self.encoded_y,
            &mut self.encoded_main_u,
            &mut self.encoded_main_v,
        );
        prepare_yuv420_encode_planes(
            self.width,
            self.height,
            &self.aux_y,
            &self.aux_u,
            &self.aux_v,
            self.chroma_reference.as_ref(),
            &chroma_regions,
            &mut self.encoded_aux_y,
            &mut self.encoded_aux_u,
            &mut self.encoded_aux_v,
        );

        let luma = self.luma_encoder.encode_yuv420(
            &self.encoded_y,
            &self.encoded_main_u,
            &self.encoded_main_v,
        )?;
        let chroma = self.chroma_encoder.encode_yuv420(
            &self.encoded_aux_y,
            &self.encoded_aux_u,
            &self.encoded_aux_v,
        )?;
        Ok(Avc444EncodedFrame {
            luma,
            chroma,
            luma_regions,
            chroma_regions,
        })
    }

    pub fn commit_reference(&mut self) {
        copy_yuv420_reference(
            &mut self.luma_reference,
            self.width,
            self.height,
            &self.encoded_y,
            &self.encoded_main_u,
            &self.encoded_main_v,
        );
        copy_yuv420_reference(
            &mut self.chroma_reference,
            self.width,
            self.height,
            &self.encoded_aux_y,
            &self.encoded_aux_u,
            &self.encoded_aux_v,
        );
    }

    pub fn force_idr(&mut self) {
        self.luma_encoder.force_idr();
        self.chroma_encoder.force_idr();
    }

    fn bgra_to_yuv444(&mut self, bgra: &[u8], stride: usize) -> Result<()> {
        let mut yuv = YuvPlanarImageMut {
            y_plane: BufferStoreMut::Borrowed(&mut self.y444),
            y_stride: self.width as u32,
            u_plane: BufferStoreMut::Borrowed(&mut self.u444),
            u_stride: self.width as u32,
            v_plane: BufferStoreMut::Borrowed(&mut self.v444),
            v_stride: self.width as u32,
            width: self.width as u32,
            height: self.height as u32,
        };

        bgra_to_yuv444(
            &mut yuv,
            bgra,
            stride as u32,
            YuvRange::Full,
            YuvStandardMatrix::Bt709,
            YuvConversionMode::Balanced,
        )
        .context("BGRA to YUV444 conversion failed")
    }
}

struct Yuv420Reference {
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

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

#[allow(clippy::too_many_arguments)]
fn prepare_yuv420_encode_planes(
    width: usize,
    height: usize,
    current_y: &[u8],
    current_u: &[u8],
    current_v: &[u8],
    reference: Option<&Yuv420Reference>,
    regions: &[(i32, i32, i32, i32)],
    dst_y: &mut [u8],
    dst_u: &mut [u8],
    dst_v: &mut [u8],
) {
    let y_len = width * height;
    let uv_len = (width / 2) * (height / 2);

    if let Some(reference) = reference {
        dst_y[..y_len].copy_from_slice(&reference.y[..y_len]);
        dst_u[..uv_len].copy_from_slice(&reference.u[..uv_len]);
        dst_v[..uv_len].copy_from_slice(&reference.v[..uv_len]);
    } else {
        dst_y[..y_len].copy_from_slice(&current_y[..y_len]);
        dst_u[..uv_len].copy_from_slice(&current_u[..uv_len]);
        dst_v[..uv_len].copy_from_slice(&current_v[..uv_len]);
        return;
    }

    for &(x, y, w, h) in regions {
        let Some((left, top, right, bottom)) = clamp_region(x, y, w, h, width, height) else {
            continue;
        };

        copy_y_region(width, current_y, dst_y, left, top, right, bottom);

        let chroma_width = width / 2;
        let chroma_left = left / 2;
        let chroma_top = top / 2;
        let chroma_right = right.div_ceil(2);
        let chroma_bottom = bottom.div_ceil(2);
        copy_y_region(
            chroma_width,
            current_u,
            dst_u,
            chroma_left,
            chroma_top,
            chroma_right,
            chroma_bottom,
        );
        copy_y_region(
            chroma_width,
            current_v,
            dst_v,
            chroma_left,
            chroma_top,
            chroma_right,
            chroma_bottom,
        );
    }
}

fn copy_y_region(
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
fn pack_avc444_planes(
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

    for cy in 0..chroma_h {
        for cx in 0..chroma_w {
            let x = cx * 2;
            let y = cy * 2;
            let dst = cy * chroma_w + cx;
            main_u[dst] = avg_2x2(u444, width, x, y);
            main_v[dst] = avg_2x2(v444, width, x, y);

            let odd_x = x + 1;
            aux_u[dst] = u444[y * width + odd_x];
            aux_v[dst] = v444[y * width + odd_x];
        }
    }

    aux_y.fill(128);
    for y in 0..height {
        let macroblock_base = (y / 16) * 16;
        let macro_row = y % 16;
        let (src_plane, src_y) = if macro_row < 8 {
            (u444, macroblock_base + macro_row * 2 + 1)
        } else {
            (v444, macroblock_base + (macro_row - 8) * 2 + 1)
        };
        if src_y < height {
            let dst_start = y * width;
            let src_start = src_y * width;
            aux_y[dst_start..dst_start + width]
                .copy_from_slice(&src_plane[src_start..src_start + width]);
        }
    }
}

fn avg_2x2(plane: &[u8], stride: usize, x: usize, y: usize) -> u8 {
    let a = u32::from(plane[y * stride + x]);
    let b = u32::from(plane[y * stride + x + 1]);
    let c = u32::from(plane[(y + 1) * stride + x]);
    let d = u32::from(plane[(y + 1) * stride + x + 1]);
    ((a + b + c + d + 2) / 4) as u8
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

fn align_avc444_v1_chroma_regions(
    width: usize,
    height: usize,
    regions: &[(i32, i32, i32, i32)],
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

        let left = x.clamp(0, width) & !1;
        let top = y.clamp(0, height) & !15;
        let right = x.saturating_add(w).clamp(0, width);
        let bottom = y.saturating_add(h).clamp(0, height);
        let right = (right.saturating_add(1) & !1).clamp(0, width);
        let bottom = (bottom.saturating_add(15) & !15).clamp(0, height);

        if right > left && bottom > top {
            merge_region(&mut aligned, (left, top, right - left, bottom - top));
        }
    }

    aligned
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

fn encode_yuv_source(
    encoder: &mut Encoder,
    cached_sps_pps: &mut Option<Vec<u8>>,
    yuv: &impl YUVSource,
) -> Result<Vec<u8>> {
    let bitstream = encoder.encode(yuv).context("OpenH264 encode failed")?;

    let mut data = bitstream.to_vec();
    if data.is_empty() {
        return Ok(data);
    }

    let is_keyframe = bitstream.frame_type() == openh264::encoder::FrameType::IDR
        || bitstream.frame_type() == openh264::encoder::FrameType::I;

    if is_keyframe {
        if let Some(sps_pps) = super::extract_sps_pps(&data) {
            *cached_sps_pps = Some(sps_pps);
        }
    } else if let Some(sps_pps) = cached_sps_pps {
        let mut combined = Vec::with_capacity(sps_pps.len() + data.len());
        combined.extend_from_slice(sps_pps);
        combined.extend_from_slice(&data);
        data = combined;
    }

    Ok(data)
}

/// Reference to pre-allocated YUV planes implementing OpenH264's YUVSource.
struct YuvRef<'a> {
    y: &'a [u8],
    u: &'a [u8],
    v: &'a [u8],
    width: usize,
    height: usize,
}

impl YUVSource for YuvRef<'_> {
    fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn strides(&self) -> (usize, usize, usize) {
        (self.width, self.width / 2, self.width / 2)
    }

    fn y(&self) -> &[u8] {
        self.y
    }

    fn u(&self) -> &[u8] {
        self.u
    }

    fn v(&self) -> &[u8] {
        self.v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avc444_packing_preserves_odd_chroma_samples() {
        let width = 4;
        let height = 4;
        let u444: Vec<u8> = (0..16).map(|v| v as u8).collect();
        let v444: Vec<u8> = (100..116).map(|v| v as u8).collect();
        let mut main_u = vec![0; 4];
        let mut main_v = vec![0; 4];
        let mut aux_y = vec![0; 16];
        let mut aux_u = vec![0; 4];
        let mut aux_v = vec![0; 4];

        pack_avc444_planes(
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

        assert_eq!(main_u, vec![3, 5, 11, 13]);
        assert_eq!(main_v, vec![103, 105, 111, 113]);
        assert_eq!(&aux_y[0..4], &u444[4..8]);
        assert_eq!(&aux_y[4..8], &u444[12..16]);
        assert_eq!(&aux_y[8..16], &[128; 8]);
        assert_eq!(aux_u, vec![1, 3, 9, 11]);
        assert_eq!(aux_v, vec![101, 103, 109, 111]);
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
    fn avc444_v1_chroma_candidates_are_16_row_aligned() {
        let regions = align_avc444_v1_chroma_regions(1920, 1200, &[(101, 105, 17, 9)]);

        assert_eq!(regions, vec![(100, 96, 18, 32)]);
    }

    #[test]
    fn avc444_v1_chroma_detection_covers_repacked_odd_rows() {
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
        changed_y[100 * width + 110] = 127;
        let candidates = align_avc444_v1_chroma_regions(width, height, &[(110, 105, 1, 1)]);
        let regions = detect_yuv420_regions(
            width,
            height,
            &changed_y,
            &u,
            &v,
            Some(&reference),
            &candidates,
        );

        assert_eq!(regions, vec![(110, 96, 2, 16)]);
    }

    #[test]
    fn yuv420_encode_planes_keep_unsent_reference_pixels() {
        let width = 128;
        let height = 64;
        let current_y = vec![1; width * height];
        let current_u = vec![2; (width / 2) * (height / 2)];
        let current_v = vec![3; (width / 2) * (height / 2)];
        let reference = Yuv420Reference {
            y: vec![10; width * height],
            u: vec![20; (width / 2) * (height / 2)],
            v: vec![30; (width / 2) * (height / 2)],
        };
        let mut dst_y = vec![0; width * height];
        let mut dst_u = vec![0; (width / 2) * (height / 2)];
        let mut dst_v = vec![0; (width / 2) * (height / 2)];

        prepare_yuv420_encode_planes(
            width,
            height,
            &current_y,
            &current_u,
            &current_v,
            Some(&reference),
            &[(64, 0, 64, 64)],
            &mut dst_y,
            &mut dst_u,
            &mut dst_v,
        );

        assert_eq!(dst_y[10 * width + 10], 10);
        assert_eq!(dst_y[10 * width + 90], 1);
        assert_eq!(dst_u[5 * (width / 2) + 5], 20);
        assert_eq!(dst_u[5 * (width / 2) + 45], 2);
        assert_eq!(dst_v[5 * (width / 2) + 5], 30);
        assert_eq!(dst_v[5 * (width / 2) + 45], 3);
    }
}
