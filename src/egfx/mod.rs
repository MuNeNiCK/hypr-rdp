pub mod encoder;
#[cfg(feature = "vaapi")]
pub mod vaapi;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;

/// Encoder backend: VAAPI hardware or OpenH264 software.
pub enum FrameEncoder {
    #[cfg(feature = "vaapi")]
    Vaapi(vaapi::VaapiEncoder),
    Software(encoder::H264Encoder),
}

impl FrameEncoder {
    /// Try VAAPI first, fall back to software.
    pub fn new(width: u32, height: u32) -> Result<Self> {
        #[cfg(feature = "vaapi")]
        {
            match vaapi::VaapiEncoder::new(width, height) {
                Ok(enc) => {
                    tracing::info!("Using VA-API hardware encoder");
                    return Ok(Self::Vaapi(enc));
                }
                Err(e) => {
                    tracing::warn!("VA-API init failed, falling back to software: {:#}", e);
                }
            }
        }

        let enc = encoder::H264Encoder::new(width, height)?;
        tracing::info!("Using OpenH264 software encoder");
        Ok(Self::Software(enc))
    }

    pub fn encode(&mut self, bgra: &[u8]) -> Result<Vec<u8>> {
        match self {
            #[cfg(feature = "vaapi")]
            Self::Vaapi(enc) => enc.encode(bgra),
            Self::Software(enc) => enc.encode(bgra),
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
        let start_code_len =
            if i + 4 <= data.len() && data[i..i + 4] == [0x00, 0x00, 0x00, 0x01] {
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
use ironrdp_pdu::gcc::{Monitor, MonitorFlags};
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
    /// Whether the EGFX surface has been created and setup PDUs sent
    surface_initialized: AtomicBool,
    /// Event sender for routing encoded frames to the RDP wire
    event_sender: Mutex<Option<mpsc::UnboundedSender<ServerEvent>>>,
}

impl EgfxShared {
    pub fn new() -> Self {
        Self {
            handle: Mutex::new(None),
            ready: AtomicBool::new(false),
            surface_initialized: AtomicBool::new(false),
            event_sender: Mutex::new(None),
        }
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    pub fn get_handle(&self) -> Option<GfxServerHandle> {
        self.handle.lock().unwrap().clone()
    }

    pub fn get_event_sender(&self) -> Option<mpsc::UnboundedSender<ServerEvent>> {
        self.event_sender.lock().unwrap().clone()
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
        let mut server = handle.lock().unwrap();

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

        server.resize_with_monitors(
            width,
            height,
            vec![Monitor {
                left: 0,
                top: 0,
                right: (width as i32) - 1,
                bottom: (height as i32) - 1,
                flags: MonitorFlags::PRIMARY,
            }],
        );

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
    pub fn send_frame(
        handle: &GfxServerHandle,
        sender: &mpsc::UnboundedSender<ServerEvent>,
        surface_id: u16,
        width: u16,
        height: u16,
        h264_data: &[u8],
        timestamp_ms: u32,
    ) -> bool {
        // Lock, send frame, drain — minimize lock duration
        let (frame_id, dvc_messages, channel_id) = {
            let mut server = handle.lock().unwrap();

            if !server.is_ready() || server.should_backpressure() {
                return false;
            }

            let channel_id = match server.channel_id() {
                Some(id) => id,
                None => return false,
            };

            let regions = [Avc420Region::full_frame(width, height, 22)];
            let frame_id = match server.send_avc420_frame(surface_id, h264_data, &regions, timestamp_ms) {
                Some(id) => id,
                None => return false,
            };

            let dvc_messages = server.drain_output();
            (frame_id, dvc_messages, channel_id)
            // Lock released here
        };

        if dvc_messages.is_empty() {
            tracing::trace!(frame_id, "No DVC output after send_avc420_frame");
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
        *self.shared.event_sender.lock().unwrap() = Some(sender);
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

        *self.shared.handle.lock().unwrap() = Some(Arc::clone(&handle));

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
    fn capabilities_advertise(
        &mut self,
        pdu: &ironrdp_egfx::pdu::CapabilitiesAdvertisePdu,
    ) {
        tracing::info!(
            count = pdu.0.len(),
            "EGFX: client advertised capabilities"
        );
    }

    fn on_ready(&mut self, cap: &ironrdp_egfx::pdu::CapabilitySet) {
        tracing::info!(?cap, "EGFX: channel ready");
        // Reset surface state for new/reconnecting client
        self.shared.surface_initialized.store(false, Ordering::Release);
        self.shared.ready.store(true, Ordering::Release);
    }

    fn on_frame_ack(&mut self, frame_id: u32, queue_depth: u32) {
        tracing::trace!(frame_id, queue_depth, "EGFX: frame ack");
    }

    fn on_close(&mut self) {
        tracing::info!("EGFX: channel closed");
        self.shared.ready.store(false, Ordering::Release);
        self.shared.surface_initialized.store(false, Ordering::Release);
    }
}
