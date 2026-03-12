mod keymap;
mod virtual_keyboard;

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use ironrdp_pdu::input::fast_path::SynchronizeFlags;
use ironrdp_server::{KeyboardEvent, MouseEvent, RdpServerInputHandler};
use wayland_client::protocol::wl_pointer::{Axis, AxisSource, ButtonState};
use wayland_client::protocol::{wl_keyboard, wl_registry, wl_seat};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, QueueHandle, WEnum};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};
use xkbcommon::xkb;

use self::virtual_keyboard::{ZwpVirtualKeyboardManagerV1, ZwpVirtualKeyboardV1};

const XKB_KEYCODE_OFFSET: u32 = 8;
const KEY_CAPSLOCK: u32 = 58;
const KEY_NUMLOCK: u32 = 69;
const KEY_SCROLLLOCK: u32 = 70;
const KEY_KATAKANAHIRAGANA: u32 = 93;

#[derive(Clone, Debug)]
struct OutputLayoutSnapshot {
    output_name: String,
    layout_extent_w: u32,
    layout_extent_h: u32,
    output_offset_x: u32,
    output_offset_y: u32,
}

#[derive(Debug, Default)]
pub struct SharedOutputLayout {
    inner: Mutex<Option<OutputLayoutSnapshot>>,
}

impl SharedOutputLayout {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update_from_output(&self, output_name: &str) -> Result<()> {
        let (layout_extent_w, layout_extent_h, output_offset_x, output_offset_y) =
            query_layout(output_name)?;
        let snapshot = OutputLayoutSnapshot {
            output_name: output_name.to_string(),
            layout_extent_w,
            layout_extent_h,
            output_offset_x,
            output_offset_y,
        };
        tracing::info!(
            output = %snapshot.output_name,
            layout_extent_w = snapshot.layout_extent_w,
            layout_extent_h = snapshot.layout_extent_h,
            output_offset_x = snapshot.output_offset_x,
            output_offset_y = snapshot.output_offset_y,
            "Updated input layout mapping"
        );
        *self.inner.lock().unwrap() = Some(snapshot);
        Ok(())
    }

    fn snapshot(&self) -> Option<OutputLayoutSnapshot> {
        self.inner.lock().unwrap().clone()
    }
}

/// Shared state for sending input commands to the Wayland thread
struct InputState {
    conn: Connection,
    event_queue: EventQueue<WlState>,
    wl_state: WlState,
    vk: ZwpVirtualKeyboardV1,
    vp: ZwlrVirtualPointerV1,
    keyboard_state: KeyboardStateTracker,
    output_layout: Arc<SharedOutputLayout>,
}

impl InputState {
    /// Drain pending Wayland events and flush outgoing requests
    fn flush(&mut self) {
        // Read any pending events from the compositor
        if let Some(guard) = self.conn.prepare_read() {
            let _ = guard.read();
        }
        let _ = self.event_queue.dispatch_pending(&mut self.wl_state);
        if let Err(e) = self.conn.flush() {
            tracing::error!("Wayland flush failed: {}", e);
        }
    }
}

struct KeyboardStateTracker {
    modifier_masks_by_key: HashMap<u32, u32>,
    pressed_keys: HashSet<u32>,
    depressed_mods: u32,
    locked_mods: u32,
    caps_lock_mask: u32,
    num_lock_mask: u32,
    scroll_lock_mask: u32,
    kana_lock_mask: u32,
}

impl KeyboardStateTracker {
    fn new(keymap_data: &[u8]) -> Result<Self> {
        let keymap = compile_xkb_keymap(keymap_data)?;

        Ok(Self {
            modifier_masks_by_key: build_modifier_masks_by_key(&keymap),
            pressed_keys: HashSet::new(),
            depressed_mods: 0,
            locked_mods: 0,
            caps_lock_mask: locked_mask_for_key(&keymap, KEY_CAPSLOCK),
            num_lock_mask: locked_mask_for_key(&keymap, KEY_NUMLOCK),
            scroll_lock_mask: locked_mask_for_key(&keymap, KEY_SCROLLLOCK),
            kana_lock_mask: locked_mask_for_key(&keymap, KEY_KATAKANAHIRAGANA),
        })
    }

    fn key(&mut self, evdev_key: u32, pressed: bool) {
        if pressed {
            self.pressed_keys.insert(evdev_key);
            let lock_mask = self.lock_mask_for_key(evdev_key);
            if lock_mask != 0 {
                self.locked_mods ^= lock_mask;
            }
        } else {
            self.pressed_keys.remove(&evdev_key);
        }

        self.depressed_mods = self
            .pressed_keys
            .iter()
            .filter_map(|key| self.modifier_masks_by_key.get(key))
            .fold(0, |mods, mask| mods | *mask);
    }

