/// Maps RDP XT Set 1 scancodes to Linux evdev keycodes.
///
/// For non-extended keys, XT scancodes are numerically identical to evdev keycodes (1-88).
/// For extended keys (0xE0 prefix), a lookup table is needed.
pub fn xt_to_evdev(code: u8, extended: bool) -> Option<u32> {
    if !extended {
        // Non-extended: identity mapping for the basic range
        Some(code as u32)
    } else {
        // Extended keys (0xE0 prefix)
        xt_extended_to_evdev(code)
    }
}

fn xt_extended_to_evdev(code: u8) -> Option<u32> {
    let evdev = match code {
        0x1C => 96,  // KEY_KPENTER
        0x1D => 97,  // KEY_RIGHTCTRL
        0x35 => 98,  // KEY_KPSLASH
        0x37 => 99,  // KEY_SYSRQ (PrintScreen)
        0x38 => 100, // KEY_RIGHTALT
        0x46 => 119, // KEY_PAUSE
        0x47 => 102, // KEY_HOME
        0x48 => 103, // KEY_UP
        0x49 => 104, // KEY_PAGEUP
        0x4B => 105, // KEY_LEFT
        0x4D => 106, // KEY_RIGHT
        0x4F => 107, // KEY_END
        0x50 => 108, // KEY_DOWN
        0x51 => 109, // KEY_PAGEDOWN
        0x52 => 110, // KEY_INSERT
        0x53 => 111, // KEY_DELETE
        0x5B => 125, // KEY_LEFTMETA (Windows key)
        0x5C => 126, // KEY_RIGHTMETA
        0x5D => 127, // KEY_COMPOSE (Menu)
        _ => return None,
    };
    Some(evdev)
}

/// Linux BTN_* codes for mouse buttons
pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;
pub const BTN_SIDE: u32 = 0x113;   // Button4 (back)
pub const BTN_EXTRA: u32 = 0x114;  // Button5 (forward)
