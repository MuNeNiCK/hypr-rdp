mod keymap;
mod virtual_keyboard;

use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use ironrdp_server::{KeyboardEvent, MouseEvent, RdpServerInputHandler};
use wayland_client::protocol::wl_pointer::{Axis, AxisSource, ButtonState};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{delegate_noop, Connection, Dispatch, QueueHandle};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

use self::virtual_keyboard::{ZwpVirtualKeyboardManagerV1, ZwpVirtualKeyboardV1};

/// Shared state for sending input commands to the Wayland thread
struct InputState {
    conn: Connection,
    vk: ZwpVirtualKeyboardV1,
    vp: ZwlrVirtualPointerV1,
    screen_width: u32,
    screen_height: u32,
}

pub struct HyprInputHandler {
    state: Arc<Mutex<InputState>>,
}

impl HyprInputHandler {
    pub fn new() -> Result<Self> {
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

        let seat = wl_state.seat.context("wl_seat not found")?;
        let vk_mgr = wl_state
            .vk_manager
            .context("zwp_virtual_keyboard_manager_v1 not found")?;
        let vp_mgr = wl_state
            .vp_manager
            .context("zwlr_virtual_pointer_manager_v1 not found")?;

        // Create virtual keyboard
        let vk = vk_mgr.create_virtual_keyboard(&seat, &qh, ());

        // Set a minimal XKB keymap
        let keymap_str = minimal_xkb_keymap();
        let keymap_size = keymap_str.len() + 1; // +1 for null terminator
        let keymap_fd = create_keymap_fd(&keymap_str)?;
        vk.keymap(1, keymap_fd.as_fd(), keymap_size as u32); // 1 = XKB_V1

        // Create virtual pointer
        let vp = vp_mgr.create_virtual_pointer(Some(&seat), &qh, ());

        // Flush to send all requests
        conn.flush().context("failed to flush Wayland connection")?;

        // TODO: get actual screen size from wl_output
        let screen_width = 3840;
        let screen_height = 1080;

        tracing::info!("Input handler initialized (virtual keyboard + pointer)");

        let state = Arc::new(Mutex::new(InputState {
            conn,
            vk,
            vp,
            screen_width,
            screen_height,
        }));

        Ok(Self { state })
    }
}

impl RdpServerInputHandler for HyprInputHandler {
    fn keyboard(&mut self, event: KeyboardEvent) {
        let state = self.state.lock().unwrap();
        let time = timestamp_ms();

        match event {
            KeyboardEvent::Pressed { code, extended } => {
                if let Some(evdev_key) = keymap::xt_to_evdev(code, extended) {
                    state.vk.key(time, evdev_key, 1); // 1 = pressed
                    let _ = state.conn.flush();
                }
            }
            KeyboardEvent::Released { code, extended } => {
                if let Some(evdev_key) = keymap::xt_to_evdev(code, extended) {
                    state.vk.key(time, evdev_key, 0); // 0 = released
                    let _ = state.conn.flush();
                }
            }
            _ => {}
        }
    }

