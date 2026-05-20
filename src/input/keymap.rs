/// Maps RDP XT Set 1 scancodes to Linux evdev keycodes.
///
/// Most base set keys map directly, but international and extended keys
/// need an explicit translation table.
pub fn xt_to_evdev(code: u8, extended: bool) -> Option<u32> {
    match (extended, code) {
        (false, 0x01..=0x53) | (false, 0x56..=0x58) => Some(code as u32),
        (false, 0x70) => Some(93),  // KEY_KATAKANAHIRAGANA
        (false, 0x73) => Some(89),  // KEY_RO
        (false, 0x79) => Some(92),  // KEY_HENKAN
        (false, 0x7B) => Some(94),  // KEY_MUHENKAN
        (false, 0x7D) => Some(124), // KEY_YEN
        (false, 0x7E) => Some(121), // KEY_KPCOMMA
        (true, code) => xt_extended_to_evdev(code),
        _ => None,
    }
}

fn xt_extended_to_evdev(code: u8) -> Option<u32> {
    let evdev = match code {
        0x10 => 165, // KEY_PREVIOUSSONG
        0x19 => 163, // KEY_NEXTSONG
        0x1C => 96,  // KEY_KPENTER
        0x1D => 97,  // KEY_RIGHTCTRL
        0x20 => 113, // KEY_MUTE
        0x21 => 140, // KEY_CALC
        0x22 => 164, // KEY_PLAYPAUSE
        0x24 => 166, // KEY_STOPCD
        0x2E => 114, // KEY_VOLUMEDOWN
        0x30 => 115, // KEY_VOLUMEUP
        0x32 => 172, // KEY_HOMEPAGE
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
        0x65 => 217, // KEY_SEARCH
        0x66 => 156, // KEY_BOOKMARKS
        0x6B => 157, // KEY_COMPUTER
        0x6C => 155, // KEY_MAIL
        _ => return None,
    };
    Some(evdev)
}

/// Linux BTN_* codes for mouse buttons
pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;
pub const BTN_SIDE: u32 = 0x113; // Button4 (back)
pub const BTN_EXTRA: u32 = 0x114; // Button5 (forward)

#[cfg(test)]
mod tests {
    use super::{xt_to_evdev, BTN_EXTRA, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, BTN_SIDE};

    #[test]
    fn maps_base_modifiers_locks_and_common_keys() {
        assert_eq!(xt_to_evdev(0x0f, false), Some(15)); // KEY_TAB
        assert_eq!(xt_to_evdev(0x1d, false), Some(29)); // KEY_LEFTCTRL
        assert_eq!(xt_to_evdev(0x38, false), Some(56)); // KEY_LEFTALT
        assert_eq!(xt_to_evdev(0x3a, false), Some(58)); // KEY_CAPSLOCK
        assert_eq!(xt_to_evdev(0x45, false), Some(69)); // KEY_NUMLOCK
        assert_eq!(xt_to_evdev(0x46, false), Some(70)); // KEY_SCROLLLOCK
    }

    #[test]
    fn maps_international_non_extended_keys() {
        assert_eq!(xt_to_evdev(0x70, false), Some(93));
        assert_eq!(xt_to_evdev(0x73, false), Some(89));
        assert_eq!(xt_to_evdev(0x7D, false), Some(124));
    }

    #[test]
    fn maps_extended_media_and_browser_keys() {
        assert_eq!(xt_to_evdev(0x10, true), Some(165));
        assert_eq!(xt_to_evdev(0x20, true), Some(113));
        assert_eq!(xt_to_evdev(0x32, true), Some(172));
        assert_eq!(xt_to_evdev(0x6C, true), Some(155));
    }

    #[test]
    fn maps_extended_navigation_and_right_modifiers() {
        assert_eq!(xt_to_evdev(0x1c, true), Some(96)); // KEY_KPENTER
        assert_eq!(xt_to_evdev(0x1d, true), Some(97)); // KEY_RIGHTCTRL
        assert_eq!(xt_to_evdev(0x38, true), Some(100)); // KEY_RIGHTALT
        assert_eq!(xt_to_evdev(0x47, true), Some(102)); // KEY_HOME
        assert_eq!(xt_to_evdev(0x48, true), Some(103)); // KEY_UP
        assert_eq!(xt_to_evdev(0x4b, true), Some(105)); // KEY_LEFT
        assert_eq!(xt_to_evdev(0x4d, true), Some(106)); // KEY_RIGHT
        assert_eq!(xt_to_evdev(0x50, true), Some(108)); // KEY_DOWN
        assert_eq!(xt_to_evdev(0x53, true), Some(111)); // KEY_DELETE
    }

    #[test]
    fn rejects_unmapped_scancodes() {
        assert_eq!(xt_to_evdev(0x00, false), None);
        assert_eq!(xt_to_evdev(0x54, false), None);
        assert_eq!(xt_to_evdev(0x59, false), None);
        assert_eq!(xt_to_evdev(0xff, false), None);
        assert_eq!(xt_to_evdev(0x70, true), None);
        assert_eq!(xt_to_evdev(0xff, true), None);
    }

    #[test]
    fn mouse_button_constants_match_linux_evdev_button_range() {
        assert_eq!(BTN_LEFT, 0x110);
        assert_eq!(BTN_RIGHT, 0x111);
        assert_eq!(BTN_MIDDLE, 0x112);
        assert_eq!(BTN_SIDE, 0x113);
        assert_eq!(BTN_EXTRA, 0x114);
    }
}
