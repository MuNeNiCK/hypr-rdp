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
}

impl HyprDisplay {
    pub fn dimensions(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    pub async fn new(
        resolution: (u32, u32),
        capture_mode: CaptureMode,
        egfx_shared: Arc<EgfxShared>,
        output_layout: Arc<SharedOutputLayout>,
    ) -> Result<Self> {
        let (tx, rx) = mpsc::channel(128);

        let capture_info = wayland::start_capture(
            tx.clone(),
            resolution,
            capture_mode,
            Some(Arc::clone(&egfx_shared)),
            Arc::clone(&output_layout),
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
        let rx = match self.update_rx.take() {
            Some(rx) => rx,
            None => {
                let (tx, rx) = mpsc::channel(128);
                self.update_tx = tx.clone();
                let capture_info = wayland::start_capture(
                    tx,
                    self.resolution,
                    self.capture_mode,
                    self.egfx_shared.clone(),
                    Arc::clone(&self.output_layout),
                )
                .await?;
                self.width = capture_info.width as u16;
                self.height = capture_info.height as u16;
                self.output_name = capture_info.output_name;
                tracing::info!(
                    width = self.width,
                    height = self.height,
                    output = %self.output_name,
                    "Restarted display capture"
                );
                rx
            }
        };

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
