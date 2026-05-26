use anyhow::Result;

use super::encoder;
use super::frame::{EgfxFrameCodec, EncodedEgfxFrame};
#[cfg(feature = "vaapi")]
use super::vaapi;

#[cfg(test)]
pub(crate) type Avc444ReferenceRegions<'a> =
    (&'a [(i32, i32, i32, i32)], &'a [(i32, i32, i32, i32)]);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum H264RateControl {
    Vbr,
    Cqp,
}

/// Encoder backend: hardware VAAPI, common H.264 software, or AVC444 wrapper.
pub enum FrameEncoder {
    #[cfg(feature = "vaapi")]
    Vaapi(Box<vaapi::VaapiEncoder>),
    #[cfg(test)]
    FailingVaapiForTest {
        force_idr_requests: u32,
    },
    Software(Box<encoder::H264Encoder>),
    SoftwareAvc444(Box<encoder::Avc444Encoder>),
}

impl FrameEncoder {
    /// Try VAAPI first, fall back to software.
    pub fn new(
        width: u32,
        height: u32,
        bitrate: u32,
        fps: u32,
        quality: u8,
        rate_control: H264RateControl,
    ) -> Result<Self> {
        #[cfg(feature = "vaapi")]
        {
            match vaapi::VaapiEncoder::new(width, height, bitrate, fps, quality, rate_control) {
                Ok(enc) => {
                    tracing::info!("Using VA-API hardware encoder");
                    return Ok(Self::Vaapi(Box::new(enc)));
                }
                Err(e) => {
                    tracing::warn!("VA-API init failed, falling back to software: {:#}", e);
                }
            }
        }

        let enc = encoder::H264Encoder::new(width, height, bitrate, fps, quality, rate_control)?;
        tracing::info!("Using FFmpeg/libavcodec software H.264 encoder");
        Ok(Self::Software(Box::new(enc)))
    }

    pub fn encode(&mut self, bgra: &[u8], stride: usize) -> Result<Vec<u8>> {
        match self {
            #[cfg(feature = "vaapi")]
            Self::Vaapi(enc) => enc.encode(bgra, stride),
            #[cfg(test)]
            Self::FailingVaapiForTest { .. } => anyhow::bail!("test VA-API encode failure"),
            Self::Software(enc) => enc.encode(bgra, stride),
            Self::SoftwareAvc444(_) => anyhow::bail!("AVC444 encoder requires encode_avc444"),
        }
    }

    pub(crate) fn encode_egfx_frame(
        &mut self,
        codec: EgfxFrameCodec,
        bgra: &[u8],
        stride: usize,
        candidate_regions: &[(i32, i32, i32, i32)],
    ) -> Result<EncodedEgfxFrame> {
        match codec {
            EgfxFrameCodec::Avc420 => self.encode(bgra, stride).map(EncodedEgfxFrame::Avc420),
            EgfxFrameCodec::Avc444 => self
                .encode_avc444(bgra, stride, candidate_regions)
                .map(EncodedEgfxFrame::Avc444),
        }
    }

    pub fn encode_avc444(
        &mut self,
        bgra: &[u8],
        stride: usize,
        candidate_regions: &[(i32, i32, i32, i32)],
    ) -> Result<encoder::Avc444EncodedFrame> {
        match self {
            Self::SoftwareAvc444(enc) => enc.encode(bgra, stride, candidate_regions),
            #[cfg(feature = "vaapi")]
            Self::Vaapi(_) => anyhow::bail!("AVC444 encoding requires software encoder"),
            #[cfg(test)]
            Self::FailingVaapiForTest { .. } => {
                anyhow::bail!("AVC444 encoding requires software encoder")
            }
            Self::Software(_) => anyhow::bail!("AVC444 encoding requires AVC444 encoder"),
        }
    }

    pub fn commit_avc444_reference(&mut self) {
        if let Self::SoftwareAvc444(enc) = self {
            enc.commit_reference();
        }
    }

    #[cfg(test)]
    pub(crate) fn avc444_luma_reference_y_for_test(&self) -> Option<&[u8]> {
        match self {
            Self::SoftwareAvc444(enc) => enc.luma_reference_y_for_test(),
            #[cfg(feature = "vaapi")]
            Self::Vaapi(_) | Self::Software(_) => None,
            Self::FailingVaapiForTest { .. } => None,
            #[cfg(not(feature = "vaapi"))]
            Self::Software(_) => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn avc444_last_reference_regions_for_test(
        &self,
    ) -> Option<Avc444ReferenceRegions<'_>> {
        match self {
            Self::SoftwareAvc444(enc) => Some(enc.last_reference_regions_for_test()),
            #[cfg(feature = "vaapi")]
            Self::Vaapi(_) | Self::Software(_) => None,
            Self::FailingVaapiForTest { .. } => None,
            #[cfg(not(feature = "vaapi"))]
            Self::Software(_) => None,
        }
    }

    /// Encode from an NV12 DMA-BUF (zero-copy path). Only available with VA-API backend.
    #[cfg(feature = "vaapi")]
    #[allow(clippy::too_many_arguments)]
    pub fn encode_dmabuf(
        &mut self,
        nv12_fd: std::os::unix::io::RawFd,
        width: u32,
        height: u32,
        stride: u32,
        offset: u32,
        modifier: u64,
        uv_stride: u32,
        uv_offset: u32,
    ) -> Result<Vec<u8>> {
        match self {
            Self::Vaapi(enc) => enc.encode_dmabuf(
                nv12_fd, width, height, stride, offset, modifier, uv_stride, uv_offset,
            ),
            #[cfg(test)]
            Self::FailingVaapiForTest { .. } => anyhow::bail!("test VA-API DMA-BUF failure"),
            Self::Software(_) => anyhow::bail!("DMA-BUF encode requires VA-API backend"),
            Self::SoftwareAvc444(_) => anyhow::bail!("DMA-BUF encode requires VA-API backend"),
        }
    }

    pub fn backend_name(&self) -> &'static str {
        match self {
            #[cfg(feature = "vaapi")]
            Self::Vaapi(_) => "vaapi",
            #[cfg(test)]
            Self::FailingVaapiForTest { .. } => "vaapi-test-failing",
            Self::Software(_) => "ffmpeg-h264",
            Self::SoftwareAvc444(enc) => enc.backend_name(),
        }
    }

