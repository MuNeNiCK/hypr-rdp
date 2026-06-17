use ironrdp_server::{ClientKeyboardData, KeyboardEvent, MouseEvent, RdpServerInputHandler};
use wayland_client::protocol::wl_pointer::{Axis, AxisSource, ButtonState};

use super::keyboard::{generate_xkb_keymap_from_names, xkb_names_for_rdp_keyboard_layout};
use super::layout::OutputLayoutSnapshot;
use super::wayland::HyprInputHandler;
use super::{keymap, wayland::InputState, KeyboardLayoutPolicy};

impl RdpServerInputHandler for HyprInputHandler {
    fn client_keyboard_data(&mut self, keyboard_data: ClientKeyboardData) {
        let Some(keymap_data) = client_keymap_from_keyboard_layout(
            self.keyboard_layout_policy,
            keyboard_data.keyboard_layout,
        ) else {
            tracing::info!(
                keyboard_layout = %format_args!("{:#010x}", keyboard_data.keyboard_layout),
                keyboard_type = ?keyboard_data.keyboard_type,
                keyboard_subtype = keyboard_data.keyboard_subtype,
                keyboard_layout_policy = ?self.keyboard_layout_policy,
                "Keeping existing keyboard keymap"
            );
            return;
        };

        let Ok(mut state) = self.state.lock() else {
            return;
        };

        apply_client_keymap(&mut state, keymap_data, keyboard_data);
    }

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
                let (source_x, source_y) = map_rdp_pointer_to_source(&layout, x, y);
                state
                    .vp
                    .motion_absolute(t, source_x, source_y, layout.output_w, layout.output_h);
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

fn map_rdp_pointer_to_source(layout: &OutputLayoutSnapshot, x: u16, y: u16) -> (u32, u32) {
    layout
        .presentation_geometry
        .map_presentation_point_to_source(u32::from(x), u32::from(y))
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

fn apply_client_keymap(
    state: &mut InputState,
    keymap_data: Vec<u8>,
    keyboard_data: ClientKeyboardData,
) {
    if let Err(err) = state.apply_keymap(keymap_data, "rdp-client") {
        tracing::warn!(
            keyboard_layout = %format_args!("{:#010x}", keyboard_data.keyboard_layout),
            keyboard_type = ?keyboard_data.keyboard_type,
            keyboard_subtype = keyboard_data.keyboard_subtype,
            "Failed to apply client keyboard keymap: {:#}",
            err
        );
    } else {
        tracing::info!(
            keyboard_layout = %format_args!("{:#010x}", keyboard_data.keyboard_layout),
            keyboard_type = ?keyboard_data.keyboard_type,
            keyboard_subtype = keyboard_data.keyboard_subtype,
            keyboard_functional_keys_count = keyboard_data.keyboard_functional_keys_count,
            "Applied client keyboard layout"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{client_keymap_from_keyboard_layout, map_rdp_pointer_to_source};
    use crate::display::geometry::{PresentationGeometry, Size};
    use crate::input::keyboard::KeyboardStateTracker;
    use crate::input::KeyboardLayoutPolicy;
    use crate::input::OutputLayoutSnapshot;

    fn layout_snapshot(source: (u32, u32), presentation: (u32, u32)) -> OutputLayoutSnapshot {
        let source_size = Size::new(source.0, source.1).unwrap();
        let presentation_size = Size::new(presentation.0, presentation.1).unwrap();
        OutputLayoutSnapshot {
            output_name: "DP-1".into(),
            output_w: source.0,
            output_h: source.1,
            layout_extent_w: source.0,
            layout_extent_h: source.1,
            output_offset_x: 0,
            output_offset_y: 0,
            presentation_geometry: PresentationGeometry::new(source_size, presentation_size),
            geometry_generation: 0,
        }
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
    fn rdp_pointer_mapping_uses_source_coordinates_for_scaled_output() {
        let layout = layout_snapshot((3840, 2160), (1920, 1080));

        assert_eq!(map_rdp_pointer_to_source(&layout, 960, 540), (1920, 1080));
        assert_eq!(map_rdp_pointer_to_source(&layout, 1919, 1079), (3839, 2159));
    }

    #[test]
    fn rdp_pointer_mapping_clamps_fallback_letterbox_bars_to_source_edges() {
        let layout = layout_snapshot((1920, 1080), (1024, 768));

        assert_eq!(map_rdp_pointer_to_source(&layout, 512, 0).1, 0);
        assert_eq!(map_rdp_pointer_to_source(&layout, 512, 767).1, 1079);
    }
}
