use std::sync::atomic::Ordering;

use ironrdp_server::{EgfxServerMessage, GfxServerHandle, ServerEvent};
use tokio::sync::mpsc;

use super::{EgfxFrameReadiness, EgfxShared};

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
}