    pub fn is_vaapi(&self) -> bool {
        match self {
            #[cfg(feature = "vaapi")]
            Self::Vaapi(_) => true,
            #[cfg(test)]
            Self::FailingVaapiForTest { .. } => true,
            Self::Software(_) => false,
            Self::SoftwareAvc444(enc) => enc.is_vaapi(),
        }
    }

    /// Create a software-only encoder (fallback when VA-API fails at runtime).
    pub fn new_software_only(
        width: u32,
        height: u32,
        bitrate: u32,
        fps: u32,
        quality: u8,
        rate_control: H264RateControl,
    ) -> Result<Self> {
        let enc = encoder::H264Encoder::new(width, height, bitrate, fps, quality, rate_control)?;
        tracing::info!("Using FFmpeg/libavcodec software H.264 encoder (runtime fallback)");
        Ok(Self::Software(Box::new(enc)))
    }

    pub(crate) fn new_for_egfx_codec(
        codec: EgfxFrameCodec,
        width: u32,
        height: u32,
        bitrate: u32,
        fps: u32,
        quality: u8,
        rate_control: H264RateControl,
    ) -> Result<Self> {
        match codec {
            EgfxFrameCodec::Avc420 => Self::new(width, height, bitrate, fps, quality, rate_control),
            EgfxFrameCodec::Avc444 => {
                Self::new_avc444(width, height, bitrate, fps, quality, rate_control)
            }
        }
    }

    pub fn new_avc444(
        width: u32,
        height: u32,
        bitrate: u32,
        fps: u32,
        quality: u8,
        rate_control: H264RateControl,
    ) -> Result<Self> {
        #[cfg(feature = "vaapi")]
        {
            match encoder::Avc444Encoder::new_with_vaapi(
                width,
                height,
                bitrate,
                fps,
                quality,
                rate_control,
            ) {
                Ok(enc) => {
                    tracing::info!("Using FFmpeg/VAAPI hardware AVC444 encoder");
                    return Ok(Self::SoftwareAvc444(Box::new(enc)));
                }
                Err(e) => {
                    tracing::warn!(
                        "VA-API AVC444 init failed, falling back to software: {:#}",
                        e
                    );
                }
            }
        }

        let enc = encoder::Avc444Encoder::new(width, height, bitrate, fps, quality, rate_control)?;
        tracing::info!("Using FFmpeg/libx264 software AVC444 encoder");
        Ok(Self::SoftwareAvc444(Box::new(enc)))
    }

    pub fn new_avc444_software_only(
        width: u32,
        height: u32,
        bitrate: u32,
        fps: u32,
        quality: u8,
        rate_control: H264RateControl,
    ) -> Result<Self> {
        let enc = encoder::Avc444Encoder::new(width, height, bitrate, fps, quality, rate_control)?;
        tracing::info!("Using FFmpeg/libx264 software AVC444 encoder");
        Ok(Self::SoftwareAvc444(Box::new(enc)))
    }

    pub(crate) fn new_software_only_for_egfx_codec(
        codec: EgfxFrameCodec,
        width: u32,
        height: u32,
        bitrate: u32,
        fps: u32,
        quality: u8,
        rate_control: H264RateControl,
    ) -> Result<Self> {
        match codec {
            EgfxFrameCodec::Avc420 => {
                Self::new_software_only(width, height, bitrate, fps, quality, rate_control)
            }
            EgfxFrameCodec::Avc444 => {
                Self::new_avc444_software_only(width, height, bitrate, fps, quality, rate_control)
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn failing_vaapi_for_test() -> Self {
        Self::FailingVaapiForTest {
            force_idr_requests: 0,
        }
    }

    /// Force the next encoded frame to be an IDR (recovery after dropped frames).
    pub fn force_idr(&mut self) {
        match self {
            #[cfg(feature = "vaapi")]
            Self::Vaapi(enc) => enc.force_idr(),
            #[cfg(test)]
            Self::FailingVaapiForTest { force_idr_requests } => {
                *force_idr_requests = force_idr_requests.saturating_add(1);
            }
            Self::Software(enc) => enc.force_idr(),
            Self::SoftwareAvc444(enc) => enc.force_idr(),
        }
    }

    #[cfg(test)]
    pub(crate) fn force_idr_requests_for_test(&self) -> Option<u32> {
        match self {
            #[cfg(feature = "vaapi")]
            Self::Vaapi(_) => None,
            Self::FailingVaapiForTest { force_idr_requests } => Some(*force_idr_requests),
            Self::Software(enc) => Some(enc.force_idr_requests_for_test()),
            Self::SoftwareAvc444(enc) => Some(enc.force_idr_requests_for_test()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avc444_software_backend_reports_ffmpeg_and_not_vaapi() {
        let encoder =
            FrameEncoder::new_avc444_software_only(64, 64, 1_000_000, 30, 23, H264RateControl::Cqp)
                .expect("software AVC444 encoder initializes");

        assert_eq!(encoder.backend_name(), "ffmpeg-avc444");
        assert!(!encoder.is_vaapi());
    }
}
