// Generated bindings for zwp_virtual_keyboard_unstable_v1 protocol

pub mod generated {
    #![allow(unused)]

    // The generate_interfaces! macro expects these in scope
    use wayland_client::backend as wayland_backend;
    use wayland_client::protocol::__interfaces::*;

    wayland_scanner::generate_interfaces!("protocols/virtual-keyboard-unstable-v1.xml");

    pub mod client {
        // The generate_client_code! macro expects these in scope
        use wayland_client;
        use wayland_client::protocol::*;

        // Also need parent module's interfaces
        use super::*;

        wayland_scanner::generate_client_code!("protocols/virtual-keyboard-unstable-v1.xml");
    }
}

pub use generated::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1;
pub use generated::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1;
