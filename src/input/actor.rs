//! Input actor.
//!
//! `xkb::State` is `!Send`, so the keyboard state lives on one owning
//! thread. Everything that touches input — keyboard and mouse events, RDP
//! client keymaps, external layout switches — is an [`InputCommand`] sent to
//! that thread, which keeps one total order across devices: a Ctrl press
//! enqueued before a click reaches the compositor before that click. The
//! virtual-keyboard requests derived from the state are emitted in command
//! order through a [`KeyboardSink`].

use std::sync::mpsc;
use std::time::{Duration, Instant};

use ironrdp_server::{KeyboardEvent, MouseEvent};

use super::keyboard::{KeyboardModifierState, KeyboardStateTracker};
use super::keymap;

/// How long the actor waits for a command before servicing the transport.
const SOCKET_PUMP_INTERVAL: Duration = Duration::from_secs(1);
const LEFT_SHIFT_KEY: u32 = 42;

/// Ordered initial-sync candidates from a compositor devices query:
/// the main physical keyboard, if any, plus the remaining physical
/// keyboards in enumeration order, as `(device, layout)` pairs.
pub(super) struct InitialLayoutCandidates {
    pub(super) main: Option<(String, String)>,
    pub(super) others: Vec<(String, String)>,
}

pub(super) enum InputCommand {
    /// A keyboard event forwarded from the RDP input handler.
    Keyboard(KeyboardEvent),
    /// A mouse event forwarded from the RDP input handler.
    Mouse(MouseEvent),
    /// The session ended: release anything the client left held. The actor
    /// outlives a single connection, so nothing else would.
    ReleaseHeldKeys,
    /// Replace the active keymap (client keyboard layout policy).
    ApplyKeymap {
        keymap_data: Vec<u8>,
        keymap_source: &'static str,
    },
    /// Lock the layout group resolved from an XKB layout display name
    /// (external layout switch reported by the compositor). Events from our
    /// own virtual keyboard are feedback about the replica's device: they
    /// never change the replica, and a divergent one triggers a re-announce
    /// of the replica state.
    SetLockedLayout {
        layout_name: String,
        from_own_keyboard: bool,
    },
    /// Seed the layout group when the listener (re)connects. activelayout
    /// only fires on switches, so the initial state comes from a devices
    /// query.
    SetInitialLayout { candidates: InitialLayoutCandidates },
}

#[derive(Default)]
struct DepressedKeys {
    plain: Vec<u32>,
    modifiers: Vec<u32>,
    unicode_shift_owners: Vec<u16>,
}

impl DepressedKeys {
    fn is_empty(&self) -> bool {
        self.plain.is_empty() && self.modifiers.is_empty() && self.unicode_shift_owners.is_empty()
    }

    fn holds_modifier(&self, evdev_key: u32) -> bool {
        self.modifiers.contains(&evdev_key)
    }

    fn press_unicode_shift(&mut self, code_point: u16) {
        if !self.unicode_shift_owners.contains(&code_point) {
            self.unicode_shift_owners.push(code_point);
        }
    }

    fn release_unicode_shift(&mut self, code_point: u16) -> bool {
        let previous_len = self.unicode_shift_owners.len();
        self.unicode_shift_owners
            .retain(|&owner| owner != code_point);
        self.unicode_shift_owners.len() != previous_len
    }

    fn press(&mut self, evdev_key: u32, modifier: bool) {
        let keys = if modifier {
            &mut self.modifiers
        } else {
            &mut self.plain
        };
        if !keys.contains(&evdev_key) {
            keys.push(evdev_key);
        }
    }

    fn release(&mut self, evdev_key: u32, modifier: bool) {
        let keys = if modifier {
            &mut self.modifiers
        } else {
            &mut self.plain
        };
        keys.retain(|&key| key != evdev_key);
    }

    fn release_all(&mut self) -> impl Iterator<Item = u32> + '_ {
        self.plain
            .drain(..)
            .rev()
            .chain(self.modifiers.drain(..).rev())
    }
}

/// Where the actor's virtual-keyboard requests go. The production sink wraps
/// `zwp_virtual_keyboard_v1`; tests record the calls.
pub(super) trait KeyboardSink: Send {
    fn key(&mut self, time: u32, evdev_key: u32, pressed: bool);
    fn modifiers(&mut self, state: KeyboardModifierState);
    /// Announce a keymap. Returns false when the announcement could not be
    /// made, in which case the device keeps its previous keymap.
    fn keymap(&mut self, keymap_data: &[u8]) -> bool;
    fn flush(&mut self);
}

/// Full input backend: keyboard requests plus mouse event execution.
pub(super) trait InputBackend: KeyboardSink {
    fn mouse(&mut self, time: u32, event: MouseEvent);
    /// Service the underlying transport while idle: read and dispatch
    /// whatever the compositor sent, and flush our side.
    fn pump(&mut self) {}
}

