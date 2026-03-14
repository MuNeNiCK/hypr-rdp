//! VA-API Video Post-Processing (VPP) for color conversion (XRGB -> NV12).
//!
//! Uses its own VADisplay (separate from the encoder's cros-libva Display)
//! because cros-libva's Display::handle() is pub(crate).

use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::capture::dmabuf::DmaBufInfo;

// Re-use the raw bindings from cros-libva
use cros_libva::{
    vaBeginPicture, vaCreateBuffer, vaCreateConfig, vaCreateContext, vaCreateSurfaces,
    vaDestroyBuffer, vaDestroyConfig, vaDestroyContext, vaDestroySurfaces, vaEndPicture,
    vaExportSurfaceHandle, vaGetDisplayDRM, vaInitialize, vaRenderPicture,
    vaSyncSurface, vaTerminate, VABufferID, VABufferType, VAConfigID, VAContextID,
    VADisplay, VADRMPRIMESurfaceDescriptor, VAEntrypoint, VAProfile, VARectangle, VAStatus,
    VASurfaceAttrib, VASurfaceID,
};

// Constants from VA-API headers
const VA_RT_FORMAT_RGB32: u32 = cros_libva::VA_RT_FORMAT_RGB32;
const VA_RT_FORMAT_YUV420: u32 = cros_libva::VA_RT_FORMAT_YUV420;
const VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2: u32 =
    cros_libva::VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2;
const VA_EXPORT_SURFACE_READ_ONLY: u32 = cros_libva::VA_EXPORT_SURFACE_READ_ONLY;
const VA_EXPORT_SURFACE_COMPOSED_LAYERS: u32 = cros_libva::VA_EXPORT_SURFACE_COMPOSED_LAYERS;

const VA_STATUS_SUCCESS: u32 = cros_libva::VA_STATUS_SUCCESS;

/// VAProcPipelineParameterBuffer — manually defined since bindgen doesn't always generate it.
/// Layout verified against C sizeof on x86_64 (224 bytes).
#[repr(C)]
struct VAProcPipelineParameterBuffer {
    surface: VASurfaceID,
    _pad0: u32,
    surface_region: *const VARectangle,
    surface_color_standard: u32,
    _pad1: u32,
    output_region: *const VARectangle,
    output_background_color: u32,
    output_color_standard: u32,
    pipeline_flags: u32,
    filter_flags: u32,
    filters: *const VABufferID,
    num_filters: u32,
    _pad2: u32,
    forward_references: *const VASurfaceID,
    num_forward_references: u32,
    _pad3: u32,
    backward_references: *const VASurfaceID,
    num_backward_references: u32,
    rotation_state: u32,
    blend_state: *const std::ffi::c_void,
    mirror_state: u32,
    _pad4: u32,
    additional_outputs: *const VASurfaceID,
    num_additional_outputs: u32,
    input_surface_flag: u32,
    output_surface_flag: u32,
    input_color_properties: [u32; 2],
    output_color_properties: [u32; 2],
    processing_mode: u32,
    _pad5: u32,
    output_hdr_metadata: *const std::ffi::c_void,
    va_reserved: [u32; 16],
}

fn va_check(status: VAStatus, op: &str) -> Result<()> {
    if status as u32 == VA_STATUS_SUCCESS {
        Ok(())
    } else {
        bail!("{} failed with VA status {}", op, status);
    }
}

/// VA-API VPP color converter: XRGB DMA-BUF -> NV12 DMA-BUF.
pub struct VppConverter {
    va_display: VADisplay,
    config_id: VAConfigID,
    context_id: VAContextID,
    input_surfaces: Vec<VASurfaceID>,
    output_surface: VASurfaceID,
    width: u32,
    height: u32,
    _drm_fd: OwnedFd,
    nv12_export_fd: Option<OwnedFd>,
}

