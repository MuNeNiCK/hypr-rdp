use anyhow::{bail, Context, Result};
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, QpRange, RateControlMode,
    UsageType, VuiConfig,
};
use openh264::formats::YUVSource;
use openh264::OpenH264API;
use yuv::{
    bgra_to_yuv420, BufferStoreMut, YuvConversionMode, YuvPlanarImageMut, YuvRange,
    YuvStandardMatrix,
};

use super::H264RateControl;

/// Extract SPS (NAL type 7) and PPS (NAL type 8) from Annex B bitstream.
/// Shared between VAAPI and software encoders.
pub fn extract_sps_pps(data: &[u8]) -> Option<Vec<u8>> {
    let mut sps_pps = Vec::new();
    let mut i = 0;

    while i < data.len() {
        let start_code_len = if i + 4 <= data.len() && data[i..i + 4] == [0x00, 0x00, 0x00, 0x01] {
            4
        } else if i + 3 <= data.len() && data[i..i + 3] == [0x00, 0x00, 0x01] {
            3
        } else {
            i += 1;
            continue;
        };

        let nal_start = i + start_code_len;
        if nal_start >= data.len() {
            break;
        }

        let nal_type = data[nal_start] & 0x1F;

        // Find next start code
        let mut nal_end = data.len();
        let mut j = nal_start + 1;
        while j + 2 < data.len() {
            if data[j..j + 3] == [0x00, 0x00, 0x01]
                || (j + 3 < data.len() && data[j..j + 4] == [0x00, 0x00, 0x00, 0x01])
            {
                nal_end = j;
                if j > 0 && data[j - 1] == 0x00 {
                    nal_end = j - 1;
                }
                break;
            }
            j += 1;
        }

        if nal_type == 7 || nal_type == 8 {
            sps_pps.extend_from_slice(&data[i..nal_end]);
        }

        i = nal_end;
    }

    if sps_pps.is_empty() {
        None
    } else {
        Some(sps_pps)
    }
}

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
        Self::new_with_options(
            width,
            height,
            bitrate,
            fps,
            qp,
            rate_control,
            H264EncoderOptions::default(),
        )
    }

    pub(super) fn new_with_options(
        width: u32,
        height: u32,
        bitrate: u32,
        fps: u32,
        qp: u8,
        rate_control: H264RateControl,
        options: H264EncoderOptions,
    ) -> Result<Self> {
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            bail!("dimensions must be non-zero and even: {}x{}", width, height);
        }

        let api = unsafe {
            OpenH264API::from_blob_path_unchecked("libopenh264.so")
                .context("failed to load libopenh264.so (install openh264 package)")?
        };

        let threads = openh264_thread_count();
        let mut config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(bitrate))
            .max_frame_rate(FrameRate::from_hz(fps as f32))
            .usage_type(options.usage_type)
            .complexity(Complexity::Medium)
            .scene_change_detect(options.scene_change_detect)
            .adaptive_quantization(false)
            .background_detection(false)
            .long_term_reference(options.long_term_reference)
            .vui(VuiConfig::bt709_full());
        if threads > 1 {
            config = config.num_threads(threads);
            if let Some(max_slice_len) = openh264_size_limited_slice_len() {
                config = config.max_slice_len(max_slice_len);
            }
        }
        config = match rate_control {
            H264RateControl::Vbr => config
                .rate_control_mode(RateControlMode::Bitrate)
                .qp(QpRange::new(0, 51))
                .skip_frames(options.frame_skip),
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
            .map(|encoded| encoded.data)
    }

    pub(super) fn encode_yuv420_raw(
        &mut self,
        y: &[u8],
        u: &[u8],
        v: &[u8],
    ) -> Result<EncodedH264> {
        self.encode_yuv420_with_options(y, u, v, false)
    }

    fn encode_yuv420_with_options(
        &mut self,
        y: &[u8],
        u: &[u8],
        v: &[u8],
        prepend_cached_sps_pps: bool,
    ) -> Result<EncodedH264> {
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

        encode_yuv_source_with_options(
            &mut self.encoder,
            &mut self.cached_sps_pps,
            &yuv,
            prepend_cached_sps_pps,
        )
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

#[derive(Clone, Copy)]
pub(super) struct H264EncoderOptions {
    pub(super) usage_type: UsageType,
    pub(super) scene_change_detect: bool,
    pub(super) long_term_reference: bool,
    pub(super) frame_skip: bool,
}

impl Default for H264EncoderOptions {
    fn default() -> Self {
        Self {
            usage_type: UsageType::ScreenContentRealTime,
            scene_change_detect: true,
            long_term_reference: false,
            frame_skip: true,
        }
    }
}

pub(super) fn avc444_h264_encoder_options() -> H264EncoderOptions {
    H264EncoderOptions {
        // AVC444 LC=0 carries main and auxiliary H.264 streams in one
        // RDPEGFX bitmap payload. A skipped OpenH264 frame produces an empty
        // bitstream, which cannot be represented as a valid LC=0 update.
        usage_type: UsageType::ScreenContentRealTime,
        scene_change_detect: true,
        long_term_reference: false,
        frame_skip: false,
    }
}

fn openh264_thread_count() -> u16 {
    if let Ok(value) = std::env::var("HYPR_RDP_OPENH264_THREADS") {
        if let Ok(parsed) = value.parse::<u16>() {
            return parsed.clamp(1, 16);
        }
    }

    std::thread::available_parallelism()
        .map(|threads| threads.get().clamp(1, 4) as u16)
        .unwrap_or(1)
}

fn openh264_size_limited_slice_len() -> Option<u32> {
    if let Ok(value) = std::env::var("HYPR_RDP_OPENH264_MAX_SLICE_LEN") {
        if let Ok(parsed) = value.parse::<u32>() {
            return Some(parsed.clamp(4096, 262_144));
        }
    }

    None
}

pub(super) struct EncodedH264 {
    pub(super) data: Vec<u8>,
    pub(super) frame_type: FrameType,
}

impl EncodedH264 {
    pub(super) fn empty() -> Self {
        Self {
            data: Vec::new(),
            frame_type: FrameType::Skip,
        }
    }
}

pub(super) fn is_h264_keyframe(frame_type: FrameType) -> bool {
    frame_type == FrameType::IDR || frame_type == FrameType::I
}

fn encode_yuv_source(
    encoder: &mut Encoder,
    cached_sps_pps: &mut Option<Vec<u8>>,
    yuv: &impl YUVSource,
) -> Result<EncodedH264> {
    encode_yuv_source_with_options(encoder, cached_sps_pps, yuv, true)
}

fn encode_yuv_source_with_options(
    encoder: &mut Encoder,
    cached_sps_pps: &mut Option<Vec<u8>>,
    yuv: &impl YUVSource,
    prepend_cached_sps_pps: bool,
) -> Result<EncodedH264> {
    let bitstream = encoder.encode(yuv).context("OpenH264 encode failed")?;

    let mut data = bitstream.to_vec();
    let frame_type = bitstream.frame_type();
    if data.is_empty() {
        return Ok(EncodedH264 { data, frame_type });
    }

    if is_h264_keyframe(frame_type) {
        if let Some(sps_pps) = extract_sps_pps(&data) {
            *cached_sps_pps = Some(sps_pps);
        }
    } else if prepend_cached_sps_pps {
        if let Some(sps_pps) = cached_sps_pps {
            let mut combined = Vec::with_capacity(sps_pps.len() + data.len());
            combined.extend_from_slice(sps_pps);
            combined.extend_from_slice(&data);
            data = combined;
        }
    }

    Ok(EncodedH264 { data, frame_type })
}

pub(super) fn annex_b_nal_types(data: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    let mut i = 0;

    while i < data.len() {
        let start_code_len = if i + 4 <= data.len() && data[i..i + 4] == [0x00, 0x00, 0x00, 0x01] {
            4
        } else if i + 3 <= data.len() && data[i..i + 3] == [0x00, 0x00, 0x01] {
            3
        } else {
            i += 1;
            continue;
        };

        let nal_start = i + start_code_len;
        if nal_start >= data.len() {
            break;
        }

        types.push(data[nal_start] & 0x1F);
        i = nal_start + 1;
    }

    types
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
