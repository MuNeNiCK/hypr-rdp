use anyhow::{bail, Context, Result};
use ffmpeg_next as ffmpeg;
use std::ffi::CString;
use std::os::raw::c_int;
use std::ptr::{from_mut, null_mut};
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

/// H.264 encoder for screen capture encoding.
///
/// Pre-allocates YUV planes and uses BT.709 full-range conversion. Caches
/// SPS/PPS from IDR frames and prepends them to P-frames for Windows MFT
/// decoder compatibility.
pub struct H264Encoder {
    encoder: H264EncoderImpl,
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

        tracing::info!(
            bitrate,
            fps,
            quality = qp,
            rate_control = ?rate_control,
            vaapi = options.ffmpeg_vaapi,
            "FFmpeg/libavcodec H.264 encoder settings"
        );
        Self::from_encoder_impl(
            H264EncoderImpl::Ffmpeg(FfmpegH264Encoder::new(
                width as i32,
                height as i32,
                bitrate,
                fps,
                qp,
                rate_control,
                options.ffmpeg_backend(),
            )?),
            width,
            height,
        )
    }

    fn from_encoder_impl(encoder: H264EncoderImpl, width: u32, height: u32) -> Result<Self> {
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
        };

        self.encoder
            .encode_yuv_source(&yuv, &mut self.cached_sps_pps, true)
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
        };

        self.encoder
            .encode_yuv_source(&yuv, &mut self.cached_sps_pps, prepend_cached_sps_pps)
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

#[derive(Clone, Copy, Default)]
pub(super) struct H264EncoderOptions {
    pub(super) ffmpeg_vaapi: bool,
}

impl H264EncoderOptions {
    fn ffmpeg_backend(self) -> FfmpegH264Backend {
        if self.ffmpeg_vaapi {
            FfmpegH264Backend::Vaapi
        } else {
            FfmpegH264Backend::Software
        }
    }
}

pub(super) fn avc444_h264_encoder_options() -> H264EncoderOptions {
    H264EncoderOptions {
        ffmpeg_vaapi: false,
    }
}

#[cfg(feature = "vaapi")]
pub(super) fn avc444_h264_vaapi_encoder_options() -> H264EncoderOptions {
    H264EncoderOptions { ffmpeg_vaapi: true }
}

enum H264EncoderImpl {
    Ffmpeg(FfmpegH264Encoder),
}

impl H264EncoderImpl {
    fn encode_yuv_source(
        &mut self,
        yuv: &impl Yuv420Source,
        cached_sps_pps: &mut Option<Vec<u8>>,
        prepend_cached_sps_pps: bool,
    ) -> Result<EncodedH264> {
        match self {
            Self::Ffmpeg(encoder) => {
                encoder.encode_yuv_source(yuv, cached_sps_pps, prepend_cached_sps_pps)
            }
        }
    }

    fn force_intra_frame(&mut self) {
        match self {
            Self::Ffmpeg(encoder) => encoder.force_intra_frame(),
        }
    }
}

struct FfmpegH264Encoder {
    encoder: ffmpeg::encoder::Video,
    width: usize,
    height: usize,
    frame_index: i64,
    force_idr: bool,
    codec_headers: Option<Vec<u8>>,
    backend: FfmpegH264Backend,
    hw_device_ctx: *mut ffmpeg::ffi::AVBufferRef,
}

