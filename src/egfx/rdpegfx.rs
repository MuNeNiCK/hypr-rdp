use std::sync::atomic::Ordering;

use ironrdp_egfx::pdu::{Avc420Region, Codec1Type, Encoding};
use ironrdp_server::{EgfxServerMessage, GfxServerHandle, ServerEvent};
use tokio::sync::mpsc;

use super::{encoder, EgfxShared};

impl EgfxShared {
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
    pub(super) fn queue_avc444_frame_with_regions(
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
