use std::ffi::{c_void, CStr};
use std::mem;
use std::os::unix::io::{AsRawFd, OwnedFd};
use std::path::Path;
use std::rc::Rc;

use anyhow::{bail, Context, Result};
use libva_sys::va_display_drm as va;

pub(crate) use va::{
    VABufferID, VAConfigAttrib, VAConfigID, VAContextID, VADisplay, VAEntrypoint, VAImage,
    VAImageFormat, VAProfile, VARectangle, VAStatus, VASurfaceAttrib, VASurfaceID,
};

#[repr(C)]
#[derive(Copy, Clone)]
pub(crate) struct VADRMPRIMESurfaceObject {
    pub(crate) fd: i32,
    pub(crate) size: u32,
    pub(crate) drm_format_modifier: u64,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub(crate) struct VADRMPRIMESurfaceLayer {
    pub(crate) drm_format: u32,
    pub(crate) num_planes: u32,
    pub(crate) object_index: [u32; 4],
    pub(crate) offset: [u32; 4],
    pub(crate) pitch: [u32; 4],
}

#[repr(C)]
#[derive(Copy, Clone)]
pub(crate) struct VADRMPRIMESurfaceDescriptor {
    pub(crate) fourcc: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) num_objects: u32,
    pub(crate) objects: [VADRMPRIMESurfaceObject; 4],
    pub(crate) num_layers: u32,
    pub(crate) layers: [VADRMPRIMESurfaceLayer; 4],
}

pub(crate) const VA_STATUS_SUCCESS: u32 = va::VA_STATUS_SUCCESS;
pub(crate) const VA_INVALID_ID: u32 = va::VA_INVALID_ID;
pub(crate) const VA_INVALID_SURFACE: u32 = va::VA_INVALID_SURFACE;
pub(crate) const VA_RT_FORMAT_YUV420: u32 = va::VA_RT_FORMAT_YUV420;
pub(crate) const VA_RT_FORMAT_RGB32: u32 = va::VA_RT_FORMAT_RGB32;
pub(crate) const VA_RC_VBR: u32 = va::VA_RC_VBR;
pub(crate) const VA_RC_CQP: u32 = va::VA_RC_CQP;
pub(crate) const VA_CONFIG_ATTRIB_RT_FORMAT: u32 = va::VAConfigAttribType_VAConfigAttribRTFormat;
pub(crate) const VA_CONFIG_ATTRIB_RATE_CONTROL: u32 =
    va::VAConfigAttribType_VAConfigAttribRateControl;
pub(crate) const VA_PICTURE_H264_INVALID: u32 = va::VA_PICTURE_H264_INVALID;
pub(crate) const VA_PICTURE_H264_SHORT_TERM_REFERENCE: u32 =
    va::VA_PICTURE_H264_SHORT_TERM_REFERENCE;
pub(crate) const VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2: u32 = 0x4000_0000;
pub(crate) const VA_EXPORT_SURFACE_READ_ONLY: u32 = va::VA_EXPORT_SURFACE_READ_ONLY;
pub(crate) const VA_EXPORT_SURFACE_COMPOSED_LAYERS: u32 = va::VA_EXPORT_SURFACE_COMPOSED_LAYERS;

pub(crate) const VA_PROFILE_NONE: VAProfile = va::VAProfile_VAProfileNone;
pub(crate) const VA_PROFILE_H264_MAIN: VAProfile = va::VAProfile_VAProfileH264Main;
pub(crate) const VA_PROFILE_H264_HIGH: VAProfile = va::VAProfile_VAProfileH264High;
pub(crate) const VA_ENTRYPOINT_ENC_SLICE: VAEntrypoint = va::VAEntrypoint_VAEntrypointEncSlice;
pub(crate) const VA_ENTRYPOINT_VIDEO_PROC: VAEntrypoint = va::VAEntrypoint_VAEntrypointVideoProc;

pub(crate) fn fourcc(tag: &[u8; 4]) -> u32 {
    u32::from_ne_bytes(*tag)
}