impl FfmpegH264Encoder {
    fn new(
        width: i32,
        height: i32,
        bitrate: u32,
        fps: u32,
        qp: u8,
        rate_control: H264RateControl,
        backend: FfmpegH264Backend,
    ) -> Result<Self> {
        ffmpeg::init().context("failed to initialize FFmpeg")?;
        ffmpeg::log::set_level(ffmpeg::log::Level::Quiet);
        let codec = match backend {
            FfmpegH264Backend::Software => ffmpeg::encoder::find_by_name("libx264")
                .or_else(|| ffmpeg::encoder::find(ffmpeg::codec::Id::H264))
                .context("FFmpeg H.264 encoder not found")?,
            FfmpegH264Backend::Vaapi => ffmpeg::encoder::find_by_name("h264_vaapi")
                .context("FFmpeg h264_vaapi encoder not found")?,
        };
        let mut encoder = ffmpeg::codec::context::Context::new_with_codec(codec)
            .encoder()
            .video()
            .context("failed to create FFmpeg H.264 encoder context")?;

        encoder.set_width(width as u32);
        encoder.set_height(height as u32);
        encoder.set_format(backend.pixel_format());
        encoder.set_time_base(ffmpeg::Rational(1, fps as i32));
        encoder.set_frame_rate(Some(ffmpeg::Rational(fps as i32, 1)));
        encoder.set_max_b_frames(0);
        encoder.set_gop(fps.saturating_mul(10));
        encoder.set_color_range(ffmpeg::color::Range::JPEG);
        encoder.set_colorspace(ffmpeg::color::Space::BT709);
        encoder.set_flags(ffmpeg::codec::flag::Flags::LOOP_FILTER);
        unsafe {
            (*encoder.as_mut_ptr()).delay = 0;
        }
        if matches!(rate_control, H264RateControl::Vbr) {
            encoder.set_bit_rate(bitrate as usize);
        }

        let options = ffmpeg_h264_encoder_options(backend, rate_control, qp);

        let mut hw_device_ctx = null_mut();
        if matches!(backend, FfmpegH264Backend::Vaapi) {
            hw_device_ctx = create_freerdp_vaapi_device()?;
            configure_freerdp_vaapi_frames(&mut encoder, hw_device_ctx, width, height)?;
        }

        let encoder = encoder
            .open_as_with(codec, options)
            .context("failed to open FFmpeg H.264 encoder")?;
        let codec_headers = h264_headers_from_avcodec_context(unsafe { encoder.as_ptr() });

        Ok(Self {
            encoder,
            width: width as usize,
            height: height as usize,
            frame_index: 1,
            force_idr: true,
            codec_headers,
            backend,
            hw_device_ctx,
        })
    }

    fn encode_yuv_source(
        &mut self,
        yuv: &impl Yuv420Source,
        cached_sps_pps: &mut Option<Vec<u8>>,
        prepend_cached_sps_pps: bool,
    ) -> Result<EncodedH264> {
        let mut frame = self.create_input_frame(yuv)?;
        set_freerdp_ffmpeg_frame_metadata(&mut frame, self.frame_index);
        let forced_keyframe = self.force_idr;
        if forced_keyframe {
            mark_ffmpeg_h264_keyframe(&mut frame);
            self.force_idr = false;
        } else {
            frame.set_kind(ffmpeg::picture::Type::None);
        }
        self.frame_index = self.frame_index.saturating_add(1);

        self.encoder
            .send_frame(&frame)
            .context("FFmpeg H.264 send_frame failed")?;

        let mut data = Vec::new();
        let mut packet_key = false;
        loop {
            let mut packet = ffmpeg::Packet::empty();
            match self.encoder.receive_packet(&mut packet) {
                Ok(()) => {
                    packet_key |= packet.is_key();
                    if let Some(packet_data) = packet.data() {
                        data.extend_from_slice(packet_data);
                    }
                }
                Err(ffmpeg::Error::Other { errno }) if errno == libc::EAGAIN => break,
                Err(ffmpeg::Error::Eof) => break,
                Err(error) => return Err(error).context("FFmpeg H.264 receive_packet failed"),
            }
        }
        ensure_ffmpeg_vaapi_packet_progress(self.backend, &data)?;

        if self.codec_headers.is_none() {
            self.codec_headers =
                h264_headers_from_avcodec_context(unsafe { self.encoder.as_ptr() });
        }
        prepend_codec_headers_for_bootstrap(
            &mut data,
            self.codec_headers.as_deref(),
            forced_keyframe || packet_key,
        );

        let frame_type = if data.is_empty() {
            H264FrameType::Skip
        } else if annex_b_nal_types(&data).contains(&5) {
            H264FrameType::Idr
        } else if packet_key {
            H264FrameType::I
        } else {
            H264FrameType::P
        };

        apply_sps_pps_cache(
            frame_type,
            &mut data,
            cached_sps_pps,
            prepend_cached_sps_pps,
        );
        Ok(EncodedH264 { data, frame_type })
    }

    fn force_intra_frame(&mut self) {
        self.force_idr = true;
    }

    fn create_input_frame(&mut self, yuv: &impl Yuv420Source) -> Result<ffmpeg::frame::Video> {
        match self.backend {
            FfmpegH264Backend::Software => {
                let mut frame = ffmpeg::frame::Video::new(
                    ffmpeg::format::Pixel::YUV420P,
                    self.width as u32,
                    self.height as u32,
                );
                copy_yuv420p_to_frame(yuv, &mut frame, self.width, self.height);
                Ok(frame)
            }
            FfmpegH264Backend::Vaapi => self.create_vaapi_input_frame(yuv),
        }
    }

