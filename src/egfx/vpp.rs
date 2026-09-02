//! VA-API Video Post-Processing (VPP) for color conversion (XRGB -> NV12).
//!
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::rc::Rc;

use anyhow::{bail, Context, Result};
use libva_sys::va_display_drm as va;

use super::vaapi_sys::{
    self as sys, va_check, VABufferID, VAConfigID, VAContextID, VADRMPRIMESurfaceDescriptor,
    VASurfaceAttrib, VASurfaceID, VA_FOURCC_BGRA, VA_FOURCC_BGRX,
};

// Constants from VA-API headers
const VA_RT_FORMAT_RGB32: u32 = sys::VA_RT_FORMAT_RGB32;
const VA_RT_FORMAT_YUV420: u32 = sys::VA_RT_FORMAT_YUV420;
const VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2: u32 = sys::VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2;
const VA_EXPORT_SURFACE_READ_ONLY: u32 = sys::VA_EXPORT_SURFACE_READ_ONLY;
const VA_EXPORT_SURFACE_COMPOSED_LAYERS: u32 = sys::VA_EXPORT_SURFACE_COMPOSED_LAYERS;
const DRM_FORMAT_XRGB8888: u32 = 0x34325258;
const DRM_FORMAT_ARGB8888: u32 = 0x34324152;

fn input_surface_descriptor(
    dmabuf_fd: RawFd,
    width: u32,
    height: u32,
    stride: u32,
    modifier: u64,
    drm_format: u32,
) -> Result<(u32, VADRMPRIMESurfaceDescriptor)> {
    let va_fourcc = match drm_format {
        DRM_FORMAT_XRGB8888 => VA_FOURCC_BGRX,
        DRM_FORMAT_ARGB8888 => VA_FOURCC_BGRA,
        _ => bail!("unsupported DRM format for VPP input: 0x{:08x}", drm_format),
    };

    let mut desc: VADRMPRIMESurfaceDescriptor = unsafe { std::mem::zeroed() };
    desc.fourcc = va_fourcc;
    desc.width = width;
    desc.height = height;
    desc.num_objects = 1;
    desc.objects[0].fd = dmabuf_fd;
    desc.objects[0].size = stride * height;
    desc.objects[0].drm_format_modifier = modifier;
    desc.num_layers = 1;
    desc.layers[0].drm_format = drm_format;
    desc.layers[0].num_planes = 1;
    desc.layers[0].object_index[0] = 0;
    desc.layers[0].offset[0] = 0;
    desc.layers[0].pitch[0] = stride;

    Ok((VA_RT_FORMAT_RGB32, desc))
}

#[derive(Debug, Clone)]
pub(crate) struct VppDmaBufInfo {
    pub(crate) fd: RawFd,
    pub(crate) stride: u32,
    pub(crate) offset: u32,
    pub(crate) modifier: u64,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) uv_stride: u32,
    pub(crate) uv_offset: u32,
}

#[cfg(all(target_arch = "x86_64", target_pointer_width = "64"))]
const _: () = {
    assert!(std::mem::size_of::<va::VAProcPipelineParameterBuffer>() == 224);
    assert!(std::mem::offset_of!(va::VAProcPipelineParameterBuffer, output_hdr_metadata) == 152);
    assert!(std::mem::offset_of!(va::VAProcPipelineParameterBuffer, va_reserved) == 160);
};

/// VA-API VPP color converter: XRGB DMA-BUF -> NV12 DMA-BUF.
pub struct VppConverter {
    va_display: Rc<sys::VaDisplay>,
    config_id: VAConfigID,
    context_id: VAContextID,
    input_surfaces: Vec<VASurfaceID>,
    output_surface: VASurfaceID,
    width: u32,
    height: u32,
    nv12_export_fd: Option<OwnedFd>,
}

