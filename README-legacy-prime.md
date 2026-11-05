# Legacy-PRIME Import Fallback for hypr-rdp

## Problem

hypr-rdp's zero-copy capture path imports a compositor DMA-BUF into a VA-API
surface before VPP converts it to NV12 and the H.264 encoder consumes it.
That import is done with the `DRM_PRIME_2` memory type
(`VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2 = 0x4000_0000`).

On Intel Haswell Mobile (Gen7.5, PCI 00:02.0, device 0x0a16) the only
installed VA-API driver is `i965` 2.4.5, and that driver has **no**
`PRIME_2` import path at all: its `vaCreateSurfaces2` memory-type switch
only maps `VA`, `KERNEL_DRM` and `DRM_PRIME`. A `PRIME_2` descriptor is
silently mishandled — for XRGB8888 input the call returns
`VA_STATUS_ERROR_INVALID_PARAMETER` (status 18), and for NV12 input the
driver allocates its own surface and ignores the imported buffer, so the
zero-copy path produces garbage frames instead of an error.

The other candidate drivers were ruled out on this hardware:

- `iHD` 26.3.5 refuses to initialise on Gen7.5.
- Mesa `iris` ships no VA-API video driver.
- `intel-hybrid-codec-driver` does not implement `PRIME_2` import.

So the only way to keep zero-copy capture on this GPU is to describe the
imported buffer with the legacy `DRM_PRIME` memory type
(`VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME = 0x2000_0000`), which i965 does
support for XRGB input (linear and `X_TILED` layouts), with VPP then doing
`RGB32 -> NV12` and exporting the result as `PRIME_2` for the encoder.

## Detection

The driver's capability is queried once per VA config id through
`vaQuerySurfaceAttributes`, which returns a `VASurfaceAttribMemoryType`
bitmask. `VaDisplay::supports_prime2_import(config_id)` caches the result
in a `RefCell<HashMap<VAConfigID, bool>>` so the per-frame capture path
never re-queries.

A mask that has the `DRM_PRIME_2` bit set selects the existing
`VADRMPRIMESurfaceDescriptor` import path. A mask without it selects the
legacy `VASurfaceAttribExternalBuffers` descriptor with `MEM_TYPE_DRM_PRIME`.

On i965 the measured mask is `0x3000_0001` = `VA | KERNEL_DRM | DRM_PRIME`,
with no `PRIME_2` bit, so the legacy path is taken automatically. Drivers
that advertise no memory-type attribute yield an empty mask and likewise
fall back to the legacy path.

Two i965 quirks are handled in the query:

- `*num_attribs` is treated as the **capacity** of the output buffer, not
  as a count to fill. Passing zero returns `MAX_NUM_EXCEEDED` and leaves
  the list untouched, so a real capacity (64) is always passed.
- `VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME` is not bound by libva-sys (only
  the low-valued members `VA`, `V4L2`, `USER_PTR` are), so it is defined
  locally like the existing `DRM_PRIME_2` constant.

## Tiling

The tiling flag in the legacy descriptor must follow the actual buffer
modifier. Passing `linear + ENABLE_TILING` fails on i965, so the flag is
set from the real modifier reported by the compositor.

## Architecture (upstream-compatible)

- Detection is by capability (the memory-type mask), not by driver name, so
  the same binary works on modern drivers (keeps `PRIME_2`) and on i965
  (uses legacy `PRIME`).
- Legacy `PRIME` is used for **both** imports: the XRGB capture input into
  VPP, and the NV12 frame into the encoder. `PRIME_2` remains only as the
  memory type for exporting the VPP output as a DMA-BUF, which i965 does
  implement.
- `VaConfig.id` is exposed as `pub(crate)` so the VPP layer and the encoder
  can query capability against their own config.
- The query answer is cached per display, keyed by config id, so the
  capture and encode paths never re-query.

## Why both imports, not just one

Gating only the VPP input leaves a green screen. The encoder imports its
NV12 frame with the same helper, and on i965 a `PRIME_2` import is not an
error at all: `vaCreateSurfaces` returns success, the driver allocates its
own surface and ignores the buffer. The encoder then compresses an empty
NV12 surface, which decodes to a uniform green wall, while the log shows no
failure and frames keep queuing. So "the picture is green although nothing
reports an error" means the *last* import in the chain is the one binding
nothing — each import has to be capability-gated separately, because the
two use different configs (VideoProc for VPP, H.264 encode for the encoder).

## Files touched

- `src/egfx/vaapi_sys.rs` — `supports_prime2_import` and
  `surface_memory_types`, cached per config id; `prime2_import_supported`
  helper + tests; legacy `DRM_PRIME` descriptor for both the packed XRGB
  input and the two-plane NV12 encoder input.
- `src/egfx/vaapi.rs` — `VaConfig.id` visibility; the encoder input import
  selects its memory type from its own config.
- `src/egfx/vpp.rs` — import-input-surface selection between `PRIME_2`
  and legacy `PRIME`.