    fn create_vaapi_input_frame(
        &mut self,
        yuv: &impl Yuv420Source,
    ) -> Result<ffmpeg::frame::Video> {
        let mut software_frame = ffmpeg::frame::Video::new(
            ffmpeg::format::Pixel::NV12,
            self.width as u32,
            self.height as u32,
        );
        set_freerdp_ffmpeg_frame_color(&mut software_frame);
        copy_yuv420p_to_nv12_frame(yuv, &mut software_frame, self.width, self.height);

        let mut hardware_frame = ffmpeg::frame::Video::empty();
        let status = unsafe {
            ffmpeg::ffi::av_hwframe_get_buffer(
                (*self.encoder.as_mut_ptr()).hw_frames_ctx,
                hardware_frame.as_mut_ptr(),
                0,
            )
        };
        anyhow::ensure!(
            status >= 0,
            "FFmpeg VAAPI av_hwframe_get_buffer failed: {}",
            ffmpeg_status(status)
        );

        let status = unsafe {
            ffmpeg::ffi::av_hwframe_transfer_data(
                hardware_frame.as_mut_ptr(),
                software_frame.as_ptr(),
                0,
            )
        };
        anyhow::ensure!(
            status >= 0,
            "FFmpeg VAAPI av_hwframe_transfer_data failed: {}",
            ffmpeg_status(status)
        );

        Ok(hardware_frame)
    }
}

fn ffmpeg_h264_encoder_options(
    backend: FfmpegH264Backend,
    rate_control: H264RateControl,
    qp: u8,
) -> ffmpeg::Dictionary<'static> {
    let mut options = ffmpeg::Dictionary::new();
    options.set("preset", backend.freerdp_preset());
    options.set("tune", "zerolatency");
    match backend {
        FfmpegH264Backend::Software => {
            options.set("repeat-headers", "1");
            options.set("annexb", "1");
            options.set("open-gop", "0");
            // AVC444 alternates luma/chroma pictures in one H.264 sequence; an
            // autonomous scene-cut on the chroma picture resets references mid-LC.
            options.set("sc_threshold", "0");
        }
        FfmpegH264Backend::Vaapi => {
            options.set("idr_interval", "1");
            options.set("async_depth", "1");
            options.set("quality", "1");
            if matches!(rate_control, H264RateControl::Cqp) {
                options.set("rc_mode", "CQP");
            }
        }
    }
    if matches!(rate_control, H264RateControl::Cqp) {
        let qp = qp.min(51).to_string();
        options.set("qp", &qp);
    }
    options
}

fn mark_ffmpeg_h264_keyframe(frame: &mut ffmpeg::frame::Video) {
    frame.set_kind(ffmpeg::picture::Type::I);
    unsafe {
        (*frame.as_mut_ptr()).flags |= ffmpeg::ffi::AV_FRAME_FLAG_KEY;
    }
}

fn ensure_ffmpeg_vaapi_packet_progress(backend: FfmpegH264Backend, data: &[u8]) -> Result<()> {
    if data.is_empty() && matches!(backend, FfmpegH264Backend::Vaapi) {
        bail!("FFmpeg VAAPI H.264 receive_packet produced no packet for the submitted frame");
    }
    Ok(())
}

fn set_freerdp_ffmpeg_frame_metadata(frame: &mut ffmpeg::frame::Video, pts: i64) {
    frame.set_pts(Some(pts));
    set_freerdp_ffmpeg_frame_color(frame);
}

fn set_freerdp_ffmpeg_frame_color(frame: &mut ffmpeg::frame::Video) {
    frame.set_color_space(ffmpeg::color::Space::BT709);
    frame.set_color_range(ffmpeg::color::Range::JPEG);
    unsafe {
        (*frame.as_mut_ptr()).chroma_location = ffmpeg::ffi::AVChromaLocation::AVCHROMA_LOC_LEFT;
    }
}

fn h264_headers_from_avcodec_context(
    context: *const ffmpeg::ffi::AVCodecContext,
) -> Option<Vec<u8>> {
    if context.is_null() {
        return None;
    }

    let (extradata, extradata_size) = unsafe { ((*context).extradata, (*context).extradata_size) };
    if extradata.is_null() || extradata_size <= 0 {
        return None;
    }

    let extradata = unsafe { std::slice::from_raw_parts(extradata, extradata_size as usize) };
    h264_headers_from_extradata(extradata)
}

