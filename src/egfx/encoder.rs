use anyhow::{Context, Result};
use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, RateControlMode, UsageType};
use openh264::formats::YUVSource;
use openh264::OpenH264API;

/// H.264 encoder wrapping OpenH264 for screen capture encoding.
///
/// Pre-allocates YUV planes and uses integer BT.601 limited-range conversion
/// (matching OpenH264's internal matrix). Caches SPS/PPS from IDR frames
/// and prepends to P-frames for Windows MFT decoder compatibility.
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
    pub fn new(width: u32, height: u32) -> Result<Self> {
        let api = unsafe {
            OpenH264API::from_blob_path_unchecked("libopenh264.so")
                .context("failed to load libopenh264.so (install openh264 package)")?
        };

        let config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(4_000_000))
            .max_frame_rate(FrameRate::from_hz(30.0))
            .rate_control_mode(RateControlMode::Bitrate)
            .usage_type(UsageType::ScreenContentRealTime)
            .skip_frames(false);

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
    pub fn encode(&mut self, bgra: &[u8]) -> Result<Vec<u8>> {
        self.bgra_to_yuv420(bgra);

        let yuv = YuvRef {
            y: &self.y_buf,
            u: &self.u_buf,
            v: &self.v_buf,
            width: self.width,
            height: self.height,
        };

        let bitstream = self
            .encoder
            .encode(&yuv)
            .context("OpenH264 encode failed")?;

        let mut data = bitstream.to_vec();
        if data.is_empty() {
            return Ok(data);
        }

        let is_keyframe = bitstream.frame_type() == openh264::encoder::FrameType::IDR
            || bitstream.frame_type() == openh264::encoder::FrameType::I;

        if is_keyframe {
            // IDR: extract and cache SPS/PPS
            if let Some(sps_pps) = super::extract_sps_pps(&data) {
                tracing::debug!(len = sps_pps.len(), "Cached SPS/PPS from IDR frame");
                self.cached_sps_pps = Some(sps_pps);
            }
        } else {
            // P-frame: prepend cached SPS/PPS
            if let Some(ref sps_pps) = self.cached_sps_pps {
                let mut combined = Vec::with_capacity(sps_pps.len() + data.len());
                combined.extend_from_slice(sps_pps);
                combined.extend_from_slice(&data);
                data = combined;
            }
        }

        Ok(data)
    }

    /// Convert BGRA pixels to YUV420P planes.
    /// BT.601 limited-range (matches OpenH264's internal conversion matrix).
    fn bgra_to_yuv420(&mut self, bgra: &[u8]) {
        let w = self.width;
        let h = self.height;

        // Full-resolution Y plane
        for row in 0..h {
            for col in 0..w {
                let idx = (row * w + col) * 4;
                let b = bgra[idx] as i32;
                let g = bgra[idx + 1] as i32;
                let r = bgra[idx + 2] as i32;

                let y = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
                self.y_buf[row * w + col] = y.clamp(0, 255) as u8;
            }
        }

        // Half-resolution U/V planes (2x2 subsampling)
        let half_w = w / 2;
        for row in 0..(h / 2) {
            for col in 0..half_w {
                let src_row = row * 2;
                let src_col = col * 2;

                let mut r_sum = 0i32;
                let mut g_sum = 0i32;
                let mut b_sum = 0i32;

                for dy in 0..2 {
                    for dx in 0..2 {
                        let idx = ((src_row + dy) * w + (src_col + dx)) * 4;
                        b_sum += bgra[idx] as i32;
                        g_sum += bgra[idx + 1] as i32;
                        r_sum += bgra[idx + 2] as i32;
                    }
                }

                let r = r_sum / 4;
                let g = g_sum / 4;
                let b = b_sum / 4;

                let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
                let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;

                let uv_idx = row * half_w + col;
                self.u_buf[uv_idx] = u.clamp(0, 255) as u8;
                self.v_buf[uv_idx] = v.clamp(0, 255) as u8;
            }
        }
    }

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
