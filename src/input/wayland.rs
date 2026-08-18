use std::fs::File;
use std::io::Read;
use std::os::fd::AsFd;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use ironrdp_server::MouseEvent;
use wayland_client::protocol::wl_pointer::{Axis, AxisSource, ButtonState};
use wayland_client::protocol::{wl_keyboard, wl_output, wl_registry, wl_seat};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, QueueHandle, WEnum};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

use crate::hyprland;

use super::actor::{
    run_input_actor, InitialLayoutCandidates, InputBackend, InputCommand, KeyboardSink,
};
use super::keyboard::{
    create_keymap_fd, generate_xkb_keymap, generate_xkb_keymap_from_names, KeyboardModifierState,
    KeyboardStateTracker, XkbKeymapNames,
};
use super::layout::{OutputLayoutSnapshot, SharedOutputLayout};
use super::virtual_keyboard::{ZwpVirtualKeyboardManagerV1, ZwpVirtualKeyboardV1};
use super::{keymap, KeyboardLayoutPolicy};

const LAYOUT_LISTENER_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Everything the input actor thread owns: the Wayland handles for both
/// virtual devices plus the connection and its event queue. Living on one
/// thread keeps keyboard and mouse requests in command order and leaves no
/// shared input state to lock.
struct WaylandInput {
    conn: Connection,
    event_queue: EventQueue<WlState>,
    wl_state: WlState,
    vk: ZwpVirtualKeyboardV1,
    vp: ZwlrVirtualPointerV1,
    output_layout: Arc<SharedOutputLayout>,
}

impl WaylandInput {
    fn dispatch_pending(&mut self) {
        if let Err(e) = self.event_queue.dispatch_pending(&mut self.wl_state) {
            tracing::trace!("Wayland dispatch_pending failed: {}", e);
        }
    }
}

impl KeyboardSink for WaylandInput {
    fn key(&mut self, time: u32, evdev_key: u32, pressed: bool) {
        self.vk.key(time, evdev_key, u32::from(pressed));
    }

    fn modifiers(&mut self, state: KeyboardModifierState) {
        self.vk
            .modifiers(state.depressed, state.latched, state.locked, state.group);
    }

    fn keymap(&mut self, keymap_data: &[u8]) -> bool {
        match create_keymap_fd(keymap_data) {
            Ok(fd) => {
                self.vk.keymap(1, fd.as_fd(), keymap_data.len() as u32); // 1 = XKB_V1
                true
            }
            Err(err) => {
                tracing::warn!("Failed to create keymap fd: {:#}", err);
                false
            }
        }
    }

    /// Dispatch events already read off the socket, then flush outgoing
    /// requests. The socket itself is read in [`InputBackend::pump`], which
    /// the actor runs after commands and while idle.
    fn flush(&mut self) {
        self.dispatch_pending();

        if let Err(e) = self.conn.flush() {
            tracing::warn!("Wayland flush failed: {}", e);
        }
    }
}