pub(crate) fn va_check(status: VAStatus, op: &str) -> Result<()> {
    if status as u32 == VA_STATUS_SUCCESS {
        Ok(())
    } else {
        bail!("{} failed with VA status {}", op, status);
    }
}

pub(crate) struct VaDisplay {
    raw: VADisplay,
    _fd: OwnedFd,
}

impl VaDisplay {
    pub(crate) fn open_drm(path: &Path) -> Result<Rc<Self>> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("failed to open DRM device {}", path.display()))?;
        let fd = OwnedFd::from(file);

        let raw = unsafe { va::vaGetDisplayDRM(fd.as_raw_fd()) };
        if raw.is_null() {
            bail!("vaGetDisplayDRM returned NULL for {}", path.display());
        }

        let mut major = 0;
        let mut minor = 0;
        va_check(
            unsafe { va::vaInitialize(raw, &mut major, &mut minor) },
            "vaInitialize",
        )?;

        Ok(Rc::new(Self { raw, _fd: fd }))
    }

    pub(crate) fn query_vendor_string(&self) -> Result<String> {
        let ptr = unsafe { va::vaQueryVendorString(self.raw) };
        if ptr.is_null() {
            bail!("vaQueryVendorString returned NULL");
        }
        Ok(unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned())
    }

    pub(crate) fn query_config_profiles(&self) -> Result<Vec<VAProfile>> {
        let max = unsafe { va::vaMaxNumProfiles(self.raw) };
        if max <= 0 {
            return Ok(Vec::new());
        }

        let mut profiles = vec![0 as VAProfile; max as usize];
        let mut count = 0;
        va_check(
            unsafe { va::vaQueryConfigProfiles(self.raw, profiles.as_mut_ptr(), &mut count) },
            "vaQueryConfigProfiles",
        )?;
        profiles.truncate(count.max(0) as usize);
        Ok(profiles)
    }

    pub(crate) fn query_config_entrypoints(&self, profile: VAProfile) -> Result<Vec<VAEntrypoint>> {
        let max = unsafe { va::vaMaxNumEntrypoints(self.raw) };
        if max <= 0 {
            return Ok(Vec::new());
        }

        let mut entrypoints = vec![0 as VAEntrypoint; max as usize];
        let mut count = 0;
        va_check(
            unsafe {
                va::vaQueryConfigEntrypoints(
                    self.raw,
                    profile,
                    entrypoints.as_mut_ptr(),
                    &mut count,
                )
            },
            "vaQueryConfigEntrypoints",
        )?;
        entrypoints.truncate(count.max(0) as usize);
        Ok(entrypoints)
    }

    pub(crate) fn query_image_formats(&self) -> Result<Vec<VAImageFormat>> {
        let max = unsafe { va::vaMaxNumImageFormats(self.raw) };
        if max <= 0 {
            return Ok(Vec::new());
        }

        let mut formats: Vec<VAImageFormat> = (0..max).map(|_| unsafe { mem::zeroed() }).collect();
        let mut count = 0;
        va_check(
            unsafe { va::vaQueryImageFormats(self.raw, formats.as_mut_ptr(), &mut count) },
            "vaQueryImageFormats",
        )?;
        formats.truncate(count.max(0) as usize);
        Ok(formats)
    }

    pub(crate) fn create_config(
        self: &Rc<Self>,
        profile: VAProfile,
        entrypoint: VAEntrypoint,
        attribs: &[VAConfigAttrib],
    ) -> Result<VaConfig> {
        let mut id = 0;
        va_check(
            unsafe {
                va::vaCreateConfig(
                    self.raw,
                    profile,
                    entrypoint,
                    attribs.as_ptr().cast_mut(),
                    attribs.len() as i32,
                    &mut id,
                )
            },
            "vaCreateConfig",
        )?;
        Ok(VaConfig {
            display: Rc::clone(self),
            id,
        })
    }

    pub(crate) fn create_surfaces(
        self: &Rc<Self>,
        rt_format: u32,
        pixel_format: Option<u32>,
        width: u32,
        height: u32,
        usage_hint: Option<u32>,
        count: usize,
    ) -> Result<Vec<VaSurface>> {
        let mut attrs = Vec::new();
        if let Some(pixel_format) = pixel_format {
            attrs.push(integer_surface_attr(
                va::VASurfaceAttribType_VASurfaceAttribPixelFormat,
                pixel_format as i32,
            ));
        }
        if let Some(usage_hint) = usage_hint {
            attrs.push(integer_surface_attr(
                va::VASurfaceAttribType_VASurfaceAttribUsageHint,
                usage_hint as i32,
            ));
        }

        let mut ids = vec![0 as VASurfaceID; count];
        va_check(
            unsafe {
                va::vaCreateSurfaces(
                    self.raw,
                    rt_format,
                    width,
                    height,
                    ids.as_mut_ptr(),
                    ids.len() as u32,
                    attrs.as_mut_ptr(),
                    attrs.len() as u32,
                )
            },
            "vaCreateSurfaces",
        )?;

        Ok(ids
            .into_iter()
            .map(|id| VaSurface {
                display: Rc::clone(self),
                id,
            })
            .collect())
    }

    pub(crate) fn import_prime_surface(
        self: &Rc<Self>,
        rt_format: u32,
        width: u32,
        height: u32,
        desc: &mut VADRMPRIMESurfaceDescriptor,
    ) -> Result<VaSurface> {
        let mut attrs = [
            integer_surface_attr(
                va::VASurfaceAttribType_VASurfaceAttribMemoryType,
                VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2 as i32,
            ),
            pointer_surface_attr(
                va::VASurfaceAttribType_VASurfaceAttribExternalBufferDescriptor,
                desc as *mut _ as *mut c_void,
            ),
        ];

        let mut id = 0 as VASurfaceID;
        va_check(
            unsafe {
                va::vaCreateSurfaces(
                    self.raw,
                    rt_format,
                    width,
                    height,
                    &mut id,
                    1,
                    attrs.as_mut_ptr(),
                    attrs.len() as u32,
                )
            },
            "vaCreateSurfaces (DMA-BUF import)",
        )?;

        Ok(VaSurface {
            display: Rc::clone(self),
            id,
        })
    }

    pub(crate) fn create_context(
        self: &Rc<Self>,
        config: &VaConfig,
        width: u32,
        height: u32,
        render_targets: &mut [VASurfaceID],
    ) -> Result<VaContext> {
        let (targets_ptr, targets_len) = if render_targets.is_empty() {
            (std::ptr::null_mut(), 0)
        } else {
            (render_targets.as_mut_ptr(), render_targets.len() as i32)
        };

        let mut id = 0 as VAContextID;
        va_check(
            unsafe {
                va::vaCreateContext(
                    self.raw,
                    config.id,
                    width as i32,
                    height as i32,
                    va::VA_PROGRESSIVE as i32,
                    targets_ptr,
                    targets_len,
                    &mut id,
                )
            },
            "vaCreateContext",
        )?;

        Ok(VaContext {
            display: Rc::clone(self),
            id,
        })
    }
}

