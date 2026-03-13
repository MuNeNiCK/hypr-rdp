//! VA-API H.264 hardware encoder for Intel/AMD GPUs.
//!
//! Uses cros-libva for GPU-accelerated H.264 encoding. Surfaces are pooled
//! for triple-buffering, and a separate reconstructed surface pool enables
//! true inter-frame prediction via DPB tracking.
//!
//! Thread Safety: NOT Send — VA-API is thread-local. Create and use on the
//! same thread (the capture thread satisfies this).

use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{bail, Context, Result};
use cros_libva::{
    self as libva, BufferType, Context as VaContext, Display, EncCodedBuffer, EncMiscParameter,
    EncMiscParameterFrameRate, EncMiscParameterHRD, EncMiscParameterRateControl,
    EncPictureParameter, EncSequenceParameter, EncSliceParameter, MappedCodedBuffer, Picture,
    RcFlags, Surface, UsageHint, VAEntrypoint, VAImageFormat, VAProfile, VA_INVALID_ID,
    VA_INVALID_SURFACE, VA_PICTURE_H264_INVALID, VA_PICTURE_H264_SHORT_TERM_REFERENCE,
    VA_RT_FORMAT_YUV420,
};

const INPUT_SURFACE_POOL_SIZE: usize = 3;
const RECON_SURFACE_POOL_SIZE: usize = 2;
const CODED_BUFFER_COUNT: usize = 3;

const SLICE_TYPE_I: u8 = 2;
const SLICE_TYPE_P: u8 = 0;

const IDR_INTERVAL: u32 = 30;

