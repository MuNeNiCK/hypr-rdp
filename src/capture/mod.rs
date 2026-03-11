mod wayland;

use anyhow::Result;
use async_trait::async_trait;
use ironrdp_server::{
    DesktopSize, DisplayUpdate, RdpServerDisplay, RdpServerDisplayUpdates,
};
use tokio::sync::mpsc;

/// Captures frames from Hyprland via ext-image-copy-capture-v1 protocol.
pub struct HyprDisplay {
    width: u16,
    height: u16,
    update_tx: mpsc::Sender<DisplayUpdate>,
    update_rx: Option<mpsc::Receiver<DisplayUpdate>>,
}

impl HyprDisplay {
    pub async fn new() -> Result<Self> {
        let (tx, rx) = mpsc::channel(4);

        let capture_info = wayland::start_capture(tx.clone()).await?;

        tracing::info!(
            width = capture_info.width,
            height = capture_info.height,
            "Display capture initialized via ext-image-copy-capture-v1"
        );

        Ok(Self {
            width: capture_info.width as u16,
            height: capture_info.height as u16,
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
                let (tx, rx) = mpsc::channel(4);
                self.update_tx = tx.clone();
                let width = self.width;
                let height = self.height;
                // Restart capture
                wayland::start_capture(tx).await?;
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