impl Drop for VaDisplay {
    fn drop(&mut self) {
        unsafe {
            va::vaTerminate(self.raw);
        }
    }
}

pub(crate) struct VaConfig {
    display: Rc<VaDisplay>,
    id: u32,
}

impl Drop for VaConfig {
    fn drop(&mut self) {
        unsafe {
            va::vaDestroyConfig(self.display.raw, self.id);
        }
    }
}

pub(crate) struct VaContext {
    display: Rc<VaDisplay>,
    id: VAContextID,
}

impl VaContext {
    pub(crate) fn create_coded_buffer(&self, size: usize) -> Result<VaBuffer> {
        let mut id = 0 as VABufferID;
        va_check(
            unsafe {
                va::vaCreateBuffer(
                    self.display.raw,
                    self.id,
                    va::VABufferType_VAEncCodedBufferType,
                    size as u32,
                    1,
                    std::ptr::null_mut(),
                    &mut id,
                )
            },
            "vaCreateBuffer (coded)",
        )?;
        Ok(VaBuffer {
            display: Rc::clone(&self.display),
            id,
        })
    }

    pub(crate) fn create_buffer<T>(&self, type_: u32, mut value: T, op: &str) -> Result<VaBuffer> {
        let mut id = 0 as VABufferID;
        va_check(
            unsafe {
                va::vaCreateBuffer(
                    self.display.raw,
                    self.id,
                    type_,
                    mem::size_of::<T>() as u32,
                    1,
                    &mut value as *mut _ as *mut c_void,
                    &mut id,
                )
            },
            op,
        )?;
        Ok(VaBuffer {
            display: Rc::clone(&self.display),
            id,
        })
    }