/// Run the actor until every command sender is dropped. Keyboard and mouse
/// commands execute on this thread in arrival order, so cross-device
/// sequences like Ctrl+Click reach the compositor in the order the client
/// sent them.
pub(super) fn run_input_actor(
    commands: mpsc::Receiver<InputCommand>,
    initial_keymap: Vec<u8>,
    keymap_source: &'static str,
    epoch: Instant,
    mut backend: impl InputBackend,
) {
    let mut tracker = match KeyboardStateTracker::new(&initial_keymap) {
        Ok(tracker) => tracker,
        Err(err) => {
            tracing::error!("Input actor failed to load keymap: {:#}", err);
            return;
        }
    };

    let mut keymap_data = initial_keymap;
    let mut depressed = DepressedKeys::default();
    if !backend.keymap(&keymap_data) {
        // Without a device keymap every further request is a protocol
        // error; stop loudly instead of running dead.
        tracing::error!("Failed to announce the initial keymap; input actor is stopping");
        return;
    }
    backend.modifiers(tracker.modifier_state());
    backend.flush();
    if !tracker.supports_locked_layout() {
        tracing::warn!(
            "libxkbcommon lacks xkb_state_update_latched_locked (< 1.10); \
             external layout switches will not be mirrored"
        );
    }
    tracing::info!(
        len = keymap_data.len(),
        keymap_source,
        "Input actor started"
    );

    loop {
        match commands.recv_timeout(SOCKET_PUMP_INTERVAL) {
            Ok(command) => {
                handle_command(
                    &mut tracker,
                    &mut keymap_data,
                    &mut depressed,
                    &mut backend,
                    &epoch,
                    command,
                );
                // A continuously busy command queue must not starve incoming
                // Wayland traffic. `pump` is non-blocking when the socket has
                // no data, so service it after every command as well as while
                // idle.
                backend.pump();
            }
            // Nothing else reads the transport; service it while idle so
            // compositor events do not accumulate unread.
            Err(mpsc::RecvTimeoutError::Timeout) => backend.pump(),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    release_held_keys(
        &mut tracker,
        &mut depressed,
        &mut backend,
        epoch.elapsed().as_millis() as u32,
        "shutdown",
    );

    tracing::debug!("Input actor stopped");
}

fn handle_command(
    tracker: &mut KeyboardStateTracker,
    keymap_data: &mut Vec<u8>,
    depressed: &mut DepressedKeys,
    sink: &mut impl InputBackend,
    epoch: &Instant,
    command: InputCommand,
) {
    let t = epoch.elapsed().as_millis() as u32;

    match command {
        InputCommand::ReleaseHeldKeys => {
            release_held_keys(tracker, depressed, sink, t, "session-end")
        }
        InputCommand::Keyboard(event) => handle_rdp_event(tracker, depressed, sink, t, event),
        InputCommand::Mouse(event) => sink.mouse(t, event),
        InputCommand::ApplyKeymap {
            keymap_data: new_keymap,
            keymap_source,
        } => match KeyboardStateTracker::new(&new_keymap) {
            Ok(new_tracker) => {
                // Announce first: the replica must not switch unless the
                // device accepted the keymap.
                if !sink.keymap(&new_keymap) {
                    tracing::warn!(keymap_source, "Keeping previous keymap");
                    return;
                }
                release_depressed(tracker, depressed, sink, t);
                *tracker = new_tracker;
                *keymap_data = new_keymap;
                sink.modifiers(tracker.modifier_state());
                sink.flush();
                tracing::info!(
                    len = keymap_data.len(),
                    keymap_source,
                    "Applied keyboard keymap"
                );
            }
            Err(err) => {
                tracing::warn!("Failed to apply keyboard keymap: {:#}", err);
            }
        },
        InputCommand::SetLockedLayout {
            layout_name,
            from_own_keyboard,
        } => {
            if from_own_keyboard {
                handle_own_layout_event(tracker, keymap_data, sink, &layout_name);
                return;
            }
            let Some(group) = tracker.layout_index_by_name(&layout_name) else {
                tracing::debug!(layout_name, "Layout name not present in keymap");
                return;
            };
            if tracker.set_locked_group(group) {
                sink.modifiers(tracker.modifier_state());
                sink.flush();
                tracing::info!(group, "Switched virtual keyboard layout group");
            }
        }
        InputCommand::SetInitialLayout { candidates } => {
            let Some((device, group)) = pick_initial_layout(tracker, &candidates) else {
                tracing::debug!("No initial layout candidate resolves in the keymap");
                return;
            };
            if tracker.set_locked_group(group) {
                sink.modifiers(tracker.modifier_state());
                sink.flush();
                tracing::info!(device, group, "Seeded virtual keyboard layout group");
            }
        }
    }
}

/// Feedback about our own device, not input. Hyprland emits activelayout
/// for every group change of ours, and it also resets a device's xkb state
/// out of band (keyboard config applied to a fresh device; every keymap
/// upload recreates the state), so an event can describe a state older
/// than our latest announcements. The replica therefore never follows
/// these events: a matching one is dropped, and a divergent one
/// re-announces the replica — announcing the replica converges, since a
/// repeat announcement changes nothing device-side and emits no further
/// event, while announcing the event's value feeds the stream that
/// produced it and oscillates. An unresolvable layout name means the
/// device keymap itself was replaced, so the replica keymap is
/// re-announced the same way.
fn handle_own_layout_event(
    tracker: &KeyboardStateTracker,
    keymap_data: &[u8],
    sink: &mut impl KeyboardSink,
    layout_name: &str,
) {
    let replica_group = tracker.modifier_state().group;
    match tracker.layout_index_by_name(layout_name) {
        Some(group) if group == replica_group => {}
        Some(group) => {
            sink.modifiers(tracker.modifier_state());
            sink.flush();
            tracing::debug!(
                group,
                replica_group,
                "Re-asserted replica state over divergent own-device layout event"
            );
        }
        None => {
            if sink.keymap(keymap_data) {
                sink.modifiers(tracker.modifier_state());
            }
            sink.flush();
            tracing::warn!(
                layout_name,
                "Re-announced replica keymap over replaced own-device keymap"
            );
        }
    }
}

/// The main physical keyboard's state is authoritative when present.
/// Without one, prefer a candidate naming a non-default group: devices
/// that never see the group toggle keys (power buttons, mouse keyboard
/// endpoints) sit on the default group forever, so a non-default group is
/// evidence of an actual switch.
fn pick_initial_layout<'a>(
    tracker: &KeyboardStateTracker,
    candidates: &'a InitialLayoutCandidates,
) -> Option<(&'a str, u32)> {
    if let Some((device, layout)) = &candidates.main {
        if let Some(group) = tracker.layout_index_by_name(layout) {
            return Some((device.as_str(), group));
        }
    }
    let resolved: Vec<(&str, u32)> = candidates
        .others
        .iter()
        .filter_map(|(device, layout)| {
            tracker
                .layout_index_by_name(layout)
                .map(|group| (device.as_str(), group))
        })
        .collect();
    resolved
        .iter()
        .find(|(_, group)| *group != 0)
        .or_else(|| resolved.first())
        .copied()
}

fn handle_rdp_event(
    tracker: &mut KeyboardStateTracker,
    depressed: &mut DepressedKeys,
    sink: &mut impl KeyboardSink,
    t: u32,
    event: KeyboardEvent,
) {
    match event {
        KeyboardEvent::Pressed { code, extended } => {
            if let Some(evdev_key) = keymap::xt_to_evdev(code, extended) {
                let modifier = is_modifier(evdev_key);
                if modifier && depressed.holds_modifier(evdev_key) {
                    tracing::trace!(evdev_key, "Suppressed duplicate modifier press");
                    return;
                }
                let already_active =
                    evdev_key == LEFT_SHIFT_KEY && !depressed.unicode_shift_owners.is_empty();
                depressed.press(evdev_key, modifier);
                if already_active {
                    return;
                }
                sink.key(t, evdev_key, true);
                if tracker.key(evdev_key, true) {
                    sink.modifiers(tracker.modifier_state());
                }
                sink.flush();
            } else {
                tracing::trace!(code, extended, "No evdev mapping for scancode");
            }
        }
        KeyboardEvent::Released { code, extended } => {
            if let Some(evdev_key) = keymap::xt_to_evdev(code, extended) {
                let modifier = is_modifier(evdev_key);
                if modifier && !depressed.holds_modifier(evdev_key) {
                    tracing::trace!(evdev_key, "Suppressed stray modifier release");
                    return;
                }
                depressed.release(evdev_key, modifier);
                if evdev_key == LEFT_SHIFT_KEY && !depressed.unicode_shift_owners.is_empty() {
                    return;
                }
                sink.key(t, evdev_key, false);
                if tracker.key(evdev_key, false) {
                    sink.modifiers(tracker.modifier_state());
                }
                sink.flush();
            }
        }
        KeyboardEvent::Synchronize(flags) => {
            // Announce unconditionally: clients send Synchronize on focus,
            // which makes it a periodic repair point for out-of-band device
            // resets that never fire an activelayout event. Release any
            // tracked key first, then apply locks to the client view.
            release_depressed(tracker, depressed, sink, t);
            tracker.synchronize_locks(flags);
            sink.modifiers(tracker.modifier_state());
            sink.flush();
        }
        KeyboardEvent::UnicodePressed(code_point) => {
            if let Some(mapping) = tracker.unicode_to_evdev(code_point) {
                if mapping.needs_shift {
                    let shift_already_active = depressed.holds_modifier(LEFT_SHIFT_KEY)
                        || !depressed.unicode_shift_owners.is_empty();
                    depressed.press_unicode_shift(code_point);
                    if !shift_already_active {
                        sink.key(t, LEFT_SHIFT_KEY, true);
                        if tracker.key(LEFT_SHIFT_KEY, true) {
                            sink.modifiers(tracker.modifier_state());
                        }
                    }
                }
                sink.key(t, mapping.evdev_key, true);
                depressed.press(mapping.evdev_key, false);
                sink.flush();
            } else {
                tracing::trace!(code_point, "No evdev mapping for Unicode character");
            }
        }
        KeyboardEvent::UnicodeReleased(code_point) => {
            if let Some(mapping) = tracker.unicode_to_evdev(code_point) {
                sink.key(t, mapping.evdev_key, false);
                depressed.release(mapping.evdev_key, false);
                if mapping.needs_shift
                    && depressed.release_unicode_shift(code_point)
                    && depressed.unicode_shift_owners.is_empty()
                    && !depressed.holds_modifier(LEFT_SHIFT_KEY)
                {
                    sink.key(t, LEFT_SHIFT_KEY, false);
                    if tracker.key(LEFT_SHIFT_KEY, false) {
                        sink.modifiers(tracker.modifier_state());
                    }
                }
                sink.flush();
            }
        }
    }
}

fn release_depressed(
    tracker: &mut KeyboardStateTracker,
    depressed: &mut DepressedKeys,
    sink: &mut impl KeyboardSink,
    t: u32,
) -> usize {
    let unicode_shift_only =
        !depressed.unicode_shift_owners.is_empty() && !depressed.holds_modifier(LEFT_SHIFT_KEY);
    let mut released = 0;
    for evdev_key in depressed.release_all() {
        sink.key(t, evdev_key, false);
        tracker.key(evdev_key, false);
        released += 1;
    }
    if unicode_shift_only {
        sink.key(t, LEFT_SHIFT_KEY, false);
        tracker.key(LEFT_SHIFT_KEY, false);
        released += 1;
    }
    depressed.unicode_shift_owners.clear();
    released
}

fn release_held_keys(
    tracker: &mut KeyboardStateTracker,
    depressed: &mut DepressedKeys,
    sink: &mut impl KeyboardSink,
    t: u32,
    reason: &'static str,
) {
    if depressed.is_empty() {
        return;
    }

    let released = release_depressed(tracker, depressed, sink, t);
    sink.modifiers(tracker.modifier_state());
    sink.flush();
    tracing::info!(released, reason, "Released held keys");
}

fn is_modifier(evdev_key: u32) -> bool {
    matches!(
        evdev_key,
        29 | LEFT_SHIFT_KEY | 54 | 56 | 97 | 100 | 125 | 126
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::keyboard::{generate_xkb_keymap_from_names, XkbKeymapNames};
    use ironrdp_pdu::input::fast_path::SynchronizeFlags;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[derive(Debug, PartialEq, Eq)]
    enum SinkCall {
        Key { evdev_key: u32, pressed: bool },
        Modifiers(KeyboardModifierState),
        Keymap(usize),
        Flush,
        Mouse,
    }

    #[derive(Default)]
    struct TestSink {
        calls: Vec<SinkCall>,
    }

    impl KeyboardSink for TestSink {
        fn key(&mut self, _time: u32, evdev_key: u32, pressed: bool) {
            self.calls.push(SinkCall::Key { evdev_key, pressed });
        }
        fn modifiers(&mut self, state: KeyboardModifierState) {
            self.calls.push(SinkCall::Modifiers(state));
        }
        fn keymap(&mut self, keymap_data: &[u8]) -> bool {
            self.calls.push(SinkCall::Keymap(keymap_data.len()));
            true
        }
        fn flush(&mut self) {
            self.calls.push(SinkCall::Flush);
        }
    }

    impl InputBackend for TestSink {
        fn mouse(&mut self, _time: u32, _event: MouseEvent) {
            self.calls.push(SinkCall::Mouse);
        }
    }

    struct SharedSink {
        calls: Arc<std::sync::Mutex<Vec<SinkCall>>>,
    }

    impl KeyboardSink for SharedSink {
        fn key(&mut self, _time: u32, evdev_key: u32, pressed: bool) {
            self.calls
                .lock()
                .unwrap()
                .push(SinkCall::Key { evdev_key, pressed });
        }
        fn modifiers(&mut self, state: KeyboardModifierState) {
            self.calls.lock().unwrap().push(SinkCall::Modifiers(state));
        }
        fn keymap(&mut self, keymap_data: &[u8]) -> bool {
            self.calls
                .lock()
                .unwrap()
                .push(SinkCall::Keymap(keymap_data.len()));
            true
        }
        fn flush(&mut self) {
            self.calls.lock().unwrap().push(SinkCall::Flush);
        }
    }

    impl InputBackend for SharedSink {
        fn mouse(&mut self, _time: u32, _event: MouseEvent) {
            self.calls.lock().unwrap().push(SinkCall::Mouse);
        }
    }

    #[test]
    fn session_end_releases_all_key_owners_and_flushes() {
        let mut tracker = tracker();
        let calls = run(
            &mut tracker,
            vec![
                InputCommand::Keyboard(KeyboardEvent::Pressed {
                    code: 0x1c,
                    extended: false,
                }),
                InputCommand::Keyboard(KeyboardEvent::Pressed {
                    code: 0x1d,
                    extended: false,
                }),
                InputCommand::Keyboard(KeyboardEvent::UnicodePressed('A' as u16)),
                InputCommand::ReleaseHeldKeys,
            ],
        );

        let release_suffix = &calls[calls.len() - 6..];
        assert_eq!(
            release_suffix,
            [
                SinkCall::Key {
                    evdev_key: 30,
                    pressed: false,
                },
                SinkCall::Key {
                    evdev_key: 28,
                    pressed: false,
                },
                SinkCall::Key {
                    evdev_key: 29,
                    pressed: false,
                },
                SinkCall::Key {
                    evdev_key: LEFT_SHIFT_KEY,
                    pressed: false,
                },
                SinkCall::Modifiers(KeyboardModifierState::default()),
                SinkCall::Flush,
            ]
        );
    }

    #[test]
    fn empty_session_end_is_a_noop() {
        let mut tracker = tracker();
        assert!(run(&mut tracker, vec![InputCommand::ReleaseHeldKeys]).is_empty());
    }

    #[test]
    fn actor_shutdown_releases_held_keys_and_flushes() {
        let keymap = generate_xkb_keymap_from_names(&XkbKeymapNames::default())
            .expect("default keymap compiles");
        let (commands, receiver) = mpsc::channel();
        commands
            .send(InputCommand::Keyboard(KeyboardEvent::Pressed {
                code: 0x1c,
                extended: false,
            }))
            .unwrap();
        drop(commands);

        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        run_input_actor(
            receiver,
            keymap,
            "test",
            Instant::now(),
            SharedSink {
                calls: Arc::clone(&calls),
            },
        );

        let calls = calls.lock().unwrap();
        assert_eq!(
            &calls[calls.len() - 3..],
            [
                SinkCall::Key {
                    evdev_key: 28,
                    pressed: false,
                },
                SinkCall::Modifiers(KeyboardModifierState::default()),
                SinkCall::Flush,
            ]
        );
    }

    /// A backend whose device rejects every keymap announcement.
    #[derive(Default)]
    struct RejectingKeymapSink {
        inner: TestSink,
    }

    impl KeyboardSink for RejectingKeymapSink {
        fn key(&mut self, time: u32, evdev_key: u32, pressed: bool) {
            self.inner.key(time, evdev_key, pressed);
        }
        fn modifiers(&mut self, state: KeyboardModifierState) {
            self.inner.modifiers(state);
        }
        fn keymap(&mut self, _keymap_data: &[u8]) -> bool {
            false
        }
        fn flush(&mut self) {
            self.inner.flush();
        }
    }

    impl InputBackend for RejectingKeymapSink {
        fn mouse(&mut self, time: u32, event: MouseEvent) {
            self.inner.mouse(time, event);
        }
    }

    struct PumpCountingSink {
        pumps: Arc<AtomicUsize>,
    }

    impl KeyboardSink for PumpCountingSink {
        fn key(&mut self, _time: u32, _evdev_key: u32, _pressed: bool) {}
        fn modifiers(&mut self, _state: KeyboardModifierState) {}
        fn keymap(&mut self, _keymap_data: &[u8]) -> bool {
            true
        }
        fn flush(&mut self) {}
    }

    impl InputBackend for PumpCountingSink {
        fn mouse(&mut self, _time: u32, _event: MouseEvent) {}

        fn pump(&mut self) {
            self.pumps.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn tracker() -> KeyboardStateTracker {
        let keymap = generate_xkb_keymap_from_names(&XkbKeymapNames {
            layout: Some("us,ua".into()),
            options: Some("grp:alt_shift_toggle".into()),
            ..Default::default()
        })
        .expect("multi-layout keymap compiles");
        KeyboardStateTracker::new(&keymap).expect("keymap loads")
    }

    const TEST_KEYMAP: &[u8] = b"test-keymap";

    fn run(tracker: &mut KeyboardStateTracker, commands: Vec<InputCommand>) -> Vec<SinkCall> {
        let mut sink = TestSink::default();
        let mut keymap_data = TEST_KEYMAP.to_vec();
        let mut depressed = DepressedKeys::default();
        let epoch = Instant::now();
        for command in commands {
            handle_command(
                tracker,
                &mut keymap_data,
                &mut depressed,
                &mut sink,
                &epoch,
                command,
            );
        }
        sink.calls
    }

    fn key_calls(calls: &[SinkCall]) -> Vec<(u32, bool)> {
        calls
            .iter()
            .filter_map(|call| match call {
                SinkCall::Key { evdev_key, pressed } => Some((*evdev_key, *pressed)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn modifier_key_sends_key_then_modifiers_then_flush() {
        let mut tracker = tracker();
        // 0x2A = left shift XT scancode
        let calls = run(
            &mut tracker,
            vec![InputCommand::Keyboard(KeyboardEvent::Pressed {
                code: 0x2a,
                extended: false,
            })],
        );

        assert!(matches!(
            calls[0],
            SinkCall::Key {
                evdev_key: 42,
                pressed: true
            }
        ));
        assert!(matches!(calls[1], SinkCall::Modifiers(state) if state.depressed != 0));
        assert_eq!(calls[2], SinkCall::Flush);
    }

    #[test]
    fn modifier_repeat_press_is_suppressed_and_single_release_neutralizes() {
        let mut tracker = tracker();
        // 0x5B extended = left Super (KEY_LEFTMETA, evdev 125).
        let calls = run(
            &mut tracker,
            vec![
                InputCommand::Keyboard(KeyboardEvent::Pressed {
                    code: 0x5b,
                    extended: true,
                }),
                InputCommand::Keyboard(KeyboardEvent::Pressed {
                    code: 0x5b,
                    extended: true,
                }),
                InputCommand::Keyboard(KeyboardEvent::Released {
                    code: 0x5b,
                    extended: true,
                }),
            ],
        );

        let key_calls = key_calls(&calls);
        assert_eq!(key_calls, vec![(125, true), (125, false)]);
        assert_eq!(tracker.modifier_state().depressed, 0);
    }

    #[test]
    fn ordinary_key_repeated_presses_remain_forwarded() {
        let mut tracker = tracker();
        // 0x1E = 'A' XT scancode -> evdev 30.
        let calls = run(
            &mut tracker,
            vec![
                InputCommand::Keyboard(KeyboardEvent::Pressed {
                    code: 0x1e,
                    extended: false,
                }),
                InputCommand::Keyboard(KeyboardEvent::Pressed {
                    code: 0x1e,
                    extended: false,
                }),
                InputCommand::Keyboard(KeyboardEvent::Released {
                    code: 0x1e,
                    extended: false,
                }),
            ],
        );

        let key_calls = key_calls(&calls);
        assert_eq!(key_calls, vec![(30, true), (30, true), (30, false)]);
    }

    #[test]
    fn stray_modifier_release_is_suppressed() {
        let mut tracker = tracker();
        let calls = run(
            &mut tracker,
            vec![InputCommand::Keyboard(KeyboardEvent::Released {
                code: 0x2a,
                extended: false,
            })],
        );

        assert!(calls.is_empty());
        assert_eq!(tracker.modifier_state().depressed, 0);
    }

    #[test]
    fn unicode_shift_release_keeps_held_physical_shift() {
        let mut tracker = tracker();
        let calls = run(
            &mut tracker,
            vec![
                InputCommand::Keyboard(KeyboardEvent::Pressed {
                    code: 0x2a,
                    extended: false,
                }),
                InputCommand::Keyboard(KeyboardEvent::UnicodePressed('A' as u16)),
                InputCommand::Keyboard(KeyboardEvent::UnicodeReleased('A' as u16)),
                InputCommand::Keyboard(KeyboardEvent::Released {
                    code: 0x2a,
                    extended: false,
                }),
            ],
        );

        let key_calls = key_calls(&calls);
        assert_eq!(
            key_calls,
            vec![(42, true), (30, true), (30, false), (42, false)]
        );
        assert_eq!(tracker.modifier_state().depressed, 0);
    }

    #[test]
    fn physical_shift_release_keeps_unicode_owned_shift() {
        let mut tracker = tracker();
        let calls = run(
            &mut tracker,
            vec![
                InputCommand::Keyboard(KeyboardEvent::UnicodePressed('A' as u16)),
                InputCommand::Keyboard(KeyboardEvent::Pressed {
                    code: 0x2a,
                    extended: false,
                }),
                InputCommand::Keyboard(KeyboardEvent::UnicodeReleased('A' as u16)),
                InputCommand::Keyboard(KeyboardEvent::Released {
                    code: 0x2a,
                    extended: false,
                }),
            ],
        );

        let key_calls = key_calls(&calls);
        assert_eq!(
            key_calls,
            vec![(42, true), (30, true), (30, false), (42, false)]
        );
        assert_eq!(tracker.modifier_state().depressed, 0);
    }

    #[test]
    fn unicode_shift_repeat_press_single_release_neutralizes() {
        let mut tracker = tracker();
        let calls = run(
            &mut tracker,
            vec![
                InputCommand::Keyboard(KeyboardEvent::UnicodePressed('A' as u16)),
                InputCommand::Keyboard(KeyboardEvent::UnicodePressed('A' as u16)),
                InputCommand::Keyboard(KeyboardEvent::UnicodeReleased('A' as u16)),
            ],
        );

        let key_calls = key_calls(&calls);
        assert_eq!(
            key_calls,
            vec![(42, true), (30, true), (30, true), (30, false), (42, false),]
        );
        assert_eq!(tracker.modifier_state().depressed, 0);
    }

    #[test]
    fn unicode_shift_waits_for_all_distinct_owners() {
        let mut tracker = tracker();
        let calls = run(
            &mut tracker,
            vec![
                InputCommand::Keyboard(KeyboardEvent::UnicodePressed('A' as u16)),
                InputCommand::Keyboard(KeyboardEvent::UnicodePressed('B' as u16)),
                InputCommand::Keyboard(KeyboardEvent::UnicodeReleased('A' as u16)),
                InputCommand::Keyboard(KeyboardEvent::UnicodeReleased('B' as u16)),
            ],
        );

        let key_calls = key_calls(&calls);
        assert_eq!(
            key_calls,
            vec![
                (42, true),
                (30, true),
                (48, true),
                (30, false),
                (48, false),
                (42, false),
            ]
        );
        assert_eq!(tracker.modifier_state().depressed, 0);
    }

    #[test]
    fn keyboard_and_mouse_commands_keep_arrival_order() {
        // Ctrl queued before a click must reach the backend before it.
        let mut tracker = tracker();
        let calls = run(
            &mut tracker,
            vec![
                InputCommand::Keyboard(KeyboardEvent::Pressed {
                    code: 0x1d,
                    extended: false,
                }),
                InputCommand::Mouse(MouseEvent::Button {
                    x: 0,
                    y: 0,
                    button: ironrdp_server::MouseButton::Left,
                    pressed: true,
                }),
            ],
        );

        let ctrl_pos = calls
            .iter()
            .position(|c| {
                matches!(
                    c,
                    SinkCall::Key {
                        evdev_key: 29,
                        pressed: true
                    }
                )
            })
            .expect("ctrl forwarded");
        let mouse_pos = calls
            .iter()
            .position(|c| *c == SinkCall::Mouse)
            .expect("click forwarded");
        assert!(ctrl_pos < mouse_pos);
    }

    #[test]
    fn busy_command_queue_still_pumps_the_transport() {
        let keymap = generate_xkb_keymap_from_names(&XkbKeymapNames::default())
            .expect("default keymap compiles");
        let (commands, receiver) = mpsc::channel();
        commands
            .send(InputCommand::Mouse(MouseEvent::Button {
                x: 0,
                y: 0,
                button: ironrdp_server::MouseButton::Left,
                pressed: true,
            }))
            .unwrap();
        commands
            .send(InputCommand::Mouse(MouseEvent::Button {
                x: 0,
                y: 0,
                button: ironrdp_server::MouseButton::Left,
                pressed: false,
            }))
            .unwrap();
        drop(commands);

        let pumps = Arc::new(AtomicUsize::new(0));
        run_input_actor(
            receiver,
            keymap,
            "test",
            Instant::now(),
            PumpCountingSink {
                pumps: Arc::clone(&pumps),
            },
        );

        assert_eq!(pumps.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn plain_key_sends_no_modifiers() {
        let mut tracker = tracker();
        // 0x1E = 'A' key XT scancode -> evdev 30
        let calls = run(
            &mut tracker,
            vec![InputCommand::Keyboard(KeyboardEvent::Pressed {
                code: 0x1e,
                extended: false,
            })],
        );

        assert_eq!(
            calls,
            vec![
                SinkCall::Key {
                    evdev_key: 30,
                    pressed: true
                },
                SinkCall::Flush,
            ]
        );
    }

    #[test]
    fn synchronize_always_reannounces_lock_state() {
        // Even a no-change Synchronize re-announces: clients send it on
        // focus, which repairs out-of-band device resets that never fire
        // an activelayout event.
        let mut tracker = tracker();
        let calls = run(
            &mut tracker,
            vec![
                InputCommand::Keyboard(KeyboardEvent::Synchronize(SynchronizeFlags::CAPS_LOCK)),
                InputCommand::Keyboard(KeyboardEvent::Synchronize(SynchronizeFlags::CAPS_LOCK)),
            ],
        );

        let modifier_sends = calls
            .iter()
            .filter(|c| matches!(c, SinkCall::Modifiers(_)))
            .count();
        assert_eq!(modifier_sends, 2);
        assert_eq!(calls.iter().filter(|c| **c == SinkCall::Flush).count(), 2);
    }

    #[test]
    fn synchronize_releases_plain_keys_before_modifiers() {
        let mut tracker = tracker();
        let calls = run(
            &mut tracker,
            vec![
                InputCommand::Keyboard(KeyboardEvent::Pressed {
                    code: 0x5b,
                    extended: true,
                }),
                InputCommand::Keyboard(KeyboardEvent::Pressed {
                    code: 0x1e,
                    extended: false,
                }),
                InputCommand::Keyboard(KeyboardEvent::Synchronize(SynchronizeFlags::empty())),
            ],
        );

        let releases: Vec<u32> = calls
            .iter()
            .filter_map(|call| match call {
                SinkCall::Key {
                    evdev_key,
                    pressed: false,
                } => Some(*evdev_key),
                _ => None,
            })
            .collect();
        assert_eq!(releases, vec![30, 125]);
        assert!(matches!(
            calls.iter().rev().find(|call| matches!(call, SinkCall::Modifiers(_))),
            Some(SinkCall::Modifiers(state)) if state.depressed == 0
        ));
    }

    #[test]
    fn synchronize_releases_unicode_owned_shift() {
        let mut tracker = tracker();
        let calls = run(
            &mut tracker,
            vec![
                InputCommand::Keyboard(KeyboardEvent::UnicodePressed('A' as u16)),
                InputCommand::Keyboard(KeyboardEvent::Synchronize(SynchronizeFlags::empty())),
            ],
        );

        let key_calls = key_calls(&calls);
        assert_eq!(
            key_calls,
            vec![(42, true), (30, true), (30, false), (42, false)]
        );
        assert_eq!(tracker.modifier_state().depressed, 0);
    }

    #[test]
    fn synchronize_releases_shared_physical_and_unicode_shift_once() {
        let mut tracker = tracker();
        let calls = run(
            &mut tracker,
            vec![
                InputCommand::Keyboard(KeyboardEvent::Pressed {
                    code: 0x2a,
                    extended: false,
                }),
                InputCommand::Keyboard(KeyboardEvent::UnicodePressed('A' as u16)),
                InputCommand::Keyboard(KeyboardEvent::Synchronize(SynchronizeFlags::empty())),
            ],
        );

        let key_calls = key_calls(&calls);
        assert_eq!(
            key_calls,
            vec![(42, true), (30, true), (30, false), (42, false)]
        );
        assert_eq!(tracker.modifier_state().depressed, 0);
    }

    #[test]
    fn locked_layout_switch_sends_modifiers_and_composes_with_toggle() {
        let mut tracker = tracker();
        if !tracker.supports_locked_layout() {
            eprintln!("skipping: libxkbcommon lacks xkb_state_update_latched_locked");
            return;
        }
        let calls = run(
            &mut tracker,
            vec![InputCommand::SetLockedLayout {
                layout_name: "Ukrainian".into(),
                from_own_keyboard: false,
            }],
        );

        assert!(matches!(calls[0], SinkCall::Modifiers(state) if state.group == 1));
        assert_eq!(calls[1], SinkCall::Flush);

        // A later Alt+Shift toggles relative to the locked group.
        let calls = run(
            &mut tracker,
            vec![
                InputCommand::Keyboard(KeyboardEvent::Pressed {
                    code: 0x38,
                    extended: false,
                }),
                InputCommand::Keyboard(KeyboardEvent::Pressed {
                    code: 0x2a,
                    extended: false,
                }),
                InputCommand::Keyboard(KeyboardEvent::Released {
                    code: 0x2a,
                    extended: false,
                }),
                InputCommand::Keyboard(KeyboardEvent::Released {
                    code: 0x38,
                    extended: false,
                }),
            ],
        );
        let last_modifiers = calls
            .iter()
            .rev()
            .find_map(|c| match c {
                SinkCall::Modifiers(state) => Some(*state),
                _ => None,
            })
            .expect("modifiers sent");
        assert_eq!(last_modifiers.group, 0);
    }

    #[test]
    fn own_keyboard_event_matching_replica_is_dropped() {
        let mut tracker = tracker();
        let calls = run(
            &mut tracker,
            vec![InputCommand::SetLockedLayout {
                layout_name: "English (US)".into(),
                from_own_keyboard: true,
            }],
        );

        assert!(calls.is_empty());
        assert_eq!(tracker.modifier_state().group, 0);
    }

    #[test]
    fn own_keyboard_divergence_reasserts_replica_group() {
        let mut tracker = tracker();
        // The compositor reset our device out of band; the replica must
        // win, not follow.
        let calls = run(
            &mut tracker,
            vec![InputCommand::SetLockedLayout {
                layout_name: "Ukrainian".into(),
                from_own_keyboard: true,
            }],
        );

        assert_eq!(tracker.modifier_state().group, 0);
        assert!(matches!(calls[0], SinkCall::Modifiers(state) if state.group == 0));
        assert_eq!(calls[1], SinkCall::Flush);
    }

    #[test]
    fn own_keyboard_unresolvable_name_reannounces_keymap() {
        // An own-device event naming a layout outside the replica keymap
        // means the compositor replaced the device keymap; restore it.
        let mut tracker = tracker();
        let state = tracker.modifier_state();
        let calls = run(
            &mut tracker,
            vec![InputCommand::SetLockedLayout {
                layout_name: "German".into(),
                from_own_keyboard: true,
            }],
        );

        assert_eq!(
            calls,
            vec![
                SinkCall::Keymap(TEST_KEYMAP.len()),
                SinkCall::Modifiers(state),
                SinkCall::Flush,
            ]
        );
    }

    #[test]
    fn apply_keymap_keeps_replica_when_device_rejects_the_keymap() {
        let mut tracker = tracker();
        let german = generate_xkb_keymap_from_names(&XkbKeymapNames {
            layout: Some("de".into()),
            ..Default::default()
        })
        .expect("German keymap compiles");

        let mut sink = RejectingKeymapSink::default();
        let mut keymap_data = TEST_KEYMAP.to_vec();
        let mut depressed = DepressedKeys::default();
        let epoch = Instant::now();
        handle_command(
            &mut tracker,
            &mut keymap_data,
            &mut depressed,
            &mut sink,
            &epoch,
            InputCommand::ApplyKeymap {
                keymap_data: german,
                keymap_source: "rdp-client",
            },
        );

        // The replica still resolves 'z' at the us position, and the stored
        // keymap is unchanged.
        assert_eq!(tracker.unicode_to_evdev('z' as u16).unwrap().evdev_key, 44);
        assert_eq!(keymap_data, TEST_KEYMAP);
        assert!(sink.inner.calls.is_empty());
    }

    #[test]
    fn seed_prefers_the_main_physical_keyboard_even_on_the_default_group() {
        let mut tracker = tracker();
        let calls = run(
            &mut tracker,
            vec![InputCommand::SetInitialLayout {
                candidates: InitialLayoutCandidates {
                    main: Some(("keychron-keychron-k8".into(), "English (US)".into())),
                    others: vec![("stale-keyboard".into(), "Ukrainian".into())],
                },
            }],
        );

        // The main keyboard's default group wins over a stale non-default
        // sibling: no switch, no announcement.
        assert!(calls.is_empty());
        assert_eq!(tracker.modifier_state().group, 0);
    }

    #[test]
    fn seed_prefers_a_non_default_group_without_a_main_keyboard() {
        let mut tracker = tracker();
        if !tracker.supports_locked_layout() {
            eprintln!("skipping: libxkbcommon lacks xkb_state_update_latched_locked");
            return;
        }
        // Pseudo-keyboards never see the toggle keys and sit on the default
        // group; the device that actually switched wins regardless of
        // enumeration order.
        let calls = run(
            &mut tracker,
            vec![InputCommand::SetInitialLayout {
                candidates: InitialLayoutCandidates {
                    main: None,
                    others: vec![
                        ("power-button".into(), "English (US)".into()),
                        ("keychron-keychron-k8".into(), "Ukrainian".into()),
                    ],
                },
            }],
        );

        assert!(matches!(calls[0], SinkCall::Modifiers(state) if state.group == 1));
        assert_eq!(tracker.modifier_state().group, 1);
    }

    #[test]
    fn seed_falls_through_an_unresolvable_main_keyboard() {
        let mut tracker = tracker();
        if !tracker.supports_locked_layout() {
            eprintln!("skipping: libxkbcommon lacks xkb_state_update_latched_locked");
            return;
        }
        let calls = run(
            &mut tracker,
            vec![InputCommand::SetInitialLayout {
                candidates: InitialLayoutCandidates {
                    main: Some(("laptop-kbd".into(), "German".into())),
                    others: vec![("keychron-keychron-k8".into(), "Ukrainian".into())],
                },
            }],
        );

        assert!(matches!(calls[0], SinkCall::Modifiers(state) if state.group == 1));
    }

    #[test]
    fn seed_with_no_resolvable_candidate_does_nothing() {
        let mut tracker = tracker();
        let calls = run(
            &mut tracker,
            vec![InputCommand::SetInitialLayout {
                candidates: InitialLayoutCandidates {
                    main: None,
                    others: vec![("laptop-kbd".into(), "German".into())],
                },
            }],
        );

        assert!(calls.is_empty());
        assert_eq!(tracker.modifier_state().group, 0);
    }

    #[test]
    fn compositor_reset_racing_initial_sync_converges_to_synced_group() {
        // The startup race: initial sync locks the physical keyboard's
        // group, then Hyprland's deferred device configuration resets our
        // virtual keyboard and its stale events arrive afterwards.
        let mut tracker = tracker();
        if !tracker.supports_locked_layout() {
            eprintln!("skipping: libxkbcommon lacks xkb_state_update_latched_locked");
            return;
        }
        let calls = run(
            &mut tracker,
            vec![
                InputCommand::SetLockedLayout {
                    layout_name: "Ukrainian".into(),
                    from_own_keyboard: false,
                },
                InputCommand::SetLockedLayout {
                    layout_name: "English (US)".into(),
                    from_own_keyboard: true,
                },
                InputCommand::SetLockedLayout {
                    layout_name: "Ukrainian".into(),
                    from_own_keyboard: true,
                },
            ],
        );

        assert_eq!(tracker.modifier_state().group, 1);
        let announced: Vec<_> = calls
            .iter()
            .filter_map(|c| match c {
                SinkCall::Modifiers(state) => Some(state.group),
                _ => None,
            })
            .collect();
        // Sync announce plus one re-assert over the reset — both group 1,
        // and the matching final event announces nothing.
        assert_eq!(announced, vec![1, 1]);
    }

    #[test]
    fn unknown_layout_name_is_ignored() {
        let mut tracker = tracker();
        let calls = run(
            &mut tracker,
            vec![InputCommand::SetLockedLayout {
                layout_name: "German".into(),
                from_own_keyboard: false,
            }],
        );

        assert!(calls.is_empty());
    }

    #[test]
    fn repeated_layout_switch_is_a_noop() {
        let mut tracker = tracker();
        if !tracker.supports_locked_layout() {
            eprintln!("skipping: libxkbcommon lacks xkb_state_update_latched_locked");
            return;
        }
        let calls = run(
            &mut tracker,
            vec![
                InputCommand::SetLockedLayout {
                    layout_name: "Ukrainian".into(),
                    from_own_keyboard: false,
                },
                InputCommand::SetLockedLayout {
                    layout_name: "Ukrainian".into(),
                    from_own_keyboard: false,
                },
            ],
        );

        let modifier_sends = calls
            .iter()
            .filter(|c| matches!(c, SinkCall::Modifiers(_)))
            .count();
        assert_eq!(modifier_sends, 1);
    }

    #[test]
    fn apply_keymap_replaces_tracker_and_reannounces() {
        let mut tracker = tracker();
        let german = generate_xkb_keymap_from_names(&XkbKeymapNames {
            layout: Some("de".into()),
            ..Default::default()
        })
        .expect("German keymap compiles");
        let len = german.len();

        let calls = run(
            &mut tracker,
            vec![InputCommand::ApplyKeymap {
                keymap_data: german,
                keymap_source: "rdp-client",
            }],
        );

        assert_eq!(calls[0], SinkCall::Keymap(len));
        assert!(matches!(calls[1], SinkCall::Modifiers(_)));
        assert_eq!(calls[2], SinkCall::Flush);
        // German keymap: 'z' lives on evdev 21.
        assert_eq!(tracker.unicode_to_evdev('z' as u16).unwrap().evdev_key, 21);
    }

    #[test]
    fn apply_keymap_releases_modifier_inventory() {
        let mut tracker = tracker();
        let german = generate_xkb_keymap_from_names(&XkbKeymapNames {
            layout: Some("de".into()),
            ..Default::default()
        })
        .expect("German keymap compiles");

        let calls = run(
            &mut tracker,
            vec![
                InputCommand::Keyboard(KeyboardEvent::Pressed {
                    code: 0x2a,
                    extended: false,
                }),
                InputCommand::ApplyKeymap {
                    keymap_data: german,
                    keymap_source: "rdp-client",
                },
                InputCommand::Keyboard(KeyboardEvent::Pressed {
                    code: 0x2a,
                    extended: false,
                }),
                InputCommand::Keyboard(KeyboardEvent::Released {
                    code: 0x2a,
                    extended: false,
                }),
            ],
        );

        let key_calls = key_calls(&calls);
        assert_eq!(
            key_calls,
            vec![(42, true), (42, false), (42, true), (42, false)]
        );
        assert_eq!(tracker.modifier_state().depressed, 0);
    }

    #[test]
    fn unicode_shift_sequence_preserves_order() {
        let mut tracker = tracker();
        // 'A' requires shift on the us layout.
        let calls = run(
            &mut tracker,
            vec![
                InputCommand::Keyboard(KeyboardEvent::UnicodePressed('A' as u16)),
                InputCommand::Keyboard(KeyboardEvent::UnicodeReleased('A' as u16)),
            ],
        );

        let keys: Vec<_> = calls
            .iter()
            .filter_map(|c| match c {
                SinkCall::Key { evdev_key, pressed } => Some((*evdev_key, *pressed)),
                _ => None,
            })
            .collect();
        assert_eq!(keys, vec![(42, true), (30, true), (30, false), (42, false)]);
    }
}