fn h264_headers_from_extradata(extradata: &[u8]) -> Option<Vec<u8>> {
    if let Some(headers) = extract_sps_pps(extradata) {
        return Some(headers);
    }

    h264_headers_from_avcc_extradata(extradata)
}

fn h264_headers_from_avcc_extradata(extradata: &[u8]) -> Option<Vec<u8>> {
    if extradata.len() < 7 || extradata[0] != 1 {
        return None;
    }

    let mut offset = 5;
    let sps_count = extradata[offset] & 0x1f;
    offset += 1;

    let mut headers = Vec::new();
    for _ in 0..sps_count {
        append_avcc_parameter_set(extradata, &mut offset, &mut headers)?;
    }

    if offset >= extradata.len() {
        return None;
    }
    let pps_count = extradata[offset];
    offset += 1;
    for _ in 0..pps_count {
        append_avcc_parameter_set(extradata, &mut offset, &mut headers)?;
    }

    if headers.is_empty() {
        None
    } else {
        Some(headers)
    }
}

fn append_avcc_parameter_set(
    extradata: &[u8],
    offset: &mut usize,
    headers: &mut Vec<u8>,
) -> Option<()> {
    if *offset + 2 > extradata.len() {
        return None;
    }
    let len = u16::from_be_bytes([extradata[*offset], extradata[*offset + 1]]) as usize;
    *offset += 2;
    if len == 0 || *offset + len > extradata.len() {
        return None;
    }

    headers.extend_from_slice(&[0, 0, 0, 1]);
    headers.extend_from_slice(&extradata[*offset..*offset + len]);
    *offset += len;
    Some(())
}

fn prepend_codec_headers_for_bootstrap(
    data: &mut Vec<u8>,
    codec_headers: Option<&[u8]>,
    bootstrap_frame: bool,
) {
    let Some(codec_headers) = codec_headers else {
        return;
    };
    if data.is_empty()
        || !bootstrap_frame
        || extract_sps_pps(data).is_some()
        || codec_headers.is_empty()
    {
        return;
    }

    let mut combined = Vec::with_capacity(codec_headers.len() + data.len());
    combined.extend_from_slice(codec_headers);
    combined.extend_from_slice(data);
    *data = combined;
}