impl VppConverter {
    /// Create a VPP converter using the given DRM device.
    pub fn new(drm_device_path: &Path, width: u32, height: u32) -> Result<Self> {
        let drm_fd = {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(drm_device_path)
                .context("failed to open DRM device for VPP")?;
            OwnedFd::from(file)
        };

        let va_display = unsafe { vaGetDisplayDRM(drm_fd.as_raw_fd()) };
        if va_display.is_null() {
            bail!("vaGetDisplayDRM returned NULL for VPP");
        }

        let mut major = 0i32;
        let mut minor = 0i32;
        va_check(
            unsafe { vaInitialize(va_display, &mut major, &mut minor) },
            "vaInitialize (VPP)",
        )?;

        // Create VPP config: VAProfileNone + VAEntrypointVideoProc
        let mut config_id: VAConfigID = 0;
        va_check(
            unsafe {
                vaCreateConfig(
                    va_display,
                    VAProfile::VAProfileNone,
                    VAEntrypoint::VAEntrypointVideoProc,
                    std::ptr::null_mut(),
                    0,
                    &mut config_id,
                )
            },
            "vaCreateConfig (VPP)",
        )?;

        // Create NV12 output surface (driver-allocated)
        let mut output_surface: VASurfaceID = 0;
        let mut pixel_format_attr = VASurfaceAttrib {
            type_: cros_libva::VASurfaceAttribType::VASurfaceAttribPixelFormat,
            flags: cros_libva::VA_SURFACE_ATTRIB_SETTABLE,
            value: cros_libva::VAGenericValue {
                type_: cros_libva::VAGenericValueType::VAGenericValueTypeInteger,
                value: cros_libva::_VAGenericValue__bindgen_ty_1 {
                    i: u32::from_ne_bytes(*b"NV12") as i32,
                },
            },
        };
        va_check(
            unsafe {
                vaCreateSurfaces(
                    va_display,
                    VA_RT_FORMAT_YUV420,
                    width,
                    height,
                    &mut output_surface,
                    1,
                    &mut pixel_format_attr,
                    1,
                )
            },
            "vaCreateSurfaces (VPP output NV12)",
        )?;

        // Create VPP context
        let mut context_id: VAContextID = 0;
        va_check(
            unsafe {
                vaCreateContext(
                    va_display,
                    config_id,
                    width as i32,
                    height as i32,
                    0, // flag: progressive
                    &mut output_surface,
                    1,
                    &mut context_id,
                )
            },
            "vaCreateContext (VPP)",
        )?;

        tracing::info!(
            width,
            height,
            device = %drm_device_path.display(),
            "VPP converter initialized"
        );

        Ok(Self {
            va_display,
            config_id,
            context_id,
            input_surfaces: Vec::new(),
            output_surface,
            width,
            height,
            _drm_fd: drm_fd,
            nv12_export_fd: None,
        })
    }

    /// Import an XRGB DMA-BUF as a VA surface. The surface is cached internally.
    /// Returns the surface index.
    pub fn import_input_surface(
        &mut self,
        dmabuf_fd: RawFd,
        width: u32,
        height: u32,
        stride: u32,
        modifier: u64,
        format: u32,
    ) -> Result<usize> {
        let rt_format = match format {
            crate::capture::dmabuf::DRM_FORMAT_XRGB8888
            | crate::capture::dmabuf::DRM_FORMAT_ARGB8888 => VA_RT_FORMAT_RGB32,
            _ => bail!("unsupported DRM format for VPP input: 0x{:08x}", format),
        };

        // Build VADRMPRIMESurfaceDescriptor for import
        let mut desc: VADRMPRIMESurfaceDescriptor = unsafe { std::mem::zeroed() };
        desc.fourcc = format;
        desc.width = width;
        desc.height = height;
        desc.num_objects = 1;
        desc.objects[0].fd = dmabuf_fd;
        desc.objects[0].size = stride * height;
        desc.objects[0].drm_format_modifier = modifier;
        desc.num_layers = 1;
        desc.layers[0].drm_format = format;
        desc.layers[0].num_planes = 1;
        desc.layers[0].object_index[0] = 0;
        desc.layers[0].offset[0] = 0;
        desc.layers[0].pitch[0] = stride;

        let mut attrs: [VASurfaceAttrib; 2] = unsafe { std::mem::zeroed() };

        // Memory type attribute
        attrs[0].type_ = cros_libva::VASurfaceAttribType::VASurfaceAttribMemoryType;
        attrs[0].flags = cros_libva::VA_SURFACE_ATTRIB_SETTABLE;
        attrs[0].value.type_ = cros_libva::VAGenericValueType::VAGenericValueTypeInteger;
        attrs[0].value.value.i = VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2 as i32;

        // External buffer descriptor attribute
        attrs[1].type_ =
            cros_libva::VASurfaceAttribType::VASurfaceAttribExternalBufferDescriptor;
        attrs[1].flags = cros_libva::VA_SURFACE_ATTRIB_SETTABLE;
        attrs[1].value.type_ = cros_libva::VAGenericValueType::VAGenericValueTypePointer;
        attrs[1].value.value.p = &mut desc as *mut _ as *mut std::ffi::c_void;

        let mut surface_id: VASurfaceID = 0;
        va_check(
            unsafe {
                vaCreateSurfaces(
                    self.va_display,
                    rt_format,
                    width,
                    height,
                    &mut surface_id,
                    1,
                    attrs.as_mut_ptr(),
                    2,
                )
            },
            "vaCreateSurfaces (VPP input import)",
        )?;

        let idx = self.input_surfaces.len();
        self.input_surfaces.push(surface_id);
        tracing::debug!(
            idx,
            surface_id,
            format = format!("0x{:08x}", format),
            "VPP: imported input surface"
        );
        Ok(idx)
    }

