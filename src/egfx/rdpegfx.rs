use std::sync::atomic::Ordering;

use ironrdp_dvc::DvcMessage;
use ironrdp_egfx::server::GraphicsPipelineServer;
use ironrdp_server::{EgfxServerMessage, GfxServerHandle, ServerEvent};
use tokio::sync::mpsc;

use super::{EgfxFrameReadiness, EgfxShared};

pub(in crate::egfx) struct QueuedRdpegfxFrame {
    pub(in crate::egfx) frame_id: u32,
    pub(in crate::egfx) dvc_messages: Vec<DvcMessage>,
    pub(in crate::egfx) channel_id: u32,
}

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
        self.clear_current_surface();

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
                        self.clear_frame_queue();
                        self.ready_generation.fetch_add(1, Ordering::Release);
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to encode resize PDUs: {}", e);
                }
            }
        } else {
            self.clear_frame_queue();
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

    pub fn init_or_reuse_surface(
        &self,
        handle: &GfxServerHandle,
        sender: &mpsc::UnboundedSender<ServerEvent>,
        width: u16,
        height: u16,
    ) -> Option<u16> {
        if let Some(surface_id) = self.current_surface_id(width, height) {
            if let Ok(server) = handle.lock() {
                if server.is_ready() && server.get_surface(surface_id).is_some() {
                    tracing::trace!(surface_id, width, height, "EGFX surface reused");
                    return Some(surface_id);
                }
            }
            self.clear_current_surface();
        }

        let surface_id = Self::init_surface(handle, sender, width, height)?;
        self.set_current_surface(surface_id, width, height);
        Some(surface_id)
    }

    /// Return whether an EGFX frame can be queued now.
    ///
    /// This is checked before H.264 encoding so a frame that cannot be sent does
    /// not advance the encoder reference chain.
    pub fn can_send_frame(&self, handle: &GfxServerHandle) -> bool {
        self.frame_readiness(handle).is_ready()
    }

    pub(crate) fn frame_readiness(&self, handle: &GfxServerHandle) -> EgfxFrameReadiness {
        if self.should_backpressure_frames() {
            return EgfxFrameReadiness::LocalBackpressure {
                in_flight: self.frames_in_flight(),
                max: self.max_frames_in_flight(),
                client_queue_depth: self.client_queue_depth(),
                ack_suspended: self.frame_ack_suspended(),
            };
        }

        Self::transport_frame_readiness(handle)
    }

    pub(in crate::egfx) fn transport_frame_readiness(
        handle: &GfxServerHandle,
    ) -> EgfxFrameReadiness {
        let Ok(server) = handle.lock() else {
            return EgfxFrameReadiness::TransportUnavailable;
        };
        if !server.is_ready() {
            return EgfxFrameReadiness::TransportNotReady;
        }
        if server.channel_id().is_none() {
            return EgfxFrameReadiness::TransportNoChannel;
        }
        if server.should_backpressure() {
            return EgfxFrameReadiness::TransportBackpressure {
                in_flight: server.frames_in_flight(),
                client_queue_depth: server.client_queue_depth(),
            };
        }
        EgfxFrameReadiness::Ready
    }

    pub(in crate::egfx) fn rdpegfx_event_sender_closed(
        sender: &mpsc::UnboundedSender<ServerEvent>,
        trace_name: &'static str,
    ) -> bool {
        if sender.is_closed() {
            tracing::trace!("{trace_name}: EGFX event channel already closed");
            return true;
        }
        false
    }

    pub(in crate::egfx) fn send_tracked_rdpegfx_frame(
        &self,
        handle: &GfxServerHandle,
        sender: &mpsc::UnboundedSender<ServerEvent>,
        codec_name: &'static str,
        trace_name: &'static str,
        queue_frame: impl FnOnce(&mut GraphicsPipelineServer) -> Option<u32>,
    ) -> bool {
        if !self.can_send_frame(handle) {
            return false;
        }

        let Some(queued) = Self::queue_rdpegfx_frame(handle, trace_name, queue_frame) else {
            return false;
        };

        let frame_id = queued.frame_id;
        if Self::send_rdpegfx_dvc_messages(sender, queued, codec_name, trace_name) {
            self.record_frame_queued(frame_id);
            true
        } else {
            false
        }
    }

    pub(in crate::egfx) fn queue_rdpegfx_frame(
        handle: &GfxServerHandle,
        trace_name: &'static str,
        queue_frame: impl FnOnce(&mut GraphicsPipelineServer) -> Option<u32>,
    ) -> Option<QueuedRdpegfxFrame> {
        let (frame_id, dvc_messages, channel_id) = {
            let Ok(mut server) = handle.lock() else {
                return None;
            };

            if !server.is_ready() {
                tracing::trace!("{trace_name}: server not ready");
                return None;
            }
            if server.should_backpressure() {
                tracing::trace!(
                    in_flight = server.frames_in_flight(),
                    "{trace_name}: backpressure"
                );
                return None;
            }

            let channel_id = match server.channel_id() {
                Some(id) => id,
                None => {
                    tracing::trace!("{trace_name}: no channel_id");
                    return None;
                }
            };

            let frame_id = queue_frame(&mut server)?;
            let dvc_messages = server.drain_output();
            (frame_id, dvc_messages, channel_id)
        };

        if dvc_messages.is_empty() {
            return None;
        }

        Some(QueuedRdpegfxFrame {
            frame_id,
            dvc_messages,
            channel_id,
        })
    }

    pub(in crate::egfx) fn send_rdpegfx_dvc_messages(
        sender: &mpsc::UnboundedSender<ServerEvent>,
        queued: QueuedRdpegfxFrame,
        codec_name: &'static str,
        trace_name: &'static str,
    ) -> bool {
        match ironrdp_dvc::encode_dvc_messages(
            queued.channel_id,
            queued.dvc_messages,
            ironrdp_svc::ChannelFlags::SHOW_PROTOCOL,
        ) {
            Ok(svc_messages) => {
                if sender
                    .send(ServerEvent::Egfx(EgfxServerMessage::SendMessages {
                        messages: svc_messages,
                    }))
                    .is_err()
                {
                    tracing::trace!("{trace_name}: EGFX event channel closed");
                    return false;
                }
            }
            Err(e) => {
                tracing::error!("Failed to encode EGFX {} frame: {}", codec_name, e);
                return false;
            }
        }

        true
    }
}
