mod avc420;
mod avc444;
mod backend;
pub mod encoder;
mod factory;
mod h264;
mod rdpegfx;
mod shared;
#[cfg(feature = "vaapi")]
mod vaapi;
#[cfg(feature = "vaapi")]
mod vpp;

#[cfg(test)]
pub(crate) use avc420::avc420_full_frame_region;
pub use backend::{FrameEncoder, H264RateControl};
pub use factory::HyprGfxFactory;
#[cfg(feature = "vaapi")]
pub(crate) use h264::extract_sps_pps;
pub(crate) use shared::EgfxFrameReadiness;
pub use shared::{EgfxCodecPolicy, EgfxShared, DEFAULT_MAX_FRAMES_IN_FLIGHT};
#[cfg(feature = "vaapi")]
pub(crate) use vpp::{VppConverter, VppDmaBufInfo};

#[cfg(test)]
mod tests;
