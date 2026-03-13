pub mod encoder;
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

    pub fn encode(&mut self, bgra: &[u8]) -> Result<Vec<u8>> {
        match self {
            #[cfg(feature = "vaapi")]
            Self::Vaapi(enc) => enc.encode(bgra),
            Self::Software(enc) => enc.encode(bgra),
        }
    }

    /// Encode from an NV12 DMA-BUF (zero-copy path). Only available with VA-API backend.
    #[cfg(feature = "vaapi")]
    pub fn encode_dmabuf(
        &mut self,
        nv12_fd: std::os::unix::io::RawFd,
        width: u32,
        height: u32,
        stride: u32,
        offset: u32,
        modifier: u64,
    ) -> Result<Vec<u8>> {
        match self {
            Self::Vaapi(enc) => enc.encode_dmabuf(nv12_fd, width, height, stride, offset, modifier),
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
    /// Incremented each time on_ready fires; lets capture thread detect re-negotiation
    ready_generation: AtomicU32,
    /// Event sender for routing encoded frames to the RDP wire
    event_sender: Mutex<Option<mpsc::UnboundedSender<ServerEvent>>>,
}

impl EgfxShared {
    pub fn new() -> Self {
        Self {
            handle: Mutex::new(None),
            ready: AtomicBool::new(false),
            ready_generation: AtomicU32::new(0),
            event_sender: Mutex::new(None),
        }
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
        let server = GraphicsPipelineServer::new(handler);
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
    fn max_frames_in_flight(&self) -> u32 {
        10
    }

    fn capabilities_advertise(&mut self, pdu: &ironrdp_egfx::pdu::CapabilitiesAdvertisePdu) {
        tracing::info!(count = pdu.0.len(), "EGFX: client advertised capabilities");
    }

    fn on_ready(&mut self, cap: &ironrdp_egfx::pdu::CapabilitySet) {
        let was_ready = self.shared.ready.load(Ordering::Acquire);
        if was_ready {
            tracing::info!(?cap, "EGFX: client re-negotiated (keeping surface)");
        } else {
            tracing::info!(?cap, "EGFX: channel ready (first time)");
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
