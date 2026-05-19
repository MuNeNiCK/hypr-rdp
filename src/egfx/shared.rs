use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;

use ironrdp_server::{GfxServerHandle, ServerEvent};
use tokio::sync::mpsc;

pub const DEFAULT_MAX_FRAMES_IN_FLIGHT: u32 = 120;

pub(in crate::egfx) fn avc444_disabled_by_env() -> bool {
    std::env::var_os("HYPR_RDP_DISABLE_AVC444").is_some()
}

/// Shared EGFX state accessible from factory, handler, and capture thread.
pub struct EgfxShared {
    /// The GFX server handle (set once during build_server_with_handle)
    pub(in crate::egfx) handle: Mutex<Option<GfxServerHandle>>,
    /// Whether EGFX capability negotiation is complete
    pub(in crate::egfx) ready: AtomicBool,
    /// Whether the negotiated capability supports AVC420/AVC444 (H.264)
    pub(in crate::egfx) avc_enabled: AtomicBool,
    /// Whether the negotiated capability supports AVC444.
    pub(in crate::egfx) avc444_enabled: AtomicBool,
    /// Incremented each time on_ready fires; lets capture thread detect re-negotiation
    pub(in crate::egfx) ready_generation: AtomicU32,
    /// Event sender for routing encoded frames to the RDP wire
    pub(in crate::egfx) event_sender: Mutex<Option<mpsc::UnboundedSender<ServerEvent>>>,
    /// Surface size for auto-create in EGFX negotiation
    surface_size: Mutex<(u16, u16)>,
    /// Flow-control window for latency/throughput tuning.
    max_frames_in_flight: AtomicU32,
}

impl EgfxShared {
    pub fn new(max_frames_in_flight: u32) -> Self {
        Self {
            handle: Mutex::new(None),
            ready: AtomicBool::new(false),
            avc_enabled: AtomicBool::new(false),
            avc444_enabled: AtomicBool::new(false),
            ready_generation: AtomicU32::new(0),
            event_sender: Mutex::new(None),
            surface_size: Mutex::new((0, 0)),
            max_frames_in_flight: AtomicU32::new(max_frames_in_flight.max(1)),
        }
    }

    pub fn set_surface_size(&self, width: u16, height: u16) {
        if let Ok(mut guard) = self.surface_size.lock() {
            *guard = (width, height);
        }
    }

    pub fn get_surface_size(&self) -> (u16, u16) {
        self.surface_size.lock().map(|g| *g).unwrap_or((0, 0))
    }

    pub(in crate::egfx) fn max_frames_in_flight(&self) -> u32 {
        self.max_frames_in_flight.load(Ordering::Acquire).max(1)
    }

    pub fn is_avc_enabled(&self) -> bool {
        self.avc_enabled.load(Ordering::Acquire)
    }

    pub fn is_avc444_enabled(&self) -> bool {
        self.avc444_enabled.load(Ordering::Acquire) && !avc444_disabled_by_env()
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    pub fn generation(&self) -> u32 {
        self.ready_generation.load(Ordering::Acquire)
    }

    pub fn get_handle(&self) -> Option<GfxServerHandle> {
        self.handle.lock().ok()?.clone()
    }

    pub fn get_event_sender(&self) -> Option<mpsc::UnboundedSender<ServerEvent>> {
        self.event_sender.lock().ok()?.clone()
    }

    /// Reset readiness state for a new client connection.
    /// Called from updates() so each connection starts with a clean EGFX slate.
    /// The handle and event_sender are preserved (set per-connection by the factory).
    pub fn reset_for_new_client(&self) {
        self.ready.store(false, Ordering::Release);
        self.avc_enabled.store(false, Ordering::Release);
        self.avc444_enabled.store(false, Ordering::Release);
    }
}
