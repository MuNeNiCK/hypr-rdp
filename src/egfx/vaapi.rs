//! VA-API H.264 hardware encoder for Intel/AMD GPUs.
//!
//! Uses VA-API for GPU-accelerated H.264 encoding. Surfaces are pooled for
//! triple-buffering, and a separate reconstructed surface pool enables true
//! inter-frame prediction via DPB tracking.
//!
//! Thread Safety: NOT Send — VA-API is thread-local. Create and use on the
//! same thread (the capture thread satisfies this).

use std::mem;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{bail, Context, Result};
use libva_sys::va_display_drm as va;

use super::vaapi_sys::{
    self as sys, VAConfigAttrib, VADRMPRIMESurfaceDescriptor, VAImageFormat, VAProfile, VaBuffer,
    VaConfig, VaContext, VaDisplay, VaImageMapping, VaSurface, VA_INVALID_ID, VA_INVALID_SURFACE,
    VA_PICTURE_H264_INVALID, VA_PICTURE_H264_SHORT_TERM_REFERENCE, VA_RT_FORMAT_YUV420,
};
use super::H264RateControl;

const INPUT_SURFACE_POOL_SIZE: usize = 3;
const CODED_BUFFER_COUNT: usize = 3;

const SLICE_TYPE_I: u8 = 2;
const SLICE_TYPE_P: u8 = 0;

const IDR_INTERVAL: u32 = 30;
const H264_DEFAULT_PIC_INIT_QP: u8 = 26;

