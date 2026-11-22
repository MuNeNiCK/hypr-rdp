use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{c_void, CStr};
use std::fmt;
use std::mem;
use std::os::unix::io::{AsRawFd, OwnedFd};
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use libva_sys::va_display_drm as va;

pub(crate) use va::{
    VABufferID, VAConfigAttrib, VAConfigID, VAContextID, VADisplay, VAEntrypoint, VAImage,
    VAImageFormat, VAProfile, VAStatus, VASurfaceAttrib, VASurfaceAttribExternalBuffers,
    VASurfaceID,
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
pub(crate) const VA_FOURCC_BGRX: u32 = va::VA_FOURCC_BGRX;
pub(crate) const VA_FOURCC_BGRA: u32 = va::VA_FOURCC_BGRA;
pub(crate) const VA_RC_VBR: u32 = va::VA_RC_VBR;
pub(crate) const VA_RC_CQP: u32 = va::VA_RC_CQP;
pub(crate) const VA_CONFIG_ATTRIB_RT_FORMAT: u32 = va::VAConfigAttribType_VAConfigAttribRTFormat;
pub(crate) const VA_CONFIG_ATTRIB_RATE_CONTROL: u32 =
    va::VAConfigAttribType_VAConfigAttribRateControl;
pub(crate) const VA_PICTURE_H264_INVALID: u32 = va::VA_PICTURE_H264_INVALID;
pub(crate) const VA_PICTURE_H264_SHORT_TERM_REFERENCE: u32 =
    va::VA_PICTURE_H264_SHORT_TERM_REFERENCE;
pub(crate) const VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME: u32 = 0x2000_0000;
pub(crate) const VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2: u32 = 0x4000_0000;
pub(crate) const VA_SURFACE_EXTBUF_DESC_ENABLE_TILING: u32 = 0x0000_0008;
const DRM_FORMAT_MOD_LINEAR: u64 = 0;
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

/// Deadline for one GPU surface-completion wait.
///
/// A healthy 1080p VPP conversion or H.264 encode finishes in single-digit
/// milliseconds, so 2000 ms is roughly a hundred frames of headroom: a wait
/// that reaches it is a hung driver, not a slow frame. The blocking
/// `vaSyncSurface` this replaced had no bound at all and once stalled a live
/// session for 90 minutes without writing a single log line.
const GPU_SURFACE_SYNC_TIMEOUT: Duration = Duration::from_millis(2_000);

/// Pause between `vaQuerySurfaceStatus` polls. Short enough to stay inside a
/// frame interval, long enough not to burn a core while the GPU works.
const GPU_SURFACE_SYNC_POLL_INTERVAL: Duration = Duration::from_millis(1);

const VA_SURFACE_READY: u32 = va::VASurfaceStatus_VASurfaceReady;

/// A GPU surface that never reported completion before its deadline.
#[derive(Debug)]
pub(crate) struct SurfaceSyncTimeout {
    operation: &'static str,
    surface: VASurfaceID,
    status: u32,
}

impl SurfaceSyncTimeout {
    pub(crate) fn operation(&self) -> &'static str {
        self.operation
    }

    pub(crate) fn surface(&self) -> VASurfaceID {
        self.surface
    }

    pub(crate) fn status(&self) -> u32 {
        self.status
    }
}

impl fmt::Display for SurfaceSyncTimeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} on surface {} did not complete within {} ms (last VA status {})",
            self.operation,
            self.surface,
            GPU_SURFACE_SYNC_TIMEOUT.as_millis(),
            self.status
        )
    }
}

impl std::error::Error for SurfaceSyncTimeout {}

/// Whether `now` has reached `deadline`.
fn sync_deadline_passed(deadline: Instant, now: Instant) -> bool {
    now >= deadline
}

/// Whether a `vaQuerySurfaceStatus` result means the surface is done.
fn surface_status_complete(status: u32) -> bool {
    status & VA_SURFACE_READY != 0
}