    pub(crate) fn create_misc_buffer<T>(&self, type_: u32, value: T, op: &str) -> Result<VaBuffer> {
        let mut packet: MiscEncParamBuffer<T> = unsafe { mem::zeroed() };
        packet.header.type_ = type_;
        packet.value = value;
        self.create_buffer(va::VABufferType_VAEncMiscParameterBufferType, packet, op)
    }

    pub(crate) fn render_picture(&self, surface: VASurfaceID, buffers: &[VaBuffer]) -> Result<()> {
        let mut ids: Vec<VABufferID> = buffers.iter().map(VaBuffer::id).collect();
        va_check(
            unsafe { va::vaBeginPicture(self.display.raw, self.id, surface) },
            "vaBeginPicture",
        )?;
        let render_result = va_check(
            unsafe {
                va::vaRenderPicture(
                    self.display.raw,
                    self.id,
                    ids.as_mut_ptr(),
                    ids.len() as i32,
                )
            },
            "vaRenderPicture",
        );
        let end_result = va_check(
            unsafe { va::vaEndPicture(self.display.raw, self.id) },
            "vaEndPicture",
        );
        render_result?;
        end_result?;
        va_check(
            unsafe { va::vaSyncSurface(self.display.raw, surface) },
            "vaSyncSurface",
        )
    }
}

impl Drop for VaContext {
    fn drop(&mut self) {
        unsafe {
            va::vaDestroyContext(self.display.raw, self.id);
        }
    }
}

pub(crate) struct VaSurface {
    display: Rc<VaDisplay>,
    id: VASurfaceID,
}

impl VaSurface {
    pub(crate) fn id(&self) -> VASurfaceID {
        self.id
    }

    pub(crate) fn display(&self) -> &Rc<VaDisplay> {
        &self.display
    }
}

impl Drop for VaSurface {
    fn drop(&mut self) {
        let mut id = self.id;
        unsafe {
            va::vaDestroySurfaces(self.display.raw, &mut id, 1);
        }
    }
}

pub(crate) struct VaBuffer {
    display: Rc<VaDisplay>,
    id: VABufferID,
}

impl VaBuffer {
    pub(crate) fn id(&self) -> VABufferID {
        self.id
    }

    pub(crate) fn read_coded(&self) -> Result<Vec<u8>> {
        let mut ptr = std::ptr::null_mut();
        va_check(
            unsafe { va::vaMapBuffer(self.display.raw, self.id, &mut ptr) },
            "vaMapBuffer (coded)",
        )?;
        let _mapping = MappedBuffer {
            display: Rc::clone(&self.display),
            id: self.id,
        };

        let mut data = Vec::new();
        let mut segment = ptr as *const va::VACodedBufferSegment;
        while !segment.is_null() {
            let segment_ref = unsafe { &*segment };
            if !segment_ref.buf.is_null() && segment_ref.size > 0 {
                let bytes = unsafe {
                    std::slice::from_raw_parts(
                        segment_ref.buf as *const u8,
                        segment_ref.size as usize,
                    )
                };
                data.extend_from_slice(bytes);
            }
            segment = segment_ref.next as *const va::VACodedBufferSegment;
        }

        Ok(data)
    }
}

