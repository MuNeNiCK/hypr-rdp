use std::sync::{mpsc, Weak};

use ironrdp_server::{KeyboardEvent, MouseEvent, RdpServerInputHandler};

use super::actor::InputCommand;
use super::keyboard::{generate_xkb_keymap_from_names, xkb_names_for_rdp_keyboard_layout};
use super::wayland::HyprInputHandler;
use super::KeyboardLayoutPolicy;

impl RdpServerInputHandler for HyprInputHandler {
    fn keyboard(&mut self, event: KeyboardEvent) {
        self.send_input_command(InputCommand::Keyboard(event));
    }

    fn mouse(&mut self, event: MouseEvent) {
        self.send_input_command(InputCommand::Mouse(event));
    }
}

/// Owner-specific sink for client keyboard-layout metadata.
///
/// The server connection layer forwards `ConnectionInfo.keyboard_layout`
/// (MS-RDPBCGR 2.2.1.3.2 Client Core Data) here after authentication. The
/// input module owns the policy that turns the HKL into an XKB keymap
/// command; the connection layer must not depend on `InputCommand`.
pub(crate) trait ClientKeyboardLayoutSink: Send {
    fn set_keyboard_layout(&self, keyboard_layout: u32);

    /// The session ended: release anything the client left held. The input
    /// actor outlives a single connection, so its own shutdown release comes
    /// far too late for this. Default is a no-op so test sinks need not care.
    fn release_held_keys(&self) {}
}

/// Production `ClientKeyboardLayoutSink` owned by the input module: applies
/// the layout policy and enqueues an `ApplyKeymap` command on the input
/// actor's channel.
pub(crate) struct ClientKeyboardLayoutHandle {
    keyboard_layout_policy: KeyboardLayoutPolicy,
    commands: Weak<mpsc::Sender<InputCommand>>,
}

impl ClientKeyboardLayoutHandle {
    pub(super) fn new(
        keyboard_layout_policy: KeyboardLayoutPolicy,
        commands: Weak<mpsc::Sender<InputCommand>>,
    ) -> Self {
        Self {
            keyboard_layout_policy,
            commands,
        }
    }
}

impl ClientKeyboardLayoutSink for ClientKeyboardLayoutHandle {
    fn release_held_keys(&self) {
        let Some(commands) = self.commands.upgrade() else {
            return;
        };
        if commands.send(InputCommand::ReleaseHeldKeys).is_err() {
            tracing::warn!("Input actor is gone; keys held at session end stay held");
        }
    }

