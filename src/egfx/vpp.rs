//! VA-API Video Post-Processing (VPP) for color conversion (XRGB -> NV12).
//!
use std::ffi::c_void;
use std::os::raw::c_ulong;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::rc::Rc;

use anyhow::{bail, Context, Result};
use libva_sys::va_display_drm as va;

use super::vaapi_sys::{
    self as sys, integer_surface_attr, pointer_surface_attr, va_check, VABufferID, VAConfigID,
    VAContextID, VADRMPRIMESurfaceDescriptor, VASurfaceAttrib, VASurfaceAttribExternalBuffers,
    VASurfaceID, VA_FOURCC_BGRA, VA_FOURCC_BGRX,
};

// Constants from VA-API headers
const VA_RT_FORMAT_RGB32: u32 = sys::VA_RT_FORMAT_RGB32;
const VA_RT_FORMAT_YUV420: u32 = sys::VA_RT_FORMAT_YUV420;
const VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME: u32 = sys::VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME;
const VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2: u32 = sys::VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2;
const VA_SURFACE_EXTBUF_DESC_ENABLE_TILING: u32 = sys::VA_SURFACE_EXTBUF_DESC_ENABLE_TILING;
const VA_EXPORT_SURFACE_READ_ONLY: u32 = sys::VA_EXPORT_SURFACE_READ_ONLY;
const VA_EXPORT_SURFACE_COMPOSED_LAYERS: u32 = sys::VA_EXPORT_SURFACE_COMPOSED_LAYERS;
const DRM_FORMAT_XRGB8888: u32 = 0x34325258;
const DRM_FORMAT_ARGB8888: u32 = 0x34324152;
const DRM_FORMAT_MOD_LINEAR: u64 = 0;

fn input_va_fourcc(drm_format: u32) -> Result<u32> {
    match drm_format {
        DRM_FORMAT_XRGB8888 => Ok(VA_FOURCC_BGRX),
        DRM_FORMAT_ARGB8888 => Ok(VA_FOURCC_BGRA),
        _ => bail!("unsupported DRM format for VPP input: 0x{:08x}", drm_format),
    }
}