    fn mouse(&mut self, event: MouseEvent) {
        let state = self.state.lock().unwrap();
        let time = timestamp_ms();

        match event {
            MouseEvent::Move { x, y } => {
                state.vp.motion_absolute(
                    time,
                    x as u32,
                    y as u32,
                    state.screen_width,
                    state.screen_height,
                );
                state.vp.frame();
                let _ = state.conn.flush();
            }
            MouseEvent::LeftPressed => {
                state.vp.button(time, keymap::BTN_LEFT, ButtonState::Pressed);
                state.vp.frame();
                let _ = state.conn.flush();
            }
            MouseEvent::LeftReleased => {
                state.vp.button(time, keymap::BTN_LEFT, ButtonState::Released);
                state.vp.frame();
                let _ = state.conn.flush();
            }
            MouseEvent::RightPressed => {
                state.vp.button(time, keymap::BTN_RIGHT, ButtonState::Pressed);
                state.vp.frame();
                let _ = state.conn.flush();
            }
            MouseEvent::RightReleased => {
                state.vp.button(time, keymap::BTN_RIGHT, ButtonState::Released);
                state.vp.frame();
                let _ = state.conn.flush();
            }
            MouseEvent::MiddlePressed => {
                state.vp.button(time, keymap::BTN_MIDDLE, ButtonState::Pressed);
                state.vp.frame();
                let _ = state.conn.flush();
            }
            MouseEvent::MiddleReleased => {
                state.vp.button(time, keymap::BTN_MIDDLE, ButtonState::Released);
                state.vp.frame();
                let _ = state.conn.flush();
            }
            MouseEvent::Button4Pressed => {
                state.vp.button(time, keymap::BTN_SIDE, ButtonState::Pressed);
                state.vp.frame();
                let _ = state.conn.flush();
            }
            MouseEvent::Button4Released => {
                state.vp.button(time, keymap::BTN_SIDE, ButtonState::Released);
                state.vp.frame();
                let _ = state.conn.flush();
            }
            MouseEvent::Button5Pressed => {
                state.vp.button(time, keymap::BTN_EXTRA, ButtonState::Pressed);
                state.vp.frame();
                let _ = state.conn.flush();
            }
            MouseEvent::Button5Released => {
                state.vp.button(time, keymap::BTN_EXTRA, ButtonState::Released);
                state.vp.frame();
                let _ = state.conn.flush();
            }
            MouseEvent::VerticalScroll { value } => {
                let axis_value = (value as f64 / 120.0) * 15.0;
                state.vp.axis_source(AxisSource::Wheel);
                state.vp.axis(time, Axis::VerticalScroll, axis_value);
                state.vp.frame();
                let _ = state.conn.flush();
            }
            MouseEvent::RelMove { x, y } => {
                state.vp.motion(time, x as f64, y as f64);
                state.vp.frame();
                let _ = state.conn.flush();
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

/// Create a minimal XKB keymap that maps evdev keycodes
fn minimal_xkb_keymap() -> String {
    // This is a minimal but complete XKB keymap that uses evdev keycodes
    r#"xkb_keymap {
  xkb_keycodes "evdev" {
    minimum = 8;
    maximum = 255;
    <ESC>  = 9;
    <AE01> = 10; <AE02> = 11; <AE03> = 12; <AE04> = 13;
    <AE05> = 14; <AE06> = 15; <AE07> = 16; <AE08> = 17;
    <AE09> = 18; <AE10> = 19; <AE11> = 20; <AE12> = 21;
    <BKSP> = 22; <TAB>  = 23;
    <AD01> = 24; <AD02> = 25; <AD03> = 26; <AD04> = 27;
    <AD05> = 28; <AD06> = 29; <AD07> = 30; <AD08> = 31;
    <AD09> = 32; <AD10> = 33; <AD11> = 34; <AD12> = 35;
    <RTRN> = 36; <LCTL> = 37;
    <AC01> = 38; <AC02> = 39; <AC03> = 40; <AC04> = 41;
    <AC05> = 42; <AC06> = 43; <AC07> = 44; <AC08> = 45;
    <AC09> = 46; <AC10> = 47; <AC11> = 48; <TLDE> = 49;
    <LFSH> = 50; <BKSL> = 51;
    <AB01> = 52; <AB02> = 53; <AB03> = 54; <AB04> = 55;
    <AB05> = 56; <AB06> = 57; <AB07> = 58; <AB08> = 59;
    <AB09> = 60; <AB10> = 61; <RTSH> = 62;
    <KPMU> = 63; <LALT> = 64; <SPCE> = 65; <CAPS> = 66;
    <FK01> = 67; <FK02> = 68; <FK03> = 69; <FK04> = 70;
    <FK05> = 71; <FK06> = 72; <FK07> = 73; <FK08> = 74;
    <FK09> = 75; <FK10> = 76;
    <NMLK> = 77; <SCLK> = 78;
    <KP7>  = 79; <KP8>  = 80; <KP9>  = 81; <KPSU> = 82;
    <KP4>  = 83; <KP5>  = 84; <KP6>  = 85; <KPAD> = 86;
    <KP1>  = 87; <KP2>  = 88; <KP3>  = 89; <KP0>  = 90;
    <KPDL> = 91;
    <FK11> = 95; <FK12> = 96;
    <KPEN> = 104; <RCTL> = 105; <KPDV> = 106;
    <PRSC> = 107; <RALT> = 108; <PAUS> = 127;
    <HOME> = 110; <UP>   = 111; <PGUP> = 112;
    <LEFT> = 113; <RGHT> = 114;
    <END>  = 115; <DOWN> = 116; <PGDN> = 117;
    <INS>  = 118; <DELE> = 119;
    <LWIN> = 133; <RWIN> = 134; <COMP> = 135;
  };
  xkb_types "complete" {
    type "ONE_LEVEL" { modifiers= none; level_name[Level1]= "Any"; };
    type "TWO_LEVEL" { modifiers= Shift; map[Shift]= Level2; level_name[Level1]= "Base"; level_name[Level2]= "Shift"; };
    type "ALPHABETIC" { modifiers= Shift+Lock; map[Shift]= Level2; map[Lock]= Level2; level_name[Level1]= "Base"; level_name[Level2]= "Caps"; };
    type "KEYPAD" { modifiers= Shift+NumLock; map[Shift]= Level2; map[NumLock]= Level2; level_name[Level1]= "Base"; level_name[Level2]= "Number"; };
  };
  xkb_compatibility "complete" {
    interpret Any+AnyOf(all) { action= SetMods(modifiers=modMapMods,clearLocks); };
    interpret Shift_L+AnyOf(all) { action= SetMods(modifiers=Shift,clearLocks); };
    interpret Num_Lock+AnyOf(all) { action= LockMods(modifiers=NumLock); };
    interpret Caps_Lock+AnyOf(all) { action= LockMods(modifiers=Lock); };
  };
  xkb_symbols "us" {
    name[group1]="English (US)";
    key <ESC>  { [ Escape ] };
    key <AE01> { [ 1, exclam ] };
    key <AE02> { [ 2, at ] };
    key <AE03> { [ 3, numbersign ] };
    key <AE04> { [ 4, dollar ] };
    key <AE05> { [ 5, percent ] };
    key <AE06> { [ 6, asciicircum ] };
    key <AE07> { [ 7, ampersand ] };
    key <AE08> { [ 8, asterisk ] };
    key <AE09> { [ 9, parenleft ] };
    key <AE10> { [ 0, parenright ] };
    key <AE11> { [ minus, underscore ] };
    key <AE12> { [ equal, plus ] };
    key <BKSP> { [ BackSpace ] };
    key <TAB>  { [ Tab, ISO_Left_Tab ] };
    key <AD01> { [ q, Q ] }; key <AD02> { [ w, W ] };
    key <AD03> { [ e, E ] }; key <AD04> { [ r, R ] };
    key <AD05> { [ t, T ] }; key <AD06> { [ y, Y ] };
    key <AD07> { [ u, U ] }; key <AD08> { [ i, I ] };
    key <AD09> { [ o, O ] }; key <AD10> { [ p, P ] };
    key <AD11> { [ bracketleft, braceleft ] };
    key <AD12> { [ bracketright, braceright ] };
    key <RTRN> { [ Return ] };
    key <LCTL> { [ Control_L ] };
    key <AC01> { [ a, A ] }; key <AC02> { [ s, S ] };
    key <AC03> { [ d, D ] }; key <AC04> { [ f, F ] };
    key <AC05> { [ g, G ] }; key <AC06> { [ h, H ] };
    key <AC07> { [ j, J ] }; key <AC08> { [ k, K ] };
    key <AC09> { [ l, L ] }; key <AC10> { [ semicolon, colon ] };
    key <AC11> { [ apostrophe, quotedbl ] };
    key <TLDE> { [ grave, asciitilde ] };
    key <LFSH> { [ Shift_L ] };
    key <BKSL> { [ backslash, bar ] };
    key <AB01> { [ z, Z ] }; key <AB02> { [ x, X ] };
    key <AB03> { [ c, C ] }; key <AB04> { [ v, V ] };
    key <AB05> { [ b, B ] }; key <AB06> { [ n, N ] };
    key <AB07> { [ m, M ] }; key <AB08> { [ comma, less ] };
    key <AB09> { [ period, greater ] }; key <AB10> { [ slash, question ] };
    key <RTSH> { [ Shift_R ] };
    key <KPMU> { [ KP_Multiply ] };
    key <LALT> { [ Alt_L, Meta_L ] };
    key <SPCE> { [ space ] };
    key <CAPS> { [ Caps_Lock ] };
    key <FK01> { [ F1 ] }; key <FK02> { [ F2 ] };
    key <FK03> { [ F3 ] }; key <FK04> { [ F4 ] };
    key <FK05> { [ F5 ] }; key <FK06> { [ F6 ] };
    key <FK07> { [ F7 ] }; key <FK08> { [ F8 ] };
    key <FK09> { [ F9 ] }; key <FK10> { [ F10 ] };
    key <FK11> { [ F11 ] }; key <FK12> { [ F12 ] };
    key <NMLK> { [ Num_Lock ] }; key <SCLK> { [ Scroll_Lock ] };
    key <KP7>  { [ KP_Home, KP_7 ] }; key <KP8> { [ KP_Up, KP_8 ] };
    key <KP9>  { [ KP_Prior, KP_9 ] }; key <KPSU> { [ KP_Subtract ] };
    key <KP4>  { [ KP_Left, KP_4 ] }; key <KP5> { [ KP_Begin, KP_5 ] };
    key <KP6>  { [ KP_Right, KP_6 ] }; key <KPAD> { [ KP_Add ] };
    key <KP1>  { [ KP_End, KP_1 ] }; key <KP2> { [ KP_Down, KP_2 ] };
    key <KP3>  { [ KP_Next, KP_3 ] }; key <KP0> { [ KP_Insert, KP_0 ] };
    key <KPDL> { [ KP_Delete, KP_Decimal ] };
    key <KPEN> { [ KP_Enter ] }; key <RCTL> { [ Control_R ] };
    key <KPDV> { [ KP_Divide ] }; key <PRSC> { [ Print ] };
    key <RALT> { [ Alt_R, Meta_R ] }; key <PAUS> { [ Pause ] };
    key <HOME> { [ Home ] }; key <UP>   { [ Up ] };
    key <PGUP> { [ Prior ] }; key <LEFT> { [ Left ] };
    key <RGHT> { [ Right ] }; key <END>  { [ End ] };
    key <DOWN> { [ Down ] }; key <PGDN> { [ Next ] };
    key <INS>  { [ Insert ] }; key <DELE> { [ Delete ] };
    key <LWIN> { [ Super_L ] }; key <RWIN> { [ Super_R ] };
    key <COMP> { [ Menu ] };
    modifier_map Shift { <LFSH>, <RTSH> };
    modifier_map Lock { <CAPS> };
    modifier_map Control { <LCTL>, <RCTL> };
    modifier_map Mod1 { <LALT>, <RALT> };
    modifier_map Mod2 { <NMLK> };
    modifier_map Mod4 { <LWIN>, <RWIN> };
  };
};
"#
    .to_string()
}

fn create_keymap_fd(keymap: &str) -> Result<OwnedFd> {
    use std::ffi::CStr;
    let name = CStr::from_bytes_with_nul(b"hypr-rdp-keymap\0").unwrap();
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        bail!("memfd_create failed");
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // Write keymap including null terminator
    let data = keymap.as_bytes();
    let written = unsafe { libc::write(fd.as_raw_fd(), data.as_ptr() as *const _, data.len()) };
    if written != data.len() as isize {
        bail!("failed to write keymap");
    }
    // Write null terminator
    let null = [0u8];
    unsafe { libc::write(fd.as_raw_fd(), null.as_ptr() as *const _, 1) };
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
