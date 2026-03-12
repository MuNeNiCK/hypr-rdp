//! VA-API H.264 hardware encoder for Intel/AMD GPUs.
//!
//! Uses cros-libva for GPU-accelerated H.264 encoding. Surfaces are pooled
//! for triple-buffering, and a separate reconstructed surface pool enables
//! true inter-frame prediction via DPB tracking.
//!
//! Thread Safety: NOT Send — VA-API is thread-local. Create and use on the
//! same thread (the capture thread satisfies this).

use std::path::Path;
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

const DEVICE_PATH: &str = "/dev/dri/renderD128";
const BITRATE_BPS: u32 = 5_000_000;
const IDR_INTERVAL: u32 = 30;

#[derive(Clone)]
struct DpbEntry {
    surface_id: u32,
    frame_num: u16,
    poc: i32,
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
    frame_count: u64,
    force_idr: bool,
    nv12_format: VAImageFormat,
    nv12_buf: Vec<u8>,
}

impl VaapiEncoder {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            bail!("dimensions must be non-zero and even: {}x{}", width, height);
        }

        if !Path::new(DEVICE_PATH).exists() {
            bail!("VA-API device not found: {}", DEVICE_PATH);
        }

        tracing::info!(
            "Initializing VA-API encoder: {}x{}, device={}",
            width,
            height,
            DEVICE_PATH
        );

        let display = Display::open_drm_display(Path::new(DEVICE_PATH))
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

        let y_size = (width * height) as usize;
        let uv_size = (width as usize / 2) * (height as usize / 2) * 2;

        tracing::info!(
            profile = ?h264_profile,
            "VA-API encoder ready: {}x{}, {}kbps, IDR every {} frames",
            width, height, BITRATE_BPS / 1000, IDR_INTERVAL,
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
            frame_count: 0,
            force_idr: true,
            nv12_format,
            nv12_buf: vec![0u8; y_size + uv_size],
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

        // Convert BGRA to NV12 and upload to surface
        self.bgra_to_nv12(bgra);

        {
            let mut image = libva::Image::create_from(
                &self.input_surfaces[input_idx],
                self.nv12_format,
                (self.width, self.height),
                (self.width, self.height),
            )
            .context("Failed to create VA image")?;

            let image_data = image.as_mut();
            let copy_len = self.nv12_buf.len().min(image_data.len());
            image_data[..copy_len].copy_from_slice(&self.nv12_buf[..copy_len]);
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

        // SPS/PPS: cache from IDR, prepend to P-frames
        if is_idr {
            if let Some(sps_pps) = super::extract_sps_pps(&data) {
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

    /// Convert BGRA to NV12 (BT.601 limited range, integer math).
    fn bgra_to_nv12(&mut self, bgra: &[u8]) {
        let w = self.width as usize;
        let h = self.height as usize;
        let y_size = w * h;

        // Y plane (full resolution)
        for row in 0..h {
            for col in 0..w {
                let idx = (row * w + col) * 4;
                let b = bgra[idx] as i32;
                let g = bgra[idx + 1] as i32;
                let r = bgra[idx + 2] as i32;
                let y = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
                self.nv12_buf[row * w + col] = y.clamp(0, 255) as u8;
            }
        }

        // UV plane (half resolution, interleaved U-V pairs)
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

                let uv_idx = y_size + (row * half_w + col) * 2;
                self.nv12_buf[uv_idx] = u.clamp(0, 255) as u8;
                self.nv12_buf[uv_idx + 1] = v.clamp(0, 255) as u8;
            }
        }
    }

    fn get_h264_level(&self) -> u8 {
        let macroblocks_per_sec = (self.width / 16) * (self.height / 16) * 30;
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
            0,  // bitstream_restriction_flag
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
            BITRATE_BPS,           // bits_per_second
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
            1,  // num_units_in_tick
            30, // time_scale
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
            BITRATE_BPS,
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

        let hrd = EncMiscParameterHRD::new(BITRATE_BPS / 2, BITRATE_BPS);
        buffers.push(BufferType::EncMiscParameter(EncMiscParameter::HRD(hrd)));

        let fr = EncMiscParameterFrameRate::new(30, 0);
        buffers.push(BufferType::EncMiscParameter(EncMiscParameter::FrameRate(
            fr,
        )));

        buffers
    }
}