impl VppConverter {
    /// Create a VPP converter using the given DRM device.
    pub fn new(drm_device_path: &Path, width: u32, height: u32) -> Result<Self> {
        let va_display = sys::VaDisplay::open_drm(drm_device_path)?;
        let display = va_display.raw();

        // Create VPP config: VAProfileNone + VAEntrypointVideoProc
        let mut config_id: VAConfigID = 0;
        va_check(
            unsafe {
                va::vaCreateConfig(
                    display,
                    sys::VA_PROFILE_NONE,
                    sys::VA_ENTRYPOINT_VIDEO_PROC,
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
            type_: va::VASurfaceAttribType_VASurfaceAttribPixelFormat,
            flags: va::VA_SURFACE_ATTRIB_SETTABLE,
            value: va::VAGenericValue {
                type_: va::VAGenericValueType_VAGenericValueTypeInteger,
                value: va::_VAGenericValue__bindgen_ty_1 {
                    i: u32::from_ne_bytes(*b"NV12") as i32,
                },
            },
        };
        va_check(
            unsafe {
                va::vaCreateSurfaces(
                    display,
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
                va::vaCreateContext(
                    display,
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
        let (rt_format, mut desc) =
            input_surface_descriptor(dmabuf_fd, width, height, stride, modifier, format)?;

        let mut attrs: [VASurfaceAttrib; 2] = unsafe { std::mem::zeroed() };

        // Memory type attribute
        attrs[0].type_ = va::VASurfaceAttribType_VASurfaceAttribMemoryType;
        attrs[0].flags = va::VA_SURFACE_ATTRIB_SETTABLE;
        attrs[0].value.type_ = va::VAGenericValueType_VAGenericValueTypeInteger;
        attrs[0].value.value.i = VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2 as i32;

        // External buffer descriptor attribute
        attrs[1].type_ = va::VASurfaceAttribType_VASurfaceAttribExternalBufferDescriptor;
        attrs[1].flags = va::VA_SURFACE_ATTRIB_SETTABLE;
        attrs[1].value.type_ = va::VAGenericValueType_VAGenericValueTypePointer;
        attrs[1].value.value.p = &mut desc as *mut _ as *mut std::ffi::c_void;

        let mut surface_id: VASurfaceID = 0;
        va_check(
            unsafe {
                va::vaCreateSurfaces(
                    self.va_display.raw(),
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
        tracing::trace!(
            idx,
            surface_id,
            format = format!("0x{:08x}", format),
            "VPP: imported input surface"
        );
        Ok(idx)
    }

    /// Export the NV12 output surface as a DMA-BUF.
    pub fn export_nv12_output(&mut self) -> Result<VppDmaBufInfo> {
        let mut desc: VADRMPRIMESurfaceDescriptor = unsafe { std::mem::zeroed() };
        va_check(
            unsafe {
                va::vaExportSurfaceHandle(
                    self.va_display.raw(),
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

        Ok(VppDmaBufInfo {
            fd,
            stride: desc.layers[0].pitch[0],
            offset: desc.layers[0].offset[0],
            modifier: desc.objects[0].drm_format_modifier,
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

        // vaCreateBuffer copies the full struct, including reserved bytes and padding.
        // SAFETY: the binding contains only integers, raw pointers, and arrays thereof.
        let mut pipeline_param: va::VAProcPipelineParameterBuffer = unsafe { std::mem::zeroed() };
        pipeline_param.surface = input_surface;

        let mut buffer_id: VABufferID = 0;
        va_check(
            unsafe {
                va::vaCreateBuffer(
                    self.va_display.raw(),
                    self.context_id,
                    va::VABufferType_VAProcPipelineParameterBufferType,
                    std::mem::size_of::<va::VAProcPipelineParameterBuffer>() as u32,
                    1,
                    &pipeline_param as *const _ as *mut std::ffi::c_void,
                    &mut buffer_id,
                )
            },
            "vaCreateBuffer (VPP pipeline)",
        )?;

        let result = (|| -> Result<()> {
            va_check(
                unsafe {
                    va::vaBeginPicture(self.va_display.raw(), self.context_id, self.output_surface)
                },
                "vaBeginPicture (VPP)",
            )?;
            va_check(
                unsafe {
                    va::vaRenderPicture(self.va_display.raw(), self.context_id, &mut buffer_id, 1)
                },
                "vaRenderPicture (VPP)",
            )?;
            va_check(
                unsafe { va::vaEndPicture(self.va_display.raw(), self.context_id) },
                "vaEndPicture (VPP)",
            )?;
            va_check(
                unsafe { va::vaSyncSurface(self.va_display.raw(), self.output_surface) },
                "vaSyncSurface (VPP output)",
            )?;
            Ok(())
        })();

        unsafe {
            va::vaDestroyBuffer(self.va_display.raw(), buffer_id);
        }

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
                va::vaDestroySurfaces(self.va_display.raw(), surface_id, 1);
            }
            va::vaDestroySurfaces(self.va_display.raw(), &mut self.output_surface, 1);
            va::vaDestroyContext(self.va_display.raw(), self.context_id);
            va::vaDestroyConfig(self.va_display.raw(), self.config_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vpp_import_descriptor_maps_xrgb_to_va_bgrx() {
        let (rt_format, desc) =
            input_surface_descriptor(7, 1920, 1080, 7680, 42, DRM_FORMAT_XRGB8888).unwrap();

        assert_eq!(rt_format, VA_RT_FORMAT_RGB32);
        assert_eq!(desc.fourcc, VA_FOURCC_BGRX);
        assert_eq!(desc.layers[0].drm_format, DRM_FORMAT_XRGB8888);
        assert_eq!(desc.objects[0].fd, 7);
        assert_eq!(desc.objects[0].drm_format_modifier, 42);
        assert_eq!(desc.layers[0].pitch[0], 7680);
    }

    #[test]
    fn vpp_import_descriptor_maps_argb_to_va_bgra() {
        let (_, desc) =
            input_surface_descriptor(8, 1280, 720, 5120, 9, DRM_FORMAT_ARGB8888).unwrap();

        assert_eq!(desc.fourcc, VA_FOURCC_BGRA);
        assert_eq!(desc.layers[0].drm_format, DRM_FORMAT_ARGB8888);
    }

    #[test]
    fn vpp_import_descriptor_rejects_unsupported_drm_format() {
        let error = input_surface_descriptor(0, 1, 1, 4, 0, 0)
            .err()
            .expect("unsupported format must fail");

        assert!(error
            .to_string()
            .contains("unsupported DRM format for VPP input"));
    }
}