/// Wait for `surface` to finish, bounded by [`GPU_SURFACE_SYNC_TIMEOUT`].
///
/// `vaSyncSurface` blocks inside the driver, where no timeout can interrupt
/// it; `vaQuerySurfaceStatus` can be polled. The healthy path returns after a
/// single query and writes nothing to the log.
pub(crate) fn va_sync_surface(
    display: VADisplay,
    surface: VASurfaceID,
    operation: &'static str,
) -> Result<()> {
    let deadline = Instant::now() + GPU_SURFACE_SYNC_TIMEOUT;
    loop {
        let mut status: u32 = 0;
        va_check(
            unsafe { va::vaQuerySurfaceStatus(display, surface, &mut status) },
            "vaQuerySurfaceStatus",
        )?;

        if surface_status_complete(status) {
            return Ok(());
        }

        if sync_deadline_passed(deadline, Instant::now()) {
            return Err(SurfaceSyncTimeout {
                operation,
                surface,
                status,
            }
            .into());
        }

        std::thread::sleep(GPU_SURFACE_SYNC_POLL_INTERVAL);
    }
}

/// Whether a `VASurfaceAttribMemoryType` bitmask advertises `DRM_PRIME_2`
/// import. Drivers that predate `PRIME_2` (i965) report only the legacy
/// `DRM_PRIME` bit, and a driver that reports no memory-type attribute at all
/// yields an empty mask; both mean the caller must use legacy `DRM_PRIME`.
pub(crate) fn prime2_import_supported(memory_types: u32) -> bool {
    memory_types & VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2 != 0
}

/// Legacy `DRM_PRIME` descriptor for the NV12 encoder input, built from the
/// same layers as the `PRIME_2` descriptor. The UV offset is passed through
/// unchanged: i965 needs the driver-reported (tile-padded) value.
/// `buffer` must outlive the `vaCreateSurfaces` call that consumes it.
fn legacy_nv12_surface_descriptor(
    buffer: &mut usize,
    width: u32,
    height: u32,
    desc: &VADRMPRIMESurfaceDescriptor,
) -> VASurfaceAttribExternalBuffers {
    let y_stride = desc.layers[0].pitch[0];
    let y_offset = desc.layers[0].offset[0];
    let uv_stride = desc.layers[1].pitch[0];
    let uv_offset = desc.layers[1].offset[0];

    let mut legacy: VASurfaceAttribExternalBuffers = unsafe { mem::zeroed() };
    legacy.pixel_format = fourcc(b"NV12");
    legacy.width = width;
    legacy.height = height;
    legacy.num_planes = 2;
    legacy.pitches[0] = y_stride;
    legacy.pitches[1] = uv_stride;
    legacy.offsets[0] = y_offset;
    legacy.offsets[1] = uv_offset;
    legacy.data_size = (y_offset + height * y_stride).max(uv_offset + (height / 2) * uv_stride);
    legacy.buffers = buffer as *mut usize;
    legacy.num_buffers = 1;
    legacy.flags = if desc.objects[0].drm_format_modifier == DRM_FORMAT_MOD_LINEAR {
        0
    } else {
        VA_SURFACE_EXTBUF_DESC_ENABLE_TILING.into()
    };
    legacy
}

pub(crate) struct VaDisplay {
    raw: VADisplay,
    _fd: OwnedFd,
    surface_memory_types_cache: RefCell<HashMap<VAConfigID, Option<u32>>>,
}

struct PendingVaDisplay(VADisplay);

impl PendingVaDisplay {
    fn into_raw(self) -> VADisplay {
        let raw = self.0;
        mem::forget(self);
        raw
    }
}

impl Drop for PendingVaDisplay {
    fn drop(&mut self) {
        unsafe {
            va::vaTerminate(self.0);
        }
    }
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
        let pending = PendingVaDisplay(raw);

        let mut major = 0;
        let mut minor = 0;
        va_check(
            unsafe { va::vaInitialize(raw, &mut major, &mut minor) },
            "vaInitialize",
        )?;