    fn set_keyboard_layout(&self, keyboard_layout: u32) {
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
        let Some(commands) = self.commands.upgrade() else {
            tracing::warn!("Input actor is gone; dropping keyboard layout command");
            return;
        };
        if commands
            .send(InputCommand::ApplyKeymap {
                keymap_data,
                keymap_source: "rdp-client",
            })
            .is_err()
        {
            tracing::warn!("Input actor is gone; dropping keyboard layout command");
        }
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
    use std::sync::{mpsc, Arc};
    use std::time::Duration;

    use super::{client_keymap_from_keyboard_layout, ClientKeyboardLayoutHandle};
    use crate::input::actor::InputCommand;
    use crate::input::keyboard::KeyboardStateTracker;
    use crate::input::rdp::ClientKeyboardLayoutSink;
    use crate::input::wayland::HyprInputHandler;
    use crate::input::KeyboardLayoutPolicy;
    use ironrdp_server::{KeyboardEvent, RdpServerInputHandler};

    #[test]
    fn keyboard_handler_enqueues_exact_event_order() {
        let (commands, receiver) = mpsc::channel();
        let mut handler = HyprInputHandler::test_handler_with_commands(Arc::new(commands));

        handler.keyboard(KeyboardEvent::Pressed {
            code: 0x5b,
            extended: true,
        });
        handler.keyboard(KeyboardEvent::Pressed {
            code: 0x5b,
            extended: true,
        });
        handler.keyboard(KeyboardEvent::Released {
            code: 0x5b,
            extended: true,
        });

        assert!(matches!(
            receiver
                .recv_timeout(Duration::from_secs(1))
                .expect("first"),
            InputCommand::Keyboard(KeyboardEvent::Pressed {
                code: 0x5b,
                extended: true
            })
        ));
        assert!(matches!(
            receiver
                .recv_timeout(Duration::from_secs(1))
                .expect("second"),
            InputCommand::Keyboard(KeyboardEvent::Pressed {
                code: 0x5b,
                extended: true
            })
        ));
        assert!(matches!(
            receiver
                .recv_timeout(Duration::from_secs(1))
                .expect("third"),
            InputCommand::Keyboard(KeyboardEvent::Released {
                code: 0x5b,
                extended: true
            })
        ));
        assert!(
            receiver.try_recv().is_err(),
            "no extra commands may be enqueued"
        );
    }

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

    #[test]
    fn client_keyboard_layout_handle_sends_release_held_keys() {
        let (commands, receiver) = mpsc::channel();
        let commands = Arc::new(commands);
        let handle = ClientKeyboardLayoutHandle::new(
            KeyboardLayoutPolicy::Client,
            Arc::downgrade(&commands),
        );

        handle.release_held_keys();

        // The trait supplies a do-nothing default, so an override that never
        // reached the channel would compile and look invoked from the
        // connection handler's side while releasing nothing at all.
        let command = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("the end of a session must reach the input actor");
        assert!(matches!(command, InputCommand::ReleaseHeldKeys));
    }

    #[test]
    fn release_held_keys_does_not_keep_the_input_actor_alive() {
        let (commands, receiver) = mpsc::channel();
        let commands = Arc::new(commands);
        let handle = ClientKeyboardLayoutHandle::new(
            KeyboardLayoutPolicy::Client,
            Arc::downgrade(&commands),
        );
        drop(commands);
        drop(receiver);

        handle.release_held_keys();
    }

    #[test]
    fn client_keyboard_layout_handle_sends_apply_keymap_for_supported_layout() {
        let (commands, receiver) = mpsc::channel();
        let commands = Arc::new(commands);
        let handle = ClientKeyboardLayoutHandle::new(
            KeyboardLayoutPolicy::Client,
            Arc::downgrade(&commands),
        );

        handle.set_keyboard_layout(0x00000407);

        let command = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("command");
        assert!(
            matches!(
                command,
                InputCommand::ApplyKeymap {
                    keymap_source: "rdp-client",
                    ..
                }
            ),
            "supported client HKL must enqueue an ApplyKeymap command"
        );
    }

    #[test]
    fn client_keyboard_layout_handle_keeps_existing_keymap_when_unknown() {
        let (commands, receiver) = mpsc::channel();
        let commands = Arc::new(commands);
        let handle = ClientKeyboardLayoutHandle::new(
            KeyboardLayoutPolicy::Client,
            Arc::downgrade(&commands),
        );

        handle.set_keyboard_layout(0x0000ffff);

        assert!(
            receiver.try_recv().is_err(),
            "unknown client HKL must not enqueue a keymap command"
        );
    }

    #[test]
    fn compositor_keyboard_layout_handle_ignores_supported_client_layout() {
        let (commands, receiver) = mpsc::channel();
        let commands = Arc::new(commands);
        let handle = ClientKeyboardLayoutHandle::new(
            KeyboardLayoutPolicy::Compositor,
            Arc::downgrade(&commands),
        );

        handle.set_keyboard_layout(0x00000407);

        assert!(
            receiver.try_recv().is_err(),
            "compositor policy must not enqueue a keymap command"
        );
    }

    #[test]
    fn client_keyboard_layout_handle_does_not_keep_input_actor_alive() {
        let (commands, receiver) = mpsc::channel();
        let commands = Arc::new(commands);
        let handle = ClientKeyboardLayoutHandle::new(
            KeyboardLayoutPolicy::Client,
            Arc::downgrade(&commands),
        );

        drop(commands);

        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
        handle.set_keyboard_layout(0x00000407);
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
    }
}
