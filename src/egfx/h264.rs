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
    #[cfg(test)]
    force_idr_requests: u32,
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
        let openh264_rate_control = h264_rate_control_mode(rate_control, options);
        tracing::info!(
            bitrate,
            fps,
            quality = qp,
            rate_control = ?rate_control,
            openh264_rate_control = ?openh264_rate_control,
            usage_type = ?options.usage_type,
            complexity = ?options.complexity,
            scene_change_detect = options.scene_change_detect,
            adaptive_quantization = options.adaptive_quantization,
            background_detection = options.background_detection,
            long_term_reference = options.long_term_reference,
            frame_skip = options.frame_skip,
            threads,
            "OpenH264 encoder settings"
        );
        let mut config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(bitrate))
            .max_frame_rate(FrameRate::from_hz(fps as f32))
            .usage_type(options.usage_type)
            .complexity(options.complexity)
            .scene_change_detect(options.scene_change_detect)
            .adaptive_quantization(options.adaptive_quantization)
            .background_detection(options.background_detection)
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
                .rate_control_mode(openh264_rate_control)
                .qp(QpRange::new(0, vbr_max_qp(qp)))
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
            #[cfg(test)]
            force_idr_requests: 0,
        })
    }

    /// Encode a BGRA frame to H.264 NAL units (Annex B format).
    ///
    /// SPS/PPS from IDR frames are cached and prepended to P-frames.
    /// `stride` is the byte stride of each row in the BGRA buffer.
    pub fn encode(&mut self, bgra: &[u8], stride: usize) -> Result<Vec<u8>> {
        validate_bgra_buffer(self.width, self.height, stride, bgra.len())?;
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
        #[cfg(test)]
        {
            self.force_idr_requests = self.force_idr_requests.saturating_add(1);
        }
        self.encoder.force_intra_frame();
    }

    #[cfg(test)]
    pub(crate) fn force_idr_requests_for_test(&self) -> u32 {
        self.force_idr_requests
    }

    /// Convert BGRA pixels to YUV420P planes (BT.709 full range).
    fn bgra_to_yuv420(&mut self, bgra: &[u8], stride: usize) -> Result<()> {
        convert_bgra_to_yuv420_planes(
            self.width,
            self.height,
            bgra,
            stride,
            &mut self.y_buf,
            &mut self.u_buf,
            &mut self.v_buf,
        )
    }
}