        Ok(Rc::new(Self {
            raw: pending.into_raw(),
            _fd: fd,
            surface_memory_types_cache: RefCell::new(HashMap::new()),
        }))
    }

    /// Memory-type bitmask that the driver advertises for a config, or `None`
    /// when it refuses the query. Cached per config id: the capture path asks
    /// on every start-up, never per frame.
    pub(crate) fn surface_memory_types(&self, config_id: VAConfigID) -> Option<u32> {
        if let Some(cached) = self.surface_memory_types_cache.borrow().get(&config_id) {
            return *cached;
        }
        let queried = self.query_surface_memory_types(config_id);
        self.surface_memory_types_cache
            .borrow_mut()
            .insert(config_id, queried);
        queried
    }

    /// Whether `DRM_PRIME_2` surfaces can be imported for this config.
    ///
    /// A failed query keeps the pre-existing behaviour (`PRIME_2`), while a
    /// successful query without the `PRIME_2` bit — including one that reports
    /// no memory-type attribute — selects the legacy `DRM_PRIME` path.
    pub(crate) fn supports_prime2_import(&self, config_id: VAConfigID) -> bool {
        self.surface_memory_types(config_id)
            .is_none_or(prime2_import_supported)
    }

    /// i965 treats `*num_attribs` as the capacity of the caller's buffer and
    /// fills in the written count, so a zero capacity fails with
    /// `MAX_NUM_EXCEEDED` and leaves the list untouched.
    fn query_surface_memory_types(&self, config_id: VAConfigID) -> Option<u32> {
        const ATTRIB_CAPACITY: usize = 64;

        let mut attribs: Vec<VASurfaceAttrib> = (0..ATTRIB_CAPACITY)
            .map(|_| unsafe { mem::zeroed() })
            .collect();
        let mut count = ATTRIB_CAPACITY as u32;
        let status = unsafe {
            va::vaQuerySurfaceAttributes(
                self.raw,
                config_id,
                attribs.as_mut_ptr(),
                &mut count,
            )
        };
        if let Err(error) = va_check(status, "vaQuerySurfaceAttributes") {
            tracing::debug!(
                config_id,
                %error,
                "Surface attribute query failed; assuming PRIME_2 import"
            );
            return None;
        }

        attribs
            .iter()
            .take((count as usize).min(ATTRIB_CAPACITY))
            .find(|attrib| attrib.type_ == va::VASurfaceAttribType_VASurfaceAttribMemoryType)
            .map(|attrib| unsafe { attrib.value.value.i as u32 })
    }

    pub(super) fn raw(&self) -> VADisplay {
        self.raw
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

    /// Query a single configuration attribute for (profile, entrypoint).
    /// Returns the raw attribute value (`VA_ATTRIB_NOT_SUPPORTED` when the
    /// driver does not know the attribute).
    pub(crate) fn get_config_attrib(
        &self,
        profile: VAProfile,
        entrypoint: VAEntrypoint,
        attrib_type: u32,
    ) -> Result<u32> {
        let mut attrib = VAConfigAttrib {
            type_: attrib_type,
            value: 0,
        };
        va_check(
            unsafe { va::vaGetConfigAttributes(self.raw, profile, entrypoint, &mut attrib, 1) },
            "vaGetConfigAttributes",
        )?;
        Ok(attrib.value)
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
        config_id: VAConfigID,
        rt_format: u32,
        width: u32,
        height: u32,
        desc: &mut VADRMPRIMESurfaceDescriptor,
    ) -> Result<VaSurface> {
        let mut id = 0 as VASurfaceID;

        if self.supports_prime2_import(config_id) {
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
        } else {
            let mut buffer = desc.objects[0].fd as usize;
            let mut legacy = legacy_nv12_surface_descriptor(&mut buffer, width, height, desc);
            let mut attrs = [
                integer_surface_attr(
                    va::VASurfaceAttribType_VASurfaceAttribMemoryType,
                    VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME as i32,
                ),
                pointer_surface_attr(
                    va::VASurfaceAttribType_VASurfaceAttribExternalBufferDescriptor,
                    &mut legacy as *mut _ as *mut c_void,
                ),
            ];

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
                "vaCreateSurfaces (legacy DMA-BUF import)",
            )?;
        }

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
    pub(crate) id: u32,
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

    /// Create a buffer from raw bytes (e.g. packed header data).
    pub(crate) fn create_data_buffer(&self, type_: u32, data: &[u8], op: &str) -> Result<VaBuffer> {
        let mut id = 0 as VABufferID;
        va_check(
            unsafe {
                va::vaCreateBuffer(
                    self.display.raw,
                    self.id,
                    type_,
                    data.len() as u32,
                    1,
                    data.as_ptr() as *mut c_void,
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
        va_sync_surface(self.display.raw, surface, "vaSyncSurface (encoder)")
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

unsafe fn collect_coded_segments(mut segment: *const va::VACodedBufferSegment) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    while !segment.is_null() {
        let segment_ref = unsafe { &*segment };
        if segment_ref.status & va::VA_CODED_BUF_STATUS_SLICE_OVERFLOW_MASK != 0 {
            bail!("coded buffer overflow: encoded slice exceeded the coded buffer");
        }
        if !segment_ref.buf.is_null() && segment_ref.size > 0 {
            let bytes = unsafe {
                std::slice::from_raw_parts(segment_ref.buf as *const u8, segment_ref.size as usize)
            };
            data.extend_from_slice(bytes);
        }
        segment = segment_ref.next as *const va::VACodedBufferSegment;
    }
    Ok(data)
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

        // The driver truncates the bitstream and marks the segment when a
        // slice outgrows the coded buffer. Reject the whole access unit before
        // any of its bytes can be forwarded to the client.
        unsafe { collect_coded_segments(ptr as *const va::VACodedBufferSegment) }
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

pub(crate) fn integer_surface_attr(type_: u32, value: i32) -> VASurfaceAttrib {
    let mut attr: VASurfaceAttrib = unsafe { mem::zeroed() };
    attr.type_ = type_;
    attr.flags = va::VA_SURFACE_ATTRIB_SETTABLE;
    attr.value.type_ = va::VAGenericValueType_VAGenericValueTypeInteger;
    attr.value.value.i = value;
    attr
}

pub(crate) fn pointer_surface_attr(type_: u32, value: *mut c_void) -> VASurfaceAttrib {
    let mut attr: VASurfaceAttrib = unsafe { mem::zeroed() };
    attr.type_ = type_;
    attr.flags = va::VA_SURFACE_ATTRIB_SETTABLE;
    attr.value.type_ = va::VAGenericValueType_VAGenericValueTypePointer;
    attr.value.value.p = value;
    attr
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(bytes: &mut [u8]) -> va::VACodedBufferSegment {
        let mut segment: va::VACodedBufferSegment = unsafe { std::mem::zeroed() };
        segment.buf = bytes.as_mut_ptr().cast();
        segment.size = bytes.len() as u32;
        segment
    }

    #[test]
    fn prime2_import_is_detected_from_the_memory_type_mask() {
        // VA | DRM_PRIME_2, as a modern driver advertises.
        assert!(prime2_import_supported(0x4000_0001));
    }

    #[test]
    fn legacy_prime_mask_selects_the_legacy_path() {
        // Measured i965 2.4.5: VA | KERNEL_DRM | DRM_PRIME, no PRIME_2 bit.
        assert!(!prime2_import_supported(0x3000_0001));
        assert!(!prime2_import_supported(
            VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME
        ));
    }

    #[test]
    fn empty_memory_type_mask_selects_the_legacy_path() {
        assert!(!prime2_import_supported(0));
    }

    fn nv12_prime_desc(fd: i32, modifier: u64) -> VADRMPRIMESurfaceDescriptor {
        let mut desc: VADRMPRIMESurfaceDescriptor = unsafe { mem::zeroed() };
        desc.fourcc = fourcc(b"NV12");
        desc.width = 1920;
        desc.height = 1080;
        desc.num_objects = 1;
        desc.objects[0].fd = fd;
        desc.objects[0].drm_format_modifier = modifier;
        desc.num_layers = 2;
        desc.layers[0].drm_format = fourcc(b"NV12");
        desc.layers[0].num_planes = 1;
        desc.layers[0].pitch[0] = 1920;
        desc.layers[0].offset[0] = 0;
        desc.layers[1].drm_format = fourcc(b"NV12");
        desc.layers[1].num_planes = 1;
        desc.layers[1].pitch[0] = 1920;
        // Tile-padded Y plane size as i965 reports it, not stride * height / 2.
        desc.layers[1].offset[0] = 1920 * 1088;
        desc
    }

    #[test]
    fn legacy_nv12_descriptor_keeps_the_driver_reported_uv_offset() {
        let prime2 = nv12_prime_desc(7, DRM_FORMAT_MOD_LINEAR);
        let mut buffer = 7usize;
        let legacy = legacy_nv12_surface_descriptor(&mut buffer, 1920, 1080, &prime2);

        assert_eq!(legacy.pixel_format, fourcc(b"NV12"));
        assert_eq!(legacy.width, 1920);
        assert_eq!(legacy.height, 1080);
        assert_eq!(legacy.num_planes, 2);
        assert_eq!(legacy.pitches[0], 1920);
        assert_eq!(legacy.pitches[1], 1920);
        assert_eq!(legacy.offsets[0], 0);
        assert_eq!(legacy.offsets[1], 1920 * 1088);
        assert_eq!(legacy.num_buffers, 1);
        assert_eq!(unsafe { *legacy.buffers }, 7);
    }

    #[test]
    fn legacy_nv12_descriptor_data_size_covers_both_planes() {
        let mut prime2 = nv12_prime_desc(3, DRM_FORMAT_MOD_LINEAR);
        let mut buffer = 3usize;

        prime2.layers[1].offset[0] = 0;
        let legacy = legacy_nv12_surface_descriptor(&mut buffer, 1920, 1080, &prime2);
        assert_eq!(legacy.data_size, 1080 * 1920);

        prime2.layers[1].offset[0] = 1920 * 1088;
        let legacy = legacy_nv12_surface_descriptor(&mut buffer, 1920, 1080, &prime2);
        assert_eq!(legacy.data_size, 1920 * 1088 + 540 * 1920);
    }

    #[test]
    fn legacy_nv12_descriptor_tiles_only_for_tiled_modifiers() {
        let linear = nv12_prime_desc(3, DRM_FORMAT_MOD_LINEAR);
        let mut buffer = 3usize;
        assert_eq!(
            legacy_nv12_surface_descriptor(&mut buffer, 1920, 1080, &linear).flags,
            0
        );

        let tiled = nv12_prime_desc(3, 0x0100_0000_0000_0001);
        assert_eq!(
            legacy_nv12_surface_descriptor(&mut buffer, 1920, 1080, &tiled).flags as u32,
            VA_SURFACE_EXTBUF_DESC_ENABLE_TILING
        );
    }

    #[test]
    fn sync_deadline_is_not_expired_before_its_instant() {
        let deadline = Instant::now() + GPU_SURFACE_SYNC_TIMEOUT;

        assert!(!sync_deadline_passed(deadline, Instant::now()));
    }

    #[test]
    fn sync_deadline_is_expired_at_and_after_its_instant() {
        let deadline = Instant::now() + GPU_SURFACE_SYNC_TIMEOUT;

        assert!(sync_deadline_passed(deadline, deadline));
        assert!(sync_deadline_passed(
            deadline,
            deadline + Duration::from_millis(1)
        ));
    }

    #[test]
    fn surface_status_ready_completes_the_wait() {
        assert!(surface_status_complete(va::VASurfaceStatus_VASurfaceReady));
        assert!(surface_status_complete(
            va::VASurfaceStatus_VASurfaceReady | va::VASurfaceStatus_VASurfaceDisplaying
        ));
        assert!(!surface_status_complete(
            va::VASurfaceStatus_VASurfaceRendering
        ));
        assert!(!surface_status_complete(0));
    }

    #[test]
    fn coded_segment_collection_joins_all_segments() {
        let mut first_bytes = [1u8, 2];
        let mut second_bytes = [3u8, 4, 5];
        let mut second = segment(&mut second_bytes);
        let mut first = segment(&mut first_bytes);
        first.next = (&mut second as *mut va::VACodedBufferSegment).cast();

        let data = unsafe { collect_coded_segments(&first) }.unwrap();
        assert_eq!(data, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn coded_segment_collection_rejects_overflow_without_partial_payload() {
        let mut first_bytes = [1u8, 2];
        let mut overflow_bytes = [3u8, 4];
        let mut overflow = segment(&mut overflow_bytes);
        overflow.status = va::VA_CODED_BUF_STATUS_SLICE_OVERFLOW_MASK;
        let mut first = segment(&mut first_bytes);
        first.next = (&mut overflow as *mut va::VACodedBufferSegment).cast();

        let err = unsafe { collect_coded_segments(&first) }.unwrap_err();
        assert!(err.to_string().contains("coded buffer overflow"));
    }
}