/// Scan /dev/dri/renderD* for a VA-API device that supports H.264 encoding.
fn find_vaapi_device() -> Result<PathBuf> {
    let dri_path = Path::new("/dev/dri");
    if !dri_path.exists() {
        bail!("/dev/dri not found");
    }

    let mut devices: Vec<PathBuf> = std::fs::read_dir(dri_path)
        .context("failed to read /dev/dri")?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("renderD"))
        .map(|e| e.path())
        .collect();
    devices.sort();

    if devices.is_empty() {
        bail!("no render devices found in /dev/dri");
    }

    for device in &devices {
        let display = match VaDisplay::open_drm(device) {
            Ok(d) => d,
            Err(e) => {
                tracing::trace!(device = %device.display(), "Skipping VA-API device: {:?}", e);
                continue;
            }
        };

        let profiles = match display.query_config_profiles() {
            Ok(p) => p,
            Err(_) => continue,
        };

        let h264_profile = if profiles.contains(&sys::VA_PROFILE_H264_HIGH) {
            sys::VA_PROFILE_H264_HIGH
        } else if profiles.contains(&sys::VA_PROFILE_H264_MAIN) {
            sys::VA_PROFILE_H264_MAIN
        } else {
            continue;
        };

        let entrypoints = match display.query_config_entrypoints(h264_profile) {
            Ok(e) => e,
            Err(_) => continue,
        };

        if entrypoints.contains(&sys::VA_ENTRYPOINT_ENC_SLICE) {
            let vendor = display
                .query_vendor_string()
                .unwrap_or_else(|_| "unknown".into());
            tracing::trace!(
                device = %device.display(),
                vendor = %vendor,
                profile = ?h264_profile,
                "Found VA-API device with H.264 encode support"
            );
            return Ok(device.clone());
        }
    }

    bail!(
        "no VA-API device with H.264 encode support found (checked {} devices)",
        devices.len()
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DpbEntry {
    surface_id: u32,
    frame_num: u16,
    poc: i32,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum VaapiAvc444Subframe {
    Luma,
    Chroma,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VaapiEncodeRole {
    Generic,
    #[cfg(test)]
    Avc444(VaapiAvc444Subframe),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VaapiReferenceMode {
    Single,
    #[cfg(test)]
    Avc444Subframes,
}

impl VaapiReferenceMode {
    fn max_num_ref_frames(self) -> u32 {
        1
    }

    fn recon_surface_pool_size(self) -> usize {
        self.max_num_ref_frames() as usize + 1
    }

    fn uses_periodic_idr(self) -> bool {
        self == Self::Single
    }
}

#[derive(Clone, Debug, Default)]
struct VaapiReferenceState {
    last: Option<DpbEntry>,
    #[cfg(test)]
    avc444_luma: Option<DpbEntry>,
    #[cfg(test)]
    avc444_chroma: Option<DpbEntry>,
}

impl VaapiReferenceState {
    fn clear(&mut self) {
        self.last = None;
        #[cfg(test)]
        {
            self.avc444_luma = None;
            self.avc444_chroma = None;
        }
    }

    fn primary_reference(&self, role: VaapiEncodeRole) -> Option<DpbEntry> {
        let _ = role;
        self.last
    }

    fn reference_frames(&self, mode: VaapiReferenceMode, role: VaapiEncodeRole) -> Vec<DpbEntry> {
        let mut refs = Vec::new();
        if let Some(reference) = self.primary_reference(role) {
            refs.push(reference);
        }
        let _ = mode;
        refs
    }

    fn record(&mut self, role: VaapiEncodeRole, entry: DpbEntry) {
        self.last = Some(entry);
        match role {
            VaapiEncodeRole::Generic => {}
            #[cfg(test)]
            VaapiEncodeRole::Avc444(VaapiAvc444Subframe::Luma) => {
                self.avc444_luma = Some(entry);
            }
            #[cfg(test)]
            VaapiEncodeRole::Avc444(VaapiAvc444Subframe::Chroma) => {
                self.avc444_chroma = Some(entry);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct VaapiRateControlPolicy {
    config_mode: u32,
    pic_init_qp: u8,
    bits_per_second: u32,
    target_percentage: u32,
    initial_qp: u32,
    min_qp: u32,
    rc_flags: u32,
    max_qp: u32,
}

fn vaapi_rate_control_policy(
    bitrate: u32,
    quality: u8,
    rate_control: H264RateControl,
) -> VaapiRateControlPolicy {
    let qp = quality.min(51);
    match rate_control {
        H264RateControl::Vbr => VaapiRateControlPolicy {
            config_mode: sys::VA_RC_VBR,
            pic_init_qp: H264_DEFAULT_PIC_INIT_QP,
            bits_per_second: bitrate,
            target_percentage: 100,
            initial_qp: 0,
            min_qp: 0,
            rc_flags: 0,
            max_qp: 0,
        },
        H264RateControl::Cqp => VaapiRateControlPolicy {
            config_mode: sys::VA_RC_CQP,
            pic_init_qp: qp,
            bits_per_second: 0,
            target_percentage: 100,
            initial_qp: qp.into(),
            min_qp: qp.into(),
            rc_flags: 1 << 1,
            max_qp: qp.into(),
        },
    }
}

pub struct VaapiEncoder {
    input_surfaces: Vec<VaSurface>,
    recon_surfaces: Vec<VaSurface>,
    coded_buffers: Vec<VaBuffer>,
    dmabuf_input_surface: Option<VaSurface>,
    context: VaContext,
    _config: VaConfig,
    display: Rc<VaDisplay>,
    current_input_surface: usize,
    current_recon_surface: usize,
    current_coded_buffer: usize,
    references: VaapiReferenceState,
    reference_mode: VaapiReferenceMode,
    cached_sps_pps: Option<Vec<u8>>,
    width: u32,
    height: u32,
    bitrate: u32,
    quality: u8,
    rate_control: H264RateControl,
    fps: u32,
    frame_count: u64,
    force_idr: bool,
    nv12_format: VAImageFormat,
    profile: VAProfile,
}

impl VaapiEncoder {
    pub fn new(
        width: u32,
        height: u32,
        bitrate: u32,
        fps: u32,
        quality: u8,
        rate_control: H264RateControl,
    ) -> Result<Self> {
        Self::new_with_reference_mode(
            width,
            height,
            bitrate,
            fps,
            quality,
            rate_control,
            VaapiReferenceMode::Single,
        )
    }

    fn new_with_reference_mode(
        width: u32,
        height: u32,
        bitrate: u32,
        fps: u32,
        quality: u8,
        rate_control: H264RateControl,
        reference_mode: VaapiReferenceMode,
    ) -> Result<Self> {
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            bail!("dimensions must be non-zero and even: {}x{}", width, height);
        }

        let device_path = find_vaapi_device()?;

        tracing::info!(
            "Initializing VA-API encoder: {}x{}, device={}",
            width,
            height,
            device_path.display()
        );

        let display = VaDisplay::open_drm(&device_path)
            .map_err(|e| anyhow::anyhow!("Failed to open VA display: {:?}", e))?;

        let driver = display
            .query_vendor_string()
            .unwrap_or_else(|_| "unknown".into());
        tracing::info!("VA-API vendor: {}", driver);

        let profiles = display
            .query_config_profiles()
            .map_err(|e| anyhow::anyhow!("Failed to query profiles: {}", e))?;

        let h264_profile = if profiles.contains(&sys::VA_PROFILE_H264_HIGH) {
            sys::VA_PROFILE_H264_HIGH
        } else if profiles.contains(&sys::VA_PROFILE_H264_MAIN) {
            sys::VA_PROFILE_H264_MAIN
        } else {
            bail!("H.264 encode not supported by VA-API driver");
        };

        let entrypoints = display
            .query_config_entrypoints(h264_profile)
            .map_err(|e| anyhow::anyhow!("Failed to query entrypoints: {}", e))?;

        if !entrypoints.contains(&sys::VA_ENTRYPOINT_ENC_SLICE) {
            bail!("H.264 encode entrypoint not supported");
        }

        let rc_policy = vaapi_rate_control_policy(bitrate, quality, rate_control);
        let config_attribs = [
            VAConfigAttrib {
                type_: sys::VA_CONFIG_ATTRIB_RT_FORMAT,
                value: VA_RT_FORMAT_YUV420,
            },
            VAConfigAttrib {
                type_: sys::VA_CONFIG_ATTRIB_RATE_CONTROL,
                value: rc_policy.config_mode,
            },
        ];
        let config = display
            .create_config(h264_profile, sys::VA_ENTRYPOINT_ENC_SLICE, &config_attribs)
            .map_err(|e| anyhow::anyhow!("Failed to create config: {}", e))?;

        let input_surfaces = display
            .create_surfaces(
                VA_RT_FORMAT_YUV420,
                Some(sys::fourcc(b"NV12")),
                width,
                height,
                Some(va::VA_SURFACE_ATTRIB_USAGE_HINT_ENCODER),
                INPUT_SURFACE_POOL_SIZE,
            )
            .map_err(|e| anyhow::anyhow!("Failed to create input surfaces: {}", e))?;

        let recon_surface_count = reference_mode.recon_surface_pool_size();
        let recon_surfaces = display
            .create_surfaces(
                VA_RT_FORMAT_YUV420,
                Some(sys::fourcc(b"NV12")),
                width,
                height,
                Some(va::VA_SURFACE_ATTRIB_USAGE_HINT_ENCODER),
                recon_surface_count,
            )
            .map_err(|e| anyhow::anyhow!("Failed to create recon surfaces: {}", e))?;

        let context = display
            .create_context(&config, width, height, &mut [])
            .map_err(|e| anyhow::anyhow!("Failed to create context: {}", e))?;

        let image_formats = display
            .query_image_formats()
            .map_err(|e| anyhow::anyhow!("Failed to query image formats: {}", e))?;

        let nv12_format = image_formats
            .iter()
            .find(|f| f.fourcc == sys::fourcc(b"NV12"))
            .copied()
            .ok_or_else(|| anyhow::anyhow!("NV12 format not supported"))?;

        let coded_buffer_size = ((width * height * 3) / 2) as usize;
        let mut coded_buffers = Vec::with_capacity(CODED_BUFFER_COUNT);
        for i in 0..CODED_BUFFER_COUNT {
            let buf = context
                .create_coded_buffer(coded_buffer_size)
                .map_err(|e| anyhow::anyhow!("Failed to create coded buffer {}: {}", i, e))?;
            coded_buffers.push(buf);
        }

        tracing::info!(
            profile = ?h264_profile,
            rate_control = ?rate_control,
            quality = quality,
            "VA-API encoder ready: {}x{}, {}kbps, IDR every {} frames",
            width, height, bitrate / 1000, IDR_INTERVAL,
        );

        Ok(Self {
            input_surfaces,
            recon_surfaces,
            coded_buffers,
            dmabuf_input_surface: None,
            context,
            _config: config,
            display,
            current_input_surface: 0,
            current_recon_surface: 0,
            current_coded_buffer: 0,
            references: VaapiReferenceState::default(),
            reference_mode,
            cached_sps_pps: None,
            width,
            height,
            bitrate,
            quality,
            rate_control,
            fps,
            frame_count: 0,
            force_idr: true,
            nv12_format,
            profile: h264_profile,
        })
    }

    /// Force the next encoded frame to be an IDR (used after error recovery).
    pub fn force_idr(&mut self) {
        self.force_idr = true;
    }

    fn is_idr_frame(&self) -> bool {
        self.force_idr
            || (self.reference_mode.uses_periodic_idr()
                && self.frame_count.is_multiple_of(IDR_INTERVAL as u64))
    }

    pub fn encode(&mut self, bgra: &[u8], stride: usize) -> Result<Vec<u8>> {
        let role = VaapiEncodeRole::Generic;
        let is_idr = self.is_idr_frame();
        let input_idx = self.current_input_surface;
        self.current_input_surface = (self.current_input_surface + 1) % self.input_surfaces.len();

        let recon_idx = self.current_recon_surface;
        self.current_recon_surface = (self.current_recon_surface + 1) % self.recon_surfaces.len();

        let coded_idx = self.current_coded_buffer;
        self.current_coded_buffer = (self.current_coded_buffer + 1) % self.coded_buffers.len();

        // Convert BGRA to NV12 and upload to surface via VA image,
        // using the image's actual pitches/offsets (not assuming stride == width).
        {
            let mut image = VaImageMapping::create_from(
                &self.input_surfaces[input_idx],
                self.nv12_format,
                self.width,
                self.height,
            )
            .context("Failed to create VA image")?;

            let (y_pitch, uv_pitch, y_offset, uv_offset) = {
                let i = image.image();
                (
                    i.pitches[0] as usize,
                    i.pitches[1] as usize,
                    i.offsets[0] as usize,
                    i.offsets[1] as usize,
                )
            };

            Self::bgra_to_nv12(
                bgra,
                image.data_mut(),
                self.width as usize,
                self.height as usize,
                stride,
                y_offset,
                y_pitch,
                uv_offset,
                uv_pitch,
            );
        }

        // Build encoding parameters
        let mb_width = self.width.div_ceil(16);
        let mb_height = self.height.div_ceil(16);
        let num_macroblocks = mb_width * mb_height;
        // max_frame_num = 2^(log2_max_frame_num_minus4+4) = 256
        let frame_num = (self.frame_count % 256) as u16;
        // max_pic_order_cnt_lsb = 2^(log2_max_pic_order_cnt_lsb_minus4+4) = 256
        let poc = ((self.frame_count * 2) % 256) as i32;

        let mut picture_buffers = Vec::new();

        if is_idr {
            self.references.clear();

            let seq_param = self.build_sequence_params(mb_width as u16, mb_height as u16);
            let seq_buffer = self
                .context
                .create_buffer(
                    va::VABufferType_VAEncSequenceParameterBufferType,
                    seq_param,
                    "vaCreateBuffer (H.264 sequence)",
                )
                .context("Failed to create seq buffer")?;
            picture_buffers.push(seq_buffer);

            picture_buffers.extend(self.create_rate_control_buffers()?);
        }

        let pic_param = self.build_picture_params(
            self.recon_surfaces[recon_idx].id(),
            self.coded_buffers[coded_idx].id(),
            is_idr,
            frame_num,
            poc,
            role,
        );
        let pic_buffer = self
            .context
            .create_buffer(
                va::VABufferType_VAEncPictureParameterBufferType,
                pic_param,
                "vaCreateBuffer (H.264 picture)",
            )
            .context("Failed to create pic buffer")?;
        picture_buffers.push(pic_buffer);

        let slice_param = self.build_slice_params(num_macroblocks, is_idr, frame_num, poc, role);
        let slice_buffer = self
            .context
            .create_buffer(
                va::VABufferType_VAEncSliceParameterBufferType,
                slice_param,
                "vaCreateBuffer (H.264 slice)",
            )
            .context("Failed to create slice buffer")?;
        picture_buffers.push(slice_buffer);

        self.context
            .render_picture(self.input_surfaces[input_idx].id(), &picture_buffers)?;

        // Update DPB
        self.references.record(
            role,
            DpbEntry {
                surface_id: self.recon_surfaces[recon_idx].id(),
                frame_num,
                poc,
            },
        );

        // Read encoded bitstream
        let mut data = self.coded_buffers[coded_idx].read_coded()?;

        // SPS/PPS handling: extract from IDR output or generate if missing
        if is_idr {
            if let Some(sps_pps) = super::extract_sps_pps(&data) {
                self.cached_sps_pps = Some(sps_pps);
            } else {
                // VA-API driver did not include SPS/PPS; synthesize it here.
                tracing::trace!("VA-API IDR missing SPS/PPS, synthesizing parameter sets");
                let sps_pps = self.generate_sps_pps();
                // Prepend to IDR frame
                let mut combined = Vec::with_capacity(sps_pps.len() + data.len());
                combined.extend_from_slice(&sps_pps);
                combined.extend_from_slice(&data);
                data = combined;
                self.cached_sps_pps = Some(sps_pps);
            }
        } else if let Some(ref sps_pps) = self.cached_sps_pps {
            let mut combined = Vec::with_capacity(sps_pps.len() + data.len());
            combined.extend_from_slice(sps_pps);
            combined.extend_from_slice(&data);
            data = combined;
        }

        if self.force_idr {
            self.force_idr = false;
        }
        self.frame_count += 1;

        Ok(data)
    }

    /// Encode from an NV12 DMA-BUF (zero-copy path).
    ///
    /// Imports the NV12 DMA-BUF as an encoder input surface (cached after first call),
    /// then encodes using the same pipeline as the BGRA path but skipping the color conversion.
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
        if self.dmabuf_input_surface.is_none() {
            let mut desc: VADRMPRIMESurfaceDescriptor = unsafe { mem::zeroed() };
            desc.fourcc = sys::fourcc(b"NV12");
            desc.width = width;
            desc.height = height;
            desc.num_objects = 1;
            desc.objects[0].fd = nv12_fd;
            let y_end = offset + height * stride;
            let uv_end = uv_offset + (height / 2) * uv_stride;
            desc.objects[0].size = y_end.max(uv_end);
            desc.objects[0].drm_format_modifier = modifier;
            desc.num_layers = 2;
            desc.layers[0].drm_format = sys::fourcc(b"NV12");
            desc.layers[0].num_planes = 1;
            desc.layers[0].object_index[0] = 0;
            desc.layers[0].offset[0] = offset;
            desc.layers[0].pitch[0] = stride;
            desc.layers[1].drm_format = sys::fourcc(b"NV12");
            desc.layers[1].num_planes = 1;
            desc.layers[1].object_index[0] = 0;
            desc.layers[1].offset[0] = uv_offset;
            desc.layers[1].pitch[0] = uv_stride;

            let surface = self
                .display
                .import_prime_surface(VA_RT_FORMAT_YUV420, width, height, &mut desc)
                .map_err(|e| anyhow::anyhow!("Failed to import NV12 DMA-BUF surface: {}", e))?;
            tracing::trace!(
                surface_id = surface.id(),
                "Encoder: imported NV12 DMA-BUF surface"
            );
            self.dmabuf_input_surface = Some(surface);
        }

        let dmabuf_surface_id = self
            .dmabuf_input_surface
            .as_ref()
            .expect("dmabuf_input_surface must be set before encode")
            .id();
        let role = VaapiEncodeRole::Generic;
        let is_idr = self.is_idr_frame();

        let recon_idx = self.current_recon_surface;
        self.current_recon_surface = (self.current_recon_surface + 1) % self.recon_surfaces.len();

        let coded_idx = self.current_coded_buffer;
        self.current_coded_buffer = (self.current_coded_buffer + 1) % self.coded_buffers.len();

        let mb_width = self.width.div_ceil(16);
        let mb_height = self.height.div_ceil(16);
        let num_macroblocks = mb_width * mb_height;
        // max_frame_num = 2^(log2_max_frame_num_minus4+4) = 256
        let frame_num = (self.frame_count % 256) as u16;
        // max_pic_order_cnt_lsb = 2^(log2_max_pic_order_cnt_lsb_minus4+4) = 256
        let poc = ((self.frame_count * 2) % 256) as i32;

        let mut picture_buffers = Vec::new();

        if is_idr {
            self.references.clear();
            let seq_param = self.build_sequence_params(mb_width as u16, mb_height as u16);
            let seq_buffer = self
                .context
                .create_buffer(
                    va::VABufferType_VAEncSequenceParameterBufferType,
                    seq_param,
                    "vaCreateBuffer (H.264 sequence)",
                )
                .context("Failed to create seq buffer")?;
            picture_buffers.push(seq_buffer);

            picture_buffers.extend(self.create_rate_control_buffers()?);
        }

        let pic_param = self.build_picture_params(
            self.recon_surfaces[recon_idx].id(),
            self.coded_buffers[coded_idx].id(),
            is_idr,
            frame_num,
            poc,
            role,
        );
        let pic_buffer = self
            .context
            .create_buffer(
                va::VABufferType_VAEncPictureParameterBufferType,
                pic_param,
                "vaCreateBuffer (H.264 picture)",
            )
            .context("Failed to create pic buffer")?;
        picture_buffers.push(pic_buffer);

        let slice_param = self.build_slice_params(num_macroblocks, is_idr, frame_num, poc, role);
        let slice_buffer = self
            .context
            .create_buffer(
                va::VABufferType_VAEncSliceParameterBufferType,
                slice_param,
                "vaCreateBuffer (H.264 slice)",
            )
            .context("Failed to create slice buffer")?;
        picture_buffers.push(slice_buffer);

        self.context
            .render_picture(dmabuf_surface_id, &picture_buffers)
            .map_err(|e| anyhow::anyhow!("VA-API render failed (dmabuf): {}", e))?;

        self.references.record(
            role,
            DpbEntry {
                surface_id: self.recon_surfaces[recon_idx].id(),
                frame_num,
                poc,
            },
        );

        let mut data = self.coded_buffers[coded_idx].read_coded()?;

        // SPS/PPS handling (same as encode())
        if is_idr {
            if let Some(sps_pps) = super::extract_sps_pps(&data) {
                self.cached_sps_pps = Some(sps_pps);
            } else {
                tracing::trace!("VA-API IDR missing SPS/PPS, synthesizing parameter sets");
                let sps_pps = self.generate_sps_pps();
                let mut combined = Vec::with_capacity(sps_pps.len() + data.len());
                combined.extend_from_slice(&sps_pps);
                combined.extend_from_slice(&data);
                data = combined;
                self.cached_sps_pps = Some(sps_pps);
            }
        } else if let Some(ref sps_pps) = self.cached_sps_pps {
            let mut combined = Vec::with_capacity(sps_pps.len() + data.len());
            combined.extend_from_slice(sps_pps);
            combined.extend_from_slice(&data);
            data = combined;
        }

        if self.force_idr {
            self.force_idr = false;
        }
        self.frame_count += 1;

        Ok(data)
    }

    /// Convert BGRA to NV12 (BT.709 full range) directly into VA image buffer,
    /// respecting the image's pitch and plane offsets.
    #[allow(clippy::too_many_arguments)]
    fn bgra_to_nv12(
        bgra: &[u8],
        dst: &mut [u8],
        w: usize,
        h: usize,
        src_stride: usize,
        y_offset: usize,
        y_pitch: usize,
        uv_offset: usize,
        uv_pitch: usize,
    ) {
        if bgra.len() < h * src_stride {
            tracing::warn!(
                bgra_len = bgra.len(),
                expected = h * src_stride,
                "BGRA buffer too small, skipping conversion"
            );
            return;
        }
        // Y plane (BT.709 full range).
        for row in 0..h {
            let dst_start = y_offset + row * y_pitch;
            for col in 0..w {
                let idx = row * src_stride + col * 4;
                let b = bgra[idx] as i32;
                let g = bgra[idx + 1] as i32;
                let r = bgra[idx + 2] as i32;
                let y = (54 * r + 183 * g + 18 * b) >> 8;
                dst[dst_start + col] = y.clamp(0, 255) as u8;
            }
        }

        // UV plane (NV12 interleaved, BT.709 full range, half resolution)
        for row in 0..(h / 2) {
            let dst_start = uv_offset + row * uv_pitch;
            for col in 0..(w / 2) {
                let src_row = row * 2;
                let src_col = col * 2;

                let mut r_sum = 0i32;
                let mut g_sum = 0i32;
                let mut b_sum = 0i32;

                for dy in 0..2 {
                    for dx in 0..2 {
                        let idx = (src_row + dy) * src_stride + (src_col + dx) * 4;
                        b_sum += bgra[idx] as i32;
                        g_sum += bgra[idx + 1] as i32;
                        r_sum += bgra[idx + 2] as i32;
                    }
                }

                let r = r_sum / 4;
                let g = g_sum / 4;
                let b = b_sum / 4;

                let u = ((-29 * r - 99 * g + 128 * b) >> 8) + 128;
                let v = ((128 * r - 116 * g - 12 * b) >> 8) + 128;

                dst[dst_start + col * 2] = u.clamp(0, 255) as u8;
                dst[dst_start + col * 2 + 1] = v.clamp(0, 255) as u8;
            }
        }
    }

    fn get_h264_level(&self) -> u8 {
        let macroblocks_per_sec = (self.width / 16) * (self.height / 16) * self.fps;
        if macroblocks_per_sec <= 40500 {
            30
        } else if macroblocks_per_sec <= 108000 {
            31
        } else if macroblocks_per_sec <= 245760 {
            40
        } else if macroblocks_per_sec <= 589824 {
            41
        } else if macroblocks_per_sec <= 983040 {
            50
        } else {
            51
        }
    }

    fn build_sequence_params(
        &self,
        mb_width: u16,
        mb_height: u16,
    ) -> va::VAEncSequenceParameterBufferH264 {
        let mut params: va::VAEncSequenceParameterBufferH264 = unsafe { mem::zeroed() };
        params.seq_parameter_set_id = 0;
        params.level_idc = self.get_h264_level();
        params.intra_period = IDR_INTERVAL;
        params.intra_idr_period = IDR_INTERVAL;
        params.ip_period = 1;
        params.bits_per_second = self.bitrate;
        params.max_num_ref_frames = self.reference_mode.max_num_ref_frames();
        params.picture_width_in_mbs = mb_width;
        params.picture_height_in_mbs = mb_height;
        params.seq_fields = va::_VAEncSequenceParameterBufferH264__bindgen_ty_1 {
            value: h264_seq_fields_value(),
        };
        params.bit_depth_luma_minus8 = 0;
        params.bit_depth_chroma_minus8 = 0;
        params.num_ref_frames_in_pic_order_cnt_cycle = 0;
        params.offset_for_non_ref_pic = 0;
        params.offset_for_top_to_bottom_field = 0;
        params.offset_for_ref_frame = [0; 256];
        params.frame_cropping_flag = 0;
        params.vui_parameters_present_flag = 1;
        params.vui_fields = va::_VAEncSequenceParameterBufferH264__bindgen_ty_2 {
            value: h264_vui_fields_value(),
        };
        params.aspect_ratio_idc = 0;
        params.sar_width = 1;
        params.sar_height = 1;
        params.num_units_in_tick = 1;
        params.time_scale = self.fps * 2;
        params
    }

    fn build_picture_params(
        &self,
        recon_surface_id: u32,
        coded_buf_id: u32,
        is_idr: bool,
        frame_num: u16,
        poc: i32,
        role: VaapiEncodeRole,
    ) -> va::VAEncPictureParameterBufferH264 {
        let curr_pic = picture_h264(
            recon_surface_id,
            frame_num as u32,
            VA_PICTURE_H264_SHORT_TERM_REFERENCE,
            poc,
            poc,
        );

        let mut reference_frames: [va::VAPictureH264; 16] = std::array::from_fn(|_| {
            picture_h264(VA_INVALID_SURFACE, 0, VA_PICTURE_H264_INVALID, 0, 0)
        });

        let refs = if is_idr {
            Vec::new()
        } else {
            self.references.reference_frames(self.reference_mode, role)
        };
        for (index, entry) in refs.iter().take(reference_frames.len()).enumerate() {
            reference_frames[index] = picture_h264(
                entry.surface_id,
                entry.frame_num as u32,
                VA_PICTURE_H264_SHORT_TERM_REFERENCE,
                entry.poc,
                entry.poc,
            );
        }

        let transform_8x8 = if self.profile == sys::VA_PROFILE_H264_HIGH {
            1
        } else {
            0
        };

        let mut params: va::VAEncPictureParameterBufferH264 = unsafe { mem::zeroed() };
        params.CurrPic = curr_pic;
        params.ReferenceFrames = reference_frames;
        params.coded_buf = coded_buf_id;
        params.pic_parameter_set_id = 0;
        params.seq_parameter_set_id = 0;
        params.last_picture = 0;
        params.frame_num = frame_num;
        params.pic_init_qp =
            vaapi_rate_control_policy(self.bitrate, self.quality, self.rate_control).pic_init_qp;
        params.num_ref_idx_l0_active_minus1 = 0;
        params.num_ref_idx_l1_active_minus1 = 0;
        params.chroma_qp_index_offset = 0;
        params.second_chroma_qp_index_offset = 0;
        params.pic_fields = va::_VAEncPictureParameterBufferH264__bindgen_ty_1 {
            value: h264_pic_fields_value(is_idr, transform_8x8),
        };
        params
    }

    fn build_slice_params(
        &self,
        num_macroblocks: u32,
        is_idr: bool,
        frame_num: u16,
        poc: i32,
        role: VaapiEncodeRole,
    ) -> va::VAEncSliceParameterBufferH264 {
        let slice_type = if is_idr { SLICE_TYPE_I } else { SLICE_TYPE_P };

        let mut ref_pic_list_0: [va::VAPictureH264; 32] = std::array::from_fn(|_| {
            picture_h264(VA_INVALID_SURFACE, 0, VA_PICTURE_H264_INVALID, 0, 0)
        });
        let ref_pic_list_1: [va::VAPictureH264; 32] = std::array::from_fn(|_| {
            picture_h264(VA_INVALID_SURFACE, 0, VA_PICTURE_H264_INVALID, 0, 0)
        });

        let primary_ref = if is_idr {
            None
        } else {
            self.references.primary_reference(role)
        };
        let (num_ref_override, num_ref_l0) = if let Some(entry) = primary_ref {
            ref_pic_list_0[0] = picture_h264(
                entry.surface_id,
                entry.frame_num as u32,
                VA_PICTURE_H264_SHORT_TERM_REFERENCE,
                entry.poc,
                entry.poc,
            );
            (1u8, 0u8)
        } else {
            (0u8, 0u8)
        };

        let mut params: va::VAEncSliceParameterBufferH264 = unsafe { mem::zeroed() };
        params.macroblock_address = 0;
        params.num_macroblocks = num_macroblocks;
        params.macroblock_info = VA_INVALID_ID;
        params.slice_type = slice_type;
        params.pic_parameter_set_id = 0;
        params.idr_pic_id = frame_num;
        params.pic_order_cnt_lsb = poc as u16;
        params.delta_pic_order_cnt_bottom = 0;
        params.delta_pic_order_cnt = [0, 0];
        params.direct_spatial_mv_pred_flag = 0;
        params.num_ref_idx_active_override_flag = num_ref_override;
        params.num_ref_idx_l0_active_minus1 = num_ref_l0;
        params.num_ref_idx_l1_active_minus1 = 0;
        params.RefPicList0 = ref_pic_list_0;
        params.RefPicList1 = ref_pic_list_1;
        params.luma_log2_weight_denom = 0;
        params.chroma_log2_weight_denom = 0;
        params.luma_weight_l0_flag = 0;
        params.chroma_weight_l0_flag = 0;
        params.luma_weight_l1_flag = 0;
        params.chroma_weight_l1_flag = 0;
        params.cabac_init_idc = 0;
        params.slice_qp_delta = 0;
        params.disable_deblocking_filter_idc = 0;
        params.slice_alpha_c0_offset_div2 = 0;
        params.slice_beta_offset_div2 = 0;
        params
    }

    fn create_rate_control_buffers(&self) -> Result<Vec<VaBuffer>> {
        let mut buffers = Vec::with_capacity(3);

        let policy = vaapi_rate_control_policy(self.bitrate, self.quality, self.rate_control);

        let mut rc: va::VAEncMiscParameterRateControl = unsafe { mem::zeroed() };
        rc.bits_per_second = policy.bits_per_second;
        rc.target_percentage = policy.target_percentage;
        rc.window_size = 1000;
        rc.initial_qp = policy.initial_qp;
        rc.min_qp = policy.min_qp;
        rc.basic_unit_size = 0;
        rc.rc_flags = va::_VAEncMiscParameterRateControl__bindgen_ty_1 {
            value: policy.rc_flags,
        };
        rc.ICQ_quality_factor = 0;
        rc.max_qp = policy.max_qp;
        rc.quality_factor = 0;
        rc.target_frame_size = 0;
        buffers.push(
            self.context
                .create_misc_buffer(
                    va::VAEncMiscParameterType_VAEncMiscParameterTypeRateControl,
                    rc,
                    "vaCreateBuffer (rate control)",
                )
                .context("Failed to create rate control buffer")?,
        );

        let mut hrd: va::VAEncMiscParameterHRD = unsafe { mem::zeroed() };
        hrd.initial_buffer_fullness = policy.bits_per_second / 2;
        hrd.buffer_size = policy.bits_per_second;
        buffers.push(
            self.context
                .create_misc_buffer(
                    va::VAEncMiscParameterType_VAEncMiscParameterTypeHRD,
                    hrd,
                    "vaCreateBuffer (HRD)",
                )
                .context("Failed to create HRD buffer")?,
        );

        let mut fr: va::VAEncMiscParameterFrameRate = unsafe { mem::zeroed() };
        fr.framerate = self.fps;
        fr.framerate_flags = va::_VAEncMiscParameterFrameRate__bindgen_ty_1 { value: 0 };
        buffers.push(
            self.context
                .create_misc_buffer(
                    va::VAEncMiscParameterType_VAEncMiscParameterTypeFrameRate,
                    fr,
                    "vaCreateBuffer (frame rate)",
                )
                .context("Failed to create frame rate buffer")?,
        );

        Ok(buffers)
    }

    /// Generate SPS and PPS NAL units matching our encoder configuration.
    /// Used when the VA-API driver doesn't include them in the coded output.
    fn generate_sps_pps(&self) -> Vec<u8> {
        let profile_idc: u32 = match self.profile {
            sys::VA_PROFILE_H264_HIGH => 100,
            sys::VA_PROFILE_H264_MAIN => 77,
            _ => 100,
        };
        let is_high_profile = profile_idc >= 100;

        let mb_width = self.width.div_ceil(16);
        let mb_height = self.height.div_ceil(16);
        let coded_width = mb_width * 16;
        let coded_height = mb_height * 16;
        let need_crop = coded_width != self.width || coded_height != self.height;
        let crop_right = if coded_width != self.width {
            (coded_width - self.width) / 2
        } else {
            0
        };
        let crop_bottom = if coded_height != self.height {
            (coded_height - self.height) / 2
        } else {
            0
        };

        let mut buf = Vec::with_capacity(64);

        buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        let mut bs = BitWriter::new();
        bs.write_bits(0, 1);
        bs.write_bits(3, 2);
        bs.write_bits(7, 5);

        bs.write_bits(profile_idc, 8);
        bs.write_bits(0, 8);
        bs.write_bits(self.get_h264_level() as u32, 8);
        bs.write_ue(0);

        if is_high_profile {
            bs.write_ue(1); // chroma_format_idc
            bs.write_ue(0); // bit_depth_luma_minus8
            bs.write_ue(0); // bit_depth_chroma_minus8
            bs.write_bits(0, 1); // qpprime_y_zero_transform_bypass_flag
            bs.write_bits(0, 1); // seq_scaling_matrix_present_flag
        }

        bs.write_ue(4); // log2_max_frame_num_minus4
        bs.write_ue(0); // pic_order_cnt_type
        bs.write_ue(4); // log2_max_pic_order_cnt_lsb_minus4
        let max_num_ref_frames = self.reference_mode.max_num_ref_frames();
        bs.write_ue(max_num_ref_frames); // max_num_ref_frames
        bs.write_bits(0, 1); // gaps_in_frame_num_value_allowed_flag
        bs.write_ue(mb_width - 1);
        bs.write_ue(mb_height - 1);
        bs.write_bits(1, 1); // frame_mbs_only_flag
        bs.write_bits(1, 1); // direct_8x8_inference_flag

        if need_crop {
            bs.write_bits(1, 1);
            bs.write_ue(0);
            bs.write_ue(crop_right);
            bs.write_ue(0);
            bs.write_ue(crop_bottom);
        } else {
            bs.write_bits(0, 1);
        }

        bs.write_bits(1, 1); // vui_parameters_present_flag
        bs.write_bits(0, 1); // aspect_ratio_info_present_flag
        bs.write_bits(0, 1); // overscan_info_present_flag
        bs.write_bits(1, 1); // video_signal_type_present_flag
        bs.write_bits(5, 3); // video_format: unspecified
        bs.write_bits(1, 1); // video_full_range_flag
        bs.write_bits(1, 1); // colour_description_present_flag
        bs.write_bits(1, 8); // colour_primaries: BT.709
        bs.write_bits(1, 8); // transfer_characteristics: BT.709
        bs.write_bits(1, 8); // matrix_coefficients: BT.709
        bs.write_bits(0, 1); // chroma_loc_info_present_flag
        bs.write_bits(1, 1); // timing_info_present_flag
        bs.write_bits(1, 32); // num_units_in_tick
        bs.write_bits(self.fps * 2, 32);
        bs.write_bits(0, 1); // fixed_frame_rate_flag
        bs.write_bits(0, 1); // nal_hrd_parameters_present_flag
        bs.write_bits(0, 1); // vcl_hrd_parameters_present_flag
        bs.write_bits(0, 1); // pic_struct_present_flag
        bs.write_bits(1, 1); // bitstream_restriction_flag
        bs.write_bits(1, 1); // motion_vectors_over_pic_boundaries_flag
        bs.write_ue(0);
        bs.write_ue(0);
        bs.write_ue(16);
        bs.write_ue(16);
        bs.write_ue(0); // max_num_reorder_frames
        bs.write_ue(max_num_ref_frames); // max_dec_frame_buffering

        bs.write_rbsp_trailing_bits();
        buf.extend_from_slice(&bs.finish_with_emulation_prevention());

        buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        let mut bs = BitWriter::new();
        bs.write_bits(0, 1);
        bs.write_bits(3, 2);
        bs.write_bits(8, 5);

        bs.write_ue(0); // pic_parameter_set_id
        bs.write_ue(0); // seq_parameter_set_id
        bs.write_bits(1, 1); // entropy_coding_mode_flag (CABAC)
        bs.write_bits(0, 1); // bottom_field_pic_order_in_frame_present_flag
        bs.write_ue(0); // num_slice_groups_minus1
        bs.write_ue(0); // num_ref_idx_l0_default_active_minus1
        bs.write_ue(0); // num_ref_idx_l1_default_active_minus1
        bs.write_bits(0, 1); // weighted_pred_flag
        bs.write_bits(0, 2); // weighted_bipred_idc
        let pic_init_qp = vaapi_rate_control_policy(self.bitrate, self.quality, self.rate_control)
            .pic_init_qp as i32;
        bs.write_se(pic_init_qp - 26); // pic_init_qp_minus26
        bs.write_se(0); // pic_init_qs_minus26
        bs.write_se(0); // chroma_qp_index_offset
        bs.write_bits(1, 1); // deblocking_filter_control_present_flag
        bs.write_bits(0, 1); // constrained_intra_pred_flag
        bs.write_bits(0, 1); // redundant_pic_cnt_present_flag

        if is_high_profile {
            bs.write_bits(1, 1); // transform_8x8_mode_flag
            bs.write_bits(0, 1); // pic_scaling_matrix_present_flag
            bs.write_se(0); // second_chroma_qp_index_offset
        } else {
            bs.write_bits(0, 1); // transform_8x8_mode_flag
            bs.write_bits(0, 1); // pic_scaling_matrix_present_flag
            bs.write_se(0); // second_chroma_qp_index_offset
        }

        bs.write_rbsp_trailing_bits();
        buf.extend_from_slice(&bs.finish_with_emulation_prevention());

        buf
    }
}

fn picture_h264(
    picture_id: u32,
    frame_idx: u32,
    flags: u32,
    top_field_order_cnt: i32,
    bottom_field_order_cnt: i32,
) -> va::VAPictureH264 {
    let mut picture: va::VAPictureH264 = unsafe { mem::zeroed() };
    picture.picture_id = picture_id;
    picture.frame_idx = frame_idx;
    picture.flags = flags;
    picture.TopFieldOrderCnt = top_field_order_cnt;
    picture.BottomFieldOrderCnt = bottom_field_order_cnt;
    picture
}

fn h264_seq_fields_value() -> u32 {
    let chroma_format_idc = 1; // 4:2:0
    let frame_mbs_only_flag = 1;
    let direct_8x8_inference_flag = 1;
    let log2_max_frame_num_minus4 = 4;
    let pic_order_cnt_type = 0;
    let log2_max_pic_order_cnt_lsb_minus4 = 4;

    chroma_format_idc
        | (frame_mbs_only_flag << 2)
        | (direct_8x8_inference_flag << 5)
        | (log2_max_frame_num_minus4 << 6)
        | (pic_order_cnt_type << 10)
        | (log2_max_pic_order_cnt_lsb_minus4 << 12)
}

fn h264_vui_fields_value() -> u32 {
    let timing_info_present_flag = 1;
    let bitstream_restriction_flag = 1;
    let log2_max_mv_length_horizontal = 16;
    let log2_max_mv_length_vertical = 16;
    let motion_vectors_over_pic_boundaries_flag = 1;

    (timing_info_present_flag << 1)
        | (bitstream_restriction_flag << 2)
        | (log2_max_mv_length_horizontal << 3)
        | (log2_max_mv_length_vertical << 8)
        | (motion_vectors_over_pic_boundaries_flag << 15)
}

fn h264_pic_fields_value(is_idr: bool, transform_8x8_mode_flag: u32) -> u32 {
    let idr_pic_flag = u32::from(is_idr);
    let reference_pic_flag = 1;
    let entropy_coding_mode_flag = 1;
    let deblocking_filter_control_present_flag = 1;

    idr_pic_flag
        | (reference_pic_flag << 1)
        | (entropy_coding_mode_flag << 3)
        | (transform_8x8_mode_flag << 8)
        | (deblocking_filter_control_present_flag << 9)
}

/// Bitstream writer for H.264 NAL unit construction.
struct BitWriter {
    data: Vec<u8>,
    current_byte: u8,
    bits_in_byte: u8,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            data: Vec::with_capacity(32),
            current_byte: 0,
            bits_in_byte: 0,
        }
    }

    fn write_bits(&mut self, value: u32, num_bits: u8) {
        for i in (0..num_bits).rev() {
            let bit = (value >> i) & 1;
            self.current_byte = (self.current_byte << 1) | bit as u8;
            self.bits_in_byte += 1;
            if self.bits_in_byte == 8 {
                self.data.push(self.current_byte);
                self.current_byte = 0;
                self.bits_in_byte = 0;
            }
        }
    }

    /// Exp-Golomb unsigned encoding
    fn write_ue(&mut self, value: u32) {
        let value = value + 1;
        let bits = 32 - value.leading_zeros(); // number of significant bits
                                               // Write (bits-1) leading zeros, then the value in `bits` bits
        for _ in 0..(bits - 1) {
            self.write_bits(0, 1);
        }
        self.write_bits(value, bits as u8);
    }

    /// Exp-Golomb signed encoding
    fn write_se(&mut self, value: i32) {
        let mapped = if value > 0 {
            (value * 2 - 1) as u32
        } else if value < 0 {
            (-value * 2) as u32
        } else {
            0
        };
        self.write_ue(mapped);
    }

    fn write_rbsp_trailing_bits(&mut self) {
        self.write_bits(1, 1); // rbsp_stop_one_bit
                               // Pad to byte boundary with zeros
        if self.bits_in_byte > 0 {
            let padding = 8 - self.bits_in_byte;
            self.write_bits(0, padding);
        }
    }

    /// Flush remaining bits and apply emulation prevention (0x03 stuffing).
    fn finish_with_emulation_prevention(mut self) -> Vec<u8> {
        // Flush partial byte
        if self.bits_in_byte > 0 {
            self.current_byte <<= 8 - self.bits_in_byte;
            self.data.push(self.current_byte);
        }

        // Insert emulation prevention bytes: 00 00 {00,01,02,03} → 00 00 03 {00,01,02,03}
        let mut result = Vec::with_capacity(self.data.len() + 4);
        let mut zero_count = 0u32;
        for &byte in &self.data {
            if zero_count >= 2 && byte <= 0x03 {
                result.push(0x03); // emulation prevention byte
                zero_count = 0;
            }
            result.push(byte);
            if byte == 0x00 {
                zero_count += 1;
            } else {
                zero_count = 0;
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dpb(surface_id: u32, frame_num: u16, poc: i32) -> DpbEntry {
        DpbEntry {
            surface_id,
            frame_num,
            poc,
        }
    }

    #[test]
    fn vaapi_avc444_reference_state_uses_single_h264_sequence_reference() {
        let mut references = VaapiReferenceState::default();
        let luma_0 = dpb(10, 0, 0);
        let chroma_1 = dpb(11, 1, 2);

        references.record(VaapiEncodeRole::Avc444(VaapiAvc444Subframe::Luma), luma_0);
        assert_eq!(
            references.primary_reference(VaapiEncodeRole::Avc444(VaapiAvc444Subframe::Chroma)),
            Some(luma_0),
            "the first chroma subframe after an IDR luma frame must continue the same H.264 sequence"
        );

        references.record(
            VaapiEncodeRole::Avc444(VaapiAvc444Subframe::Chroma),
            chroma_1,
        );

        assert_eq!(
            references.primary_reference(VaapiEncodeRole::Avc444(VaapiAvc444Subframe::Luma)),
            Some(chroma_1),
            "the next luma subframe must use the immediately preceding chroma subframe as the single H.264 sequence reference"
        );
        assert_eq!(
            references.primary_reference(VaapiEncodeRole::Avc444(VaapiAvc444Subframe::Chroma)),
            Some(chroma_1),
            "the next chroma subframe must also use the immediately preceding H.264 sequence reference"
        );

        let luma_refs = references.reference_frames(
            VaapiReferenceMode::Avc444Subframes,
            VaapiEncodeRole::Avc444(VaapiAvc444Subframe::Luma),
        );
        assert_eq!(luma_refs, vec![chroma_1]);
    }

    #[test]
    fn vaapi_avc444_reference_mode_matches_single_h264_sequence() {
        assert_eq!(VaapiReferenceMode::Single.max_num_ref_frames(), 1);
        assert_eq!(VaapiReferenceMode::Avc444Subframes.max_num_ref_frames(), 1);
        assert!(VaapiReferenceMode::Single.uses_periodic_idr());
        assert!(
            !VaapiReferenceMode::Avc444Subframes.uses_periodic_idr(),
            "AVC444 must not inject fixed IDR at subframe cadence"
        );
    }

    #[test]
    fn vaapi_avc444_recon_pool_keeps_current_surface_outside_single_ref() {
        assert_eq!(VaapiReferenceMode::Single.recon_surface_pool_size(), 2);
        assert_eq!(
            VaapiReferenceMode::Avc444Subframes.recon_surface_pool_size(),
            2
        );

        let pool_size = VaapiReferenceMode::Avc444Subframes.recon_surface_pool_size();
        let previous_recon = 0;
        let next_recon = 1 % pool_size;

        assert_ne!(
            next_recon, previous_recon,
            "next CurrPic must not reuse the single active reference surface"
        );
    }

    #[test]
    fn vaapi_vbr_policy_uses_freerdp_bitrate_without_qp_controls() {
        let policy = vaapi_rate_control_policy(10_000_000, 23, H264RateControl::Vbr);

        assert_eq!(policy.config_mode, sys::VA_RC_VBR);
        assert_eq!(policy.bits_per_second, 10_000_000);
        assert_eq!(policy.pic_init_qp, H264_DEFAULT_PIC_INIT_QP);
        assert_eq!(policy.initial_qp, 0);
        assert_eq!(policy.min_qp, 0);
        assert_eq!(policy.target_percentage, 100);
        assert_eq!(policy.rc_flags, 0);
        assert_eq!(policy.max_qp, 0);
    }

    #[test]
    fn vaapi_cqp_policy_pins_qp_and_disables_bitrate_target() {
        let policy = vaapi_rate_control_policy(10_000_000, 32, H264RateControl::Cqp);

        assert_eq!(policy.config_mode, sys::VA_RC_CQP);
        assert_eq!(policy.bits_per_second, 0);
        assert_eq!(policy.pic_init_qp, 32);
        assert_eq!(policy.initial_qp, 32);
        assert_eq!(policy.min_qp, 32);
        assert_eq!(policy.rc_flags, 1 << 1);
        assert_eq!(policy.max_qp, 32);
    }

    #[test]
    fn vaapi_quality_is_clamped_to_h264_qp_range() {
        let policy = vaapi_rate_control_policy(10_000_000, 99, H264RateControl::Cqp);

        assert_eq!(policy.pic_init_qp, 51);
        assert_eq!(policy.initial_qp, 51);
        assert_eq!(policy.min_qp, 51);
        assert_eq!(policy.max_qp, 51);
    }
}
