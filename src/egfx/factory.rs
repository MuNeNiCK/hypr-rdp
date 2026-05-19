use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use ironrdp_egfx::server::{GraphicsPipelineHandler, GraphicsPipelineServer};
use ironrdp_server::{
    GfxDvcBridge, GfxServerFactory, GfxServerHandle, ServerEvent, ServerEventSender,
};
use tokio::sync::mpsc;

use super::shared::avc444_disabled_by_env;
use super::EgfxShared;

pub(super) fn capability_avc_support(
    cap: &ironrdp_egfx::pdu::CapabilitySet,
    disable_avc444: bool,
) -> (bool, bool) {
    use ironrdp_egfx::pdu::*;
    let (avc420, avc444) = match cap {
        CapabilitySet::V8 { .. } => (false, false),
        CapabilitySet::V8_1 { flags } => {
            (flags.contains(CapabilitiesV81Flags::AVC420_ENABLED), false)
        }
        CapabilitySet::V10 { flags } | CapabilitySet::V10_2 { flags } => {
            let enabled = !flags.contains(CapabilitiesV10Flags::AVC_DISABLED);
            (enabled, enabled)
        }
        CapabilitySet::V10_1 => (true, true),
        CapabilitySet::V10_3 { flags } => {
            let enabled = !flags.contains(CapabilitiesV103Flags::AVC_DISABLED);
            (enabled, enabled)
        }
        CapabilitySet::V10_4 { flags }
        | CapabilitySet::V10_5 { flags }
        | CapabilitySet::V10_6 { flags }
        | CapabilitySet::V10_6Err { flags } => {
            let enabled = !flags.contains(CapabilitiesV104Flags::AVC_DISABLED);
            (enabled, enabled)
        }
        CapabilitySet::V10_7 { flags } => {
            let enabled = !flags.contains(CapabilitiesV107Flags::AVC_DISABLED);
            (enabled, enabled)
        }
        CapabilitySet::Unknown(_) => (false, false),
    };
    (avc420, avc444 && !disable_avc444)
}

/// Factory for creating EGFX pipeline handlers.
pub struct HyprGfxFactory {
    shared: Arc<EgfxShared>,
    event_sender: Option<mpsc::UnboundedSender<ServerEvent>>,
}

impl HyprGfxFactory {
    pub fn new(shared: Arc<EgfxShared>) -> Self {
        Self {
            shared,
            event_sender: None,
        }
    }
}

impl ServerEventSender for HyprGfxFactory {
    fn set_sender(&mut self, sender: mpsc::UnboundedSender<ServerEvent>) {
        self.event_sender = Some(sender.clone());
        if let Ok(mut guard) = self.shared.event_sender.lock() {
            *guard = Some(sender);
        }
    }
}

impl GfxServerFactory for HyprGfxFactory {
    fn build_gfx_handler(&self) -> Box<dyn GraphicsPipelineHandler> {
        Box::new(HyprGraphicsHandler {
            shared: Arc::clone(&self.shared),
        })
    }

    fn build_server_with_handle(&self) -> Option<(GfxDvcBridge, GfxServerHandle)> {
        let handler = Box::new(HyprGraphicsHandler {
            shared: Arc::clone(&self.shared),
        });
        let mut server = GraphicsPipelineServer::new(handler);

        // Pre-set output dimensions so that auto-create surface works
        // in handle_capabilities_advertise (same batch as CapabilitiesConfirm).
        let (w, h) = self.shared.get_surface_size();
        if w > 0 && h > 0 {
            server.set_output_dimensions(w, h);
        }

        let handle: GfxServerHandle = Arc::new(Mutex::new(server));
        let bridge = GfxDvcBridge::new(Arc::clone(&handle));

        if let Ok(mut guard) = self.shared.handle.lock() {
            *guard = Some(Arc::clone(&handle));
        }

        Some((bridge, handle))
    }
}

/// EGFX pipeline handler — receives callbacks from the EGFX protocol.
///
/// Only sets atomic flags; never acquires mutexes (to avoid deadlocks
/// since callbacks fire while the GfxServerHandle mutex is held).
struct HyprGraphicsHandler {
    shared: Arc<EgfxShared>,
}

impl GraphicsPipelineHandler for HyprGraphicsHandler {
    fn preferred_capabilities(&self) -> Vec<ironrdp_egfx::pdu::CapabilitySet> {
        use ironrdp_egfx::pdu::*;
        // V8_1 without SMALL_CACHE — iOS sends SMALL_CACHE but
        // intersect (AND) will clear it if server doesn't set it.
        vec![
            CapabilitySet::V10_7 {
                flags: CapabilitiesV107Flags::empty(),
            },
            CapabilitySet::V8_1 {
                flags: CapabilitiesV81Flags::empty(),
            },
            CapabilitySet::V10 {
                flags: CapabilitiesV10Flags::empty(),
            },
            CapabilitySet::V8 {
                flags: CapabilitiesV8Flags::empty(),
            },
        ]
    }

    fn max_frames_in_flight(&self) -> u32 {
        self.shared.max_frames_in_flight()
    }

    fn on_frame_ack(&mut self, _frame_id: u32, _queue_depth: u32) {}

    fn capabilities_advertise(&mut self, pdu: &ironrdp_egfx::pdu::CapabilitiesAdvertisePdu) {
        tracing::trace!(count = pdu.0.len(), "EGFX: client advertised capabilities");
        for cap in &pdu.0 {
            tracing::trace!(?cap, "EGFX: client capability");
        }
    }

    fn on_ready(&mut self, cap: &ironrdp_egfx::pdu::CapabilitySet) {
        let (avc420, avc444) = capability_avc_support(cap, avc444_disabled_by_env());
        let avc = avc420 || avc444;
        self.shared.avc_enabled.store(avc, Ordering::Release);
        self.shared.avc444_enabled.store(avc444, Ordering::Release);

        let was_ready = self.shared.ready.load(Ordering::Acquire);
        if was_ready {
            tracing::info!(
                ?cap,
                avc420,
                avc444,
                "EGFX: client re-negotiated (keeping surface)"
            );
        } else {
            tracing::info!(?cap, avc420, avc444, "EGFX: channel ready (first time)");
            self.shared.ready_generation.fetch_add(1, Ordering::Release);
        }
        self.shared.ready.store(true, Ordering::Release);
    }

    fn on_close(&mut self) {
        tracing::trace!("EGFX: channel closed");
        self.shared.ready.store(false, Ordering::Release);
    }
}
