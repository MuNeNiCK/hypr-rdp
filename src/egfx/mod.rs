pub mod encoder;
#[cfg(feature = "vaapi")]
pub mod vaapi;
#[cfg(feature = "vaapi")]
pub mod vpp;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum H264RateControl {
    Vbr,
    Cqp,
}

/// Encoder backend: VAAPI hardware or OpenH264 software.
pub enum FrameEncoder {
    #[cfg(feature = "vaapi")]
    Vaapi(Box<vaapi::VaapiEncoder>),
    Software(Box<encoder::H264Encoder>),
    SoftwareAvc444(Box<encoder::Avc444Encoder>),
}

impl FrameEncoder {
    /// Try VAAPI first, fall back to software.
    pub fn new(
        width: u32,
        height: u32,
        bitrate: u32,
        fps: u32,
        quality: u8,
        rate_control: H264RateControl,
    ) -> Result<Self> {
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

        let enc = encoder::H264Encoder::new(width, height, bitrate, fps, quality, rate_control)?;
        tracing::info!("Using OpenH264 software encoder");
        Ok(Self::Software(Box::new(enc)))
    }

    pub fn encode(&mut self, bgra: &[u8], stride: usize) -> Result<Vec<u8>> {
        match self {
            #[cfg(feature = "vaapi")]
            Self::Vaapi(enc) => enc.encode(bgra, stride),
            Self::Software(enc) => enc.encode(bgra, stride),
            Self::SoftwareAvc444(_) => anyhow::bail!("AVC444 encoder requires encode_avc444"),
        }
    }

    pub fn encode_avc444(
        &mut self,
        bgra: &[u8],
        stride: usize,
        candidate_regions: &[(i32, i32, i32, i32)],
    ) -> Result<encoder::Avc444EncodedFrame> {
        match self {
            Self::SoftwareAvc444(enc) => enc.encode(bgra, stride, candidate_regions),
            #[cfg(feature = "vaapi")]
            Self::Vaapi(_) => anyhow::bail!("AVC444 encoding requires software encoder"),
            Self::Software(_) => anyhow::bail!("AVC444 encoding requires AVC444 encoder"),
        }
    }

    pub fn commit_avc444_reference(&mut self) {
        if let Self::SoftwareAvc444(enc) = self {
            enc.commit_reference();
        }
    }

