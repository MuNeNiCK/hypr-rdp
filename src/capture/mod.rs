mod wayland;

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use ironrdp_server::{DesktopSize, DisplayUpdate, RdpServerDisplay, RdpServerDisplayUpdates};
use tokio::sync::mpsc;

use crate::egfx::EgfxShared;
use crate::input::SharedOutputLayout;

#[derive(Clone, Copy, Debug)]
pub enum CaptureMode {
    /// ext-image-copy-capture-v1
    Ext,
    /// wlr-screencopy-v1
    Wlr,
}

/// Captures frames from Hyprland via Wayland capture protocols.
pub struct HyprDisplay {
    width: u16,
    height: u16,
    resolution: (u32, u32),
    capture_mode: CaptureMode,
    output_name: String,
    egfx_shared: Option<Arc<EgfxShared>>,
    output_layout: Arc<SharedOutputLayout>,
    update_tx: mpsc::Sender<DisplayUpdate>,
    update_rx: Option<mpsc::Receiver<DisplayUpdate>>,
    bitrate: u32,
    quality: u8,
    fps: u32,
    output: Option<String>,
}

impl HyprDisplay {
    pub fn dimensions(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        resolution: (u32, u32),
        capture_mode: CaptureMode,
        egfx_shared: Arc<EgfxShared>,
        output_layout: Arc<SharedOutputLayout>,
        bitrate: u32,
        quality: u8,
        fps: u32,
        output: Option<String>,
    ) -> Result<Self> {
        let (tx, rx) = mpsc::channel(128);

        // Initial capture: bitmap-only (no EGFX). The capture thread started
        // here is replaced by updates() when a client connects. Giving it EGFX
        // access would let it consume frame-tracker slots that can never be ACK'd,
        // blocking the real capture thread's EGFX frames.
        let capture_info = wayland::start_capture(
            tx.clone(),
            resolution,
            capture_mode,
            None,
            Arc::clone(&output_layout),
            bitrate,
            quality,
            fps,
            output.clone(),
        )
        .await?;

        let protocol_name = match capture_mode {
            CaptureMode::Ext => "ext-image-copy-capture-v1",
            CaptureMode::Wlr => "wlr-screencopy-v1",
        };
        tracing::info!(
            width = capture_info.width,
            height = capture_info.height,
            "Display capture initialized via {}", protocol_name
        );

        Ok(Self {
            width: capture_info.width as u16,
            height: capture_info.height as u16,
            resolution,
            capture_mode,
            output_name: capture_info.output_name,
            egfx_shared: Some(egfx_shared),
            output_layout,
            update_tx: tx,
            update_rx: Some(rx),
            bitrate,
            quality,
            fps,
            output,
        })
    }
}

#[async_trait]
impl RdpServerDisplay for HyprDisplay {
    async fn size(&mut self) -> DesktopSize {
        DesktopSize {
            width: self.width,
            height: self.height,
        }
    }

    async fn updates(&mut self) -> Result<Box<dyn RdpServerDisplayUpdates>> {
        // Always start a fresh capture thread. On first connection, the capture
        // thread from new() has been filling the channel with stale bitmap frames
        // since server startup — feeding those to the client causes rendering
        // glitches during EGFX negotiation. Dropping the old rx causes the old
        // capture thread to exit on its next send().
        drop(self.update_rx.take());

        // Reset EGFX readiness so the new capture thread waits for this
        // connection's on_ready callback instead of using stale state.
        if let Some(ref shared) = self.egfx_shared {
            shared.reset_for_new_client();
        }

        let (tx, rx) = mpsc::channel(128);
        self.update_tx = tx.clone();
        let capture_info = wayland::start_capture(
            tx,
            self.resolution,
            self.capture_mode,
            self.egfx_shared.clone(),
            Arc::clone(&self.output_layout),
            self.bitrate,
            self.quality,
            self.fps,
            self.output.clone(),
        )
        .await?;
        self.width = capture_info.width as u16;
        self.height = capture_info.height as u16;
        self.output_name = capture_info.output_name;

        Ok(Box::new(HyprDisplayUpdates { rx }))
    }
}

struct HyprDisplayUpdates {
    rx: mpsc::Receiver<DisplayUpdate>,
}

#[async_trait]
impl RdpServerDisplayUpdates for HyprDisplayUpdates {
    async fn next_update(&mut self) -> Result<Option<DisplayUpdate>> {
        Ok(self.rx.recv().await)
    }
}
