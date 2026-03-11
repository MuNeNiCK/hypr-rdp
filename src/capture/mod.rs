use std::num::{NonZeroU16, NonZeroUsize};

use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use ironrdp_server::{
    BitmapUpdate, DesktopSize, DisplayUpdate, PixelFormat, RdpServerDisplay,
    RdpServerDisplayUpdates,
};
use tokio::sync::mpsc;

/// Captures frames from Hyprland via wlr-screencopy protocol.
pub struct HyprDisplay {
    width: u16,
    height: u16,
    update_tx: mpsc::Sender<DisplayUpdate>,
    update_rx: Option<mpsc::Receiver<DisplayUpdate>>,
}

impl HyprDisplay {
    pub async fn new() -> Result<Self> {
        // TODO: connect to Hyprland Wayland display and query output resolution
        let width = 1920;
        let height = 1080;

        let (tx, rx) = mpsc::channel(16);

        tracing::info!(width, height, "Display capture initialized (stub)");

        // Spawn a stub frame producer for testing
        let tx_clone = tx.clone();
        tokio::spawn(async move {
            stub_frame_producer(tx_clone, width, height).await;
        });

        Ok(Self {
            width,
            height,
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
        // Take the existing receiver or create a new channel
        let rx = match self.update_rx.take() {
            Some(rx) => rx,
            None => {
                let (tx, rx) = mpsc::channel(16);
                self.update_tx = tx.clone();
                let width = self.width;
                let height = self.height;
                tokio::spawn(async move {
                    stub_frame_producer(tx, width, height).await;
                });
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
        // mpsc::Receiver::recv() is cancellation-safe
        Ok(self.rx.recv().await)
    }
}

/// Stub: sends a solid-color test frame at ~1 FPS.
/// Will be replaced by actual wlr-screencopy capture.
async fn stub_frame_producer(tx: mpsc::Sender<DisplayUpdate>, width: u16, height: u16) {
    let mut frame_count: u8 = 0;
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        // Generate a simple gradient test pattern
        let stride = width as usize * 4;
        let mut data = vec![0u8; stride * height as usize];
        for y in 0..height as usize {
            for x in 0..width as usize {
                let offset = y * stride + x * 4;
                data[offset] = frame_count.wrapping_add(x as u8); // B
                data[offset + 1] = frame_count.wrapping_add(y as u8); // G
                data[offset + 2] = frame_count.wrapping_mul(3); // R
                data[offset + 3] = 255; // A
            }
        }
        frame_count = frame_count.wrapping_add(1);

        let update = DisplayUpdate::Bitmap(BitmapUpdate {
            x: 0,
            y: 0,
            width: NonZeroU16::new(width).unwrap(),
            height: NonZeroU16::new(height).unwrap(),
            format: PixelFormat::BgrA32,
            data: Bytes::from(data),
            stride: NonZeroUsize::new(stride).unwrap(),
        });

        if tx.send(update).await.is_err() {
            tracing::info!("Display update channel closed, stopping frame producer");
            break;
        }

        tracing::trace!(frame_count, "sent test frame");
    }
}