impl InputBackend for WaylandInput {
    fn mouse(&mut self, t: u32, event: MouseEvent) {
        match event {
            MouseEvent::Move { x, y } => {
                // Pointer is bound to the output via create_virtual_pointer_with_output,
                // so coordinates are mapped within that output by the compositor.
                // Use the current output dimensions as extent (updates on resize).
                let Some(layout) = self.output_layout.snapshot() else {
                    return;
                };
                let (source_x, source_y) = map_rdp_pointer_to_source(&layout, x, y);
                self.vp
                    .motion_absolute(t, source_x, source_y, layout.output_w, layout.output_h);
                self.vp.frame();
                self.flush();
            }
            MouseEvent::LeftPressed => {
                self.vp.button(t, keymap::BTN_LEFT, ButtonState::Pressed);
                self.vp.frame();
                self.flush();
            }
            MouseEvent::LeftReleased => {
                self.vp.button(t, keymap::BTN_LEFT, ButtonState::Released);
                self.vp.frame();
                self.flush();
            }
            MouseEvent::RightPressed => {
                self.vp.button(t, keymap::BTN_RIGHT, ButtonState::Pressed);
                self.vp.frame();
                self.flush();
            }
            MouseEvent::RightReleased => {
                self.vp.button(t, keymap::BTN_RIGHT, ButtonState::Released);
                self.vp.frame();
                self.flush();
            }
            MouseEvent::MiddlePressed => {
                self.vp.button(t, keymap::BTN_MIDDLE, ButtonState::Pressed);
                self.vp.frame();
                self.flush();
            }
            MouseEvent::MiddleReleased => {
                self.vp.button(t, keymap::BTN_MIDDLE, ButtonState::Released);
                self.vp.frame();
                self.flush();
            }
            MouseEvent::Button4Pressed => {
                self.vp.button(t, keymap::BTN_SIDE, ButtonState::Pressed);
                self.vp.frame();
                self.flush();
            }
            MouseEvent::Button4Released => {
                self.vp.button(t, keymap::BTN_SIDE, ButtonState::Released);
                self.vp.frame();
                self.flush();
            }
            MouseEvent::Button5Pressed => {
                self.vp.button(t, keymap::BTN_EXTRA, ButtonState::Pressed);
                self.vp.frame();
                self.flush();
            }
            MouseEvent::Button5Released => {
                self.vp.button(t, keymap::BTN_EXTRA, ButtonState::Released);
                self.vp.frame();
                self.flush();
            }
            MouseEvent::VerticalScroll { value } => {
                // Negate: RDP positive=up, Wayland positive=down
                let discrete = -((value as f64 / 120.0).round() as i32);
                let continuous = discrete as f64 * 15.0;
                self.vp.axis_source(AxisSource::Wheel);
                self.vp
                    .axis_discrete(t, Axis::VerticalScroll, continuous, discrete);
                self.vp.frame();
                self.flush();
            }
            MouseEvent::Scroll { x, y } => {
                self.vp.axis_source(AxisSource::Wheel);
                if y != 0 {
                    let discrete = -((y as f64 / 120.0).round() as i32);
                    let continuous = discrete as f64 * 15.0;
                    self.vp
                        .axis_discrete(t, Axis::VerticalScroll, continuous, discrete);
                }
                if x != 0 {
                    let discrete = -((x as f64 / 120.0).round() as i32);
                    let continuous = discrete as f64 * 15.0;
                    self.vp
                        .axis_discrete(t, Axis::HorizontalScroll, continuous, discrete);
                }
                self.vp.frame();
                self.flush();
            }
            MouseEvent::RelMove { x, y } => {
                self.vp.motion(t, x as f64, y as f64);
                self.vp.frame();
                self.flush();
            }
        }
    }

    fn pump(&mut self) {
        // Nothing else reads this connection; without an occasional read,
        // compositor events would accumulate unread in the socket buffer.
        if let Some(guard) = self.event_queue.prepare_read() {
            if let Err(err) = guard.read() {
                // WouldBlock when the socket is simply quiet.
                tracing::trace!("Wayland read failed: {}", err);
            }
        }
        self.dispatch_pending();
        if let Err(err) = self.conn.flush() {
            tracing::trace!("Wayland flush failed: {}", err);
        }
    }
}

fn map_rdp_pointer_to_source(layout: &OutputLayoutSnapshot, x: u16, y: u16) -> (u32, u32) {
    layout
        .presentation_geometry
        .map_presentation_point_to_source(u32::from(x), u32::from(y))
}

struct LayoutListener {
    shutdown: Arc<AtomicBool>,
    thread: thread::JoinHandle<()>,
}

pub struct HyprInputHandler {
    pub(super) keyboard_layout_policy: KeyboardLayoutPolicy,
    pub(super) input_commands: Option<mpsc::Sender<InputCommand>>,
    actor_thread: Option<thread::JoinHandle<()>>,
    layout_listener: Option<LayoutListener>,
}

impl HyprInputHandler {
    pub(super) fn send_input_command(&self, command: InputCommand) {
        if let Some(commands) = &self.input_commands {
            if commands.send(command).is_err() {
                tracing::warn!("Input actor is gone; dropping input command");
            }
        }
    }
}

impl Drop for HyprInputHandler {
    fn drop(&mut self) {
        // The listener holds a command sender; stop it first so dropping our
        // sender below closes the channel and stops the actor.
        if let Some(listener) = self.layout_listener.take() {
            listener.shutdown.store(true, Ordering::Relaxed);
            let _ = listener.thread.join();
        }
        self.input_commands.take();
        if let Some(actor) = self.actor_thread.take() {
            let _ = actor.join();
        }
    }
}

