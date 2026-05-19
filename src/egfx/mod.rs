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

pub use backend::{FrameEncoder, H264RateControl};
pub use factory::HyprGfxFactory;
#[cfg(feature = "vaapi")]
pub(crate) use h264::extract_sps_pps;
#[cfg(test)]
pub(crate) use rdpegfx::rdpegfx_full_frame_region;
pub(crate) use rdpegfx::rdpegfx_region_quality;
pub use shared::{EgfxShared, DEFAULT_MAX_FRAMES_IN_FLIGHT};
#[cfg(feature = "vaapi")]
pub(crate) use vpp::{VppConverter, VppDmaBufInfo};

#[cfg(test)]
mod tests;
