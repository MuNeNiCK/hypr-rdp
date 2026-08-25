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

/// What to hand GBM, given what the compositor advertised.
#[derive(Debug, PartialEq, Eq)]
pub enum Allocation {
    /// Allocate with these -- every one describes a single plane.
    WithModifiers(Vec<u64>),
    /// The compositor advertised no modifier to honour, so let the driver
    /// pick, as this path always has. Advertising only `DRM_FORMAT_MOD_INVALID`
    /// arrives here too: that is how a compositor asks for an implicit
    /// modifier, and both wlroots and mutter put it in the list deliberately.
    ImplicitModifier,
    /// It advertised modifiers and not one of them describes a single plane.
    ///
    /// Allocating without a modifier here is not a fallback: `bo.modifier()`
    /// would then be whatever the driver reports, a value the compositor never
    /// advertised, and a version-4 compositor answers an unadvertised modifier
    /// on `create_immed` with `invalid_format` -- which ends the connection,
    /// where refusing ends only the DMA-BUF path and falls back to SHM.
    /// mutter refuses the same case, with "No single plane modifiers found".
    Refuse,
}

/// Keep only the modifiers that describe a single plane.
///
/// `plane_count` is `gbm_device_get_format_modifier_plane_count` for that
/// modifier, which answers without allocating anything. It returns -1 for a
/// modifier the driver does not support, so the test is `== 1` rather than
/// "not several": an unsupported modifier is not a single-plane one.
///
/// The rest of the capture path describes exactly one plane, and a compressing
/// layout may carry a metadata plane beside the pixels: drm_fourcc.h puts AMD's
/// DCC at index 1 whenever the modifier sets the DCC bit, at 1 and 2 with
/// DCC_RETILE, and Intel's CCS at index 1 on the Y-tiled and tile4 layouts,
/// with a third plane for the clear colour on their `_CC` variants. On DG2 the
/// CCS lives outside the GEM object, so those are single-plane -- except the
/// `_CC` one, which still carries the clear colour at index 1. Which of them a
/// given driver offers is not something to guess at, so ask.
pub fn single_plane_modifiers(modifiers: &[u64], plane_count: impl Fn(u64) -> i32) -> Vec<u64> {
    modifiers
        .iter()
        .copied()
        .filter(|modifier| plane_count(*modifier) == 1)
        .collect()
}

/// Decide what to allocate with, distinguishing "nothing was offered" from
/// "everything offered is multi-plane". They used to be the same branch, and
/// only the first of them is a case this driver can honour blind.
pub fn plan_allocation(modifiers: &[u64], plane_count: impl Fn(u64) -> i32) -> Allocation {
    let single_plane = single_plane_modifiers(modifiers, plane_count);
    if !single_plane.is_empty() {
        Allocation::WithModifiers(single_plane)
    } else if modifiers.is_empty() {
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
    use super::{plan_allocation, single_plane_modifiers, Allocation};

    const LINEAR: u64 = 0;
    /// AMD DCC with pipe alignment: two planes, and the first thing this
    /// machine's iGPU advertises for XRGB8888.
    const AMD_DCC: u64 = 0x0200_0000_0056_bb03;
    /// `I915_FORMAT_MOD_Y_TILED_CCS`, which drm_fourcc.h puts at two planes:
    /// "the CCS will be plane index 1".
    const INTEL_CCS: u64 = 0x0100_0000_0000_0004;
    /// `I915_FORMAT_MOD_Y_TILED_GEN12_RC_CCS_CC`, three planes: "The clear
    /// color is stored at index 2".
    const INTEL_CCS_CC: u64 = 0x0100_0000_0000_0008;

    /// The two empty-list cases are not the same decision, which is the whole
    /// point of the enum: nothing advertised means the modifier is implicit and
    /// the driver may pick, while a list of only multi-plane modifiers means
    /// anything we allocate carries a modifier the compositor never named.
    #[test]
    fn nothing_advertised_lets_the_driver_pick() {
        assert_eq!(
            plan_allocation(&[], |_| unreachable!("no modifier to ask about")),
            Allocation::ImplicitModifier
        );
    }

    #[test]
    fn a_list_of_only_multi_plane_modifiers_is_refused() {
        assert_eq!(
            plan_allocation(&[AMD_DCC, INTEL_CCS_CC], |modifier| match modifier {
                AMD_DCC => 2,
                INTEL_CCS_CC => 3,
                _ => unreachable!(),
            }),
            Allocation::Refuse
        );
    }

    /// A driver that supports none of what was advertised answers -1, not a
    /// plane count. That is still "advertised, and nothing usable".
    #[test]
    fn modifiers_the_driver_rejects_are_refused_not_ignored() {
        assert_eq!(plan_allocation(&[AMD_DCC], |_| -1), Allocation::Refuse);
    }

    #[test]
    fn one_survivor_is_enough_to_allocate_with() {
        assert_eq!(
            plan_allocation(&[AMD_DCC, LINEAR], |modifier| match modifier {
                AMD_DCC => 2,
                LINEAR => 1,
                _ => unreachable!(),
            }),
            Allocation::WithModifiers(vec![LINEAR])
        );
    }

    #[test]
    fn only_single_plane_modifiers_survive() {
        let offered = [LINEAR, AMD_DCC, INTEL_CCS, INTEL_CCS_CC];
        let kept = single_plane_modifiers(&offered, |modifier| match modifier {
            AMD_DCC | INTEL_CCS => 2,
            INTEL_CCS_CC => 3,
            _ => 1,
        });

        assert_eq!(kept, vec![LINEAR]);
    }

    #[test]
    fn an_unsupported_modifier_is_not_a_single_plane_one() {
        // The query answers -1 for a modifier the driver does not know, and a
        // "not several" test would have kept it.
        let kept =
            single_plane_modifiers(
                &[LINEAR, AMD_DCC],
                |modifier| {
                    if modifier == AMD_DCC {
                        -1
                    } else {
                        1
                    }
                },
            );

        assert_eq!(kept, vec![LINEAR]);
    }

    #[test]
    fn nothing_usable_leaves_an_empty_set_rather_than_a_guess() {
        // Two and three both: a "not two" test would keep the clear-colour
        // variant, and one plane is all the capture path can describe.
        let kept = single_plane_modifiers(&[AMD_DCC, INTEL_CCS_CC], |modifier| {
            if modifier == INTEL_CCS_CC {
                3
            } else {
                2
            }
        });

        assert!(kept.is_empty());
    }
}