    #[cfg(test)]
    pub(crate) fn avc444_luma_reference_y_for_test(&self) -> Option<&[u8]> {
        match self {
            Self::SoftwareAvc444(enc) => enc.luma_reference_y_for_test(),
            #[cfg(feature = "vaapi")]
            Self::Vaapi(_) | Self::Software(_) => None,
            #[cfg(not(feature = "vaapi"))]
            Self::Software(_) => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn avc444_last_reference_regions_for_test(
        &self,
    ) -> Option<(&[(i32, i32, i32, i32)], &[(i32, i32, i32, i32)])> {
        match self {
            Self::SoftwareAvc444(enc) => Some(enc.last_reference_regions_for_test()),
            #[cfg(feature = "vaapi")]
            Self::Vaapi(_) | Self::Software(_) => None,
            #[cfg(not(feature = "vaapi"))]
            Self::Software(_) => None,
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
            Self::Vaapi(enc) => enc.encode_dmabuf(
                nv12_fd, width, height, stride, offset, modifier, uv_stride, uv_offset,
            ),
            Self::Software(_) => anyhow::bail!("DMA-BUF encode requires VA-API backend"),
        }
    }

    pub fn backend_name(&self) -> &'static str {
        match self {
            #[cfg(feature = "vaapi")]
            Self::Vaapi(_) => "vaapi",
            Self::Software(_) => "openh264",
            Self::SoftwareAvc444(_) => "openh264-avc444",
        }
    }

    pub fn is_vaapi(&self) -> bool {
        match self {
            #[cfg(feature = "vaapi")]
            Self::Vaapi(_) => true,
            Self::Software(_) => false,
            Self::SoftwareAvc444(_) => false,
        }
    }

    /// Create a software-only encoder (fallback when VA-API fails at runtime).
    pub fn new_software_only(
        width: u32,
        height: u32,
        bitrate: u32,
        fps: u32,
        quality: u8,
        rate_control: H264RateControl,
    ) -> Result<Self> {
        let enc = encoder::H264Encoder::new(width, height, bitrate, fps, quality, rate_control)?;
        tracing::info!("Using OpenH264 software encoder (runtime fallback)");
        Ok(Self::Software(Box::new(enc)))
    }

    pub fn new_avc444_software_only(
        width: u32,
        height: u32,
        bitrate: u32,
        fps: u32,
        quality: u8,
        rate_control: H264RateControl,
    ) -> Result<Self> {
        let enc = encoder::Avc444Encoder::new(width, height, bitrate, fps, quality, rate_control)?;
        tracing::info!("Using OpenH264 software AVC444 encoder");
        Ok(Self::SoftwareAvc444(Box::new(enc)))
    }

    /// Force the next encoded frame to be an IDR (recovery after dropped frames).
    pub fn force_idr(&mut self) {
        match self {
            #[cfg(feature = "vaapi")]
            Self::Vaapi(enc) => enc.force_idr(),
            Self::Software(enc) => enc.force_idr(),
            Self::SoftwareAvc444(enc) => enc.force_idr(),
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

use ironrdp_egfx::pdu::{Avc420Region, Codec1Type, Encoding};
use ironrdp_egfx::server::{GraphicsPipelineHandler, GraphicsPipelineServer};
use ironrdp_server::{
    EgfxServerMessage, GfxDvcBridge, GfxServerFactory, GfxServerHandle, ServerEvent,
    ServerEventSender,
};
use tokio::sync::mpsc;

pub const DEFAULT_MAX_FRAMES_IN_FLIGHT: u32 = 120;

fn avc444_disabled_by_env() -> bool {
    std::env::var_os("HYPR_RDP_DISABLE_AVC444").is_some()
}

fn capability_avc_support(
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

/// Shared EGFX state accessible from factory, handler, and capture thread.
pub struct EgfxShared {
    /// The GFX server handle (set once during build_server_with_handle)
    handle: Mutex<Option<GfxServerHandle>>,
    /// Whether EGFX capability negotiation is complete
    ready: AtomicBool,
    /// Whether the negotiated capability supports AVC420/AVC444 (H.264)
    avc_enabled: AtomicBool,
    /// Whether the negotiated capability supports AVC444.
    avc444_enabled: AtomicBool,
    /// Incremented each time on_ready fires; lets capture thread detect re-negotiation
    ready_generation: AtomicU32,
    /// Event sender for routing encoded frames to the RDP wire
    event_sender: Mutex<Option<mpsc::UnboundedSender<ServerEvent>>>,
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

    fn max_frames_in_flight(&self) -> u32 {
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

    /// Prepare EGFX state for a resize (Deactivation-Reactivation).
    /// Deletes all old surfaces, sends ResetGraphics at the new dimensions,
    /// and bumps generation so the capture thread re-creates encoder/surface.
    pub fn prepare_for_resize(&self, width: u16, height: u16) {
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
            let Ok(mut server) = handle.lock() else {
                return;
            };
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
                    if sender
                        .send(ServerEvent::Egfx(EgfxServerMessage::SendMessages {
                            messages: svc_messages,
                        }))
                        .is_ok()
                    {
                        self.ready_generation.fetch_add(1, Ordering::Release);
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to encode resize PDUs: {}", e);
                }
            }
        } else {
            self.ready_generation.fetch_add(1, Ordering::Release);
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
        if sender.is_closed() {
            return None;
        }

        let Ok(mut server) = handle.lock() else {
            return None;
        };

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
        tracing::trace!(
            count = dvc_messages.len(),
            "EGFX: draining surface setup PDUs"
        );
        drop(server); // Release lock before encoding

        if !dvc_messages.is_empty() {
            match ironrdp_dvc::encode_dvc_messages(
                channel_id,
                dvc_messages,
                ironrdp_svc::ChannelFlags::SHOW_PROTOCOL,
            ) {
                Ok(svc_messages) => {
                    if sender
                        .send(ServerEvent::Egfx(EgfxServerMessage::SendMessages {
                            messages: svc_messages,
                        }))
                        .is_err()
                    {
                        return None;
                    }
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

    /// Return whether an EGFX frame can be queued now.
    ///
    /// This is checked before H.264 encoding so a frame that cannot be sent does
    /// not advance the encoder reference chain.
    pub fn can_send_frame(handle: &GfxServerHandle) -> bool {
        let Ok(server) = handle.lock() else {
            return false;
        };
        server.is_ready() && server.channel_id().is_some() && !server.should_backpressure()
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
        let regions = [rdpegfx_full_frame_region(width, height, quality)];
        Self::send_frame_with_regions(
            handle,
            sender,
            surface_id,
            h264_data,
            &regions,
            timestamp_ms,
        )
    }

    /// Send an encoded H.264 frame via EGFX using RDPEGFX AVC420 region metadata.
    /// Surface must already be initialized via `init_surface`.
    pub fn send_frame_with_regions(
        handle: &GfxServerHandle,
        sender: &mpsc::UnboundedSender<ServerEvent>,
        surface_id: u16,
        h264_data: &[u8],
        regions: &[Avc420Region],
        timestamp_ms: u32,
    ) -> bool {
        if sender.is_closed() {
            tracing::trace!("send_frame: EGFX event channel already closed");
            return false;
        }

        if regions.is_empty() {
            tracing::trace!("send_frame_with_regions: no regions");
            return false;
        }

        // Lock, send frame, drain — minimize lock duration
        let (_frame_id, dvc_messages, channel_id) = {
            let Ok(mut server) = handle.lock() else {
                return false;
            };

            if !server.is_ready() {
                tracing::trace!("send_frame: server not ready");
                return false;
            }
            if server.should_backpressure() {
                tracing::trace!(
                    in_flight = server.frames_in_flight(),
                    "send_frame: backpressure"
                );
                return false;
            }

            let channel_id = match server.channel_id() {
                Some(id) => id,
                None => {
                    tracing::trace!("send_frame: no channel_id");
                    return false;
                }
            };

            let frame_id =
                match server.send_avc420_frame(surface_id, h264_data, regions, timestamp_ms) {
                    Some(id) => id,
                    None => {
                        tracing::trace!("send_frame: send_avc420_frame returned None");
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
                if sender
                    .send(ServerEvent::Egfx(EgfxServerMessage::SendMessages {
                        messages: svc_messages,
                    }))
                    .is_err()
                {
                    tracing::trace!("send_frame: EGFX event channel closed");
                    return false;
                }
            }
            Err(e) => {
                tracing::error!("Failed to encode EGFX frame: {}", e);
                return false;
            }
        }

        true
    }

    #[allow(clippy::too_many_arguments)]
    fn queue_avc444_frame_with_regions(
        handle: &GfxServerHandle,
        surface_id: u16,
        encoding: encoder::Avc444FrameEncoding,
        stream1_data: &[u8],
        stream1_regions: &[Avc420Region],
        stream2_data: Option<&[u8]>,
        stream2_regions: Option<&[Avc420Region]>,
        timestamp_ms: u32,
    ) -> Option<(u32, Vec<ironrdp_dvc::DvcMessage>, u32)> {
        let has_stream2_data = stream2_data.is_some_and(|data| !data.is_empty());
        let has_stream2_regions = stream2_regions.is_some_and(|regions| !regions.is_empty());
        match encoding {
            encoder::Avc444FrameEncoding::LumaAndChroma => {
                if !has_stream2_data || !has_stream2_regions {
                    tracing::trace!("send_avc444_frame_with_regions: LC=0 requires stream2");
                    return None;
                }
            }
            encoder::Avc444FrameEncoding::Luma | encoder::Avc444FrameEncoding::Chroma => {
                if stream2_data.is_some() || stream2_regions.is_some() {
                    tracing::trace!("send_avc444_frame_with_regions: LC=1/2 forbids stream2");
                    return None;
                }
            }
        }

        if stream1_regions.is_empty() {
            tracing::trace!("send_avc444_frame_with_regions: no regions");
            return None;
        }

        let encoding = match encoding {
            encoder::Avc444FrameEncoding::LumaAndChroma => Encoding::LUMA_AND_CHROMA,
            encoder::Avc444FrameEncoding::Luma => Encoding::LUMA,
            encoder::Avc444FrameEncoding::Chroma => Encoding::CHROMA,
        };

        let (_frame_id, dvc_messages, channel_id) = {
            let Ok(mut server) = handle.lock() else {
                return None;
            };

            if !server.is_ready() {
                tracing::trace!("send_avc444_frame: server not ready");
                return None;
            }
            if server.should_backpressure() {
                tracing::trace!(
                    in_flight = server.frames_in_flight(),
                    "send_avc444_frame: backpressure"
                );
                return None;
            }

            let channel_id = match server.channel_id() {
                Some(id) => id,
                None => {
                    tracing::trace!("send_avc444_frame: no channel_id");
                    return None;
                }
            };

            let frame_id = match server.send_avc444_frame_with_encoding(
                Codec1Type::Avc444v2,
                surface_id,
                encoding,
                stream1_data,
                stream1_regions,
                stream2_data,
                stream2_regions,
                timestamp_ms,
            ) {
                Some(id) => id,
                None => {
                    tracing::trace!("send_avc444_frame: send_avc444v2_frame returned None");
                    return None;
                }
            };

            let dvc_messages = server.drain_output();
            (frame_id, dvc_messages, channel_id)
        };

        if dvc_messages.is_empty() {
            return None;
        }

        Some((_frame_id, dvc_messages, channel_id))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn send_avc444_frame_with_regions(
        handle: &GfxServerHandle,
        sender: &mpsc::UnboundedSender<ServerEvent>,
        surface_id: u16,
        encoding: encoder::Avc444FrameEncoding,
        stream1_data: &[u8],
        stream1_regions: &[Avc420Region],
        stream2_data: Option<&[u8]>,
        stream2_regions: Option<&[Avc420Region]>,
        timestamp_ms: u32,
    ) -> bool {
        if sender.is_closed() {
            tracing::trace!("send_avc444_frame: EGFX event channel already closed");
            return false;
        }

        let Some((_frame_id, dvc_messages, channel_id)) = Self::queue_avc444_frame_with_regions(
            handle,
            surface_id,
            encoding,
            stream1_data,
            stream1_regions,
            stream2_data,
            stream2_regions,
            timestamp_ms,
        ) else {
            return false;
        };

        match ironrdp_dvc::encode_dvc_messages(
            channel_id,
            dvc_messages,
            ironrdp_svc::ChannelFlags::SHOW_PROTOCOL,
        ) {
            Ok(svc_messages) => {
                if sender
                    .send(ServerEvent::Egfx(EgfxServerMessage::SendMessages {
                        messages: svc_messages,
                    }))
                    .is_err()
                {
                    tracing::trace!("send_avc444_frame: EGFX event channel closed");
                    return false;
                }
            }
            Err(e) => {
                tracing::error!("Failed to encode EGFX AVC444 frame: {}", e);
                return false;
            }
        }

        true
    }
}

pub(crate) fn rdpegfx_region_quality(qp: u8) -> u8 {
    100u8.saturating_sub(qp & 0x3f)
}

pub(crate) fn rdpegfx_full_frame_region(width: u16, height: u16, qp: u8) -> Avc420Region {
    // RDPGFX_RECT16 uses exclusive right/bottom bounds. Build the region
    // explicitly to avoid depending on helper-specific bounds behavior.
    Avc420Region::new(0, 0, width, height, qp, rdpegfx_region_quality(qp))
}

/// Factory for creating EGFX pipeline handlers.
pub struct HyprGfxFactory {
    shared: Arc<EgfxShared>,
    event_sender: Option<mpsc::UnboundedSender<ServerEvent>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironrdp_core::{encode_vec, Decode, Encode, ReadCursor};
    use ironrdp_dvc::DvcProcessor as _;
    use ironrdp_egfx::pdu::{
        Avc420BitmapStream, Avc444BitmapStream, Codec1Type, Encoding, GfxPdu, PixelFormat,
        QuantQuality, WireToSurface1Pdu,
    };
    use ironrdp_pdu::geometry::InclusiveRectangle;

    const TEST_CHANNEL_ID: u32 = 1007;

    fn ready_avc444_handle(width: u16, height: u16) -> (GfxServerHandle, u16) {
        let shared = Arc::new(EgfxShared::new(DEFAULT_MAX_FRAMES_IN_FLIGHT));
        shared.set_surface_size(width, height);
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let mut factory = HyprGfxFactory::new(Arc::clone(&shared));
        ironrdp_server::ServerEventSender::set_sender(&mut factory, event_tx.clone());
        let (mut bridge, handle) =
            ironrdp_server::GfxServerFactory::build_server_with_handle(&factory)
                .expect("EGFX server builds");
        bridge.start(TEST_CHANNEL_ID).expect("channel starts");

        let caps =
            GfxPdu::CapabilitiesAdvertise(ironrdp_egfx::pdu::CapabilitiesAdvertisePdu(vec![
                ironrdp_egfx::pdu::CapabilitySet::V10_7 {
                    flags: ironrdp_egfx::pdu::CapabilitiesV107Flags::empty(),
                },
            ]));
        let caps = encode_vec(&caps).expect("capabilities encode");
        let _ = bridge
            .process(TEST_CHANNEL_ID, &caps)
            .expect("capabilities process");
        assert!(shared.is_ready());
        assert!(shared.is_avc444_enabled());

        let surface_id =
            EgfxShared::init_surface(&handle, &event_tx, width, height).expect("surface init");
        (handle, surface_id)
    }

    fn decode_gfx_output(message: &ironrdp_dvc::DvcMessage) -> GfxPdu {
        let wrapped = encode_vec(&**message).expect("DVC message encodes");
        assert_eq!(&wrapped[0..2], &[0xe0, 0x04]);
        let mut cursor = ReadCursor::new(&wrapped[2..]);
        GfxPdu::decode(&mut cursor).expect("GFX PDU decodes")
    }

    fn decode_avc444_wire_to_surface(message: &ironrdp_dvc::DvcMessage) -> WireToSurface1Pdu {
        match decode_gfx_output(message) {
            GfxPdu::WireToSurface1(pdu) => pdu,
            other => panic!("expected WireToSurface1, got {other:?}"),
        }
    }

    #[test]
    fn full_frame_region_uses_rdpegfx_exclusive_bounds() {
        let region = rdpegfx_full_frame_region(1280, 720, 23);
        assert_eq!(region.left, 0);
        assert_eq!(region.top, 0);
        assert_eq!(region.right, 1280);
        assert_eq!(region.bottom, 720);
        assert_eq!(region.quantization_parameter, 23);
        assert_eq!(region.quality, 77);
    }

    #[test]
    fn avc444_stream_info_encodes_lc_and_stream1_size() {
        let rectangle = InclusiveRectangle {
            left: 0,
            top: 0,
            right: 15,
            bottom: 15,
        };
        let quant = QuantQuality {
            quantization_parameter: 20,
            progressive: false,
            quality: 80,
        };
        let stream1 = Avc420BitmapStream {
            rectangles: vec![rectangle.clone()],
            quant_qual_vals: vec![quant.clone()],
            data: &[1, 2, 3, 4],
        };
        let stream2 = Avc420BitmapStream {
            rectangles: vec![rectangle],
            quant_qual_vals: vec![quant],
            data: &[5, 6, 7],
        };
        let stream1_size = stream1.size();
        let avc444 = Avc444BitmapStream {
            encoding: Encoding::LUMA_AND_CHROMA,
            stream1,
            stream2: Some(stream2),
        };

        let encoded = encode_vec(&avc444).expect("AVC444 stream encodes");
        let stream_info = u32::from_le_bytes(encoded[0..4].try_into().unwrap());

        assert_eq!(stream_info & 0x3fff_ffff, stream1_size as u32);
        assert_eq!(
            stream_info >> 30,
            u32::from(Encoding::LUMA_AND_CHROMA.bits())
        );
    }

    #[test]
    fn avc444_luma_only_decodes_stream1_without_stream2() {
        let rectangle = InclusiveRectangle {
            left: 2,
            top: 4,
            right: 18,
            bottom: 20,
        };
        let quant = QuantQuality {
            quantization_parameter: 18,
            progressive: false,
            quality: 82,
        };
        let avc444 = Avc444BitmapStream {
            encoding: Encoding::LUMA,
            stream1: Avc420BitmapStream {
                rectangles: vec![rectangle.clone()],
                quant_qual_vals: vec![quant.clone()],
                data: &[1, 3, 5, 7],
            },
            stream2: None,
        };

        let encoded = encode_vec(&avc444).expect("AVC444 stream encodes");
        let mut cursor = ReadCursor::new(&encoded);
        let decoded = Avc444BitmapStream::decode(&mut cursor).expect("AVC444 stream decodes");

        assert_eq!(decoded.encoding, Encoding::LUMA);
        assert_eq!(decoded.stream1.rectangles, vec![rectangle]);
        assert_eq!(decoded.stream1.quant_qual_vals, vec![quant]);
        assert_eq!(decoded.stream1.data, &[1, 3, 5, 7]);
        assert!(decoded.stream2.is_none());
    }

    #[test]
    fn avc444_chroma_only_decodes_stream1_without_stream2() {
        let rectangle = InclusiveRectangle {
            left: 4,
            top: 2,
            right: 11,
            bottom: 7,
        };
        let quant = QuantQuality {
            quantization_parameter: 23,
            progressive: false,
            quality: 77,
        };
        let avc444 = Avc444BitmapStream {
            encoding: Encoding::CHROMA,
            stream1: Avc420BitmapStream {
                rectangles: vec![rectangle.clone()],
                quant_qual_vals: vec![quant.clone()],
                data: &[9, 8, 7, 6],
            },
            stream2: None,
        };

        let encoded = encode_vec(&avc444).expect("AVC444 stream encodes");
        let mut cursor = ReadCursor::new(&encoded);
        let decoded = Avc444BitmapStream::decode(&mut cursor).expect("AVC444 stream decodes");

        assert_eq!(decoded.encoding, Encoding::CHROMA);
        assert_eq!(decoded.stream1.rectangles, vec![rectangle]);
        assert_eq!(decoded.stream1.quant_qual_vals, vec![quant]);
        assert_eq!(decoded.stream1.data, &[9, 8, 7, 6]);
        assert!(decoded.stream2.is_none());
    }

    #[test]
    fn wire_to_surface1_roundtrips_avc444v2_bitmap_payload() {
        let rectangle = InclusiveRectangle {
            left: 0,
            top: 0,
            right: 32,
            bottom: 24,
        };
        let quant = QuantQuality {
            quantization_parameter: 21,
            progressive: false,
            quality: 79,
        };
        let avc444 = Avc444BitmapStream {
            encoding: Encoding::LUMA_AND_CHROMA,
            stream1: Avc420BitmapStream {
                rectangles: vec![rectangle.clone()],
                quant_qual_vals: vec![quant.clone()],
                data: &[0xaa, 0xbb, 0xcc],
            },
            stream2: Some(Avc420BitmapStream {
                rectangles: vec![rectangle.clone()],
                quant_qual_vals: vec![quant.clone()],
                data: &[0x11, 0x22],
            }),
        };
        let pdu = WireToSurface1Pdu {
            surface_id: 7,
            codec_id: Codec1Type::Avc444v2,
            pixel_format: PixelFormat::ARgb,
            destination_rectangle: rectangle.clone(),
            bitmap_data: encode_vec(&avc444).expect("AVC444 stream encodes"),
        };

        let encoded = encode_vec(&pdu).expect("WireToSurface1 encodes");
        let mut cursor = ReadCursor::new(&encoded);
        let decoded = WireToSurface1Pdu::decode(&mut cursor).expect("WireToSurface1 decodes");
        let mut bitmap_cursor = ReadCursor::new(&decoded.bitmap_data);
        let bitmap =
            Avc444BitmapStream::decode(&mut bitmap_cursor).expect("AVC444 payload decodes");

        assert_eq!(decoded.surface_id, 7);
        assert_eq!(decoded.codec_id, Codec1Type::Avc444v2);
        assert_eq!(decoded.pixel_format, PixelFormat::ARgb);
        assert_eq!(decoded.destination_rectangle, rectangle.clone());
        assert_eq!(bitmap.encoding, Encoding::LUMA_AND_CHROMA);
        assert_eq!(bitmap.stream1.rectangles, vec![rectangle.clone()]);
        assert_eq!(bitmap.stream1.quant_qual_vals, vec![quant.clone()]);
        assert_eq!(bitmap.stream1.data, &[0xaa, 0xbb, 0xcc]);
        let stream2 = bitmap.stream2.expect("LC=0 carries stream2");
        assert_eq!(stream2.rectangles, vec![rectangle]);
        assert_eq!(stream2.quant_qual_vals, vec![quant]);
        assert_eq!(stream2.data, &[0x11, 0x22]);
    }

    #[test]
    fn avc444_send_wrapper_maps_luma_and_chroma_to_wire_payload() {
        let (handle, surface_id) = ready_avc444_handle(64, 64);
        let stream1_regions = [Avc420Region::new(0, 0, 32, 32, 21, 79)];
        let stream2_regions = [Avc420Region::new(16, 8, 64, 48, 22, 78)];

        let (_frame_id, dvc_messages, _channel_id) = EgfxShared::queue_avc444_frame_with_regions(
            &handle,
            surface_id,
            encoder::Avc444FrameEncoding::LumaAndChroma,
            &[1, 2, 3],
            &stream1_regions,
            Some(&[4, 5]),
            Some(&stream2_regions),
            123,
        )
        .expect("LC=0 AVC444v2 frame queues");

        assert_eq!(dvc_messages.len(), 3);
        assert!(matches!(
            decode_gfx_output(&dvc_messages[0]),
            GfxPdu::StartFrame(_)
        ));
        assert!(matches!(
            decode_gfx_output(&dvc_messages[2]),
            GfxPdu::EndFrame(_)
        ));
        let wire = decode_avc444_wire_to_surface(&dvc_messages[1]);
        let mut bitmap_cursor = ReadCursor::new(&wire.bitmap_data);
        let bitmap =
            Avc444BitmapStream::decode(&mut bitmap_cursor).expect("AVC444 payload decodes");
        let stream_info = u32::from_le_bytes(wire.bitmap_data[0..4].try_into().unwrap());

        assert_eq!(wire.surface_id, surface_id);
        assert_eq!(wire.codec_id, Codec1Type::Avc444v2);
        assert_eq!(
            stream_info >> 30,
            u32::from(Encoding::LUMA_AND_CHROMA.bits())
        );
        assert_eq!(bitmap.encoding, Encoding::LUMA_AND_CHROMA);
        assert_eq!(bitmap.stream1.data, &[1, 2, 3]);
        assert_eq!(bitmap.stream1.rectangles[0].left, 0);
        assert_eq!(bitmap.stream1.rectangles[0].right, 32);
        let stream2 = bitmap.stream2.expect("LC=0 has stream2");
        assert_eq!(stream2.data, &[4, 5]);
        assert_eq!(stream2.rectangles[0].left, 16);
        assert_eq!(stream2.rectangles[0].right, 64);
        assert_eq!(wire.destination_rectangle.left, 0);
        assert_eq!(wire.destination_rectangle.top, 0);
        assert_eq!(wire.destination_rectangle.right, 64);
        assert_eq!(wire.destination_rectangle.bottom, 48);
    }

    #[test]
    fn avc444_send_wrapper_maps_luma_only_and_chroma_only_to_stream1() {
        for (local_encoding, wire_encoding, payload) in [
            (
                encoder::Avc444FrameEncoding::Luma,
                Encoding::LUMA,
                &[0x10, 0x11][..],
            ),
            (
                encoder::Avc444FrameEncoding::Chroma,
                Encoding::CHROMA,
                &[0x20, 0x21, 0x22][..],
            ),
        ] {
            let (handle, surface_id) = ready_avc444_handle(64, 64);
            let regions = [Avc420Region::new(4, 6, 20, 22, 18, 82)];
            let (_frame_id, dvc_messages, _channel_id) =
                EgfxShared::queue_avc444_frame_with_regions(
                    &handle,
                    surface_id,
                    local_encoding,
                    payload,
                    &regions,
                    None,
                    None,
                    123,
                )
                .expect("single-stream AVC444v2 frame queues");

            assert_eq!(dvc_messages.len(), 3);
            let wire = decode_avc444_wire_to_surface(&dvc_messages[1]);
            let mut bitmap_cursor = ReadCursor::new(&wire.bitmap_data);
            let bitmap =
                Avc444BitmapStream::decode(&mut bitmap_cursor).expect("AVC444 payload decodes");
            let stream_info = u32::from_le_bytes(wire.bitmap_data[0..4].try_into().unwrap());

            assert_eq!(wire.surface_id, surface_id);
            assert_eq!(wire.codec_id, Codec1Type::Avc444v2);
            assert_eq!(stream_info >> 30, u32::from(wire_encoding.bits()));
            assert_eq!(bitmap.encoding, wire_encoding);
            assert_eq!(bitmap.stream1.data, payload);
            assert_eq!(bitmap.stream1.rectangles[0].left, 4);
            assert_eq!(bitmap.stream1.rectangles[0].right, 20);
            assert!(bitmap.stream2.is_none());
        }
    }

    #[test]
    fn avc444_send_wrapper_rejects_stream2_for_single_stream_lc() {
        let (handle, surface_id) = ready_avc444_handle(64, 64);
        let stream1_regions = [Avc420Region::new(0, 0, 32, 32, 21, 79)];
        let stream2_regions = [Avc420Region::new(16, 8, 64, 48, 22, 78)];

        for encoding in [
            encoder::Avc444FrameEncoding::Luma,
            encoder::Avc444FrameEncoding::Chroma,
        ] {
            assert!(EgfxShared::queue_avc444_frame_with_regions(
                &handle,
                surface_id,
                encoding,
                &[1, 2, 3],
                &stream1_regions,
                Some(&[4, 5]),
                Some(&stream2_regions),
                123,
            )
            .is_none());
        }
    }

    #[test]
    fn avc444_send_wrapper_rejects_lc0_without_stream2() {
        let (handle, surface_id) = ready_avc444_handle(64, 64);
        let stream1_regions = [Avc420Region::new(0, 0, 32, 32, 21, 79)];

        assert!(EgfxShared::queue_avc444_frame_with_regions(
            &handle,
            surface_id,
            encoder::Avc444FrameEncoding::LumaAndChroma,
            &[1, 2, 3],
            &stream1_regions,
            None,
            None,
            123,
        )
        .is_none());
    }

    #[test]
    fn avc444_send_with_closed_event_channel_does_not_queue_frame() {
        let (handle, surface_id) = ready_avc444_handle(64, 64);
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        drop(event_rx);
        let regions = [Avc420Region::new(0, 0, 32, 32, 21, 79)];

        assert!(!EgfxShared::send_avc444_frame_with_regions(
            &handle,
            &event_tx,
            surface_id,
            encoder::Avc444FrameEncoding::Luma,
            &[1, 2, 3],
            &regions,
            None,
            None,
            123,
        ));
        let server = handle.lock().expect("server lock");
        assert_eq!(server.frames_in_flight(), 0);
    }

    #[test]
    fn resize_does_not_bump_generation_when_reset_cannot_be_sent() {
        let shared = Arc::new(EgfxShared::new(DEFAULT_MAX_FRAMES_IN_FLIGHT));
        shared.set_surface_size(64, 64);
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let mut factory = HyprGfxFactory::new(Arc::clone(&shared));
        ironrdp_server::ServerEventSender::set_sender(&mut factory, event_tx.clone());
        let (mut bridge, handle) =
            ironrdp_server::GfxServerFactory::build_server_with_handle(&factory)
                .expect("EGFX server builds");
        bridge.start(TEST_CHANNEL_ID).expect("channel starts");

        let caps =
            GfxPdu::CapabilitiesAdvertise(ironrdp_egfx::pdu::CapabilitiesAdvertisePdu(vec![
                ironrdp_egfx::pdu::CapabilitySet::V10_7 {
                    flags: ironrdp_egfx::pdu::CapabilitiesV107Flags::empty(),
                },
            ]));
        let caps = encode_vec(&caps).expect("capabilities encode");
        let _ = bridge
            .process(TEST_CHANNEL_ID, &caps)
            .expect("capabilities process");
        let _ = EgfxShared::init_surface(&handle, &event_tx, 64, 64).expect("surface init");
        let generation = shared.generation();

        drop(event_rx);
        shared.prepare_for_resize(64, 64);

        assert_eq!(shared.generation(), generation);
    }

    #[test]
    fn capability_support_respects_avc_disabled_flags_and_avc444_env_switch() {
        use ironrdp_egfx::pdu::*;

        assert_eq!(
            capability_avc_support(
                &CapabilitySet::V8_1 {
                    flags: CapabilitiesV81Flags::AVC420_ENABLED,
                },
                false,
            ),
            (true, false)
        );
        assert_eq!(
            capability_avc_support(
                &CapabilitySet::V10_7 {
                    flags: CapabilitiesV107Flags::empty(),
                },
                false,
            ),
            (true, true)
        );
        assert_eq!(
            capability_avc_support(
                &CapabilitySet::V10_7 {
                    flags: CapabilitiesV107Flags::empty(),
                },
                true,
            ),
            (true, false)
        );
        assert_eq!(
            capability_avc_support(
                &CapabilitySet::V10_7 {
                    flags: CapabilitiesV107Flags::AVC_DISABLED,
                },
                false,
            ),
            (false, false)
        );
        assert_eq!(
            capability_avc_support(
                &CapabilitySet::V10 {
                    flags: CapabilitiesV10Flags::AVC_DISABLED,
                },
                false,
            ),
            (false, false)
        );
    }

    #[test]
    fn avc420_region_preserves_rect16_bounds_and_quant_quality() {
        let region = Avc420Region::new(4, 6, 20, 22, 19, 81);
        let rectangle = region.to_rectangle();
        let quant = region.to_quant_quality();

        assert_eq!(rectangle.left, 4);
        assert_eq!(rectangle.top, 6);
        assert_eq!(rectangle.right, 20);
        assert_eq!(rectangle.bottom, 22);
        assert_eq!(quant.quantization_parameter, 19);
        assert!(!quant.progressive);
        assert_eq!(quant.quality, 81);
    }
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