fn input_surface_descriptor(
    dmabuf_fd: RawFd,
    width: u32,
    height: u32,
    stride: u32,
    modifier: u64,
    drm_format: u32,
) -> Result<(u32, VADRMPRIMESurfaceDescriptor)> {
    let va_fourcc = input_va_fourcc(drm_format)?;

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

/// Legacy `DRM_PRIME` descriptor for drivers without `PRIME_2` import (i965).
/// `buffer` must outlive the `vaCreateSurfaces` call that consumes it.
fn legacy_input_surface_descriptor(
    buffer: &mut c_ulong,
    width: u32,
    height: u32,
    stride: u32,
    modifier: u64,
    drm_format: u32,
) -> Result<VASurfaceAttribExternalBuffers> {
    let mut desc: VASurfaceAttribExternalBuffers = unsafe { std::mem::zeroed() };
    desc.pixel_format = input_va_fourcc(drm_format)?;
    desc.width = width;
    desc.height = height;
    desc.data_size = stride * height;
    desc.num_planes = 1;
    desc.pitches[0] = stride;
    desc.offsets[0] = 0;
    desc.buffers = buffer as *mut c_ulong as *mut usize;
    desc.num_buffers = 1;
    // The tiling flag must follow the real modifier: linear + ENABLE_TILING
    // is rejected by i965.
    desc.flags = if modifier == DRM_FORMAT_MOD_LINEAR {
        0
    } else {
        VA_SURFACE_EXTBUF_DESC_ENABLE_TILING.into()
    };
    Ok(desc)
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
    prime2_import: bool,
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

        let prime2_import = va_display.supports_prime2_import(config_id);
        let memory_types = va_display.surface_memory_types(config_id);

        tracing::info!(
            width,
            height,
            device = %drm_device_path.display(),
            memory_types = memory_types.map_or_else(
                || "query-failed".to_owned(),
                |mask| format!("0x{mask:08x}")
            ),
            import_path = if prime2_import {
                "DRM_PRIME_2"
            } else {
                "DRM_PRIME (legacy)"
            },
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
            prime2_import,
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
        let surface_id = if self.prime2_import {
            self.import_prime2_input_surface(dmabuf_fd, width, height, stride, modifier, format)?
        } else {
            self.import_legacy_input_surface(dmabuf_fd, width, height, stride, modifier, format)?
        };

        let idx = self.input_surfaces.len();
        self.input_surfaces.push(surface_id);
        tracing::trace!(
            idx,
            surface_id,
            format = format!("0x{:08x}", format),
            prime2 = self.prime2_import,
            "VPP: imported input surface"
        );
        Ok(idx)
    }

    fn import_prime2_input_surface(
        &self,
        dmabuf_fd: RawFd,
        width: u32,
        height: u32,
        stride: u32,
        modifier: u64,
        format: u32,
    ) -> Result<VASurfaceID> {
        let (rt_format, mut desc) =
            input_surface_descriptor(dmabuf_fd, width, height, stride, modifier, format)?;
        let mut attrs = [
            integer_surface_attr(
                va::VASurfaceAttribType_VASurfaceAttribMemoryType,
                VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2 as i32,
            ),
            pointer_surface_attr(
                va::VASurfaceAttribType_VASurfaceAttribExternalBufferDescriptor,
                &mut desc as *mut _ as *mut c_void,
            ),
        ];
        self.create_input_surface(
            rt_format,
            width,
            height,
            &mut attrs,
            "vaCreateSurfaces (VPP input import)",
        )
    }

    fn import_legacy_input_surface(
        &self,
        dmabuf_fd: RawFd,
        width: u32,
        height: u32,
        stride: u32,
        modifier: u64,
        format: u32,
    ) -> Result<VASurfaceID> {
        let mut buffer_fd = dmabuf_fd as c_ulong;
        let mut desc = legacy_input_surface_descriptor(
            &mut buffer_fd,
            width,
            height,
            stride,
            modifier,
            format,
        )?;
        let mut attrs = [
            integer_surface_attr(
                va::VASurfaceAttribType_VASurfaceAttribMemoryType,
                VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME as i32,
            ),
            pointer_surface_attr(
                va::VASurfaceAttribType_VASurfaceAttribExternalBufferDescriptor,
                &mut desc as *mut _ as *mut c_void,
            ),
        ];
        self.create_input_surface(
            VA_RT_FORMAT_RGB32,
            width,
            height,
            &mut attrs,
            "vaCreateSurfaces (VPP legacy input import)",
        )
    }

    fn create_input_surface(
        &self,
        rt_format: u32,
        width: u32,
        height: u32,
        attrs: &mut [VASurfaceAttrib],
        op: &str,
    ) -> Result<VASurfaceID> {
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
                    attrs.len() as u32,
                )
            },
            op,
        )?;
        Ok(surface_id)
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

        // Every exported object FD belongs to the caller, including on a
        // layout we cannot import. Adopt them before validating the descriptor.
        let mut exported_fds: Vec<OwnedFd> = desc
            .objects
            .iter()
            .take(desc.num_objects as usize)
            .filter(|object| object.fd >= 0)
            .map(|object| {
                // SAFETY: a successful export transfers each distinct object FD.
                unsafe { OwnedFd::from_raw_fd(object.fd) }
            })
            .collect();
        anyhow::ensure!(
            desc.num_layers != 0 && desc.num_objects != 0,
            "vaExportSurfaceHandle returned empty descriptor"
        );
        anyhow::ensure!(
            desc.num_objects == 1,
            "VPP encoder import requires a single DMA-BUF object, got {}",
            desc.num_objects
        );
        let owned = exported_fds
            .pop()
            .context("vaExportSurfaceHandle returned invalid fd")?;
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
            sys::va_sync_surface(
                self.va_display.raw(),
                self.output_surface,
                "vaSyncSurface (VPP output)",
            )?;
            Ok(())
        })();

        unsafe {
            va::vaDestroyBuffer(self.va_display.raw(), buffer_id);
        }

        result
    }

    /// Memory-type mask and import path this converter chose, for the failure
    /// record. Served from the display's per-config cache, never per frame.
    pub(crate) fn import_evidence(&self) -> (Option<u32>, &'static str) {
        let memory_types = self.va_display.surface_memory_types(self.config_id);
        let import_path = if self.prime2_import {
            "DRM_PRIME_2"
        } else {
            "DRM_PRIME (legacy)"
        };
        (memory_types, import_path)
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
            // libva requires all contexts using a surface to be destroyed first.
            va::vaDestroyContext(self.va_display.raw(), self.context_id);
            for surface_id in &mut self.input_surfaces {
                va::vaDestroySurfaces(self.va_display.raw(), surface_id, 1);
            }
            va::vaDestroySurfaces(self.va_display.raw(), &mut self.output_surface, 1);
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

    fn legacy_descriptor(modifier: u64) -> VASurfaceAttribExternalBuffers {
        let mut buffer = 7;
        legacy_input_surface_descriptor(
            &mut buffer,
            1920,
            1080,
            7680,
            modifier,
            DRM_FORMAT_XRGB8888,
        )
        .unwrap()
    }

    #[test]
    fn legacy_import_descriptor_describes_the_xrgb_buffer() {
        let desc = legacy_descriptor(DRM_FORMAT_MOD_LINEAR);

        assert_eq!(desc.pixel_format, VA_FOURCC_BGRX);
        assert_eq!(desc.width, 1920);
        assert_eq!(desc.height, 1080);
        assert_eq!(desc.data_size, 7680 * 1080);
        assert_eq!(desc.num_planes, 1);
        assert_eq!(desc.pitches[0], 7680);
        assert_eq!(desc.offsets[0], 0);
        assert_eq!(desc.num_buffers, 1);
        assert_eq!(unsafe { *desc.buffers }, 7);
    }

    #[test]
    fn legacy_import_descriptor_tiles_only_when_the_modifier_is_tiled() {
        // I915_FORMAT_MOD_X_TILED: the only non-linear layout i965 accepts here.
        assert_eq!(
            legacy_descriptor(0x0100_0000_0000_0001).flags as u32,
            VA_SURFACE_EXTBUF_DESC_ENABLE_TILING
        );
        assert_eq!(legacy_descriptor(DRM_FORMAT_MOD_LINEAR).flags, 0);
    }

    #[test]
    fn legacy_import_descriptor_rejects_unsupported_drm_format() {
        let mut buffer = 0;
        let error = legacy_input_surface_descriptor(&mut buffer, 1, 1, 4, 0, 0)
            .err()
            .expect("unsupported format must fail");

        assert!(error
            .to_string()
            .contains("unsupported DRM format for VPP input"));
    }
}
