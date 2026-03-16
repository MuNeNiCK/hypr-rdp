pub mod encoder;
pub mod rfx;
#[cfg(feature = "vaapi")]
pub mod vaapi;
#[cfg(feature = "vaapi")]
pub mod vpp;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;

/// Encoder backend: VAAPI hardware or OpenH264 software.
pub enum FrameEncoder {
    #[cfg(feature = "vaapi")]
    Vaapi(Box<vaapi::VaapiEncoder>),
    Software(Box<encoder::H264Encoder>),
}

impl FrameEncoder {
    /// Try VAAPI first, fall back to software.
    pub fn new(width: u32, height: u32, bitrate: u32, fps: u32) -> Result<Self> {
        #[cfg(feature = "vaapi")]
        {
            match vaapi::VaapiEncoder::new(width, height, bitrate, fps) {
                Ok(enc) => {
                    tracing::info!("Using VA-API hardware encoder");
                    return Ok(Self::Vaapi(Box::new(enc)));
                }
                Err(e) => {
                    tracing::warn!("VA-API init failed, falling back to software: {:#}", e);
                }
            }
        }

        let enc = encoder::H264Encoder::new(width, height, bitrate, fps)?;
        tracing::info!("Using OpenH264 software encoder");
        Ok(Self::Software(Box::new(enc)))
    }

    pub fn encode(&mut self, bgra: &[u8], stride: usize) -> Result<Vec<u8>> {
        match self {
            #[cfg(feature = "vaapi")]
            Self::Vaapi(enc) => enc.encode(bgra, stride),
            Self::Software(enc) => enc.encode(bgra, stride),
        }
    }

    /// Encode from an NV12 DMA-BUF (zero-copy path). Only available with VA-API backend.
    #[cfg(feature = "vaapi")]
    #[allow(clippy::too_many_arguments)]
    pub fn encode_dmabuf(
        &mut self,
        nv12_fd: std::os::unix::io::RawFd,
        width: u32,
        height: u32,
        stride: u32,
        offset: u32,
        modifier: u64,
        uv_stride: u32,
        uv_offset: u32,
    ) -> Result<Vec<u8>> {
        match self {
            Self::Vaapi(enc) => enc.encode_dmabuf(nv12_fd, width, height, stride, offset, modifier, uv_stride, uv_offset),
            Self::Software(_) => anyhow::bail!("DMA-BUF encode requires VA-API backend"),
        }
    }

    pub fn backend_name(&self) -> &'static str {
        match self {
            #[cfg(feature = "vaapi")]
            Self::Vaapi(_) => "vaapi",
            Self::Software(_) => "openh264",
        }
    }

    pub fn is_vaapi(&self) -> bool {
        match self {
            #[cfg(feature = "vaapi")]
            Self::Vaapi(_) => true,
            Self::Software(_) => false,
        }
    }

    /// Create a software-only encoder (fallback when VA-API fails at runtime).
    pub fn new_software_only(width: u32, height: u32, bitrate: u32, fps: u32) -> Result<Self> {
        let enc = encoder::H264Encoder::new(width, height, bitrate, fps)?;
        tracing::info!("Using OpenH264 software encoder (runtime fallback)");
        Ok(Self::Software(Box::new(enc)))
    }

    /// Force the next encoded frame to be an IDR (recovery after dropped frames).
    pub fn force_idr(&mut self) {
        match self {
            #[cfg(feature = "vaapi")]
            Self::Vaapi(enc) => enc.force_idr(),
            Self::Software(_) => {} // OpenH264 manages IDR internally
        }
    }
}

/// Extract SPS (NAL type 7) and PPS (NAL type 8) from Annex B bitstream.
/// Shared between VAAPI and software encoders.
pub fn extract_sps_pps(data: &[u8]) -> Option<Vec<u8>> {
    let mut sps_pps = Vec::new();
    let mut i = 0;

    while i < data.len() {
        let start_code_len = if i + 4 <= data.len() && data[i..i + 4] == [0x00, 0x00, 0x00, 0x01] {
            4
        } else if i + 3 <= data.len() && data[i..i + 3] == [0x00, 0x00, 0x01] {
            3
        } else {
            i += 1;
            continue;
        };

        let nal_start = i + start_code_len;
        if nal_start >= data.len() {
            break;
        }

        let nal_type = data[nal_start] & 0x1F;

        // Find next start code
        let mut nal_end = data.len();
        let mut j = nal_start + 1;
        while j + 2 < data.len() {
            if data[j..j + 3] == [0x00, 0x00, 0x01]
                || (j + 3 < data.len() && data[j..j + 4] == [0x00, 0x00, 0x00, 0x01])
            {
                nal_end = j;
                if j > 0 && data[j - 1] == 0x00 {
                    nal_end = j - 1;
                }
                break;
            }
            j += 1;
        }

        if nal_type == 7 || nal_type == 8 {
            sps_pps.extend_from_slice(&data[i..nal_end]);
        }

        i = nal_end;
    }

    if sps_pps.is_empty() {
        None
    } else {
        Some(sps_pps)
    }
}