    fn synchronize_locks(&mut self, flags: SynchronizeFlags) {
        self.locked_mods = self.locked_mods_from_flags(flags);
    }

    fn send_modifiers(&self, vk: &ZwpVirtualKeyboardV1) {
        vk.modifiers(self.depressed_mods, 0, self.locked_mods, 0);
    }

    fn locked_mods_from_flags(&self, flags: SynchronizeFlags) -> u32 {
        let mut locked_mods = 0;

        if flags.contains(SynchronizeFlags::CAPS_LOCK) {
            locked_mods |= self.caps_lock_mask;
        }
        if flags.contains(SynchronizeFlags::NUM_LOCK) {
            locked_mods |= self.num_lock_mask;
        }
        if flags.contains(SynchronizeFlags::SCROLL_LOCK) {
            locked_mods |= self.scroll_lock_mask;
        }
        if flags.contains(SynchronizeFlags::KANA_LOCK) {
            locked_mods |= self.kana_lock_mask;
        }

        locked_mods
    }

    fn lock_mask_for_key(&self, evdev_key: u32) -> u32 {
        match evdev_key {
            KEY_CAPSLOCK => self.caps_lock_mask,
            KEY_NUMLOCK => self.num_lock_mask,
            KEY_SCROLLLOCK => self.scroll_lock_mask,
            KEY_KATAKANAHIRAGANA => self.kana_lock_mask,
            _ => 0,
        }
    }
}

pub struct HyprInputHandler {
    state: Arc<Mutex<InputState>>,
}

impl HyprInputHandler {
    pub fn new(
        rdp_width: u16,
        rdp_height: u16,
        output_layout: Arc<SharedOutputLayout>,
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

        // Create virtual pointer
        let vp = vp_mgr.create_virtual_pointer(Some(&seat), &qh, ());

        let (keymap_data, keymap_source) = load_keymap(&mut event_queue, &mut wl_state, &seat, &qh)
            .or_else(|err| {
                tracing::warn!(
                    "Failed to get compositor keymap, falling back to default: {:#}",
                    err
                );
                generate_xkb_keymap().map(|data| (data, "fallback"))
            })?;
        let keyboard_state = KeyboardStateTracker::new(&keymap_data)?;
        let keymap_fd = create_keymap_fd(&keymap_data)?;
        vk.keymap(1, keymap_fd.as_fd(), keymap_data.len() as u32); // 1 = XKB_V1
        keyboard_state.send_modifiers(&vk);

        // Flush to send all pending requests
        conn.flush()
            .context("Wayland flush after input setup failed")?;

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

        let state = Arc::new(Mutex::new(InputState {
            conn,
            event_queue,
            wl_state,
            vk,
            vp,
            keyboard_state,
            output_layout,
        }));

        Ok(Self { state })
    }
}

