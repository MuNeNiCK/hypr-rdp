#[cfg(feature = "vaapi")]
pub mod dmabuf;
mod wayland;

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use ironrdp_displaycontrol::pdu::DisplayControlMonitorLayout;
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
    pending_resize: bool,
    deferred_resize: Option<(u32, u32)>,
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
            None,
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
            pending_resize: false,
            deferred_resize: None,
        })
    }
}

#[async_trait]
impl RdpServerDisplay for HyprDisplay {
    async fn size(&mut self) -> DesktopSize {
        DesktopSize { width: self.width, height: self.height }
    }

    async fn request_initial_size(&mut self, client_size: DesktopSize) -> DesktopSize {
        let cw = client_size.width as u32;
        let ch = client_size.height as u32;

        if self.output.is_none() && (cw != self.resolution.0 || ch != self.resolution.1) {
            tracing::info!(client_w = cw, client_h = ch, "Deferring resize to match client");
            self.deferred_resize = Some((cw, ch));
        }

        self.size().await
    }

    fn request_layout(&mut self, layout: DisplayControlMonitorLayout) {
        let monitor = match layout.monitors().iter().find(|m| m.is_primary()) {
            Some(m) => m,
            None => match layout.monitors().first() {
                Some(m) => m,
                None => return,
            },
        };

        let (w, h) = monitor.dimensions();

        if (w == self.resolution.0 && h == self.resolution.1) || self.output.is_some() {
            return;
        }

        tracing::info!(w, h, "Client requested resize via DisplayControl");

        self.resolution = (w, h);
        self.width = w as u16;
        self.height = h as u16;
        self.pending_resize = true;

        let _ = self.update_tx.try_send(DisplayUpdate::Resize(DesktopSize {
            width: w as u16,
            height: h as u16,
        }));
    }

    async fn updates(&mut self) -> Result<Box<dyn RdpServerDisplayUpdates>> {
        drop(self.update_rx.take());

        if let Some(ref shared) = self.egfx_shared {
            if self.pending_resize {
                shared.prepare_for_resize(self.width, self.height);
                self.pending_resize = false;
            } else {
                shared.reset_for_new_client();
            }
        }

        let (tx, rx) = mpsc::channel(128);
        self.update_tx = tx.clone();

        let deferred = if let Some((w, h)) = self.deferred_resize.take() {
            self.resolution = (w, h);
            self.width = w as u16;
            self.height = h as u16;
            self.pending_resize = true;
            Some(DesktopSize { width: w as u16, height: h as u16 })
        } else {
            None
        };

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
            deferred,
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