/// Scan /dev/dri/renderD* for a VA-API device that supports H.264 encoding.
fn find_vaapi_device() -> Result<PathBuf> {
    let dri_path = Path::new("/dev/dri");
    if !dri_path.exists() {
        bail!("/dev/dri not found");
    }

    let mut devices: Vec<PathBuf> = std::fs::read_dir(dri_path)
        .context("failed to read /dev/dri")?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("renderD")
        })
        .map(|e| e.path())
        .collect();
    devices.sort();

    if devices.is_empty() {
        bail!("no render devices found in /dev/dri");
    }

    for device in &devices {
        let display = match Display::open_drm_display(device) {
            Ok(d) => d,
            Err(e) => {
                tracing::debug!(device = %device.display(), "Skipping VA-API device: {:?}", e);
                continue;
            }
        };

        let profiles = match display.query_config_profiles() {
            Ok(p) => p,
            Err(_) => continue,
        };

        let h264_profile = if profiles.contains(&VAProfile::VAProfileH264High) {
            VAProfile::VAProfileH264High
        } else if profiles.contains(&VAProfile::VAProfileH264Main) {
            VAProfile::VAProfileH264Main
        } else {
            continue;
        };

        let entrypoints = match display.query_config_entrypoints(h264_profile) {
            Ok(e) => e,
            Err(_) => continue,
        };

        if entrypoints.contains(&VAEntrypoint::VAEntrypointEncSlice) {
            let vendor = display
                .query_vendor_string()
                .unwrap_or_else(|_| "unknown".into());
            tracing::info!(
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

#[derive(Clone)]
struct DpbEntry {
    surface_id: u32,
    frame_num: u16,
    poc: i32,
}

/// Descriptor for importing a NV12 DMA-BUF as a VA surface.
pub struct DmaBufSurfaceImport {
    desc: libva::VADRMPRIMESurfaceDescriptor,
}

impl libva::ExternalBufferDescriptor for DmaBufSurfaceImport {
    const MEMORY_TYPE: libva::MemoryType = libva::MemoryType::DrmPrime2;
    type DescriptorAttribute = libva::VADRMPRIMESurfaceDescriptor;
    fn va_surface_attribute(&mut self) -> libva::VADRMPRIMESurfaceDescriptor {
        self.desc
    }
}

pub struct VaapiEncoder {
    #[allow(dead_code)]
    display: Rc<Display>,
    context: Rc<VaContext>,
    input_surfaces: Vec<Surface<()>>,
    recon_surfaces: Vec<Surface<()>>,
    current_input_surface: usize,
    current_recon_surface: usize,
    coded_buffers: Vec<EncCodedBuffer>,
    current_coded_buffer: usize,
    last_ref: Option<DpbEntry>,
    cached_sps_pps: Option<Vec<u8>>,
    width: u32,
    height: u32,
    bitrate: u32,
    fps: u32,
    frame_count: u64,
    force_idr: bool,
    nv12_format: VAImageFormat,
    /// Cached DMA-BUF imported NV12 surface for zero-copy encode
    dmabuf_input_surface: Option<Surface<DmaBufSurfaceImport>>,
}

impl VaapiEncoder {
    pub fn new(width: u32, height: u32, bitrate: u32, fps: u32) -> Result<Self> {
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            bail!("dimensions must be non-zero and even: {}x{}", width, height);
        }

        let device_path = find_vaapi_device()?;

        tracing::info!(
            "Initializing VA-API encoder: {}x{}, device={}",
            width,
            height,
            device_path.display()
        );

        let display = Display::open_drm_display(&device_path)
            .map_err(|e| anyhow::anyhow!("Failed to open VA display: {:?}", e))?;

        let driver = display
            .query_vendor_string()
            .unwrap_or_else(|_| "unknown".into());
        tracing::info!("VA-API vendor: {}", driver);

        let profiles = display
            .query_config_profiles()
            .map_err(|e| anyhow::anyhow!("Failed to query profiles: {}", e))?;

        let h264_profile = if profiles.contains(&VAProfile::VAProfileH264High) {
            VAProfile::VAProfileH264High
        } else if profiles.contains(&VAProfile::VAProfileH264Main) {
            VAProfile::VAProfileH264Main
        } else {
            bail!("H.264 encode not supported by VA-API driver");
        };

        let entrypoints = display
            .query_config_entrypoints(h264_profile)
            .map_err(|e| anyhow::anyhow!("Failed to query entrypoints: {}", e))?;

        if !entrypoints.contains(&VAEntrypoint::VAEntrypointEncSlice) {
            bail!("H.264 encode entrypoint not supported");
        }

        let config = display
            .create_config(vec![], h264_profile, VAEntrypoint::VAEntrypointEncSlice)
            .map_err(|e| anyhow::anyhow!("Failed to create config: {}", e))?;

        let input_surfaces = display
            .create_surfaces(
                VA_RT_FORMAT_YUV420,
                Some(u32::from_ne_bytes(*b"NV12")),
                width,
                height,
                Some(UsageHint::USAGE_HINT_ENCODER),
                vec![(); INPUT_SURFACE_POOL_SIZE],
            )
            .map_err(|e| anyhow::anyhow!("Failed to create input surfaces: {}", e))?;

        let recon_surfaces = display
            .create_surfaces(
                VA_RT_FORMAT_YUV420,
                Some(u32::from_ne_bytes(*b"NV12")),
                width,
                height,
                Some(UsageHint::USAGE_HINT_ENCODER),
                vec![(); RECON_SURFACE_POOL_SIZE],
            )
            .map_err(|e| anyhow::anyhow!("Failed to create recon surfaces: {}", e))?;

        let context = display
            .create_context(&config, width, height, None::<&Vec<Surface<()>>>, true)
            .map_err(|e| anyhow::anyhow!("Failed to create context: {}", e))?;

        let image_formats = display
            .query_image_formats()
            .map_err(|e| anyhow::anyhow!("Failed to query image formats: {}", e))?;

        let nv12_format = image_formats
            .iter()
            .find(|f| f.fourcc == u32::from_ne_bytes(*b"NV12"))
            .copied()
            .ok_or_else(|| anyhow::anyhow!("NV12 format not supported"))?;

        let coded_buffer_size = ((width * height * 3) / 2) as usize;
        let mut coded_buffers = Vec::with_capacity(CODED_BUFFER_COUNT);
        for i in 0..CODED_BUFFER_COUNT {
            let buf = context
                .create_enc_coded(coded_buffer_size)
                .map_err(|e| anyhow::anyhow!("Failed to create coded buffer {}: {}", i, e))?;
            coded_buffers.push(buf);
        }

        tracing::info!(
            profile = ?h264_profile,
            "VA-API encoder ready: {}x{}, {}kbps, IDR every {} frames",
            width, height, bitrate / 1000, IDR_INTERVAL,
        );

        Ok(Self {
            display,
            context,
            input_surfaces,
            recon_surfaces,
            current_input_surface: 0,
            current_recon_surface: 0,
            coded_buffers,
            current_coded_buffer: 0,
            last_ref: None,
            cached_sps_pps: None,
            width,
            height,
            bitrate,
            fps,
            frame_count: 0,
            force_idr: true, // first frame is always IDR
            nv12_format,
            dmabuf_input_surface: None,
        })
    }

    pub fn encode(&mut self, bgra: &[u8]) -> Result<Vec<u8>> {
        let is_idr = self.force_idr || self.frame_count % IDR_INTERVAL as u64 == 0;

        let input_idx = self.current_input_surface;
        self.current_input_surface = (self.current_input_surface + 1) % self.input_surfaces.len();

        let recon_idx = self.current_recon_surface;
        self.current_recon_surface = (self.current_recon_surface + 1) % self.recon_surfaces.len();

        let coded_idx = self.current_coded_buffer;
        self.current_coded_buffer = (self.current_coded_buffer + 1) % self.coded_buffers.len();

        // Convert BGRA to NV12 and upload to surface via VA image,
        // using the image's actual pitches/offsets (not assuming stride == width).
        {
            let mut image = libva::Image::create_from(
                &self.input_surfaces[input_idx],
                self.nv12_format,
                (self.width, self.height),
                (self.width, self.height),
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
                image.as_mut(),
                self.width as usize,
                self.height as usize,
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
        let frame_num = (self.frame_count % 65536) as u16;
        let poc = (self.frame_count * 2) as i32;

        let mut picture = Picture::new(
            self.frame_count,
            Rc::clone(&self.context),
            &self.input_surfaces[input_idx],
        );

        if is_idr {
            self.last_ref = None;

            let seq_param = self.build_sequence_params(mb_width as u16, mb_height as u16);
            let seq_buffer = self
                .context
                .create_buffer(BufferType::EncSequenceParameter(
                    EncSequenceParameter::H264(seq_param),
                ))
                .context("Failed to create seq buffer")?;
            picture.add_buffer(seq_buffer);

            for rc_buf_type in self.build_rate_control_buffers() {
                let buf = self
                    .context
                    .create_buffer(rc_buf_type)
                    .context("Failed to create rate control buffer")?;
                picture.add_buffer(buf);
            }
        }

        let pic_param = self.build_picture_params(
            self.recon_surfaces[recon_idx].id(),
            self.coded_buffers[coded_idx].id(),
            is_idr,
            frame_num,
            poc,
        );
        let pic_buffer = self
            .context
            .create_buffer(BufferType::EncPictureParameter(EncPictureParameter::H264(
                pic_param,
            )))
            .context("Failed to create pic buffer")?;
        picture.add_buffer(pic_buffer);

        let slice_param = self.build_slice_params(num_macroblocks, is_idr, frame_num, poc);
        let slice_buffer = self
            .context
            .create_buffer(BufferType::EncSliceParameter(EncSliceParameter::H264(
                slice_param,
            )))
            .context("Failed to create slice buffer")?;
        picture.add_buffer(slice_buffer);

        // Execute: begin → render → end → sync
        let picture = picture
            .begin()
            .map_err(|e| anyhow::anyhow!("vaBeginPicture failed: {}", e))?;
        let picture = picture
            .render()
            .map_err(|e| anyhow::anyhow!("vaRenderPicture failed: {}", e))?;
        let picture = picture
            .end()
            .map_err(|e| anyhow::anyhow!("vaEndPicture failed: {}", e))?;
        let _picture = picture
            .sync()
            .map_err(|(e, _)| anyhow::anyhow!("vaSyncSurface failed: {}", e))?;

        // Update DPB
        self.last_ref = Some(DpbEntry {
            surface_id: self.recon_surfaces[recon_idx].id(),
            frame_num,
            poc,
        });

        // Read encoded bitstream
        let mapped = MappedCodedBuffer::new(&self.coded_buffers[coded_idx])
            .map_err(|e| anyhow::anyhow!("Failed to map coded buffer: {}", e))?;

        let mut data = Vec::new();
        for segment in mapped.iter() {
            data.extend_from_slice(segment.buf);
        }

        // SPS/PPS handling: extract from IDR output or generate if missing
        if is_idr {
            if let Some(sps_pps) = super::extract_sps_pps(&data) {
                self.cached_sps_pps = Some(sps_pps);
            } else {
                // VA-API driver didn't include SPS/PPS — generate manually
                tracing::debug!("VA-API IDR missing SPS/PPS, generating manually");
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
    pub fn encode_dmabuf(
        &mut self,
        nv12_fd: std::os::unix::io::RawFd,
        width: u32,
        height: u32,
        stride: u32,
        offset: u32,
        modifier: u64,
    ) -> Result<Vec<u8>> {
        // Import the NV12 DMA-BUF surface on first call (or if dimensions changed)
        if self.dmabuf_input_surface.is_none() {
            let mut desc: libva::VADRMPRIMESurfaceDescriptor = unsafe { std::mem::zeroed() };
            desc.fourcc = u32::from_ne_bytes(*b"NV12");
            desc.width = width;
            desc.height = height;
            desc.num_objects = 1;
            desc.objects[0].fd = nv12_fd;
            desc.objects[0].size = stride * height * 3 / 2;
            desc.objects[0].drm_format_modifier = modifier;
            // NV12 has 2 planes: Y and UV
            desc.num_layers = 2;
            // Y plane
            desc.layers[0].drm_format = u32::from_ne_bytes(*b"NV12");
            desc.layers[0].num_planes = 1;
            desc.layers[0].object_index[0] = 0;
            desc.layers[0].offset[0] = offset;
            desc.layers[0].pitch[0] = stride;
            // UV plane (interleaved, after Y)
            desc.layers[1].drm_format = u32::from_ne_bytes(*b"NV12");
            desc.layers[1].num_planes = 1;
            desc.layers[1].object_index[0] = 0;
            desc.layers[1].offset[0] = offset + stride * height;
            desc.layers[1].pitch[0] = stride;

            let import = DmaBufSurfaceImport { desc };
            let surfaces = self
                .display
                .create_surfaces(
                    VA_RT_FORMAT_YUV420,
                    Some(u32::from_ne_bytes(*b"NV12")),
                    width,
                    height,
                    Some(UsageHint::USAGE_HINT_ENCODER),
                    vec![import],
                )
                .map_err(|e| anyhow::anyhow!("Failed to import NV12 DMA-BUF surface: {}", e))?;

            let surface = surfaces.into_iter().next().unwrap();
            tracing::info!(
                surface_id = surface.id(),
                "Encoder: imported NV12 DMA-BUF surface"
            );
            self.dmabuf_input_surface = Some(surface);
        }

        let dmabuf_surface = self.dmabuf_input_surface.as_ref().unwrap();
        let is_idr = self.force_idr || self.frame_count % IDR_INTERVAL as u64 == 0;

        let recon_idx = self.current_recon_surface;
        self.current_recon_surface = (self.current_recon_surface + 1) % self.recon_surfaces.len();

        let coded_idx = self.current_coded_buffer;
        self.current_coded_buffer = (self.current_coded_buffer + 1) % self.coded_buffers.len();

        let mb_width = self.width.div_ceil(16);
        let mb_height = self.height.div_ceil(16);
        let num_macroblocks = mb_width * mb_height;
        let frame_num = (self.frame_count % 65536) as u16;
        let poc = (self.frame_count * 2) as i32;

        let mut picture = Picture::new(
            self.frame_count,
            Rc::clone(&self.context),
            dmabuf_surface,
        );

        if is_idr {
            self.last_ref = None;
            let seq_param = self.build_sequence_params(mb_width as u16, mb_height as u16);
            let seq_buffer = self
                .context
                .create_buffer(BufferType::EncSequenceParameter(
                    EncSequenceParameter::H264(seq_param),
                ))
                .context("Failed to create seq buffer")?;
            picture.add_buffer(seq_buffer);

            for rc_buf_type in self.build_rate_control_buffers() {
                let buf = self
                    .context
                    .create_buffer(rc_buf_type)
                    .context("Failed to create rate control buffer")?;
                picture.add_buffer(buf);
            }
        }

        let pic_param = self.build_picture_params(
            self.recon_surfaces[recon_idx].id(),
            self.coded_buffers[coded_idx].id(),
            is_idr,
            frame_num,
            poc,
        );
        let pic_buffer = self
            .context
            .create_buffer(BufferType::EncPictureParameter(EncPictureParameter::H264(
                pic_param,
            )))
            .context("Failed to create pic buffer")?;
        picture.add_buffer(pic_buffer);

        let slice_param = self.build_slice_params(num_macroblocks, is_idr, frame_num, poc);
        let slice_buffer = self
            .context
            .create_buffer(BufferType::EncSliceParameter(EncSliceParameter::H264(
                slice_param,
            )))
            .context("Failed to create slice buffer")?;
        picture.add_buffer(slice_buffer);

        let picture = picture
            .begin()
            .map_err(|e| anyhow::anyhow!("vaBeginPicture failed (dmabuf): {}", e))?;
        let picture = picture
            .render()
            .map_err(|e| anyhow::anyhow!("vaRenderPicture failed (dmabuf): {}", e))?;
        let picture = picture
            .end()
            .map_err(|e| anyhow::anyhow!("vaEndPicture failed (dmabuf): {}", e))?;
        let _picture = picture
            .sync()
            .map_err(|(e, _)| anyhow::anyhow!("vaSyncSurface failed (dmabuf): {}", e))?;

        self.last_ref = Some(DpbEntry {
            surface_id: self.recon_surfaces[recon_idx].id(),
            frame_num,
            poc,
        });

        let mapped = MappedCodedBuffer::new(&self.coded_buffers[coded_idx])
            .map_err(|e| anyhow::anyhow!("Failed to map coded buffer: {}", e))?;

        let mut data = Vec::new();
        for segment in mapped.iter() {
            data.extend_from_slice(segment.buf);
        }

        // SPS/PPS handling (same as encode())
        if is_idr {
            if let Some(sps_pps) = super::extract_sps_pps(&data) {
                self.cached_sps_pps = Some(sps_pps);
            } else {
                tracing::debug!("VA-API IDR missing SPS/PPS, generating manually");
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

    /// Convert BGRA to NV12 (BT.709 limited range) directly into VA image buffer,
    /// respecting the image's pitch and plane offsets.
    #[allow(clippy::too_many_arguments)]
    fn bgra_to_nv12(
        bgra: &[u8],
        dst: &mut [u8],
        w: usize,
        h: usize,
        y_offset: usize,
        y_pitch: usize,
        uv_offset: usize,
        uv_pitch: usize,
    ) {
        // Y plane (BT.709 limited range)
        for row in 0..h {
            let dst_start = y_offset + row * y_pitch;
            for col in 0..w {
                let idx = (row * w + col) * 4;
                let b = bgra[idx] as i32;
                let g = bgra[idx + 1] as i32;
                let r = bgra[idx + 2] as i32;
                let y = ((47 * r + 157 * g + 16 * b + 128) >> 8) + 16;
                dst[dst_start + col] = y.clamp(0, 255) as u8;
            }
        }

        // UV plane (NV12 interleaved, BT.709 limited range, half resolution)
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
                        let idx = ((src_row + dy) * w + (src_col + dx)) * 4;
                        b_sum += bgra[idx] as i32;
                        g_sum += bgra[idx + 1] as i32;
                        r_sum += bgra[idx + 2] as i32;
                    }
                }

                let r = r_sum / 4;
                let g = g_sum / 4;
                let b = b_sum / 4;

                let u = ((-26 * r - 87 * g + 112 * b + 128) >> 8) + 128;
                let v = ((112 * r - 102 * g - 10 * b + 128) >> 8) + 128;

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
    ) -> libva::EncSequenceParameterBufferH264 {
        use libva::{EncSequenceParameterBufferH264, H264EncSeqFields, H264VuiFields};

        let seq_fields = H264EncSeqFields::new(
            1, // chroma_format_idc (4:2:0)
            1, // frame_mbs_only_flag
            0, // mb_adaptive_frame_field_flag
            0, // seq_scaling_matrix_present_flag
            1, // direct_8x8_inference_flag
            4, // log2_max_frame_num_minus4
            0, // pic_order_cnt_type
            4, // log2_max_pic_order_cnt_lsb_minus4
            0, // delta_pic_order_always_zero_flag
        );

        let vui_fields = H264VuiFields::new(
            0,  // aspect_ratio_info_present_flag
            1,  // timing_info_present_flag
            1,  // bitstream_restriction_flag (enables low-latency decode hints)
            16, // log2_max_mv_length_horizontal
            16, // log2_max_mv_length_vertical
            0,  // fixed_frame_rate_flag
            0,  // low_delay_hrd_flag
            1,  // motion_vectors_over_pic_boundaries_flag
        );

        EncSequenceParameterBufferH264::new(
            0,                     // seq_parameter_set_id
            self.get_h264_level(), // level_idc
            IDR_INTERVAL,          // intra_period
            IDR_INTERVAL,          // intra_idr_period
            1,                     // ip_period
            self.bitrate,           // bits_per_second
            1,                     // max_num_ref_frames
            mb_width,
            mb_height,
            &seq_fields,
            0,        // bit_depth_luma_minus8
            0,        // bit_depth_chroma_minus8
            0,        // num_ref_frames_in_pic_order_cnt_cycle
            0,        // offset_for_non_ref_pic
            0,        // offset_for_top_to_bottom_field
            [0; 256], // offset_for_ref_frame
            None,     // frame_crop
            Some(vui_fields),
            0,  // aspect_ratio_idc
            1,  // sar_width
            1,  // sar_height
            1,            // num_units_in_tick
            self.fps * 2, // time_scale (2 * fps for progressive)
        )
    }

    fn build_picture_params(
        &self,
        recon_surface_id: u32,
        coded_buf_id: u32,
        is_idr: bool,
        frame_num: u16,
        poc: i32,
    ) -> libva::EncPictureParameterBufferH264 {
        use libva::{EncPictureParameterBufferH264, H264EncPicFields, PictureH264};

        let curr_pic = PictureH264::new(
            recon_surface_id,
            frame_num as u32,
            VA_PICTURE_H264_SHORT_TERM_REFERENCE,
            poc,
            poc,
        );

        let mut reference_frames: [PictureH264; 16] = std::array::from_fn(|_| {
            PictureH264::new(VA_INVALID_SURFACE, 0, VA_PICTURE_H264_INVALID, 0, 0)
        });

        let num_ref_l0 = if !is_idr {
            if let Some(ref entry) = self.last_ref {
                reference_frames[0] = PictureH264::new(
                    entry.surface_id,
                    entry.frame_num as u32,
                    VA_PICTURE_H264_SHORT_TERM_REFERENCE,
                    entry.poc,
                    entry.poc,
                );
                1u8
            } else {
                0u8
            }
        } else {
            0u8
        };

        let pic_fields = H264EncPicFields::new(
            if is_idr { 1 } else { 0 }, // idr_pic_flag
            1,                          // reference_pic_flag
            1,                          // entropy_coding_mode_flag (CABAC)
            0,                          // weighted_pred_flag
            0,                          // weighted_bipred_idc
            0,                          // constrained_intra_pred_flag
            1,                          // transform_8x8_mode_flag
            1,                          // deblocking_filter_control_present_flag
            0,                          // redundant_pic_cnt_present_flag
            0,                          // pic_order_present_flag
            0,                          // pic_scaling_matrix_present_flag
        );

        EncPictureParameterBufferH264::new(
            curr_pic,
            reference_frames,
            coded_buf_id,
            0, // pic_parameter_set_id
            0, // seq_parameter_set_id
            0, // last_picture
            frame_num,
            23,                                              // pic_init_qp
            if num_ref_l0 > 0 { num_ref_l0 - 1 } else { 0 }, // num_ref_idx_l0_active_minus1
            0,                                               // num_ref_idx_l1_active_minus1
            0,                                               // chroma_qp_index_offset
            0,                                               // second_chroma_qp_index_offset
            &pic_fields,
        )
    }

    fn build_slice_params(
        &self,
        num_macroblocks: u32,
        is_idr: bool,
        frame_num: u16,
        poc: i32,
    ) -> libva::EncSliceParameterBufferH264 {
        use libva::{EncSliceParameterBufferH264, PictureH264};

        let slice_type = if is_idr { SLICE_TYPE_I } else { SLICE_TYPE_P };

        let mut ref_pic_list_0: [PictureH264; 32] = std::array::from_fn(|_| {
            PictureH264::new(VA_INVALID_SURFACE, 0, VA_PICTURE_H264_INVALID, 0, 0)
        });
        let ref_pic_list_1: [PictureH264; 32] = std::array::from_fn(|_| {
            PictureH264::new(VA_INVALID_SURFACE, 0, VA_PICTURE_H264_INVALID, 0, 0)
        });

        let (num_ref_override, num_ref_l0) = if !is_idr {
            if let Some(ref entry) = self.last_ref {
                ref_pic_list_0[0] = PictureH264::new(
                    entry.surface_id,
                    entry.frame_num as u32,
                    VA_PICTURE_H264_SHORT_TERM_REFERENCE,
                    entry.poc,
                    entry.poc,
                );
                (1u8, 0u8)
            } else {
                (0u8, 0u8)
            }
        } else {
            (0u8, 0u8)
        };

        EncSliceParameterBufferH264::new(
            0, // macroblock_address
            num_macroblocks,
            VA_INVALID_ID, // macroblock_info
            slice_type,
            0,                 // pic_parameter_set_id
            frame_num,         // idr_pic_id
            poc as u32 as u16, // pic_order_cnt_lsb
            0,                 // delta_pic_order_cnt_bottom
            [0, 0],            // delta_pic_order_cnt
            0,                 // direct_spatial_mv_pred_flag
            num_ref_override,
            num_ref_l0, // num_ref_idx_l0_active_minus1
            0,          // num_ref_idx_l1_active_minus1
            ref_pic_list_0,
            ref_pic_list_1,
            0, // luma_log2_weight_denom
            0, // chroma_log2_weight_denom
            0, // luma_weight_l0_flag
            [0; 32],
            [0; 32],
            0, // chroma_weight_l0_flag
            [[0; 2]; 32],
            [[0; 2]; 32],
            0, // luma_weight_l1_flag
            [0; 32],
            [0; 32],
            0, // chroma_weight_l1_flag
            [[0; 2]; 32],
            [[0; 2]; 32],
            0, // cabac_init_idc
            0, // slice_qp_delta
            0, // disable_deblocking_filter_idc
            0, // slice_alpha_c0_offset_div2
            0, // slice_beta_offset_div2
        )
    }

    fn build_rate_control_buffers(&self) -> Vec<BufferType> {
        let mut buffers = Vec::with_capacity(3);

        let rc = EncMiscParameterRateControl::new(
            self.bitrate,
            100,                                     // target_percentage (CBR)
            1000,                                    // window_size
            0,                                       // initial_qp
            18,                                      // min_qp
            0,                                       // basic_unit_size
            RcFlags::new(0, 1, 0, 0, 0, 0, 0, 0, 0), // disable_frame_skip=1
            0,                                       // icq_quality_factor
            40,                                      // max_qp
            0,                                       // quality_factor
            0,                                       // target_frame_size
        );
        buffers.push(BufferType::EncMiscParameter(EncMiscParameter::RateControl(
            rc,
        )));

        let hrd = EncMiscParameterHRD::new(self.bitrate / 2, self.bitrate);
        buffers.push(BufferType::EncMiscParameter(EncMiscParameter::HRD(hrd)));

        let fr = EncMiscParameterFrameRate::new(self.fps, 0);
        buffers.push(BufferType::EncMiscParameter(EncMiscParameter::FrameRate(
            fr,
        )));

        buffers
    }

    /// Generate SPS and PPS NAL units matching our encoder configuration.
    /// Used when the VA-API driver doesn't include them in the coded output.
    fn generate_sps_pps(&self) -> Vec<u8> {
        let mb_width = self.width.div_ceil(16);
        let mb_height = self.height.div_ceil(16);
        let coded_height = mb_height * 16;
        let need_crop = coded_height != self.height;
        let crop_bottom = if need_crop {
            (coded_height - self.height) / 2 // CropUnitY=2 for frame_mbs_only + 4:2:0
        } else {
            0
        };

        let mut buf = Vec::with_capacity(64);

        // === SPS (NAL type 7) ===
        buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]); // start code
        let mut bs = BitWriter::new();
        // NAL header: forbidden=0, nal_ref_idc=3, nal_unit_type=7
        bs.write_bits(0, 1); // forbidden_zero_bit
        bs.write_bits(3, 2); // nal_ref_idc
        bs.write_bits(7, 5); // nal_unit_type = SPS

        // profile_idc = 100 (High)
        bs.write_bits(100, 8);
        // constraint_set0..5_flags + reserved_zero_2bits
        bs.write_bits(0, 8);
        // level_idc
        bs.write_bits(self.get_h264_level() as u32, 8);
        // seq_parameter_set_id = 0
        bs.write_ue(0);

        // High profile extensions
        bs.write_ue(1); // chroma_format_idc = 1 (4:2:0)
        bs.write_ue(0); // bit_depth_luma_minus8
        bs.write_ue(0); // bit_depth_chroma_minus8
        bs.write_bits(0, 1); // qpprime_y_zero_transform_bypass_flag
        bs.write_bits(0, 1); // seq_scaling_matrix_present_flag

        bs.write_ue(4); // log2_max_frame_num_minus4
        bs.write_ue(0); // pic_order_cnt_type
        bs.write_ue(4); // log2_max_pic_order_cnt_lsb_minus4
        bs.write_ue(1); // max_num_ref_frames
        bs.write_bits(0, 1); // gaps_in_frame_num_value_allowed_flag
        bs.write_ue(mb_width - 1); // pic_width_in_mbs_minus1
        bs.write_ue(mb_height - 1); // pic_height_in_map_units_minus1
        bs.write_bits(1, 1); // frame_mbs_only_flag
        // (no mb_adaptive_frame_field_flag since frame_mbs_only=1)
        bs.write_bits(1, 1); // direct_8x8_inference_flag

        if need_crop {
            bs.write_bits(1, 1); // frame_cropping_flag
            bs.write_ue(0); // frame_crop_left_offset
            bs.write_ue(0); // frame_crop_right_offset
            bs.write_ue(0); // frame_crop_top_offset
            bs.write_ue(crop_bottom); // frame_crop_bottom_offset
        } else {
            bs.write_bits(0, 1); // frame_cropping_flag
        }

        // VUI parameters
        bs.write_bits(1, 1); // vui_parameters_present_flag
        bs.write_bits(0, 1); // aspect_ratio_info_present_flag
        bs.write_bits(0, 1); // overscan_info_present_flag
        bs.write_bits(0, 1); // video_signal_type_present_flag
        bs.write_bits(0, 1); // chroma_loc_info_present_flag
        bs.write_bits(1, 1); // timing_info_present_flag
        bs.write_bits(1, 32); // num_units_in_tick
        bs.write_bits(self.fps * 2, 32); // time_scale (2 * fps for progressive)
        bs.write_bits(0, 1); // fixed_frame_rate_flag
        bs.write_bits(0, 1); // nal_hrd_parameters_present_flag
        bs.write_bits(0, 1); // vcl_hrd_parameters_present_flag
        bs.write_bits(0, 1); // pic_struct_present_flag
        // Bitstream restriction: tells decoder no reordering needed → output immediately
        bs.write_bits(1, 1); // bitstream_restriction_flag
        bs.write_bits(1, 1); // motion_vectors_over_pic_boundaries_flag
        bs.write_ue(0); // max_bytes_per_pic_denom
        bs.write_ue(0); // max_bits_per_mb_denom
        bs.write_ue(16); // log2_max_mv_length_horizontal
        bs.write_ue(16); // log2_max_mv_length_vertical
        bs.write_ue(0); // max_num_reorder_frames (no B-frames → no reordering)
        bs.write_ue(1); // max_dec_frame_buffering (1 ref frame)

        bs.write_rbsp_trailing_bits();
        buf.extend_from_slice(&bs.finish_with_emulation_prevention());

        // === PPS (NAL type 8) ===
        buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]); // start code
        let mut bs = BitWriter::new();
        // NAL header
        bs.write_bits(0, 1); // forbidden_zero_bit
        bs.write_bits(3, 2); // nal_ref_idc
        bs.write_bits(8, 5); // nal_unit_type = PPS

        bs.write_ue(0); // pic_parameter_set_id
        bs.write_ue(0); // seq_parameter_set_id
        bs.write_bits(1, 1); // entropy_coding_mode_flag (CABAC)
        bs.write_bits(0, 1); // bottom_field_pic_order_in_frame_present_flag
        bs.write_ue(0); // num_slice_groups_minus1
        bs.write_ue(0); // num_ref_idx_l0_default_active_minus1
        bs.write_ue(0); // num_ref_idx_l1_default_active_minus1
        bs.write_bits(0, 1); // weighted_pred_flag
        bs.write_bits(0, 2); // weighted_bipred_idc
        bs.write_se(-3); // pic_init_qp_minus26 (23 - 26 = -3)
        bs.write_se(0); // pic_init_qs_minus26
        bs.write_se(0); // chroma_qp_index_offset
        bs.write_bits(1, 1); // deblocking_filter_control_present_flag
        bs.write_bits(0, 1); // constrained_intra_pred_flag
        bs.write_bits(0, 1); // redundant_pic_cnt_present_flag

        // High profile PPS extension
        bs.write_bits(1, 1); // transform_8x8_mode_flag
        bs.write_bits(0, 1); // pic_scaling_matrix_present_flag
        bs.write_se(0); // second_chroma_qp_index_offset

        bs.write_rbsp_trailing_bits();
        buf.extend_from_slice(&bs.finish_with_emulation_prevention());

        buf
    }
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