impl HyprInputHandler {
    pub fn new(
        rdp_width: u16,
        rdp_height: u16,
        output_layout: Arc<SharedOutputLayout>,
        keyboard_layout_policy: KeyboardLayoutPolicy,
    ) -> Result<Self> {
        let layout = output_layout
            .snapshot()
            .context("output layout not initialized")?;

        let conn = Connection::connect_to_env().context("failed to connect to Wayland display")?;
        let mut event_queue = conn.new_event_queue::<WlState>();
        let qh = event_queue.handle();

        let display = conn.display();
        let _registry = display.get_registry(&qh, ());

        let mut wl_state = WlState::default();

        // Collect globals
        event_queue
            .roundtrip(&mut wl_state)
            .context("Wayland roundtrip failed")?;

        // Second roundtrip to receive wl_output name events
        event_queue
            .roundtrip(&mut wl_state)
            .context("Wayland roundtrip (output names) failed")?;

        let seat = wl_state.seat.clone().context("wl_seat not found")?;
        let vk_mgr = wl_state
            .vk_manager
            .clone()
            .context("zwp_virtual_keyboard_manager_v1 not found")?;
        let vp_mgr = wl_state
            .vp_manager
            .clone()
            .context("zwlr_virtual_pointer_manager_v1 not found")?;

        // Create virtual keyboard
        let vk = vk_mgr.create_virtual_keyboard(&seat, &qh, ());

        // Create virtual pointer bound to the target output (enables correct
        // monitor focus for compositor keybindings like Super+N)
        let target_output = wl_state
            .outputs
            .iter()
            .find(|(_, name)| name.as_deref() == Some(&layout.output_name))
            .map(|(o, _)| o)
            .context(format!("wl_output '{}' not found", layout.output_name))?;

        let vp =
            vp_mgr.create_virtual_pointer_with_output(Some(&seat), Some(target_output), &qh, ());

        // Release all wl_output proxies — they were only needed to find the
        // target output for create_virtual_pointer_with_output. Keeping them
        // alive would require dispatching their events; without that, the
        // compositor's send buffer fills up and blocks the event loop.
        for (output, _) in wl_state.outputs.drain(..) {
            output.release();
        }

        let (keymap_data, keymap_source) =
            load_keymap(&mut event_queue, &mut wl_state, &seat, &qh)?;

        // Fail loudly here instead of inside the actor: a keymap the tracker
        // cannot load must abort startup, not leave a running server without
        // input. The throwaway state never leaves this thread.
        KeyboardStateTracker::new(&keymap_data).context("compositor keymap is not loadable")?;

        let epoch = Instant::now();
        let mut wayland_input = WaylandInput {
            conn,
            event_queue,
            wl_state,
            vk,
            vp,
            output_layout,
        };

        // Flush the device setup requests before handing the connection to
        // the actor thread.
        KeyboardSink::flush(&mut wayland_input);

        // The keyboard state is !Send, so all input lives on one owning
        // thread; the actor announces the keymap and initial modifiers
        // itself, and executes keyboard and mouse commands in arrival order.
        let (input_commands, command_rx) = mpsc::channel();
        let actor_thread = thread::Builder::new()
            .name("hypr-rdp-input".into())
            .spawn(move || {
                run_input_actor(command_rx, keymap_data, keymap_source, epoch, wayland_input)
            })
            .context("failed to spawn input actor thread")?;

        tracing::info!(
            rdp_width, rdp_height,
            layout_extent_w = layout.layout_extent_w,
            layout_extent_h = layout.layout_extent_h,
            output_offset_x = layout.output_offset_x,
            output_offset_y = layout.output_offset_y,
            output = %layout.output_name,
            keymap_source,
            "Input handler initialized (virtual keyboard + pointer)"
        );

        // Hyprland does not send wl_keyboard.modifiers to this surfaceless
        // client, so external layout switches come from Hyprland IPC instead.
        let layout_listener = if keyboard_layout_policy == KeyboardLayoutPolicy::Compositor {
            Some(spawn_layout_listener(input_commands.clone())?)
        } else {
            None
        };

        Ok(Self {
            keyboard_layout_policy,
            input_commands: Some(input_commands),
            actor_thread: Some(actor_thread),
            layout_listener,
        })
    }
}