use ironrdp_egfx::pdu::Avc420Region;
use ironrdp_egfx::server::{GraphicsPipelineHandler, GraphicsPipelineServer};
use ironrdp_server::{
    EgfxServerMessage, GfxDvcBridge, GfxServerFactory, GfxServerHandle, ServerEvent,
    ServerEventSender,
};
use tokio::sync::mpsc;

/// Shared EGFX state accessible from factory, handler, and capture thread.
pub struct EgfxShared {
    /// The GFX server handle (set once during build_server_with_handle)
    handle: Mutex<Option<GfxServerHandle>>,
    /// Whether EGFX capability negotiation is complete
    ready: AtomicBool,
    /// Whether the negotiated capability supports AVC420/AVC444 (H.264)
    avc_enabled: AtomicBool,
    /// Incremented each time on_ready fires; lets capture thread detect re-negotiation
    ready_generation: AtomicU32,
    /// Event sender for routing encoded frames to the RDP wire
    event_sender: Mutex<Option<mpsc::UnboundedSender<ServerEvent>>>,
    /// Surface size for auto-create in EGFX negotiation
    surface_size: Mutex<(u16, u16)>,
    /// Surface ID created by auto-create (set by on_ready callback)
    auto_surface_id: Mutex<Option<u16>>,
}

impl EgfxShared {
    pub fn new() -> Self {
        Self {
            handle: Mutex::new(None),
            ready: AtomicBool::new(false),
            avc_enabled: AtomicBool::new(false),
            ready_generation: AtomicU32::new(0),
            event_sender: Mutex::new(None),
            surface_size: Mutex::new((0, 0)),
            auto_surface_id: Mutex::new(None),
        }
    }

    pub fn take_auto_surface_id(&self) -> Option<u16> {
        self.auto_surface_id.lock().ok()?.take()
    }