fn convert_bgra_to_yuv420_planes(
    width: usize,
    height: usize,
    bgra: &[u8],
    stride: usize,
    y_buf: &mut [u8],
    u_buf: &mut [u8],
    v_buf: &mut [u8],
) -> Result<()> {
    let y_len = width
        .checked_mul(height)
        .context("Y plane length calculation overflow")?;
    let uv_len = (width / 2)
        .checked_mul(height / 2)
        .context("UV plane length calculation overflow")?;
    anyhow::ensure!(y_buf.len() >= y_len, "Y plane too small");
    anyhow::ensure!(u_buf.len() >= uv_len, "U plane too small");
    anyhow::ensure!(v_buf.len() >= uv_len, "V plane too small");

    let mut yuv = YuvPlanarImageMut {
        y_plane: BufferStoreMut::Borrowed(&mut y_buf[..y_len]),
        y_stride: width as u32,
        u_plane: BufferStoreMut::Borrowed(&mut u_buf[..uv_len]),
        u_stride: (width / 2) as u32,
        v_plane: BufferStoreMut::Borrowed(&mut v_buf[..uv_len]),
        v_stride: (width / 2) as u32,
        width: width as u32,
        height: height as u32,
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

fn validate_bgra_buffer(width: usize, height: usize, stride: usize, len: usize) -> Result<()> {
    let minimum_stride = width
        .checked_mul(4)
        .context("BGRA stride calculation overflow")?;
    anyhow::ensure!(
        stride >= minimum_stride,
        "BGRA stride too small: {} < {}",
        stride,
        minimum_stride
    );
    let required_len = height
        .checked_mul(stride)
        .context("BGRA buffer length calculation overflow")?;
    anyhow::ensure!(
        len >= required_len,
        "BGRA buffer too small: {} < {}",
        len,
        required_len,
    );
    Ok(())
}

#[derive(Clone, Copy)]
pub(super) struct H264EncoderOptions {
    pub(super) usage_type: UsageType,
    pub(super) complexity: Complexity,
    pub(super) scene_change_detect: bool,
    pub(super) adaptive_quantization: bool,
    pub(super) background_detection: bool,
    pub(super) long_term_reference: bool,
    pub(super) frame_skip: bool,
}

impl Default for H264EncoderOptions {
    fn default() -> Self {
        Self {
            usage_type: UsageType::ScreenContentRealTime,
            complexity: Complexity::Medium,
            scene_change_detect: true,
            adaptive_quantization: false,
            background_detection: false,
            long_term_reference: false,
            frame_skip: true,
        }
    }
}

pub(super) fn avc444_h264_encoder_options() -> H264EncoderOptions {
    H264EncoderOptions {
        usage_type: UsageType::CameraVideoRealTime,
        complexity: Complexity::Medium,
        scene_change_detect: true,
        adaptive_quantization: true,
        background_detection: true,
        long_term_reference: false,
        frame_skip: true,
    }
}

fn vbr_max_qp(qp: u8) -> u8 {
    qp.clamp(1, 51)
}

fn vbr_rate_control_mode(options: H264EncoderOptions) -> RateControlMode {
    if options.frame_skip {
        RateControlMode::Bitrate
    } else {
        RateControlMode::Quality
    }
}

fn h264_rate_control_mode(
    rate_control: H264RateControl,
    options: H264EncoderOptions,
) -> RateControlMode {
    match rate_control {
        H264RateControl::Vbr => vbr_rate_control_mode(options),
        H264RateControl::Cqp => RateControlMode::Off,
    }
}

fn openh264_thread_count() -> u16 {
    let env_value = std::env::var("HYPR_RDP_OPENH264_THREADS").ok();
    let default_parallelism = std::thread::available_parallelism()
        .map(|threads| threads.get().clamp(1, 4) as u16)
        .unwrap_or(1);

    openh264_thread_count_with(env_value.as_deref(), default_parallelism)
}

fn openh264_thread_count_with(env_value: Option<&str>, default_parallelism: u16) -> u16 {
    if let Some(value) = env_value {
        if let Ok(parsed) = value.parse::<u16>() {
            return parsed.clamp(1, 16);
        }
    }

    default_parallelism.clamp(1, 4)
}

fn openh264_size_limited_slice_len() -> Option<u32> {
    let env_value = std::env::var("HYPR_RDP_OPENH264_MAX_SLICE_LEN").ok();
    openh264_size_limited_slice_len_with(env_value.as_deref())
}

fn openh264_size_limited_slice_len_with(env_value: Option<&str>) -> Option<u32> {
    if let Some(value) = env_value {
        if let Ok(parsed) = value.parse::<u32>() {
            return (parsed != 0).then(|| parsed.clamp(4096, 262_144));
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

    apply_sps_pps_cache(
        frame_type,
        &mut data,
        cached_sps_pps,
        prepend_cached_sps_pps,
    );

    Ok(EncodedH264 { data, frame_type })
}

fn apply_sps_pps_cache(
    frame_type: FrameType,
    data: &mut Vec<u8>,
    cached_sps_pps: &mut Option<Vec<u8>>,
    prepend_cached_sps_pps: bool,
) {
    if data.is_empty() {
        return;
    }

    if is_h264_keyframe(frame_type) {
        if let Some(sps_pps) = extract_sps_pps(data) {
            *cached_sps_pps = Some(sps_pps);
        }
    } else if prepend_cached_sps_pps {
        if let Some(sps_pps) = cached_sps_pps {
            let mut combined = Vec::with_capacity(sps_pps.len() + data.len());
            combined.extend_from_slice(sps_pps);
            combined.extend_from_slice(data);
            *data = combined;
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient_bgra(width: usize, height: usize, stride: usize, seed: u8) -> Vec<u8> {
        let mut bgra = vec![0; stride * height];
        for y in 0..height {
            for x in 0..width {
                let offset = y * stride + x * 4;
                bgra[offset] = seed.wrapping_add((x * 17 + y * 3) as u8);
                bgra[offset + 1] = seed.wrapping_add((x * 5 + y * 29) as u8);
                bgra[offset + 2] = seed.wrapping_add((x * 11 + y * 7) as u8);
                bgra[offset + 3] = 255;
            }
        }
        bgra
    }

    fn test_h264_encoder(width: u32, height: u32) -> std::result::Result<H264Encoder, String> {
        H264Encoder::new(width, height, 1_000_000, 30, 23, H264RateControl::Cqp)
            .map_err(|error| format!("{error:#}"))
    }

    #[test]
    fn vbr_quality_caps_max_qp() {
        assert_eq!(vbr_max_qp(0), 1);
        assert_eq!(vbr_max_qp(23), 23);
        assert_eq!(vbr_max_qp(51), 51);
        assert_eq!(vbr_max_qp(99), 51);
    }

    #[test]
    fn default_h264_encoder_options_follow_freerdp_rdpegfx_baseline() {
        let options = H264EncoderOptions::default();

        assert!(matches!(
            options.usage_type,
            UsageType::ScreenContentRealTime
        ));
        assert!(matches!(options.complexity, Complexity::Medium));
        assert!(options.scene_change_detect);
        assert!(!options.adaptive_quantization);
        assert!(!options.background_detection);
        assert!(!options.long_term_reference);
        assert!(options.frame_skip);
    }

    #[test]
    fn vbr_uses_bitrate_mode_when_frame_skip_is_enabled() {
        let default_options = H264EncoderOptions::default();
        assert!(matches!(
            vbr_rate_control_mode(default_options),
            RateControlMode::Bitrate
        ));

        let avc444_options = avc444_h264_encoder_options();
        assert!(matches!(
            vbr_rate_control_mode(avc444_options),
            RateControlMode::Bitrate
        ));
    }

    #[test]
    fn cqp_uses_rc_off_independent_of_frame_skip_option() {
        let mut options = H264EncoderOptions {
            frame_skip: true,
            ..H264EncoderOptions::default()
        };
        assert!(matches!(
            h264_rate_control_mode(H264RateControl::Cqp, options),
            RateControlMode::Off
        ));

        options.frame_skip = false;
        assert!(matches!(
            h264_rate_control_mode(H264RateControl::Cqp, options),
            RateControlMode::Off
        ));
    }

    #[test]
    fn openh264_thread_count_env_is_bounded_and_falls_back_to_available_parallelism() {
        assert_eq!(openh264_thread_count_with(Some("0"), 4), 1);
        assert_eq!(openh264_thread_count_with(Some("2"), 1), 2);
        assert_eq!(openh264_thread_count_with(Some("32"), 1), 16);
        assert_eq!(openh264_thread_count_with(Some("invalid"), 3), 3);
        assert_eq!(openh264_thread_count_with(None, 0), 1);
        assert_eq!(openh264_thread_count_with(None, 8), 4);
    }

    #[test]
    fn openh264_size_limited_slice_len_env_is_optional_and_bounded() {
        assert_eq!(openh264_size_limited_slice_len_with(None), None);
        assert_eq!(openh264_size_limited_slice_len_with(Some("invalid")), None);
        assert_eq!(openh264_size_limited_slice_len_with(Some("0")), None);
        assert_eq!(
            openh264_size_limited_slice_len_with(Some("1024")),
            Some(4096)
        );
        assert_eq!(
            openh264_size_limited_slice_len_with(Some("65536")),
            Some(65_536)
        );
        assert_eq!(
            openh264_size_limited_slice_len_with(Some("999999")),
            Some(262_144)
        );
    }

    fn solid_bgra(width: usize, height: usize, stride: usize, r: u8, g: u8, b: u8) -> Vec<u8> {
        let mut bgra = vec![0xee; stride * height];
        for y in 0..height {
            for x in 0..width {
                let offset = y * stride + x * 4;
                bgra[offset] = b;
                bgra[offset + 1] = g;
                bgra[offset + 2] = r;
                bgra[offset + 3] = 255;
            }
        }
        bgra
    }

    fn bt709_full_range_reference_yuv(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
        let r = f64::from(r);
        let g = f64::from(g);
        let b = f64::from(b);
        let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        let u = 128.0 + (b - y) / (2.0 * (1.0 - 0.0722));
        let v = 128.0 + (r - y) / (2.0 * (1.0 - 0.2126));
        (
            y.round().clamp(0.0, 255.0) as u8,
            u.round().clamp(0.0, 255.0) as u8,
            v.round().clamp(0.0, 255.0) as u8,
        )
    }

    fn assert_near(actual: u8, expected: u8, tolerance: u8) {
        let delta = actual.abs_diff(expected);
        assert!(
            delta <= tolerance,
            "actual {actual} differs from expected {expected} by {delta}, tolerance {tolerance}"
        );
    }

    #[test]
    fn extract_sps_pps_keeps_parameter_sets_and_drops_other_nals() {
        let stream = [
            0x99, 0x88, // leading bytes before Annex B data
            0x00, 0x00, 0x01, 0x67, 0xaa, 0xbb, // SPS, 3-byte start code
            0x00, 0x00, 0x00, 0x01, 0x68, 0xcc, // PPS, 4-byte start code
            0x00, 0x00, 0x01, 0x65, 0xdd, 0xee, // IDR, not copied
        ];

        let extracted = extract_sps_pps(&stream).expect("SPS/PPS are present");

        assert_eq!(
            extracted,
            vec![0x00, 0x00, 0x01, 0x67, 0xaa, 0xbb, 0x00, 0x00, 0x00, 0x01, 0x68, 0xcc,]
        );
    }

    #[test]
    fn extract_sps_pps_ignores_non_parameter_and_incomplete_nals() {
        assert_eq!(extract_sps_pps(&[]), None);
        assert_eq!(extract_sps_pps(&[0x00, 0x00, 0x01]), None);
        assert_eq!(
            extract_sps_pps(&[
                0x00, 0x00, 0x01, 0x65, 0x11, 0x22, // IDR
                0x00, 0x00, 0x01, 0x61, 0x33, 0x44, // non-IDR slice
            ]),
            None
        );
    }

    #[test]
    fn extract_sps_pps_preserves_emulation_prevention_bytes_inside_parameter_sets() {
        let stream = [
            0x00, 0x00, 0x01, 0x67, 0x11, 0x00, 0x00, 0x03, 0x01, 0x22, // SPS
            0x00, 0x00, 0x01, 0x68, 0x33, 0x00, 0x00, 0x03, 0x02, // PPS
            0x00, 0x00, 0x00, 0x01, 0x61, 0x44, // non-IDR slice
        ];

        let extracted = extract_sps_pps(&stream).expect("SPS/PPS are present");

        assert_eq!(
            extracted,
            vec![
                0x00, 0x00, 0x01, 0x67, 0x11, 0x00, 0x00, 0x03, 0x01, 0x22, 0x00, 0x00, 0x01, 0x68,
                0x33, 0x00, 0x00, 0x03, 0x02,
            ]
        );
    }

    #[test]
    fn annex_b_nal_types_handles_mixed_start_codes_and_trailing_start() {
        let stream = [
            0x00, 0x00, 0x01, 0x67, 0xaa, // SPS
            0x00, 0x00, 0x00, 0x01, 0x68, 0xbb, // PPS
            0x00, 0x00, 0x01, 0x65, 0xcc, // IDR
            0x00, 0x00, 0x01, // incomplete trailing start code
        ];

        assert_eq!(annex_b_nal_types(&stream), vec![7, 8, 5]);
    }

    #[test]
    fn annex_b_nal_types_ignores_emulation_prevention_start_code_lookalikes() {
        let stream = [
            0x00, 0x00, 0x01, 0x67, 0xaa, 0x00, 0x00, 0x03, 0x01, 0xbb, // SPS
            0x00, 0x00, 0x01, 0x68, 0xcc, 0x00, 0x00, 0x03, 0x00, // PPS
        ];

        assert_eq!(annex_b_nal_types(&stream), vec![7, 8]);
    }

    #[test]
    fn avc420_bgra_to_yuv420_uses_bt709_full_range_reference_colors() {
        for &(r, g, b) in &[
            (0, 0, 0),
            (255, 255, 255),
            (255, 0, 0),
            (0, 255, 0),
            (0, 0, 255),
            (128, 128, 128),
            (255, 255, 0),
            (0, 255, 255),
            (255, 0, 255),
        ] {
            let width = 2;
            let height = 2;
            let stride = width * 4 + 8;
            let bgra = solid_bgra(width, height, stride, r, g, b);
            let mut y = vec![0; width * height];
            let mut u = vec![0; (width / 2) * (height / 2)];
            let mut v = vec![0; (width / 2) * (height / 2)];

            convert_bgra_to_yuv420_planes(width, height, &bgra, stride, &mut y, &mut u, &mut v)
                .expect("BGRA converts to YUV420");

            let (expected_y, expected_u, expected_v) = bt709_full_range_reference_yuv(r, g, b);
            for actual_y in y {
                assert_near(actual_y, expected_y, 2);
            }
            assert_near(u[0], expected_u, 2);
            assert_near(v[0], expected_v, 2);
        }
    }

    #[test]
    fn avc420_bgra_to_yuv420_keeps_mixed_2x2_chroma_within_bt709_tolerance() {
        let width = 2;
        let height = 2;
        let stride = width * 4 + 8;
        let colors = [(255, 0, 0), (0, 255, 0), (0, 0, 255), (255, 255, 255)];
        let mut bgra = vec![0xee; stride * height];
        for y_pos in 0..height {
            for x_pos in 0..width {
                let (r, g, b) = colors[y_pos * width + x_pos];
                let offset = y_pos * stride + x_pos * 4;
                bgra[offset] = b;
                bgra[offset + 1] = g;
                bgra[offset + 2] = r;
                bgra[offset + 3] = 255;
            }
        }

        let mut y = vec![0; width * height];
        let mut u = vec![0; 1];
        let mut v = vec![0; 1];
        convert_bgra_to_yuv420_planes(width, height, &bgra, stride, &mut y, &mut u, &mut v)
            .expect("BGRA converts to YUV420");

        let mut expected_u_sum = 0u32;
        let mut expected_v_sum = 0u32;
        for (index, (r, g, b)) in colors.into_iter().enumerate() {
            let (expected_y, expected_u, expected_v) = bt709_full_range_reference_yuv(r, g, b);
            assert_near(y[index], expected_y, 2);
            expected_u_sum += u32::from(expected_u);
            expected_v_sum += u32::from(expected_v);
        }

        assert_near(u[0], ((expected_u_sum + 2) / 4) as u8, 3);
        assert_near(v[0], ((expected_v_sum + 2) / 4) as u8, 3);
    }

    #[test]
    fn avc420_bgra_to_yuv420_keeps_2x2_chroma_blocks_separate_with_padded_stride() {
        let width = 4;
        let height = 2;
        let stride = width * 4 + 12;
        let mut bgra = vec![0xee; stride * height];
        for y in 0..height {
            for x in 0..width {
                let (r, g, b) = if x < 2 { (255, 0, 0) } else { (0, 0, 255) };
                let offset = y * stride + x * 4;
                bgra[offset] = b;
                bgra[offset + 1] = g;
                bgra[offset + 2] = r;
                bgra[offset + 3] = 255;
            }
        }
        let mut y = vec![0; width * height];
        let mut u = vec![0; (width / 2) * (height / 2)];
        let mut v = vec![0; (width / 2) * (height / 2)];

        convert_bgra_to_yuv420_planes(width, height, &bgra, stride, &mut y, &mut u, &mut v)
            .expect("BGRA converts to YUV420");

        let (red_y, red_u, red_v) = bt709_full_range_reference_yuv(255, 0, 0);
        let (blue_y, blue_u, blue_v) = bt709_full_range_reference_yuv(0, 0, 255);
        for row in 0..height {
            assert_near(y[row * width], red_y, 2);
            assert_near(y[row * width + 1], red_y, 2);
            assert_near(y[row * width + 2], blue_y, 2);
            assert_near(y[row * width + 3], blue_y, 2);
        }
        assert_near(u[0], red_u, 2);
        assert_near(v[0], red_v, 2);
        assert_near(u[1], blue_u, 2);
        assert_near(v[1], blue_v, 2);
    }

    #[test]
    fn sps_pps_cache_is_updated_from_idr_and_prepended_to_delta_without_openh264() {
        let mut cached = None;
        let mut idr = vec![
            0x00, 0x00, 0x01, 0x67, 0xaa, 0xbb, // SPS
            0x00, 0x00, 0x01, 0x68, 0xcc, 0xdd, // PPS
            0x00, 0x00, 0x01, 0x65, 0xee, 0xff, // IDR
        ];

        apply_sps_pps_cache(FrameType::IDR, &mut idr, &mut cached, true);

        let expected_parameter_sets = vec![
            0x00, 0x00, 0x01, 0x67, 0xaa, 0xbb, 0x00, 0x00, 0x01, 0x68, 0xcc, 0xdd,
        ];
        assert_eq!(cached, Some(expected_parameter_sets.clone()));

        let mut delta = vec![0x00, 0x00, 0x01, 0x41, 0x11, 0x22];
        apply_sps_pps_cache(FrameType::P, &mut delta, &mut cached, true);

        assert!(delta.starts_with(&expected_parameter_sets));
        assert_eq!(annex_b_nal_types(&delta), vec![7, 8, 1]);
    }

    #[test]
    fn sps_pps_cache_does_not_prepend_when_disabled_or_missing() {
        let mut cached = Some(vec![0x00, 0x00, 0x01, 0x67, 0xaa]);
        let original = vec![0x00, 0x00, 0x01, 0x41, 0x11, 0x22];
        let mut delta = original.clone();

        apply_sps_pps_cache(FrameType::P, &mut delta, &mut cached, false);
        assert_eq!(delta, original);

        let mut missing_cache = None;
        apply_sps_pps_cache(FrameType::P, &mut delta, &mut missing_cache, true);
        assert_eq!(delta, original);
    }

    #[test]
    fn h264_encoder_rejects_zero_and_odd_dimensions_before_loading_openh264() {
        for (width, height) in [(0, 64), (64, 0), (63, 64), (64, 63)] {
            let err = match H264Encoder::new(width, height, 1_000_000, 30, 23, H264RateControl::Cqp)
            {
                Ok(_) => panic!("invalid dimensions must fail: {width}x{height}"),
                Err(err) => err,
            };
            assert!(
                err.to_string()
                    .contains("dimensions must be non-zero and even"),
                "{width}x{height} returned unexpected error: {err:#}"
            );
        }
    }

    #[test]
    fn bgra_buffer_validation_rejects_short_buffer_and_too_narrow_stride() {
        let short = validate_bgra_buffer(16, 16, 64, 1023).expect_err("short buffer fails");
        assert!(short.to_string().contains("BGRA buffer too small"));

        let narrow_stride =
            validate_bgra_buffer(16, 16, 63, 1024).expect_err("narrow stride fails");
        assert!(narrow_stride.to_string().contains("BGRA stride too small"));

        validate_bgra_buffer(16, 16, 76, 76 * 16).expect("padded stride is accepted");
    }

    #[test]
    fn h264_encoder_rejects_short_bgra_buffer_and_accepts_padded_stride() {
        let width = 16;
        let height = 16;
        let stride = width * 4 + 12;
        let mut encoder = match test_h264_encoder(width as u32, height as u32) {
            Ok(encoder) => encoder,
            Err(error) if error.contains("libopenh264") => return,
            Err(error) => panic!("H.264 encoder initialization failed: {error}"),
        };
        let valid = gradient_bgra(width, height, stride, 0);

        let error = encoder
            .encode(&valid[..valid.len() - 1], stride)
            .expect_err("short BGRA buffer must fail");
        assert!(
            error.to_string().contains("BGRA buffer too small"),
            "unexpected short-buffer error: {error:#}"
        );

        let encoded = encoder
            .encode(&valid, stride)
            .expect("padded-stride BGRA frame encodes");
        assert!(!encoded.is_empty());
    }

    #[test]
    fn h264_encoder_prepends_cached_sps_pps_to_delta_frames() {
        let width = 64;
        let height = 64;
        let stride = width * 4;
        let mut encoder = match test_h264_encoder(width as u32, height as u32) {
            Ok(encoder) => encoder,
            Err(error) if error.contains("libopenh264") => return,
            Err(error) => panic!("H.264 encoder initialization failed: {error}"),
        };

        let first = gradient_bgra(width, height, stride, 0);
        let first_encoded = encoder.encode(&first, stride).expect("first frame encodes");
        assert!(annex_b_nal_types(&first_encoded).contains(&5));
        let cached = encoder
            .cached_sps_pps
            .clone()
            .expect("IDR frame caches SPS/PPS");
        assert_eq!(annex_b_nal_types(&cached), vec![7, 8]);

        for seed in 1..=8 {
            let next = gradient_bgra(width, height, stride, seed);
            let encoded = encoder.encode(&next, stride).expect("delta frame encodes");
            if encoded.is_empty() {
                continue;
            }
            let nal_types = annex_b_nal_types(&encoded);
            if !nal_types.contains(&5) {
                assert!(
                    encoded.starts_with(&cached),
                    "delta frame did not start with cached SPS/PPS; NAL types: {nal_types:?}"
                );
                assert!(
                    nal_types.iter().skip(2).any(|nal_type| *nal_type == 1),
                    "expected a non-IDR slice after cached SPS/PPS; NAL types: {nal_types:?}"
                );
                return;
            }
        }

        panic!("OpenH264 did not produce a delta frame within the test sequence");
    }
}
