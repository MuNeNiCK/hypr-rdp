//! GBM FFI and DMA-BUF buffer management.
//!
//! Raw FFI to libgbm.so with RAII wrappers for zero-copy screen capture.

use std::os::unix::io::{FromRawFd, OwnedFd, RawFd};
use std::path::Path;

use anyhow::{bail, Result};

// DRM format constants (drm_fourcc.h)
pub const DRM_FORMAT_XRGB8888: u32 = 0x34325258;
pub const DRM_FORMAT_ARGB8888: u32 = 0x34324152;
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
    fn gbm_bo_get_plane_count(bo: *mut gbm_bo) -> libc::c_int;
    fn gbm_device_get_format_modifier_plane_count(
        gbm: *mut gbm_device,
        format: u32,
        modifier: u64,
    ) -> libc::c_int;
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
    pub uv_stride: u32,
    pub uv_offset: u32,
}

/// RAII wrapper around `gbm_device*`.
pub struct GbmDevice {
    ptr: *mut gbm_device,
    _drm_fd: OwnedFd,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Allocation {
    WithModifiers {
        modifiers: Vec<u64>,
        allow_implicit_fallback: bool,
    },
    ImplicitModifier,
    Refuse,
}

fn single_plane_modifiers(modifiers: &[u64], plane_count: impl Fn(u64) -> i32) -> Vec<u64> {
    modifiers
        .iter()
        .copied()
        .filter(|modifier| *modifier != DRM_FORMAT_MOD_INVALID && plane_count(*modifier) == 1)
        .collect()
}

pub fn plan_allocation(modifiers: &[u64], plane_count: impl Fn(u64) -> i32) -> Allocation {
    let allow_implicit_fallback =
        modifiers.is_empty() || modifiers.contains(&DRM_FORMAT_MOD_INVALID);
    let single_plane = single_plane_modifiers(modifiers, plane_count);
    if !single_plane.is_empty() {
        Allocation::WithModifiers {
            modifiers: single_plane,
            allow_implicit_fallback,
        }
    } else if allow_implicit_fallback {
        Allocation::ImplicitModifier
    } else {
        Allocation::Refuse
    }
}

impl GbmDevice {
    /// How many planes this format and modifier need, without allocating.
    pub fn format_modifier_plane_count(&self, format: u32, modifier: u64) -> i32 {
        unsafe { gbm_device_get_format_modifier_plane_count(self.ptr, format, modifier) }
    }

    pub fn new(drm_fd: OwnedFd) -> Result<Self> {
        use std::os::unix::io::AsRawFd;
        let ptr = unsafe { gbm_create_device(drm_fd.as_raw_fd()) };
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
    cached_fd: Option<OwnedFd>,
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
        Ok(Self {
            ptr,
            cached_fd: None,
        })
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
        Ok(Self {
            ptr,
            cached_fd: None,
        })
    }

    pub fn fd(&mut self) -> Result<RawFd> {
        use std::os::unix::io::AsRawFd;
        if let Some(ref fd) = self.cached_fd {
            return Ok(fd.as_raw_fd());
        }
        let raw = unsafe { gbm_bo_get_fd(self.ptr) };
        if raw < 0 {
            bail!("gbm_bo_get_fd failed");
        }
        let owned = unsafe { OwnedFd::from_raw_fd(raw) };
        let raw = owned.as_raw_fd();
        self.cached_fd = Some(owned);
        Ok(raw)
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
    pub fn dmabuf_info(&mut self, format: u32, width: u32, height: u32) -> Result<DmaBufInfo> {
        Ok(DmaBufInfo {
            fd: self.fd()?,
            stride: self.stride(),
            offset: self.offset(0),
            modifier: self.modifier(),
            format,
            width,
            height,
            uv_stride: 0,
            uv_offset: 0,
        })
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
pub fn open_drm_device(path: &Path) -> Result<OwnedFd> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;
    Ok(OwnedFd::from(file))
}

#[cfg(test)]
mod tests {
    use super::{plan_allocation, Allocation, DRM_FORMAT_MOD_INVALID};

    const LINEAR: u64 = 0;
    const AMD_DCC: u64 = 0x0200_0000_0056_bb03;

    #[test]
    fn allocation_plan_preserves_implicit_modifier_support() {
        assert_eq!(
            plan_allocation(&[], |_| unreachable!("no modifier to ask about")),
            Allocation::ImplicitModifier
        );
        assert_eq!(
            plan_allocation(&[DRM_FORMAT_MOD_INVALID], |_| {
                panic!("implicit modifier must not be queried")
            }),
            Allocation::ImplicitModifier
        );
        assert_eq!(
            plan_allocation(&[LINEAR], |_| 1),
            Allocation::WithModifiers {
                modifiers: vec![LINEAR],
                allow_implicit_fallback: false,
            }
        );
        assert_eq!(
            plan_allocation(&[LINEAR, DRM_FORMAT_MOD_INVALID], |_| 1),
            Allocation::WithModifiers {
                modifiers: vec![LINEAR],
                allow_implicit_fallback: true,
            }
        );
    }

    #[test]
    fn allocation_plan_refuses_unusable_explicit_modifiers() {
        assert_eq!(plan_allocation(&[AMD_DCC], |_| 2), Allocation::Refuse);
        assert_eq!(plan_allocation(&[AMD_DCC], |_| -1), Allocation::Refuse);
        assert_eq!(
            plan_allocation(&[AMD_DCC, DRM_FORMAT_MOD_INVALID], |_| 2),
            Allocation::ImplicitModifier
        );
    }
}