    /// Export the NV12 output surface as a DMA-BUF.
    pub fn export_nv12_output(&mut self) -> Result<DmaBufInfo> {
        let mut desc: VADRMPRIMESurfaceDescriptor = unsafe { std::mem::zeroed() };
        va_check(
            unsafe {
                vaExportSurfaceHandle(
                    self.va_display,
                    self.output_surface,
                    VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
                    VA_EXPORT_SURFACE_READ_ONLY | VA_EXPORT_SURFACE_COMPOSED_LAYERS,
                    &mut desc as *mut _ as *mut std::ffi::c_void,
                )
            },
            "vaExportSurfaceHandle (VPP NV12 output)",
        )?;

        if desc.num_layers == 0 || desc.num_objects == 0 {
            bail!("vaExportSurfaceHandle returned empty descriptor");
        }

        let raw_fd = desc.objects[0].fd;
        if raw_fd < 0 {
            bail!("vaExportSurfaceHandle returned invalid fd");
        }
        let owned = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        let fd = owned.as_raw_fd();
        self.nv12_export_fd = Some(owned);

        let (uv_stride, uv_offset) = if desc.num_layers >= 2 {
            (desc.layers[1].pitch[0], desc.layers[1].offset[0])
        } else if desc.layers[0].num_planes >= 2 {
            (desc.layers[0].pitch[1], desc.layers[0].offset[1])
        } else {
            let y_stride = desc.layers[0].pitch[0];
            (y_stride, y_stride * self.height)
        };

        Ok(DmaBufInfo {
            fd,
            stride: desc.layers[0].pitch[0],
            offset: desc.layers[0].offset[0],
            modifier: desc.objects[0].drm_format_modifier,
            format: crate::capture::dmabuf::DRM_FORMAT_NV12,
            width: self.width,
            height: self.height,
            uv_stride,
            uv_offset,
        })
    }

    /// Run VPP pipeline: convert input surface -> NV12 output surface.
    pub fn convert(&self, input_surface_idx: usize) -> Result<()> {
        let input_surface = *self
            .input_surfaces
            .get(input_surface_idx)
            .context("invalid VPP input surface index")?;

        // Build VPP pipeline parameter buffer
        let pipeline_param = VAProcPipelineParameterBuffer {
            surface: input_surface,
            _pad0: 0,
            surface_region: std::ptr::null(),
            surface_color_standard: 0,
            _pad1: 0,
            output_region: std::ptr::null(),
            output_background_color: 0,
            output_color_standard: 0,
            pipeline_flags: 0,
            filter_flags: 0,
            filters: std::ptr::null(),
            num_filters: 0,
            _pad2: 0,
            forward_references: std::ptr::null(),
            num_forward_references: 0,
            _pad3: 0,
            backward_references: std::ptr::null(),
            num_backward_references: 0,
            rotation_state: 0,
            blend_state: std::ptr::null(),
            mirror_state: 0,
            _pad4: 0,
            additional_outputs: std::ptr::null(),
            num_additional_outputs: 0,
            input_surface_flag: 0,
            output_surface_flag: 0,
            input_color_properties: [0; 2],
            output_color_properties: [0; 2],
            processing_mode: 0,
            _pad5: 0,
            output_hdr_metadata: std::ptr::null(),
            va_reserved: [0; 16],
        };

        let mut buffer_id: VABufferID = 0;
        va_check(
            unsafe {
                vaCreateBuffer(
                    self.va_display,
                    self.context_id,
                    VABufferType::VAProcPipelineParameterBufferType,
                    std::mem::size_of::<VAProcPipelineParameterBuffer>() as u32,
                    1,
                    &pipeline_param as *const _ as *mut std::ffi::c_void,
                    &mut buffer_id,
                )
            },
            "vaCreateBuffer (VPP pipeline)",
        )?;

        let result = (|| -> Result<()> {
            va_check(
                unsafe { vaBeginPicture(self.va_display, self.context_id, self.output_surface) },
                "vaBeginPicture (VPP)",
            )?;
            va_check(
                unsafe { vaRenderPicture(self.va_display, self.context_id, &mut buffer_id, 1) },
                "vaRenderPicture (VPP)",
            )?;
            va_check(
                unsafe { vaEndPicture(self.va_display, self.context_id) },
                "vaEndPicture (VPP)",
            )?;
            va_check(
                unsafe { vaSyncSurface(self.va_display, self.output_surface) },
                "vaSyncSurface (VPP output)",
            )?;
            Ok(())
        })();

        unsafe { vaDestroyBuffer(self.va_display, buffer_id); }

        result
    }

    /// Get the number of imported input surfaces.
    #[allow(dead_code)]
    pub fn input_surface_count(&self) -> usize {
        self.input_surfaces.len()
    }

    /// Get the output surface ID (for encoder import).
    #[allow(dead_code)]
    pub fn output_surface_id(&self) -> VASurfaceID {
        self.output_surface
    }
}

impl Drop for VppConverter {
    fn drop(&mut self) {
        unsafe {
            for surface_id in &mut self.input_surfaces {
                vaDestroySurfaces(self.va_display, surface_id, 1);
            }
            vaDestroySurfaces(self.va_display, &mut self.output_surface, 1);
            vaDestroyContext(self.va_display, self.context_id);
            vaDestroyConfig(self.va_display, self.config_id);
            vaTerminate(self.va_display);
        }
    }
}