/// Query Hyprland monitor layout to compute coordinate mapping.
/// Returns (layout_total_w, layout_total_h, output_offset_x, output_offset_y)
fn query_layout(output_name: &str) -> Result<(u32, u32, u32, u32)> {
    use std::process::Command;
    let output = Command::new("hyprctl")
        .args(["monitors", "-j"])
        .output()
        .context("failed to run hyprctl monitors")?;
    if !output.status.success() {
        bail!(
            "hyprctl monitors failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let monitors: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("failed to parse hyprctl monitors")?;

    let monitors = monitors.as_array().context("expected monitors array")?;
    if monitors.is_empty() {
        bail!("hyprctl monitors returned no outputs");
    }

    // Find layout bounds
    let mut min_x = i64::MAX;
    let mut min_y = i64::MAX;
    let mut max_x = i64::MIN;
    let mut max_y = i64::MIN;
    let mut target = None;

    for m in monitors {
        let x = m["x"].as_i64().unwrap_or(0);
        let y = m["y"].as_i64().unwrap_or(0);
        let w = m["width"].as_i64().unwrap_or(0);
        let h = m["height"].as_i64().unwrap_or(0);
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x + w);
        max_y = max_y.max(y + h);

        if m["name"].as_str() == Some(output_name) {
            target = Some((x, y));
        }
    }

    let (target_x, target_y) = target.context(format!("output '{}' not found", output_name))?;
    let layout_w = (max_x - min_x) as u32;
    let layout_h = (max_y - min_y) as u32;
    if layout_w == 0 || layout_h == 0 {
        bail!("invalid layout bounds: {}x{}", layout_w, layout_h);
    }
    let offset_x = (target_x - min_x) as u32;
    let offset_y = (target_y - min_y) as u32;

    Ok((layout_w, layout_h, offset_x, offset_y))
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

        if let Some(keymap_data) = wl_state.keymap.take() {
            tracing::info!(len = keymap_data.len(), "Loaded compositor keyboard keymap");
            return Ok((keymap_data, "compositor"));
        }
    } else {
        tracing::warn!("Wayland seat has no keyboard capability, using fallback keymap");
    }

    let fallback = generate_xkb_keymap()?;
    tracing::info!(len = fallback.len(), "Generated fallback keyboard keymap");
    Ok((fallback, "fallback"))
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

impl RdpServerInputHandler for HyprInputHandler {
    fn keyboard(&mut self, event: KeyboardEvent) {
        tracing::debug!(?event, "RDP keyboard event received");
        let mut state = self.state.lock().unwrap();
        let time = timestamp_ms();

        match event {
            KeyboardEvent::Pressed { code, extended } => {
                if let Some(evdev_key) = keymap::xt_to_evdev(code, extended) {
                    state.vk.key(time, evdev_key, 1); // 1 = pressed
                    state.keyboard_state.key(evdev_key, true);
                    state.keyboard_state.send_modifiers(&state.vk);
                    state.flush();
                } else {
                    tracing::warn!(code, extended, "No evdev mapping for scancode");
                }
            }
            KeyboardEvent::Released { code, extended } => {
                if let Some(evdev_key) = keymap::xt_to_evdev(code, extended) {
                    state.vk.key(time, evdev_key, 0); // 0 = released
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
            _ => {}
        }
    }

    fn mouse(&mut self, event: MouseEvent) {
        tracing::debug!(?event, "RDP mouse event received");
        let mut state = self.state.lock().unwrap();
        let time = timestamp_ms();

        match event {
            MouseEvent::Move { x, y } => {
                let Some(layout) = state.output_layout.snapshot() else {
                    tracing::warn!("Skipping mouse move: output layout not initialized");
                    return;
                };
                // Map RDP coordinates to global layout coordinates
                let abs_x = layout.output_offset_x + x as u32;
                let abs_y = layout.output_offset_y + y as u32;
                state.vp.motion_absolute(
                    time,
                    abs_x,
                    abs_y,
                    layout.layout_extent_w,
                    layout.layout_extent_h,
                );
                state.vp.frame();
                state.flush();
            }
            MouseEvent::LeftPressed => {
                state
                    .vp
                    .button(time, keymap::BTN_LEFT, ButtonState::Pressed);
                state.vp.frame();
                state.flush();
            }
            MouseEvent::LeftReleased => {
                state
                    .vp
                    .button(time, keymap::BTN_LEFT, ButtonState::Released);
                state.vp.frame();
                state.flush();
            }
            MouseEvent::RightPressed => {
                state
                    .vp
                    .button(time, keymap::BTN_RIGHT, ButtonState::Pressed);
                state.vp.frame();
                state.flush();
            }
            MouseEvent::RightReleased => {
                state
                    .vp
                    .button(time, keymap::BTN_RIGHT, ButtonState::Released);
                state.vp.frame();
                state.flush();
            }
            MouseEvent::MiddlePressed => {
                state
                    .vp
                    .button(time, keymap::BTN_MIDDLE, ButtonState::Pressed);
                state.vp.frame();
                state.flush();
            }
            MouseEvent::MiddleReleased => {
                state
                    .vp
                    .button(time, keymap::BTN_MIDDLE, ButtonState::Released);
                state.vp.frame();
                state.flush();
            }
            MouseEvent::Button4Pressed => {
                state
                    .vp
                    .button(time, keymap::BTN_SIDE, ButtonState::Pressed);
                state.vp.frame();
                state.flush();
            }
            MouseEvent::Button4Released => {
                state
                    .vp
                    .button(time, keymap::BTN_SIDE, ButtonState::Released);
                state.vp.frame();
                state.flush();
            }
            MouseEvent::Button5Pressed => {
                state
                    .vp
                    .button(time, keymap::BTN_EXTRA, ButtonState::Pressed);
                state.vp.frame();
                state.flush();
            }
            MouseEvent::Button5Released => {
                state
                    .vp
                    .button(time, keymap::BTN_EXTRA, ButtonState::Released);
                state.vp.frame();
                state.flush();
            }
            MouseEvent::VerticalScroll { value } => {
                let axis_value = (value as f64 / 120.0) * 15.0;
                state.vp.axis_source(AxisSource::Wheel);
                state.vp.axis(time, Axis::VerticalScroll, axis_value);
                state.vp.frame();
                state.flush();
            }
            MouseEvent::Scroll { x, y } => {
                state.vp.axis_source(AxisSource::Wheel);
                if y != 0 {
                    state
                        .vp
                        .axis(time, Axis::VerticalScroll, (y as f64 / 120.0) * 15.0);
                }
                if x != 0 {
                    state
                        .vp
                        .axis(time, Axis::HorizontalScroll, (x as f64 / 120.0) * 15.0);
                }
                state.vp.frame();
                state.flush();
            }
            MouseEvent::RelMove { x, y } => {
                state.vp.motion(time, x as f64, y as f64);
                state.vp.frame();
                state.flush();
            }
        }
    }
}

fn timestamp_ms() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u32
}

fn compile_xkb_keymap(keymap_data: &[u8]) -> Result<xkb::Keymap> {
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap_text =
        String::from_utf8(keymap_data.to_vec()).context("Wayland keymap is not valid UTF-8")?;
    xkb::Keymap::new_from_string(
        &context,
        keymap_text,
        xkb::KEYMAP_FORMAT_TEXT_V1,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .context("failed to compile XKB keymap from Wayland keymap data")
}

fn build_modifier_masks_by_key(keymap: &xkb::Keymap) -> HashMap<u32, u32> {
    let mut masks = HashMap::new();

    for evdev_key in [29, 42, 54, 56, 97, 100, 125, 126] {
        let mask = depressed_mask_for_key(keymap, evdev_key);
        if mask != 0 {
            masks.insert(evdev_key, mask);
        }
    }

    masks
}

fn depressed_mask_for_key(keymap: &xkb::Keymap, evdev_key: u32) -> u32 {
    let mut state = xkb::State::new(keymap);
    let keycode = xkb::Keycode::new(evdev_key + XKB_KEYCODE_OFFSET);
    state.update_key(keycode, xkb::KeyDirection::Down);
    state.serialize_mods(xkb::STATE_MODS_DEPRESSED)
}

fn locked_mask_for_key(keymap: &xkb::Keymap, evdev_key: u32) -> u32 {
    let mut state = xkb::State::new(keymap);
    let keycode = xkb::Keycode::new(evdev_key + XKB_KEYCODE_OFFSET);
    state.update_key(keycode, xkb::KeyDirection::Down);
    state.serialize_mods(xkb::STATE_MODS_LOCKED)
}

/// Generate XKB keymap using xkbcommon (matching compositor's format)
fn generate_xkb_keymap() -> Result<Vec<u8>> {
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = xkb::Keymap::new_from_names(
        &context,
        "",   // rules: system default
        "",   // model: system default
        "",   // layout: system default
        "",   // variant: system default
        None, // options
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .context("Failed to compile XKB keymap from system defaults")?;
    let keymap_data = keymap
        .get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)
        .into_bytes();
    if keymap_data.is_empty() {
        bail!("XKB keymap generation returned empty string");
    }
    tracing::debug!(len = keymap_data.len(), "Generated XKB keymap");
    Ok(keymap_data)
}

fn create_keymap_fd(keymap: &[u8]) -> Result<OwnedFd> {
    let fd = unsafe {
        libc::memfd_create(
            c"hypr-rdp-keymap".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    if fd < 0 {
        bail!("memfd_create failed");
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let written = unsafe { libc::write(fd.as_raw_fd(), keymap.as_ptr() as *const _, keymap.len()) };
    if written != keymap.len() as isize {
        bail!("failed to write keymap");
    }
    // Seek back to beginning so compositor can read from start
    unsafe { libc::lseek(fd.as_raw_fd(), 0, libc::SEEK_SET) };
    Ok(fd)
}

// --- Wayland state for binding globals ---

#[derive(Default)]
struct WlState {
    seat: Option<wl_seat::WlSeat>,
    seat_has_keyboard: bool,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    keymap: Option<Vec<u8>>,
    vk_manager: Option<ZwpVirtualKeyboardManagerV1>,
    vp_manager: Option<ZwlrVirtualPointerManagerV1>,
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
                "wl_seat" => {
                    if state.seat.is_none() {
                        state.seat = Some(registry.bind(name, version.min(7), qh, ()));
                    }
                }
                "zwp_virtual_keyboard_manager_v1" => {
                    state.vk_manager = Some(registry.bind(name, version.min(1), qh, ()));
                }
                "zwlr_virtual_pointer_manager_v1" => {
                    state.vp_manager = Some(registry.bind(name, version.min(2), qh, ()));
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
