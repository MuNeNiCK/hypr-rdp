use ironrdp_server::{KeyboardEvent, MouseEvent, RdpServerInputHandler};

use super::actor::InputCommand;
use super::keyboard::{generate_xkb_keymap_from_names, xkb_names_for_rdp_keyboard_layout};
use super::wayland::HyprInputHandler;
use super::KeyboardLayoutPolicy;

impl RdpServerInputHandler for HyprInputHandler {
    fn client_keyboard_layout(&mut self, keyboard_layout: u32) {
        let Some(keymap_data) =
            client_keymap_from_keyboard_layout(self.keyboard_layout_policy, keyboard_layout)
        else {
            tracing::info!(
                keyboard_layout = %format_args!("{keyboard_layout:#010x}"),
                keyboard_layout_policy = ?self.keyboard_layout_policy,
                "Keeping existing keyboard keymap"
            );
            return;
        };

        tracing::info!(
            keyboard_layout = %format_args!("{keyboard_layout:#010x}"),
            "Applying client keyboard layout"
        );
        self.send_input_command(InputCommand::ApplyKeymap {
            keymap_data,
            keymap_source: "rdp-client",
        });
    }

    fn keyboard(&mut self, event: KeyboardEvent) {
        self.send_input_command(InputCommand::Keyboard(event));
    }

    fn mouse(&mut self, event: MouseEvent) {
        self.send_input_command(InputCommand::Mouse(event));
    }
}

fn client_keymap_from_keyboard_layout(
    keyboard_layout_policy: KeyboardLayoutPolicy,
    keyboard_layout: u32,
) -> Option<Vec<u8>> {
    if keyboard_layout_policy == KeyboardLayoutPolicy::Compositor {
        return None;
    }

    let names = xkb_names_for_rdp_keyboard_layout(keyboard_layout)?;
    match generate_xkb_keymap_from_names(&names) {
        Ok(keymap) => Some(keymap),
        Err(err) => {
            tracing::warn!(
                keyboard_layout = %format_args!("{keyboard_layout:#010x}"),
                layout = ?names.layout,
                variant = ?names.variant,
                options = ?names.options,
                "Failed to generate XKB keymap from client keyboard layout: {:#}",
                err
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::client_keymap_from_keyboard_layout;
    use crate::input::keyboard::KeyboardStateTracker;
    use crate::input::KeyboardLayoutPolicy;

    #[test]
    fn client_keyboard_layout_generates_non_us_keymap() {
        let keymap = client_keymap_from_keyboard_layout(KeyboardLayoutPolicy::Client, 0x00000407)
            .expect("German HKL is supported");
        let tracker = KeyboardStateTracker::new(&keymap).expect("generated keymap loads");

        assert_eq!(tracker.unicode_to_evdev('z' as u16).unwrap().evdev_key, 21);
        assert_eq!(tracker.unicode_to_evdev('y' as u16).unwrap().evdev_key, 44);
    }

    #[test]
    fn client_keyboard_layout_keeps_existing_keymap_when_unknown() {
        assert!(
            client_keymap_from_keyboard_layout(KeyboardLayoutPolicy::Client, 0x0000ffff,).is_none()
        );
    }

    #[test]
    fn compositor_keyboard_layout_policy_ignores_supported_client_layout() {
        assert!(
            client_keymap_from_keyboard_layout(KeyboardLayoutPolicy::Compositor, 0x00000407,)
                .is_none()
        );
    }
}
