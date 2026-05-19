use ironrdp_server::{KeyboardEvent, MouseEvent, RdpServerInputHandler};
use wayland_client::protocol::wl_pointer::{Axis, AxisSource, ButtonState};

use super::keymap;
use super::wayland::HyprInputHandler;

impl RdpServerInputHandler for HyprInputHandler {
    fn keyboard(&mut self, event: KeyboardEvent) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };

        let t = state.timestamp();
        match event {
            KeyboardEvent::Pressed { code, extended } => {
                if let Some(evdev_key) = keymap::xt_to_evdev(code, extended) {
                    state.vk.key(t, evdev_key, 1);
                    state.keyboard_state.key(evdev_key, true);
                    state.keyboard_state.send_modifiers(&state.vk);
                    state.flush();
                } else {
                    tracing::trace!(code, extended, "No evdev mapping for scancode");
                }
            }
            KeyboardEvent::Released { code, extended } => {
                if let Some(evdev_key) = keymap::xt_to_evdev(code, extended) {
                    state.vk.key(t, evdev_key, 0);
                    state.keyboard_state.key(evdev_key, false);
                    state.keyboard_state.send_modifiers(&state.vk);
                    state.flush();
                }
            }
            KeyboardEvent::Synchronize(flags) => {
                state.keyboard_state.synchronize_locks(flags);
                state.keyboard_state.send_modifiers(&state.vk);
                state.flush();
            }
            KeyboardEvent::UnicodePressed(code_point) => {
                if let Some(mapping) = state.keyboard_state.unicode_to_evdev(code_point) {
                    if mapping.needs_shift {
                        // 42 = KEY_LEFTSHIFT
                        state.vk.key(t, 42, 1);
                        state.keyboard_state.key(42, true);
                        state.keyboard_state.send_modifiers(&state.vk);
                    }
                    state.vk.key(t, mapping.evdev_key, 1);
                    state.flush();
                } else {
                    tracing::trace!(code_point, "No evdev mapping for Unicode character");
                }
            }
            KeyboardEvent::UnicodeReleased(code_point) => {
                if let Some(mapping) = state.keyboard_state.unicode_to_evdev(code_point) {
                    state.vk.key(t, mapping.evdev_key, 0);
                    if mapping.needs_shift {
                        state.vk.key(t, 42, 0);
                        state.keyboard_state.key(42, false);
                        state.keyboard_state.send_modifiers(&state.vk);
                    }
                    state.flush();
                }
            }
        }
    }

    fn mouse(&mut self, event: MouseEvent) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let t = state.timestamp();

        match event {
            MouseEvent::Move { x, y } => {
                // Pointer is bound to the output via create_virtual_pointer_with_output,
                // so coordinates are mapped within that output by the compositor.
                // Use the current output dimensions as extent (updates on resize).
                let Some(layout) = state.output_layout.snapshot() else {
                    return;
                };
                state
                    .vp
                    .motion_absolute(t, x as u32, y as u32, layout.output_w, layout.output_h);
                state.vp.frame();
                state.flush();
            }
            MouseEvent::LeftPressed => {
                state.vp.button(t, keymap::BTN_LEFT, ButtonState::Pressed);
                state.vp.frame();
                state.flush();
            }
            MouseEvent::LeftReleased => {
                state.vp.button(t, keymap::BTN_LEFT, ButtonState::Released);
                state.vp.frame();
                state.flush();
            }
            MouseEvent::RightPressed => {
                state.vp.button(t, keymap::BTN_RIGHT, ButtonState::Pressed);
                state.vp.frame();
                state.flush();
            }
            MouseEvent::RightReleased => {
                state.vp.button(t, keymap::BTN_RIGHT, ButtonState::Released);
                state.vp.frame();
                state.flush();
            }
            MouseEvent::MiddlePressed => {
                state.vp.button(t, keymap::BTN_MIDDLE, ButtonState::Pressed);
                state.vp.frame();
                state.flush();
            }
            MouseEvent::MiddleReleased => {
                state
                    .vp
                    .button(t, keymap::BTN_MIDDLE, ButtonState::Released);
                state.vp.frame();
                state.flush();
            }
            MouseEvent::Button4Pressed => {
                state.vp.button(t, keymap::BTN_SIDE, ButtonState::Pressed);
                state.vp.frame();
                state.flush();
            }
            MouseEvent::Button4Released => {
                state.vp.button(t, keymap::BTN_SIDE, ButtonState::Released);
                state.vp.frame();
                state.flush();
            }
            MouseEvent::Button5Pressed => {
                state.vp.button(t, keymap::BTN_EXTRA, ButtonState::Pressed);
                state.vp.frame();
                state.flush();
            }
            MouseEvent::Button5Released => {
                state.vp.button(t, keymap::BTN_EXTRA, ButtonState::Released);
                state.vp.frame();
                state.flush();
            }
            MouseEvent::VerticalScroll { value } => {
                // Negate: RDP positive=up, Wayland positive=down
                let discrete = -((value as f64 / 120.0).round() as i32);
                let continuous = discrete as f64 * 15.0;
                state.vp.axis_source(AxisSource::Wheel);
                state
                    .vp
                    .axis_discrete(t, Axis::VerticalScroll, continuous, discrete);
                state.vp.frame();
                state.flush();
            }
            MouseEvent::Scroll { x, y } => {
                state.vp.axis_source(AxisSource::Wheel);
                if y != 0 {
                    let discrete = -((y as f64 / 120.0).round() as i32);
                    let continuous = discrete as f64 * 15.0;
                    state
                        .vp
                        .axis_discrete(t, Axis::VerticalScroll, continuous, discrete);
                }
                if x != 0 {
                    let discrete = -((x as f64 / 120.0).round() as i32);
                    let continuous = discrete as f64 * 15.0;
                    state
                        .vp
                        .axis_discrete(t, Axis::HorizontalScroll, continuous, discrete);
                }
                state.vp.frame();
                state.flush();
            }
            MouseEvent::RelMove { x, y } => {
                state.vp.motion(t, x as f64, y as f64);
                state.vp.frame();
                state.flush();
            }
        }
    }
}