impl Drop for VaBuffer {
    fn drop(&mut self) {
        unsafe {
            va::vaDestroyBuffer(self.display.raw, self.id);
        }
    }
}

struct MappedBuffer {
    display: Rc<VaDisplay>,
    id: VABufferID,
}

impl Drop for MappedBuffer {
    fn drop(&mut self) {
        unsafe {
            va::vaUnmapBuffer(self.display.raw, self.id);
        }
    }
}

pub(crate) struct VaImageMapping {
    display: Rc<VaDisplay>,
    surface_id: VASurfaceID,
    image: VAImage,
    data: *mut u8,
    data_len: usize,
    dirty: bool,
}

impl VaImageMapping {
    pub(crate) fn create_from(
        surface: &VaSurface,
        mut format: VAImageFormat,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        let display = Rc::clone(surface.display());
        let mut image: VAImage = unsafe { mem::zeroed() };
        va_check(
            unsafe {
                va::vaCreateImage(
                    display.raw,
                    &mut format,
                    width as i32,
                    height as i32,
                    &mut image,
                )
            },
            "vaCreateImage",
        )?;

        let result = (|| -> Result<Self> {
            va_check(
                unsafe {
                    va::vaGetImage(
                        display.raw,
                        surface.id(),
                        0,
                        0,
                        width,
                        height,
                        image.image_id,
                    )
                },
                "vaGetImage",
            )?;

            let mut ptr = std::ptr::null_mut();
            va_check(
                unsafe { va::vaMapBuffer(display.raw, image.buf, &mut ptr) },
                "vaMapBuffer (image)",
            )?;

            Ok(Self {
                display: Rc::clone(&display),
                surface_id: surface.id(),
                image,
                data: ptr as *mut u8,
                data_len: image.data_size as usize,
                dirty: false,
            })
        })();

        if result.is_err() {
            unsafe {
                va::vaDestroyImage(display.raw, image.image_id);
            }
        }

        result
    }

    pub(crate) fn image(&self) -> &VAImage {
        &self.image
    }

    pub(crate) fn data_mut(&mut self) -> &mut [u8] {
        self.dirty = true;
        unsafe { std::slice::from_raw_parts_mut(self.data, self.data_len) }
    }
}

impl Drop for VaImageMapping {
    fn drop(&mut self) {
        if self.dirty {
            unsafe {
                va::vaPutImage(
                    self.display.raw,
                    self.surface_id,
                    self.image.image_id,
                    0,
                    0,
                    self.image.width as u32,
                    self.image.height as u32,
                    0,
                    0,
                    self.image.width as u32,
                    self.image.height as u32,
                );
            }
        }

        unsafe {
            va::vaUnmapBuffer(self.display.raw, self.image.buf);
            va::vaDestroyImage(self.display.raw, self.image.image_id);
        }
    }
}

#[repr(C)]
struct MiscEncParamBuffer<T> {
    header: va::VAEncMiscParameterBuffer,
    value: T,
}

fn integer_surface_attr(type_: u32, value: i32) -> VASurfaceAttrib {
    let mut attr: VASurfaceAttrib = unsafe { mem::zeroed() };
    attr.type_ = type_;
    attr.flags = va::VA_SURFACE_ATTRIB_SETTABLE;
    attr.value.type_ = va::VAGenericValueType_VAGenericValueTypeInteger;
    attr.value.value.i = value;
    attr
}

fn pointer_surface_attr(type_: u32, value: *mut c_void) -> VASurfaceAttrib {
    let mut attr: VASurfaceAttrib = unsafe { mem::zeroed() };
    attr.type_ = type_;
    attr.flags = va::VA_SURFACE_ATTRIB_SETTABLE;
    attr.value.type_ = va::VAGenericValueType_VAGenericValueTypePointer;
    attr.value.value.p = value;
    attr
}