    pub fn set_auto_surface_id(&self, id: u16) {
        if let Ok(mut guard) = self.auto_surface_id.lock() {
            *guard = Some(id);
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

    pub fn is_avc_enabled(&self) -> bool {
        self.avc_enabled.load(Ordering::Acquire)
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
    }

    /// Prepare EGFX state for a resize (Deactivation-Reactivation).
    /// Deletes all old surfaces, sends ResetGraphics at the new dimensions,
    /// and bumps generation so the capture thread re-creates encoder/surface.
    pub fn prepare_for_resize(&self, width: u16, height: u16) {
        self.ready_generation.fetch_add(1, Ordering::Release);

        let handle = match self.get_handle() {
            Some(h) => h,
            None => return,
        };
        let sender = match self.get_event_sender() {
            Some(s) => s,
            None => return,
        };

        let dvc_messages;
        let channel_id;
        {
            let Ok(mut server) = handle.lock() else { return };
            if !server.is_ready() {
                return;
            }
            channel_id = match server.channel_id() {
                Some(id) => id,
                None => return,
            };
            server.resize(width, height);
            dvc_messages = server.drain_output();
        }

        if !dvc_messages.is_empty() {
            match ironrdp_dvc::encode_dvc_messages(
                channel_id,
                dvc_messages,
                ironrdp_svc::ChannelFlags::SHOW_PROTOCOL,
            ) {
                Ok(svc_messages) => {
                    let _ = sender.send(ServerEvent::Egfx(EgfxServerMessage::SendMessages {
                        messages: svc_messages,
                    }));
                }
                Err(e) => {
                    tracing::error!("Failed to encode resize PDUs: {}", e);
                }
            }
        }
    }

    /// Initialize the EGFX surface (ResetGraphics + CreateSurface + MapSurfaceToOutput).
    /// Called once when EGFX becomes ready, BEFORE any frames are sent.
    /// Returns the surface_id on success.
    pub fn init_surface(
        handle: &GfxServerHandle,
        sender: &mpsc::UnboundedSender<ServerEvent>,
        width: u16,
        height: u16,
    ) -> Option<u16> {
        let Ok(mut server) = handle.lock() else { return None };

        if !server.is_ready() {
            return None;
        }

        let channel_id = match server.channel_id() {
            Some(id) => id,
            None => {
                tracing::warn!("EGFX: no channel_id during surface init");
                return None;
            }
        };

        // Set desktop dimensions; create_surface will auto-send ResetGraphics
        // (without monitor layout) on first call. Using resize_with_monitors
        // sends ResetGraphics WITH monitors, which causes Windows clients to
        // re-negotiate capabilities and invalidate the surface.
        server.set_output_dimensions(width, height);

        let sid = server.create_surface(width, height)?;
        server.map_surface_to_output(sid, 0, 0);

        // Drain and send all setup PDUs
        let dvc_messages = server.drain_output();
        tracing::debug!(count = dvc_messages.len(), "EGFX: draining surface setup PDUs");
        drop(server); // Release lock before encoding

        if !dvc_messages.is_empty() {
            match ironrdp_dvc::encode_dvc_messages(
                channel_id,
                dvc_messages,
                ironrdp_svc::ChannelFlags::SHOW_PROTOCOL,
            ) {
                Ok(svc_messages) => {
                    let _ = sender.send(ServerEvent::Egfx(EgfxServerMessage::SendMessages {
                        messages: svc_messages,
                    }));
                }
                Err(e) => {
                    tracing::error!("Failed to encode surface setup PDUs: {}", e);
                    return None;
                }
            }
        }

        tracing::info!(surface_id = sid, width, height, "EGFX surface initialized");
        Some(sid)
    }

    /// Send an encoded H.264 frame via EGFX.
    /// Surface must already be initialized via `init_surface`.
    #[allow(clippy::too_many_arguments)]
    pub fn send_frame(
        handle: &GfxServerHandle,
        sender: &mpsc::UnboundedSender<ServerEvent>,
        surface_id: u16,
        width: u16,
        height: u16,
        h264_data: &[u8],
        timestamp_ms: u32,
        quality: u8,
    ) -> bool {
        // Lock, send frame, drain — minimize lock duration
        let (_frame_id, dvc_messages, channel_id) = {
            let Ok(mut server) = handle.lock() else { return false };

            if !server.is_ready() {
                tracing::debug!("send_frame: server not ready");
                return false;
            }
            if server.should_backpressure() {
                tracing::debug!(
                    in_flight = server.frames_in_flight(),
                    "send_frame: backpressure"
                );
                return false;
            }

            let channel_id = match server.channel_id() {
                Some(id) => id,
                None => {
                    tracing::debug!("send_frame: no channel_id");
                    return false;
                }
            };

            let regions = [Avc420Region::full_frame(width, height, quality)];
            let frame_id =
                match server.send_avc420_frame(surface_id, h264_data, &regions, timestamp_ms) {
                    Some(id) => id,
                    None => {
                        tracing::debug!("send_frame: send_avc420_frame returned None");
                        return false;
                    }
                };

            let dvc_messages = server.drain_output();
            (frame_id, dvc_messages, channel_id)
            // Lock released here
        };

        if dvc_messages.is_empty() {
            return false;
        }

        match ironrdp_dvc::encode_dvc_messages(
            channel_id,
            dvc_messages,
            ironrdp_svc::ChannelFlags::SHOW_PROTOCOL,
        ) {
            Ok(svc_messages) => {
                let _ = sender.send(ServerEvent::Egfx(EgfxServerMessage::SendMessages {
                    messages: svc_messages,
                }));
            }
            Err(e) => {
                tracing::error!("Failed to encode EGFX frame: {}", e);
                return false;
            }
        }

        true
    }

    /// Send an RFX-encoded frame via EGFX.
    pub fn send_rfx_frame(
        handle: &GfxServerHandle,
        sender: &mpsc::UnboundedSender<ServerEvent>,
        surface_id: u16,
        width: u16,
        height: u16,
        rfx_data: Vec<u8>,
        timestamp_ms: u32,
    ) -> bool {
        let (_frame_id, dvc_messages, channel_id) = {
            let Ok(mut server) = handle.lock() else { return false };
            if !server.is_ready() || server.should_backpressure() { return false; }
            let channel_id = match server.channel_id() {
                Some(id) => id,
                None => return false,
            };
            let dest_rect = ironrdp_pdu::geometry::InclusiveRectangle {
                left: 0, top: 0,
                right: width.saturating_sub(1),
                bottom: height.saturating_sub(1),
            };
            let frame_id = match server.send_rfx_frame(surface_id, rfx_data, dest_rect, timestamp_ms) {
                Some(id) => id,
                None => return false,
            };
            let dvc_messages = server.drain_output();
            (frame_id, dvc_messages, channel_id)
        };
        if dvc_messages.is_empty() { return false; }
        match ironrdp_dvc::encode_dvc_messages(channel_id, dvc_messages, ironrdp_svc::ChannelFlags::SHOW_PROTOCOL) {
            Ok(svc_messages) => {
                let _ = sender.send(ServerEvent::Egfx(EgfxServerMessage::SendMessages { messages: svc_messages }));
            }
            Err(e) => {
                tracing::error!("Failed to encode RFX frame: {}", e);
                return false;
            }
        }
        true
    }
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
        // V10_7 first for Windows (AVC444), then V8_1 for iOS (RemoteFX).
        // V10 is after V8_1 because iOS disconnects when server confirms
        // V10 with AVC_DISABLED. iOS doesn't advertise V10_7, so it falls
        // through to V8_1 which works correctly.
        vec![
            CapabilitySet::V10_7 { flags: CapabilitiesV107Flags::SMALL_CACHE },
            CapabilitySet::V8_1 { flags: CapabilitiesV81Flags::AVC420_ENABLED | CapabilitiesV81Flags::SMALL_CACHE },
            CapabilitySet::V10 { flags: CapabilitiesV10Flags::SMALL_CACHE },
            CapabilitySet::V8 { flags: CapabilitiesV8Flags::SMALL_CACHE },
        ]
    }

    fn max_frames_in_flight(&self) -> u32 {
        10
    }

    fn capabilities_advertise(&mut self, pdu: &ironrdp_egfx::pdu::CapabilitiesAdvertisePdu) {
        tracing::info!(count = pdu.0.len(), "EGFX: client advertised capabilities");
        for cap in &pdu.0 {
            tracing::info!(?cap, "EGFX: client capability");
        }
    }

    fn on_ready(&mut self, cap: &ironrdp_egfx::pdu::CapabilitySet) {
        use ironrdp_egfx::pdu::*;
        let avc = match cap {
            CapabilitySet::V8 { .. } => false,
            CapabilitySet::V8_1 { flags } => flags.contains(CapabilitiesV81Flags::AVC420_ENABLED),
            CapabilitySet::V10 { flags } => !flags.contains(CapabilitiesV10Flags::AVC_DISABLED),
            CapabilitySet::V10_1 => true,
            CapabilitySet::V10_2 { flags } => !flags.contains(CapabilitiesV10Flags::AVC_DISABLED),
            CapabilitySet::V10_3 { flags } => !flags.contains(CapabilitiesV103Flags::AVC_DISABLED),
            CapabilitySet::V10_4 { flags } => !flags.contains(CapabilitiesV104Flags::AVC_DISABLED),
            CapabilitySet::V10_5 { .. } | CapabilitySet::V10_6 { .. } | CapabilitySet::V10_6Err { .. } => true,
            CapabilitySet::V10_7 { flags } => !flags.contains(CapabilitiesV107Flags::AVC_DISABLED),
            CapabilitySet::Unknown(_) => false,
        };
        self.shared.avc_enabled.store(avc, Ordering::Release);

        let was_ready = self.shared.ready.load(Ordering::Acquire);
        if was_ready {
            tracing::info!(?cap, avc, "EGFX: client re-negotiated (keeping surface)");
        } else {
            tracing::info!(?cap, avc, "EGFX: channel ready (first time)");
            self.shared.ready_generation.fetch_add(1, Ordering::Release);
        }
        self.shared.ready.store(true, Ordering::Release);
    }

    fn on_frame_ack(&mut self, _frame_id: u32, _queue_depth: u32) {}

    fn on_close(&mut self) {
        tracing::info!("EGFX: channel closed");
        self.shared.ready.store(false, Ordering::Release);
    }
}
