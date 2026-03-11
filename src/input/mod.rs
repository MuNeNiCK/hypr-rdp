use ironrdp_server::{KeyboardEvent, MouseEvent, RdpServerInputHandler};

/// Handles RDP input events and injects them into Hyprland
/// via wlr-virtual-keyboard and wlr-virtual-pointer protocols.
pub struct HyprInputHandler {
    // TODO: Wayland virtual keyboard/pointer handles
}

impl HyprInputHandler {
    pub fn new() -> anyhow::Result<Self> {
        tracing::info!("Input handler initialized (stub)");
        Ok(Self {})
    }
}

impl RdpServerInputHandler for HyprInputHandler {
    fn keyboard(&mut self, event: KeyboardEvent) {
        tracing::debug!(?event, "keyboard event");
        // TODO: translate RDP scancode → Linux keycode and inject via wlr-virtual-keyboard
    }

    fn mouse(&mut self, event: MouseEvent) {
        tracing::debug!(?event, "mouse event");
        // TODO: inject via wlr-virtual-pointer
    }
}