impl Drop for FfmpegH264Encoder {
    fn drop(&mut self) {
        if !self.hw_device_ctx.is_null() {
            unsafe {
                ffmpeg::ffi::av_buffer_unref(from_mut(&mut self.hw_device_ctx));
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FfmpegH264Backend {
    Software,
    Vaapi,
}

impl FfmpegH264Backend {
    fn pixel_format(self) -> ffmpeg::format::Pixel {
        match self {
            Self::Software => ffmpeg::format::Pixel::YUV420P,
            Self::Vaapi => ffmpeg::format::Pixel::VAAPI,
        }
    }

    fn freerdp_preset(self) -> &'static str {
        match self {
            Self::Software => "medium",
            Self::Vaapi => "veryslow",
        }
    }
}

fn create_freerdp_vaapi_device() -> Result<*mut ffmpeg::ffi::AVBufferRef> {
    let device = std::env::var("FREERDP_VAAPI_DEVICE")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "/dev/dri/renderD128".to_owned());
    let device = CString::new(device).context("VAAPI device path contains NUL byte")?;
    let mut hw_device_ctx = null_mut();
    let status = unsafe {
        ffmpeg::ffi::av_hwdevice_ctx_create(
            from_mut(&mut hw_device_ctx),
            ffmpeg::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
            device.as_ptr(),
            null_mut(),
            0,
        )
    };
    anyhow::ensure!(
        status >= 0,
        "FFmpeg VAAPI av_hwdevice_ctx_create failed: {}",
        ffmpeg_status(status)
    );
    Ok(hw_device_ctx)
}

fn configure_freerdp_vaapi_frames(
    encoder: &mut ffmpeg::codec::encoder::video::Video,
    hw_device_ctx: *mut ffmpeg::ffi::AVBufferRef,
    width: i32,
    height: i32,
) -> Result<()> {
    let mut hw_frames_ctx = unsafe { ffmpeg::ffi::av_hwframe_ctx_alloc(hw_device_ctx) };
    anyhow::ensure!(
        !hw_frames_ctx.is_null(),
        "failed to create VAAPI frame context"
    );

    let init_status = unsafe {
        let frames = (*hw_frames_ctx)
            .data
            .cast::<ffmpeg::ffi::AVHWFramesContext>();
        (*frames).format = ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
        (*frames).sw_format = ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_NV12;
        (*frames).width = width;
        (*frames).height = height;
        (*frames).initial_pool_size = FREERDP_VAAPI_INITIAL_POOL_SIZE;
        ffmpeg::ffi::av_hwframe_ctx_init(hw_frames_ctx)
    };

    if init_status < 0 {
        unsafe {
            ffmpeg::ffi::av_buffer_unref(from_mut(&mut hw_frames_ctx));
        }
        anyhow::bail!(
            "FFmpeg VAAPI av_hwframe_ctx_init failed: {}",
            ffmpeg_status(init_status)
        );
    }

    let hw_frames_ref = unsafe { ffmpeg::ffi::av_buffer_ref(hw_frames_ctx) };
    unsafe {
        ffmpeg::ffi::av_buffer_unref(from_mut(&mut hw_frames_ctx));
    }
    anyhow::ensure!(
        !hw_frames_ref.is_null(),
        "failed to reference VAAPI frame context"
    );

    unsafe {
        (*encoder.as_mut_ptr()).hw_frames_ctx = hw_frames_ref;
    }
    Ok(())
}

const FREERDP_VAAPI_INITIAL_POOL_SIZE: c_int = 20;

fn ffmpeg_status(status: c_int) -> String {
    let mut buffer = [0i8; 128];
    let result = unsafe { ffmpeg::ffi::av_strerror(status, buffer.as_mut_ptr(), buffer.len()) };
    if result < 0 {
        return format!("status {status}");
    }

    unsafe { std::ffi::CStr::from_ptr(buffer.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

fn copy_yuv420p_to_frame(
    yuv: &impl Yuv420Source,
    frame: &mut ffmpeg::frame::Video,
    width: usize,
    height: usize,
) {
    let y_stride = frame.stride(0);
    let u_stride = frame.stride(1);
    let v_stride = frame.stride(2);
    copy_yuv_plane(yuv.y(), width, frame.data_mut(0), y_stride, height);
    copy_yuv_plane(yuv.u(), width / 2, frame.data_mut(1), u_stride, height / 2);
    copy_yuv_plane(yuv.v(), width / 2, frame.data_mut(2), v_stride, height / 2);
}

fn copy_yuv420p_to_nv12_frame(
    yuv: &impl Yuv420Source,
    frame: &mut ffmpeg::frame::Video,
    width: usize,
    height: usize,
) {
    let y_stride = frame.stride(0);
    copy_yuv_plane(yuv.y(), width, frame.data_mut(0), y_stride, height);

    let uv_stride = frame.stride(1);
    let uv = frame.data_mut(1);
    for row in 0..height / 2 {
        let uv_row = row * uv_stride;
        let plane_row = row * (width / 2);
        for x in 0..width / 2 {
            uv[uv_row + x * 2] = yuv.u()[plane_row + x];
            uv[uv_row + x * 2 + 1] = yuv.v()[plane_row + x];
        }
    }
}

fn copy_yuv_plane(src: &[u8], src_stride: usize, dst: &mut [u8], dst_stride: usize, height: usize) {
    for row in 0..height {
        let src_start = row * src_stride;
        let dst_start = row * dst_stride;
        dst[dst_start..dst_start + src_stride]
            .copy_from_slice(&src[src_start..src_start + src_stride]);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum H264FrameType {
    Skip,
    Idr,
    I,
    P,
}

pub(super) struct EncodedH264 {
    pub(super) data: Vec<u8>,
    pub(super) frame_type: H264FrameType,
}

impl EncodedH264 {
    pub(super) fn empty() -> Self {
        Self {
            data: Vec::new(),
            frame_type: H264FrameType::Skip,
        }
    }
}

pub(super) fn is_h264_keyframe(frame_type: H264FrameType) -> bool {
    frame_type == H264FrameType::Idr || frame_type == H264FrameType::I
}

#[cfg(any(feature = "vaapi", test))]
pub(super) fn initial_h264_bootstrap_is_sendable(encoded: &EncodedH264) -> bool {
    !encoded.data.is_empty()
        && is_h264_keyframe(encoded.frame_type)
        && extract_sps_pps(&encoded.data).is_some()
}

fn apply_sps_pps_cache(
    frame_type: H264FrameType,
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

trait Yuv420Source {
    fn y(&self) -> &[u8];
    fn u(&self) -> &[u8];
    fn v(&self) -> &[u8];
}

/// Reference to pre-allocated YUV planes.
struct YuvRef<'a> {
    y: &'a [u8],
    u: &'a [u8],
    v: &'a [u8],
}

impl Yuv420Source for YuvRef<'_> {
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
    use proptest::prelude::*;

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
    fn default_h264_encoder_options_use_freerdp_libavcodec_backend() {
        let options = H264EncoderOptions::default();

        assert!(!options.ffmpeg_vaapi);
    }

    #[test]
    fn ffmpeg_libavcodec_context_keeps_freerdp_loop_filter_and_delay_policy() {
        let encoder = match FfmpegH264Encoder::new(
            64,
            64,
            1_000_000,
            30,
            23,
            H264RateControl::Vbr,
            FfmpegH264Backend::Software,
        ) {
            Ok(encoder) => encoder,
            Err(error) if format!("{error:#}").contains("FFmpeg H.264 encoder not found") => {
                return;
            }
            Err(error) => panic!("FFmpeg H.264 encoder initialization failed: {error:#}"),
        };

        let (flags, delay) = unsafe {
            let context = encoder.encoder.as_ptr();
            (
                ffmpeg::codec::flag::Flags::from_bits_truncate((*context).flags as _),
                (*context).delay,
            )
        };

        assert!(
            flags.contains(ffmpeg::codec::flag::Flags::LOOP_FILTER),
            "FreeRDP libavcodec sets AV_CODEC_FLAG_LOOP_FILTER"
        );
        assert_eq!(delay, 0, "FreeRDP libavcodec sets delay=0");
    }

    #[test]
    fn ffmpeg_libavcodec_initial_frame_requests_keyframe_bootstrap() {
        let encoder = match FfmpegH264Encoder::new(
            64,
            64,
            1_000_000,
            30,
            23,
            H264RateControl::Vbr,
            FfmpegH264Backend::Software,
        ) {
            Ok(encoder) => encoder,
            Err(error) if format!("{error:#}").contains("FFmpeg H.264 encoder not found") => {
                return;
            }
            Err(error) => panic!("FFmpeg H.264 encoder initialization failed: {error:#}"),
        };

        assert!(
            encoder.force_idr,
            "initial FFmpeg AVC stream must not start with a P-slice"
        );
    }

    #[test]
    fn ffmpeg_forced_keyframe_sets_picture_type_and_frame_key_flag() {
        let mut frame = ffmpeg::frame::Video::new(ffmpeg::format::Pixel::YUV420P, 64, 64);

        mark_ffmpeg_h264_keyframe(&mut frame);

        assert_eq!(frame.kind(), ffmpeg::picture::Type::I);
        assert!(
            frame.is_key(),
            "FFmpeg 8 key frames are marked with AV_FRAME_FLAG_KEY"
        );
    }

    #[test]
    fn ffmpeg_vaapi_backend_policy_matches_freerdp_hardware_frames_path() {
        assert_eq!(
            FfmpegH264Backend::Vaapi.pixel_format(),
            ffmpeg::format::Pixel::VAAPI
        );
        assert_eq!(FfmpegH264Backend::Vaapi.freerdp_preset(), "veryslow");
        assert_eq!(FREERDP_VAAPI_INITIAL_POOL_SIZE, 20);
    }

    #[test]
    fn ffmpeg_vaapi_vbr_options_leave_rate_control_to_libavcodec() {
        let options =
            ffmpeg_h264_encoder_options(FfmpegH264Backend::Vaapi, H264RateControl::Vbr, 23);

        assert_eq!(options.get("preset"), Some("veryslow"));
        assert_eq!(options.get("tune"), Some("zerolatency"));
        assert_eq!(options.get("idr_interval"), Some("1"));
        assert_eq!(options.get("async_depth"), Some("1"));
        assert_eq!(options.get("quality"), Some("1"));
        assert_eq!(options.get("rc_mode"), None);
        assert_eq!(options.get("repeat-headers"), None);
        assert_eq!(options.get("annexb"), None);
    }

    #[test]
    fn ffmpeg_vaapi_cqp_options_select_cqp_rate_control() {
        let options =
            ffmpeg_h264_encoder_options(FfmpegH264Backend::Vaapi, H264RateControl::Cqp, 23);

        assert_eq!(options.get("rc_mode"), Some("CQP"));
        assert_eq!(options.get("qp"), Some("23"));
        assert_eq!(options.get("quality"), Some("1"));
    }

    #[test]
    fn ffmpeg_frame_metadata_matches_freerdp_libavcodec_input() {
        let mut frame = ffmpeg::frame::Video::new(ffmpeg::format::Pixel::YUV420P, 64, 64);

        set_freerdp_ffmpeg_frame_metadata(&mut frame, 1);

        assert_eq!(frame.pts(), Some(1));
        assert_eq!(frame.color_space(), ffmpeg::color::Space::BT709);
        assert_eq!(frame.color_range(), ffmpeg::color::Range::JPEG);
        assert_eq!(frame.chroma_location(), ffmpeg::chroma::Location::Left);
    }

    #[test]
    fn ffmpeg_vaapi_no_packet_output_is_encode_failure() {
        let error = ensure_ffmpeg_vaapi_packet_progress(FfmpegH264Backend::Vaapi, &[])
            .expect_err("VAAPI no-packet output must not become a sendable skip");

        assert!(format!("{error:#}").contains("produced no packet"));
        assert!(ensure_ffmpeg_vaapi_packet_progress(FfmpegH264Backend::Software, &[]).is_ok());
        assert!(ensure_ffmpeg_vaapi_packet_progress(FfmpegH264Backend::Vaapi, &[1]).is_ok());
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
    fn avcc_extradata_is_converted_to_annex_b_sps_pps() {
        let extradata = [
            1, 0x64, 0, 0x1f, 0xff, 0xe1, 0x00, 0x03, 0x67, 0xaa, 0xbb, 0x01, 0x00, 0x02, 0x68,
            0xcc,
        ];

        let headers = h264_headers_from_extradata(&extradata).expect("AVCC headers");

        assert_eq!(
            headers,
            vec![0x00, 0x00, 0x00, 0x01, 0x67, 0xaa, 0xbb, 0x00, 0x00, 0x00, 0x01, 0x68, 0xcc,]
        );
        assert_eq!(annex_b_nal_types(&headers), vec![7, 8]);
    }

    #[test]
    fn bootstrap_header_prepend_rejects_type_one_only_initial_output() {
        let mut data = vec![0x00, 0x00, 0x01, 0x61, 0x11];
        let headers = vec![0x00, 0x00, 0x01, 0x67, 0xaa, 0x00, 0x00, 0x01, 0x68, 0xbb];

        assert!(!initial_h264_bootstrap_is_sendable(&EncodedH264 {
            data: data.clone(),
            frame_type: H264FrameType::P,
        }));

        prepend_codec_headers_for_bootstrap(&mut data, Some(&headers), true);

        assert_eq!(annex_b_nal_types(&data), vec![7, 8, 1]);
        assert!(initial_h264_bootstrap_is_sendable(&EncodedH264 {
            data,
            frame_type: H264FrameType::I,
        }));
    }

    #[test]
    fn initial_bootstrap_requires_parameter_sets_and_key_picture() {
        let idr_without_headers = EncodedH264 {
            data: vec![0x00, 0x00, 0x01, 0x65, 0x11],
            frame_type: H264FrameType::Idr,
        };
        let headers_with_delta = EncodedH264 {
            data: vec![
                0x00, 0x00, 0x01, 0x67, 0xaa, 0x00, 0x00, 0x01, 0x68, 0xbb, 0x00, 0x00, 0x01, 0x61,
                0xcc,
            ],
            frame_type: H264FrameType::P,
        };
        let complete = EncodedH264 {
            data: vec![
                0x00, 0x00, 0x01, 0x67, 0xaa, 0x00, 0x00, 0x01, 0x68, 0xbb, 0x00, 0x00, 0x01, 0x65,
                0xcc,
            ],
            frame_type: H264FrameType::Idr,
        };

        assert!(!initial_h264_bootstrap_is_sendable(&idr_without_headers));
        assert!(!initial_h264_bootstrap_is_sendable(&headers_with_delta));
        assert!(initial_h264_bootstrap_is_sendable(&complete));
    }

    proptest! {
        #[test]
        fn generated_annex_b_nal_type_scan_reports_each_generated_nal(
            prefix in proptest::collection::vec(1u8..=255, 0..8),
            nals in proptest::collection::vec(
                (any::<bool>(), 0u8..32, proptest::collection::vec(1u8..=255, 0..8)),
                0..24
            ),
            trailing_start in any::<bool>(),
        ) {
            let mut stream = prefix;
            let mut expected = Vec::with_capacity(nals.len());

            for (use_four_byte_start, nal_type, payload) in nals {
                if use_four_byte_start {
                    stream.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
                } else {
                    stream.extend_from_slice(&[0x00, 0x00, 0x01]);
                }
                stream.push(0x60 | nal_type);
                stream.extend_from_slice(&payload);
                expected.push(nal_type);
            }

            if trailing_start {
                stream.extend_from_slice(&[0x00, 0x00, 0x01]);
            }

            prop_assert_eq!(annex_b_nal_types(&stream), expected);
        }

        #[test]
        fn generated_sps_pps_extraction_keeps_only_parameter_sets(
            prefix in proptest::collection::vec(1u8..=255, 0..8),
            nals in proptest::collection::vec(
                (any::<bool>(), 0u8..32, proptest::collection::vec(1u8..=255, 0..8)),
                0..24
            ),
        ) {
            let mut stream = prefix;
            let mut expected = Vec::new();

            for (use_four_byte_start, nal_type, payload) in nals {
                let start_code: &[u8] = if use_four_byte_start {
                    &[0x00, 0x00, 0x00, 0x01]
                } else {
                    &[0x00, 0x00, 0x01]
                };
                let header = 0x60 | nal_type;

                stream.extend_from_slice(start_code);
                stream.push(header);
                stream.extend_from_slice(&payload);

                if nal_type == 7 || nal_type == 8 {
                    expected.extend_from_slice(start_code);
                    expected.push(header);
                    expected.extend_from_slice(&payload);
                }
            }

            match extract_sps_pps(&stream) {
                Some(extracted) => prop_assert_eq!(extracted, expected),
                None => prop_assert!(expected.is_empty()),
            }
        }
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
    fn sps_pps_cache_is_updated_from_idr_and_prepended_to_delta() {
        let mut cached = None;
        let mut idr = vec![
            0x00, 0x00, 0x01, 0x67, 0xaa, 0xbb, // SPS
            0x00, 0x00, 0x01, 0x68, 0xcc, 0xdd, // PPS
            0x00, 0x00, 0x01, 0x65, 0xee, 0xff, // IDR
        ];

        apply_sps_pps_cache(H264FrameType::Idr, &mut idr, &mut cached, true);

        let expected_parameter_sets = vec![
            0x00, 0x00, 0x01, 0x67, 0xaa, 0xbb, 0x00, 0x00, 0x01, 0x68, 0xcc, 0xdd,
        ];
        assert_eq!(cached, Some(expected_parameter_sets.clone()));

        let mut delta = vec![0x00, 0x00, 0x01, 0x41, 0x11, 0x22];
        apply_sps_pps_cache(H264FrameType::P, &mut delta, &mut cached, true);

        assert!(delta.starts_with(&expected_parameter_sets));
        assert_eq!(annex_b_nal_types(&delta), vec![7, 8, 1]);
    }

    #[test]
    fn sps_pps_cache_does_not_prepend_when_disabled_or_missing() {
        let mut cached = Some(vec![0x00, 0x00, 0x01, 0x67, 0xaa]);
        let original = vec![0x00, 0x00, 0x01, 0x41, 0x11, 0x22];
        let mut delta = original.clone();

        apply_sps_pps_cache(H264FrameType::P, &mut delta, &mut cached, false);
        assert_eq!(delta, original);

        let mut missing_cache = None;
        apply_sps_pps_cache(H264FrameType::P, &mut delta, &mut missing_cache, true);
        assert_eq!(delta, original);
    }

    #[test]
    fn h264_encoder_rejects_zero_and_odd_dimensions_before_backend_init() {
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
            Err(error) if h264_backend_unavailable(&error) => return,
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
            Err(error) if h264_backend_unavailable(&error) => return,
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

        panic!("FFmpeg H.264 encoder did not produce a delta frame within the test sequence");
    }

    fn h264_backend_unavailable(error: &str) -> bool {
        error.contains("FFmpeg H.264 encoder not found")
            || error.contains("failed to initialize FFmpeg H.264")
    }
}
