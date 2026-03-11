mod wayland;

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use ironrdp_server::{
    DesktopSize, DisplayUpdate, RdpServerDisplay, RdpServerDisplayUpdates,
};
use tokio::sync::mpsc;

use crate::egfx::EgfxShared;

/// Captures frames from Hyprland via ext-image-copy-capture-v1 protocol.
pub struct HyprDisplay {
    width: u16,
    height: u16,
    resolution: (u32, u32),
    output_name: String,
    egfx_shared: Option<Arc<EgfxShared>>,
    update_tx: mpsc::Sender<DisplayUpdate>,
    update_rx: Option<mpsc::Receiver<DisplayUpdate>>,
}

impl HyprDisplay {
    pub fn dimensions(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    pub fn output_name(&self) -> &str {
        &self.output_name
    }

    pub async fn new(resolution: (u32, u32), egfx_shared: Arc<EgfxShared>) -> Result<Self> {
        let (tx, rx) = mpsc::channel(128);

        let capture_info = wayland::start_capture(tx.clone(), resolution, Some(Arc::clone(&egfx_shared))).await?;

        tracing::info!(
            width = capture_info.width,
            height = capture_info.height,
            "Display capture initialized via ext-image-copy-capture-v1"
        );

        Ok(Self {
            width: capture_info.width as u16,
            height: capture_info.height as u16,
            resolution,
            output_name: capture_info.output_name,
            egfx_shared: Some(egfx_shared),
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
                let width = self.width;
                let height = self.height;
                // Restart capture
                wayland::start_capture(tx, self.resolution, self.egfx_shared.clone()).await?;
                tracing::info!(width, height, "Restarted display capture");
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