/// Follow compositor layout switches (`hyprctl switchxkblayout`, layout
/// binds) via Hyprland socket2 `activelayout` events and mirror them onto
/// the virtual keyboard through the keyboard actor. The current layout is
/// synced whenever the stream (re)connects, since `activelayout` only fires
/// on switches.
fn spawn_layout_listener(commands: mpsc::Sender<InputCommand>) -> Result<LayoutListener> {
    let shutdown = Arc::new(AtomicBool::new(false));
    let thread_shutdown = Arc::clone(&shutdown);

    let thread = thread::Builder::new()
        .name("hypr-rdp-layout".into())
        .spawn(move || {
            tracing::info!("Listening for Hyprland activelayout events");
            while !thread_shutdown.load(Ordering::Relaxed) {
                match connect_event_stream() {
                    Ok(mut events) => {
                        let mut seed = |candidates: InitialLayoutCandidates| {
                            commands
                                .send(InputCommand::SetInitialLayout { candidates })
                                .is_ok()
                        };
                        let mut forward = |keyboard: &str, layout: &str| {
                            let Some(command) = layout_command(keyboard, layout) else {
                                tracing::trace!(keyboard, "Ignoring virtual keyboard layout event");
                                return true;
                            };
                            commands.send(command).is_ok()
                        };
                        match drive_layout_events(
                            &mut events,
                            &thread_shutdown,
                            &mut seed,
                            &mut forward,
                        ) {
                            DriveExit::Shutdown | DriveExit::ReceiverGone => return,
                            DriveExit::StreamFailed(err) => {
                                tracing::debug!("Hyprland event stream failed: {:#}", err);
                            }
                        }
                    }
                    Err(err) => {
                        tracing::debug!("Hyprland event socket unavailable: {:#}", err);
                    }
                }
                // Brief pause before reconnecting, responsive to shutdown.
                for _ in 0..10 {
                    if thread_shutdown.load(Ordering::Relaxed) {
                        return;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            }
        })
        .context("failed to spawn layout listener thread")?;

    Ok(LayoutListener { shutdown, thread })
}

fn connect_event_stream() -> Result<hyprland::EventStream> {
    let events = hyprland::EventStream::connect()?;
    events.ensure_registered()?;
    Ok(events)
}

enum DriveExit {
    Shutdown,
    ReceiverGone,
    StreamFailed(anyhow::Error),
}

trait LayoutEventSource {
    /// Physical-keyboard layout candidates for the initial seed, from a
    /// devices query.
    fn initial_layouts(&mut self) -> Option<InitialLayoutCandidates>;

    fn next_layout_event(&mut self, timeout: Duration) -> Result<Option<(String, String)>>;
}

impl LayoutEventSource for hyprland::EventStream {
    fn initial_layouts(&mut self) -> Option<InitialLayoutCandidates> {
        match hyprland::devices() {
            Ok(devices) => physical_keyboard_layouts(&devices),
            Err(err) => {
                tracing::debug!("Failed to query Hyprland devices: {:#}", err);
                None
            }
        }
    }

    fn next_layout_event(&mut self, timeout: Duration) -> Result<Option<(String, String)>> {
        self.next_event(timeout)
    }
}

/// Seed the current layout, then pump events until shutdown is requested,
/// the forward target is gone, or the stream fails. `seed` and `forward`
/// return false when the receiver went away.
fn drive_layout_events(
    source: &mut impl LayoutEventSource,
    shutdown: &AtomicBool,
    seed: &mut impl FnMut(InitialLayoutCandidates) -> bool,
    forward: &mut impl FnMut(&str, &str) -> bool,
) -> DriveExit {
    if shutdown.load(Ordering::Relaxed) {
        return DriveExit::Shutdown;
    }

    // activelayout only fires on switches, so the state at (re)connect needs
    // a query. The stream is connected before the query: a switch racing
    // with it is buffered and re-applied right after, and repeat
    // announcements change nothing device-side, so both orders converge.
    if let Some(candidates) = source.initial_layouts() {
        if !seed(candidates) {
            return DriveExit::ReceiverGone;
        }
    }

    loop {
        if shutdown.load(Ordering::Relaxed) {
            return DriveExit::Shutdown;
        }
        match source.next_layout_event(LAYOUT_LISTENER_POLL_INTERVAL) {
            Ok(Some((event, data))) => {
                if event != "activelayout" {
                    continue;
                }
                let Some((keyboard, layout)) = parse_activelayout(&data) else {
                    continue;
                };
                if !forward(keyboard, layout) {
                    return DriveExit::ReceiverGone;
                }
            }
            Ok(None) => continue,
            Err(err) => return DriveExit::StreamFailed(err),
        }
    }
}

/// `activelayout` data is `KEYBOARDNAME,LAYOUTNAME`. The layout display name
/// may itself contain commas ("English (US, intl.)"); Hyprland's sanitized
/// device names cannot, so the first comma is the separator.
fn parse_activelayout(data: &str) -> Option<(&str, &str)> {
    data.split_once(',')
}

/// Hyprland derives virtual keyboard device names from the owning client's
/// process binary (misc:name_vk_after_proc, default on) and deduplicates
/// collisions with numeric suffixes. A second hypr-rdp instance's device
/// therefore also matches our own prefix and its events are misread as
/// ours — benign, since a re-announce that matches the device state emits
/// nothing, but a single instance per compositor is the supported
/// deployment.
const OWN_VIRTUAL_KEYBOARD_PREFIX: &str = "hl-virtual-keyboard-hypr-rdp";
const VIRTUAL_KEYBOARD_PREFIX: &str = "hl-virtual-keyboard";

/// Translate an `activelayout` event into an input command. Physical devices
/// drive the compositor's active layout and are mirrored onto the virtual
/// keyboard — any physical keyboard counts (last touched wins, matching the
/// compositor's own seat behavior), while the initial seed prefers the main
/// one. Our own virtual keyboard's events are feedback, not input — Hyprland
/// emits `activelayout` for every group change of ours and resets device
/// state out of band — so the actor treats them as consistency checks
/// against the replica. Foreign virtual keyboards (input methods, other
/// injectors) are never followed.
fn layout_command(keyboard: &str, layout: &str) -> Option<InputCommand> {
    let from_own_keyboard = keyboard.starts_with(OWN_VIRTUAL_KEYBOARD_PREFIX);
    if !from_own_keyboard && keyboard.starts_with(VIRTUAL_KEYBOARD_PREFIX) {
        return None;
    }
    Some(InputCommand::SetLockedLayout {
        layout_name: layout.to_string(),
        from_own_keyboard,
    })
}

/// Initial-seed candidates from a `devices` query: the main physical
/// keyboard, if any, plus the remaining physical keyboards in enumeration
/// order. Virtual keyboards are excluded — a freshly created one (ours
/// included) can hold Hyprland's main flag while still sitting on group 0.
fn physical_keyboard_layouts(devices: &serde_json::Value) -> Option<InitialLayoutCandidates> {
    let keyboards = devices.get("keyboards")?.as_array()?;
    let mut main = None;
    let mut others = Vec::new();
    for keyboard in keyboards {
        let Some(name) = keyboard.get("name").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if name.starts_with(VIRTUAL_KEYBOARD_PREFIX) {
            continue;
        }
        let Some(layout) = keyboard
            .get("active_keymap")
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        let entry = (name.to_string(), layout.to_string());
        if main.is_none() && keyboard.get("main").and_then(serde_json::Value::as_bool) == Some(true)
        {
            main = Some(entry);
        } else {
            others.push(entry);
        }
    }
    if main.is_none() && others.is_empty() {
        return None;
    }
    Some(InitialLayoutCandidates { main, others })
}

fn load_keymap(
    event_queue: &mut EventQueue<WlState>,
    wl_state: &mut WlState,
    seat: &wl_seat::WlSeat,
    qh: &QueueHandle<WlState>,
) -> Result<(Vec<u8>, &'static str)> {
    if wl_state.seat_has_keyboard {
        if wl_state.keyboard.is_none() {
            wl_state.keyboard = Some(seat.get_keyboard(qh, ()));
        }

        event_queue
            .roundtrip(wl_state)
            .context("Wayland roundtrip for keyboard keymap failed")?;

        if let Some(keymap) = take_loaded_keymap(wl_state)? {
            return Ok(keymap);
        }
    } else {
        tracing::warn!("Wayland seat has no keyboard capability, using fallback keymap");
    }

    generate_fallback_keymap()
}

fn generate_fallback_keymap() -> Result<(Vec<u8>, &'static str)> {
    generate_fallback_keymap_from_names(hyprland_xkb_keymap_names())
}

fn generate_fallback_keymap_from_names(
    hyprland_names: Result<XkbKeymapNames>,
) -> Result<(Vec<u8>, &'static str)> {
    let names = match hyprland_names {
        Ok(names) => names,
        Err(err) => {
            tracing::warn!("Failed to query Hyprland keyboard options: {:#}", err);
            XkbKeymapNames::default()
        }
    };

    if !names.is_empty() {
        match generate_xkb_keymap_from_names(&names) {
            Ok(keymap) => {
                tracing::info!(
                    len = keymap.len(),
                    layout = ?names.layout,
                    variant = ?names.variant,
                    options = ?names.options,
                    "Generated Hyprland fallback keyboard keymap"
                );
                return Ok((keymap, "hyprland"));
            }
            Err(err) => {
                tracing::warn!(
                    layout = ?names.layout,
                    variant = ?names.variant,
                    options = ?names.options,
                    "Failed to generate Hyprland fallback keymap, using xkb defaults: {:#}",
                    err
                );
            }
        }
    }

    let fallback = generate_xkb_keymap()?;
    tracing::info!(len = fallback.len(), "Generated fallback keyboard keymap");
    Ok((fallback, "fallback"))
}

fn hyprland_xkb_keymap_names() -> Result<XkbKeymapNames> {
    Ok(XkbKeymapNames {
        layout: hyprland::option_string("input:kb_layout")?,
        variant: hyprland::option_string("input:kb_variant")?,
        options: hyprland::option_string("input:kb_options")?,
    })
}

fn take_loaded_keymap(wl_state: &mut WlState) -> Result<Option<(Vec<u8>, &'static str)>> {
    if !wl_state.seat_has_keyboard {
        return Ok(None);
    }

    let keymap_data = wl_state
        .keymap
        .take()
        .context("Wayland seat has keyboard capability but did not provide an XKB keymap")?;
    tracing::info!(len = keymap_data.len(), "Loaded compositor keyboard keymap");
    Ok(Some((keymap_data, "compositor")))
}

fn read_keymap(fd: OwnedFd, size: u32) -> Result<Vec<u8>> {
    let size = usize::try_from(size).context("keyboard keymap too large")?;
    if size == 0 {
        bail!("keyboard keymap is empty");
    }

    let mut file = File::from(fd);
    let mut data = vec![0u8; size];
    file.read_exact(&mut data)
        .context("failed to read Wayland keyboard keymap")?;
    Ok(data)
}

#[derive(Default)]
struct WlState {
    seat: Option<wl_seat::WlSeat>,
    seat_has_keyboard: bool,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    keymap: Option<Vec<u8>>,
    vk_manager: Option<ZwpVirtualKeyboardManagerV1>,
    vp_manager: Option<ZwlrVirtualPointerManagerV1>,
    outputs: Vec<(wl_output::WlOutput, Option<String>)>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for WlState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_seat" if state.seat.is_none() => {
                    state.seat = Some(registry.bind(name, version.min(7), qh, ()));
                }
                "zwp_virtual_keyboard_manager_v1" => {
                    state.vk_manager = Some(registry.bind(name, version.min(1), qh, ()));
                }
                "zwlr_virtual_pointer_manager_v1" => {
                    state.vp_manager = Some(registry.bind(name, version.min(2), qh, ()));
                }
                "wl_output" => {
                    let output: wl_output::WlOutput = registry.bind(name, version.min(4), qh, ());
                    state.outputs.push((output, None));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for WlState {
    fn event(
        state: &mut Self,
        _: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = event
        {
            state.seat_has_keyboard = capabilities.contains(wl_seat::Capability::Keyboard);
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for WlState {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Keymap {
            format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1),
            fd,
            size,
        } = event
        {
            match read_keymap(fd, size) {
                Ok(keymap) => {
                    state.keymap = Some(keymap);
                }
                Err(err) => {
                    tracing::warn!("Failed to read compositor keymap: {:#}", err);
                }
            }
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for WlState {
    fn event(
        state: &mut Self,
        proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            if let Some(entry) = state.outputs.iter_mut().find(|(o, _)| o == proxy) {
                entry.1 = Some(name);
            }
        }
    }
}

delegate_noop!(WlState: ignore ZwpVirtualKeyboardManagerV1);
delegate_noop!(WlState: ignore ZwpVirtualKeyboardV1);
delegate_noop!(WlState: ignore ZwlrVirtualPointerManagerV1);
delegate_noop!(WlState: ignore ZwlrVirtualPointerV1);

impl Dispatch<wayland_client::protocol::wl_display::WlDisplay, ()> for WlState {
    fn event(
        _: &mut Self,
        _: &wayland_client::protocol::wl_display::WlDisplay,
        _: wayland_client::protocol::wl_display::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<wayland_client::protocol::wl_callback::WlCallback, ()> for WlState {
    fn event(
        _: &mut Self,
        _: &wayland_client::protocol::wl_callback::WlCallback,
        _: wayland_client::protocol::wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::keyboard::KeyboardStateTracker;

    #[test]
    fn fallback_keymap_uses_hyprland_layout_names_when_present() {
        let (keymap, source) = generate_fallback_keymap_from_names(Ok(XkbKeymapNames {
            layout: Some("de".into()),
            ..Default::default()
        }))
        .expect("Hyprland fallback keymap compiles");

        let tracker = KeyboardStateTracker::new(&keymap).expect("generated keymap loads");
        assert_eq!(source, "hyprland");
        assert_eq!(tracker.unicode_to_evdev('z' as u16).unwrap().evdev_key, 21);
    }

    #[test]
    fn keymap_selection_accepts_compositor_keymap_for_keyboard_seat() {
        let mut state = WlState {
            seat_has_keyboard: true,
            keymap: Some(b"xkb-keymap".to_vec()),
            ..Default::default()
        };

        let (keymap, source) = take_loaded_keymap(&mut state)
            .expect("keyboard-capable seat with keymap succeeds")
            .expect("keymap is selected");

        assert_eq!(keymap, b"xkb-keymap");
        assert_eq!(source, "compositor");
        assert!(state.keymap.is_none());
    }

    #[test]
    fn keymap_selection_rejects_keyboard_seat_without_keymap() {
        let mut state = WlState {
            seat_has_keyboard: true,
            keymap: None,
            ..Default::default()
        };

        assert!(take_loaded_keymap(&mut state).is_err());
    }

    #[test]
    fn keymap_selection_allows_fallback_for_keyboardless_seat() {
        let mut state = WlState {
            seat_has_keyboard: false,
            keymap: None,
            ..Default::default()
        };

        assert!(take_loaded_keymap(&mut state)
            .expect("keyboardless seat defers to fallback")
            .is_none());
    }

    use crate::display::geometry::{PresentationGeometry, Size};
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

    #[test]
    fn activelayout_data_parses_device_and_layout_with_commas() {
        assert_eq!(
            parse_activelayout("keychron-keychron-k8,Ukrainian"),
            Some(("keychron-keychron-k8", "Ukrainian"))
        );
        assert_eq!(
            parse_activelayout("keychron-keychron-k8,English (US, intl., with dead keys)"),
            Some((
                "keychron-keychron-k8",
                "English (US, intl., with dead keys)"
            ))
        );
        assert_eq!(parse_activelayout("no-separator"), None);
    }

    #[test]
    fn layout_events_follow_physical_devices_and_our_own_virtual_keyboard() {
        assert!(matches!(
            layout_command("keychron-keychron-k8", "Ukrainian"),
            Some(InputCommand::SetLockedLayout {
                from_own_keyboard: false,
                ..
            })
        ));
        assert!(matches!(
            layout_command("hl-virtual-keyboard-hypr-rdp", "Ukrainian"),
            Some(InputCommand::SetLockedLayout {
                from_own_keyboard: true,
                ..
            })
        ));
        assert!(matches!(
            layout_command("hl-virtual-keyboard-hypr-rdp-1", "Ukrainian"),
            Some(InputCommand::SetLockedLayout {
                from_own_keyboard: true,
                ..
            })
        ));
        assert!(layout_command("hl-virtual-keyboard-fcitx", "Ukrainian").is_none());
        assert!(layout_command("hl-virtual-keyboard-vkbd-probe", "Ukrainian").is_none());
    }

    struct ScriptedEvents {
        initial: Option<InitialLayoutCandidates>,
        script: Vec<Result<Option<(String, String)>>>,
    }

    impl ScriptedEvents {
        fn new(script: Vec<Result<Option<(String, String)>>>) -> Self {
            Self::with_initial(None, script)
        }

        fn with_initial(
            initial: Option<(&str, &str)>,
            script: Vec<Result<Option<(String, String)>>>,
        ) -> Self {
            Self {
                initial: initial.map(|(device, layout)| InitialLayoutCandidates {
                    main: Some((device.into(), layout.into())),
                    others: Vec::new(),
                }),
                // Consumed via pop() from the back.
                script: script.into_iter().rev().collect(),
            }
        }
    }

    impl LayoutEventSource for ScriptedEvents {
        fn initial_layouts(&mut self) -> Option<InitialLayoutCandidates> {
            self.initial.take()
        }

        fn next_layout_event(&mut self, _timeout: Duration) -> Result<Option<(String, String)>> {
            self.script.pop().unwrap_or(Ok(None))
        }
    }

    fn layout_event(device: &str, layout: &str) -> Result<Option<(String, String)>> {
        Ok(Some(("activelayout".into(), format!("{device},{layout}"))))
    }

    #[test]
    fn drive_forwards_physical_layout_events_and_filters_virtual_ones() {
        let mut source = ScriptedEvents::new(vec![
            layout_event("keychron-keychron-k8", "Ukrainian"),
            layout_event("hl-virtual-keyboard-fcitx", "Ukrainian"),
            Ok(Some(("monitoradded".into(), "HEADLESS-2".into()))),
            layout_event("keychron-keychron-k8", "English (US)"),
            Err(anyhow::anyhow!("socket closed")),
        ]);
        let shutdown = AtomicBool::new(false);
        let mut forwarded = Vec::new();

        let exit = drive_layout_events(
            &mut source,
            &shutdown,
            &mut |_| true,
            &mut |keyboard, layout| {
                if layout_command(keyboard, layout).is_some() {
                    forwarded.push(layout.to_string());
                }
                true
            },
        );

        assert!(matches!(exit, DriveExit::StreamFailed(_)));
        assert_eq!(forwarded, vec!["Ukrainian", "English (US)"]);
    }

    #[test]
    fn drive_exits_when_shutdown_is_requested() {
        let mut source = ScriptedEvents::with_initial(
            Some(("keychron-keychron-k8", "Ukrainian")),
            vec![layout_event("keychron-keychron-k8", "Ukrainian")],
        );
        let shutdown = AtomicBool::new(true);

        let exit = drive_layout_events(
            &mut source,
            &shutdown,
            &mut |_| panic!("must not seed after shutdown"),
            &mut |_, _| panic!("must not forward after shutdown"),
        );

        assert!(matches!(exit, DriveExit::Shutdown));
    }

    #[test]
    fn drive_seeds_initial_layout_before_events() {
        let mut source = ScriptedEvents::with_initial(
            Some(("keychron-keychron-k8", "Ukrainian")),
            vec![
                layout_event("keychron-keychron-k8", "English (US)"),
                Err(anyhow::anyhow!("socket closed")),
            ],
        );
        let shutdown = AtomicBool::new(false);
        let observed = std::cell::RefCell::new(Vec::new());

        let exit = drive_layout_events(
            &mut source,
            &shutdown,
            &mut |candidates| {
                let (_, layout) = candidates.main.expect("scripted main candidate");
                observed.borrow_mut().push(format!("seed:{layout}"));
                true
            },
            &mut |_, layout| {
                observed.borrow_mut().push(layout.to_string());
                true
            },
        );

        assert!(matches!(exit, DriveExit::StreamFailed(_)));
        assert_eq!(
            observed.into_inner(),
            vec!["seed:Ukrainian", "English (US)"]
        );
    }

    #[test]
    fn drive_exits_when_the_receiver_is_gone_during_the_seed() {
        let mut source = ScriptedEvents::with_initial(
            Some(("keychron-keychron-k8", "Ukrainian")),
            vec![layout_event("keychron-keychron-k8", "English (US)")],
        );
        let shutdown = AtomicBool::new(false);

        let exit = drive_layout_events(&mut source, &shutdown, &mut |_| false, &mut |_, _| {
            panic!("must not forward after the seed receiver is gone")
        });

        assert!(matches!(exit, DriveExit::ReceiverGone));
    }

    #[test]
    fn seed_candidates_extract_main_and_remaining_physical_keyboards() {
        let devices = serde_json::json!({
            "keyboards": [
                {"name": "hl-virtual-keyboard-hypr-rdp", "main": false, "active_keymap": "English (US)"},
                {"name": "power-button", "main": false, "active_keymap": "English (US)"},
                {"name": "keychron-keychron-k8", "main": true, "active_keymap": "Ukrainian"},
            ]
        });

        let candidates = physical_keyboard_layouts(&devices).expect("physical keyboards present");
        assert_eq!(
            candidates.main,
            Some(("keychron-keychron-k8".into(), "Ukrainian".into()))
        );
        assert_eq!(
            candidates.others,
            vec![("power-button".into(), "English (US)".into())]
        );
    }

    #[test]
    fn seed_candidates_skip_virtual_keyboards_even_when_main() {
        // A freshly created virtual keyboard can hold the main flag while
        // still sitting on its initial group.
        let devices = serde_json::json!({
            "keyboards": [
                {"name": "hl-virtual-keyboard-hypr-rdp", "main": true, "active_keymap": "English (US)"},
                {"name": "keychron-keychron-k8", "main": false, "active_keymap": "Ukrainian"},
            ]
        });

        let candidates = physical_keyboard_layouts(&devices).expect("physical keyboards present");
        assert_eq!(candidates.main, None);
        assert_eq!(
            candidates.others,
            vec![("keychron-keychron-k8".into(), "Ukrainian".into())]
        );
    }

    #[test]
    fn seed_candidates_are_none_without_physical_keyboards() {
        let devices = serde_json::json!({
            "keyboards": [
                {"name": "hl-virtual-keyboard-fcitx", "main": true, "active_keymap": "Ukrainian"},
            ]
        });

        assert!(physical_keyboard_layouts(&devices).is_none());
    }

    #[test]
    fn drive_exits_when_the_receiver_is_gone() {
        let mut source = ScriptedEvents::new(vec![
            layout_event("keychron-keychron-k8", "Ukrainian"),
            layout_event("keychron-keychron-k8", "English (US)"),
        ]);
        let shutdown = AtomicBool::new(false);

        let exit = drive_layout_events(&mut source, &shutdown, &mut |_| true, &mut |_, _| false);

        assert!(matches!(exit, DriveExit::ReceiverGone));
    }
}
