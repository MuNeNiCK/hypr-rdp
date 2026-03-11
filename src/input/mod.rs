mod keymap;
mod virtual_keyboard;

use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use ironrdp_server::{KeyboardEvent, MouseEvent, RdpServerInputHandler};
use wayland_client::protocol::wl_pointer::{Axis, AxisSource, ButtonState};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

use self::virtual_keyboard::{ZwpVirtualKeyboardManagerV1, ZwpVirtualKeyboardV1};

/// Shared state for sending input commands to the Wayland thread
struct InputState {
    conn: Connection,
    event_queue: EventQueue<WlState>,
    wl_state: WlState,
    vk: ZwpVirtualKeyboardV1,
    vp: ZwlrVirtualPointerV1,
    // Layout mapping for motion_absolute
    // motion_absolute normalizes (x/extent, y/extent) and maps to full layout
    layout_extent_w: u32,
    layout_extent_h: u32,
    output_offset_x: u32, // headless output X offset within layout (relative to min)
    output_offset_y: u32,
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

pub struct HyprInputHandler {
    state: Arc<Mutex<InputState>>,
}

impl HyprInputHandler {
    pub fn new(rdp_width: u16, rdp_height: u16, output_name: &str) -> Result<Self> {
        // Query layout from hyprctl to compute mouse coordinate mapping
        let (layout_extent_w, layout_extent_h, output_offset_x, output_offset_y) =
            query_layout(output_name)?;

        let conn =
            Connection::connect_to_env().context("failed to connect to Wayland display")?;
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

        // Generate XKB keymap using xkbcommon (same as compositor uses)
        let keymap_str = generate_xkb_keymap()?;
        let keymap_fd = create_keymap_fd(&keymap_str)?;
        vk.keymap(1, keymap_fd.as_fd(), keymap_str.len() as u32); // 1 = XKB_V1

        // Flush to send all pending requests
        conn.flush().context("Wayland flush after input setup failed")?;

        tracing::info!(
            rdp_width, rdp_height,
            layout_extent_w, layout_extent_h,
            output_offset_x, output_offset_y,
            "Input handler initialized (virtual keyboard + pointer)"
        );

        let state = Arc::new(Mutex::new(InputState {
            conn,
            event_queue,
            wl_state,
            vk,
            vp,
            layout_extent_w,
            layout_extent_h,
            output_offset_x,
            output_offset_y,
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
    let monitors: serde_json::Value = serde_json::from_slice(&output.stdout)
        .context("failed to parse hyprctl monitors")?;

    let monitors = monitors.as_array().context("expected monitors array")?;

    // Find layout bounds
    let mut min_x = i64::MAX;
    let mut min_y = i64::MAX;
    let mut max_x = i64::MIN;
    let mut max_y = i64::MIN;
    let mut target_x = 0i64;
    let mut target_y = 0i64;

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
            target_x = x;
            target_y = y;
        }
    }

    let layout_w = (max_x - min_x) as u32;
    let layout_h = (max_y - min_y) as u32;
    let offset_x = (target_x - min_x) as u32;
    let offset_y = (target_y - min_y) as u32;

    Ok((layout_w, layout_h, offset_x, offset_y))
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
                    state.flush();
                } else {
                    tracing::warn!(code, extended, "No evdev mapping for scancode");
                }
            }
            KeyboardEvent::Released { code, extended } => {
                if let Some(evdev_key) = keymap::xt_to_evdev(code, extended) {
                    state.vk.key(time, evdev_key, 0); // 0 = released
                    state.flush();
                }
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
                // Map RDP coordinates to global layout coordinates
                let abs_x = state.output_offset_x + x as u32;
                let abs_y = state.output_offset_y + y as u32;
                state.vp.motion_absolute(
                    time,
                    abs_x,
                    abs_y,
                    state.layout_extent_w,
                    state.layout_extent_h,
                );
                state.vp.frame();
                state.flush();
            }
            MouseEvent::LeftPressed => {
                state.vp.button(time, keymap::BTN_LEFT, ButtonState::Pressed);
                state.vp.frame();
                state.flush();
            }
            MouseEvent::LeftReleased => {
                state.vp.button(time, keymap::BTN_LEFT, ButtonState::Released);
                state.vp.frame();
                state.flush();
            }
            MouseEvent::RightPressed => {
                state.vp.button(time, keymap::BTN_RIGHT, ButtonState::Pressed);
                state.vp.frame();
                state.flush();
            }
            MouseEvent::RightReleased => {
                state.vp.button(time, keymap::BTN_RIGHT, ButtonState::Released);
                state.vp.frame();
                state.flush();
            }
            MouseEvent::MiddlePressed => {
                state.vp.button(time, keymap::BTN_MIDDLE, ButtonState::Pressed);
                state.vp.frame();
                state.flush();
            }
            MouseEvent::MiddleReleased => {
                state.vp.button(time, keymap::BTN_MIDDLE, ButtonState::Released);
                state.vp.frame();
                state.flush();
            }
            MouseEvent::Button4Pressed => {
                state.vp.button(time, keymap::BTN_SIDE, ButtonState::Pressed);
                state.vp.frame();
                state.flush();
            }
            MouseEvent::Button4Released => {
                state.vp.button(time, keymap::BTN_SIDE, ButtonState::Released);
                state.vp.frame();
                state.flush();
            }
            MouseEvent::Button5Pressed => {
                state.vp.button(time, keymap::BTN_EXTRA, ButtonState::Pressed);
                state.vp.frame();
                state.flush();
            }
            MouseEvent::Button5Released => {
                state.vp.button(time, keymap::BTN_EXTRA, ButtonState::Released);
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
            MouseEvent::RelMove { x, y } => {
                state.vp.motion(time, x as f64, y as f64);
                state.vp.frame();
                state.flush();
            }
            _ => {}
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

/// Generate XKB keymap using xkbcommon (matching compositor's format)
fn generate_xkb_keymap() -> Result<String> {
    use xkbcommon::xkb;
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = xkb::Keymap::new_from_names(
        &context,
        "", // rules: system default
        "", // model: system default
        "", // layout: system default
        "", // variant: system default
        None, // options
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .context("Failed to compile XKB keymap from system defaults")?;
    let keymap_string = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
    if keymap_string.is_empty() {
        bail!("XKB keymap generation returned empty string");
    }
    tracing::debug!(len = keymap_string.len(), "Generated XKB keymap");
    Ok(keymap_string)
}

fn create_keymap_fd(keymap: &str) -> Result<OwnedFd> {
    use std::ffi::CStr;
    let name = CStr::from_bytes_with_nul(b"hypr-rdp-keymap\0").unwrap();
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
    if fd < 0 {
        bail!("memfd_create failed");
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let data = keymap.as_bytes();
    let written = unsafe { libc::write(fd.as_raw_fd(), data.as_ptr() as *const _, data.len()) };
    if written != data.len() as isize {
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

// No-op dispatchers
delegate_noop!(WlState: ignore wl_seat::WlSeat);
delegate_noop!(WlState: ignore ZwpVirtualKeyboardManagerV1);
delegate_noop!(WlState: ignore ZwpVirtualKeyboardV1);
delegate_noop!(WlState: ignore ZwlrVirtualPointerManagerV1);
delegate_noop!(WlState: ignore ZwlrVirtualPointerV1);

impl Dispatch<wayland_client::protocol::wl_display::WlDisplay, ()> for WlState {
    fn event(_: &mut Self, _: &wayland_client::protocol::wl_display::WlDisplay, _: wayland_client::protocol::wl_display::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<wayland_client::protocol::wl_callback::WlCallback, ()> for WlState {
    fn event(_: &mut Self, _: &wayland_client::protocol::wl_callback::WlCallback, _: wayland_client::protocol::wl_callback::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
