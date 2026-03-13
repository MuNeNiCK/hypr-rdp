//! GBM FFI and DMA-BUF buffer management.
//!
//! Raw FFI to libgbm.so with RAII wrappers for zero-copy screen capture.

use std::os::unix::io::RawFd;
use std::path::Path;

use anyhow::{bail, Result};

// DRM format constants (drm_fourcc.h)
pub const DRM_FORMAT_XRGB8888: u32 = 0x34325258;
pub const DRM_FORMAT_ARGB8888: u32 = 0x34324152;
pub const DRM_FORMAT_NV12: u32 = 0x3231564E;
#[allow(dead_code)]
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;
pub const DRM_FORMAT_MOD_INVALID: u64 = 0x00ffffffffffffff;

const GBM_BO_USE_RENDERING: u32 = 1 << 2;

// Raw FFI to libgbm.so
#[allow(non_camel_case_types)]
type gbm_device = std::ffi::c_void;
#[allow(non_camel_case_types)]
type gbm_bo = std::ffi::c_void;

#[link(name = "gbm")]
extern "C" {
    fn gbm_create_device(fd: libc::c_int) -> *mut gbm_device;
    fn gbm_device_destroy(device: *mut gbm_device);
    fn gbm_bo_create_with_modifiers(
        device: *mut gbm_device,
        width: u32,
        height: u32,
        format: u32,
        modifiers: *const u64,
        count: libc::c_uint,
    ) -> *mut gbm_bo;
    fn gbm_bo_create(
        device: *mut gbm_device,
        width: u32,
        height: u32,
        format: u32,
        flags: u32,
    ) -> *mut gbm_bo;
    fn gbm_bo_get_fd(bo: *mut gbm_bo) -> libc::c_int;
    fn gbm_bo_get_stride(bo: *mut gbm_bo) -> u32;
    fn gbm_bo_get_offset(bo: *mut gbm_bo, plane: libc::c_int) -> u32;
    fn gbm_bo_get_modifier(bo: *mut gbm_bo) -> u64;
    #[allow(dead_code)]
    fn gbm_bo_get_plane_count(bo: *mut gbm_bo) -> libc::c_int;
    #[allow(dead_code)]
    fn gbm_bo_get_fd_for_plane(bo: *mut gbm_bo, plane: libc::c_int) -> libc::c_int;
    #[allow(dead_code)]
    fn gbm_bo_get_stride_for_plane(bo: *mut gbm_bo, plane: libc::c_int) -> u32;
    fn gbm_bo_destroy(bo: *mut gbm_bo);
}

/// Information about a DMA-BUF.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct DmaBufInfo {
    pub fd: RawFd,
    pub stride: u32,
    pub offset: u32,
    pub modifier: u64,
    pub format: u32,
    pub width: u32,
    pub height: u32,
}

/// RAII wrapper around `gbm_device*`.
pub struct GbmDevice {
    ptr: *mut gbm_device,
    _drm_fd: RawFd, // keep alive
}

impl GbmDevice {
    /// Create a GBM device from an open DRM device fd.
    /// The fd must remain open for the lifetime of the device.
    pub fn new(drm_fd: RawFd) -> Result<Self> {
        let ptr = unsafe { gbm_create_device(drm_fd) };
        if ptr.is_null() {
            bail!("gbm_create_device failed");
        }
        Ok(Self {
            ptr,
            _drm_fd: drm_fd,
        })
    }

    pub fn as_ptr(&self) -> *mut gbm_device {
        self.ptr
    }
}

impl Drop for GbmDevice {
    fn drop(&mut self) {
        unsafe {
            gbm_device_destroy(self.ptr);
        }
    }
}

/// RAII wrapper around `gbm_bo*`.
pub struct GbmBo {
    ptr: *mut gbm_bo,
}

impl GbmBo {
    /// Allocate a buffer object with modifiers (preferred).
    pub fn create_with_modifiers(
        device: &GbmDevice,
        width: u32,
        height: u32,
        format: u32,
        modifiers: &[u64],
    ) -> Result<Self> {
        let ptr = unsafe {
            gbm_bo_create_with_modifiers(
                device.as_ptr(),
                width,
                height,
                format,
                modifiers.as_ptr(),
                modifiers.len() as libc::c_uint,
            )
        };
        if ptr.is_null() {
            bail!(
                "gbm_bo_create_with_modifiers failed ({}x{}, format 0x{:08x})",
                width,
                height,
                format
            );
        }
        Ok(Self { ptr })
    }

    /// Allocate a buffer object without modifiers (fallback).
    pub fn create(device: &GbmDevice, width: u32, height: u32, format: u32) -> Result<Self> {
        let ptr =
            unsafe { gbm_bo_create(device.as_ptr(), width, height, format, GBM_BO_USE_RENDERING) };
        if ptr.is_null() {
            bail!(
                "gbm_bo_create failed ({}x{}, format 0x{:08x})",
                width,
                height,
                format
            );
        }
        Ok(Self { ptr })
    }

    pub fn fd(&self) -> RawFd {
        unsafe { gbm_bo_get_fd(self.ptr) }
    }

    pub fn stride(&self) -> u32 {
        unsafe { gbm_bo_get_stride(self.ptr) }
    }

    pub fn offset(&self, plane: i32) -> u32 {
        unsafe { gbm_bo_get_offset(self.ptr, plane) }
    }

    pub fn modifier(&self) -> u64 {
        unsafe { gbm_bo_get_modifier(self.ptr) }
    }

    #[allow(dead_code)]
    pub fn plane_count(&self) -> i32 {
        unsafe { gbm_bo_get_plane_count(self.ptr) }
    }

    #[allow(dead_code)]
    pub fn fd_for_plane(&self, plane: i32) -> RawFd {
        unsafe { gbm_bo_get_fd_for_plane(self.ptr, plane) }
    }

    #[allow(dead_code)]
    pub fn stride_for_plane(&self, plane: i32) -> u32 {
        unsafe { gbm_bo_get_stride_for_plane(self.ptr, plane) }
    }

    /// Get DMA-BUF info for this buffer object.
    pub fn dmabuf_info(&self, format: u32, width: u32, height: u32) -> DmaBufInfo {
        DmaBufInfo {
            fd: self.fd(),
            stride: self.stride(),
            offset: self.offset(0),
            modifier: self.modifier(),
            format,
            width,
            height,
        }
    }
}

impl Drop for GbmBo {
    fn drop(&mut self) {
        unsafe {
            gbm_bo_destroy(self.ptr);
        }
    }
}

/// Find the DRM device path matching a dev_t value.
pub fn drm_device_from_devt(dev: libc::dev_t) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::MetadataExt;
    for entry in std::fs::read_dir("/dev/dri").ok()? {
        let entry = entry.ok()?;
        let path = entry.path();
        let metadata = std::fs::metadata(&path).ok()?;
        if metadata.rdev() == dev {
            return Some(path);
        }
    }
    None
}

/// Open a DRM device fd from a path.
pub fn open_drm_device(path: &Path) -> Result<RawFd> {
    use std::os::unix::io::IntoRawFd;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;
    Ok(file.into_raw_fd())
}
