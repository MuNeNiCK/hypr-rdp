use std::collections::{HashMap, HashSet};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use anyhow::{bail, Context, Result};
use ironrdp_pdu::input::fast_path::SynchronizeFlags;
use xkbcommon::xkb;

use super::virtual_keyboard::ZwpVirtualKeyboardV1;

const XKB_KEYCODE_OFFSET: u32 = 8;
const KEY_CAPSLOCK: u32 = 58;
const KEY_NUMLOCK: u32 = 69;
const KEY_SCROLLLOCK: u32 = 70;
const KEY_KATAKANAHIRAGANA: u32 = 93;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct XkbKeymapNames {
    pub(super) layout: Option<String>,
    pub(super) variant: Option<String>,
    pub(super) options: Option<String>,
}

impl XkbKeymapNames {
    pub(super) fn is_empty(&self) -> bool {
        self.layout.is_none() && self.variant.is_none() && self.options.is_none()
    }
}

pub(super) fn xkb_names_for_rdp_keyboard_layout(keyboard_layout: u32) -> Option<XkbKeymapNames> {
    let layout_id = keyboard_layout & 0xffff;
    let layout = match layout_id {
        0x0401 => "ara",
        0x0404 | 0x0804 | 0x0c04 | 0x1004 | 0x1404 => "cn",
        0x0405 => "cz",
        0x0406 => "dk",
        0x0407 => "de",
        0x0408 => "gr",
        0x0409 => "us",
        0x040a | 0x0c0a => "es",
        0x040b => "fi",
        0x040c => "fr",
        0x040d => "il",
        0x040e => "hu",
        0x0410 => "it",
        0x0411 => "jp",
        0x0412 => "kr",
        0x0413 => "nl",
        0x0414 => "no",
        0x0415 => "pl",
        0x0416 => "br",
        0x0419 => "ru",
        0x041d => "se",
        0x041f => "tr",
        0x0807 => "ch",
        0x0809 => "gb",
        0x0816 => "pt",
        _ => return None,
    };

    Some(XkbKeymapNames {
        layout: Some(layout.to_owned()),
        ..Default::default()
    })
}

/// Evdev keycode + required modifier (e.g. Shift) for a Unicode character.
#[derive(Clone, Copy)]
pub(super) struct UnicodeKeyMapping {
    pub(super) evdev_key: u32,
    pub(super) needs_shift: bool,
}

pub(super) struct KeyboardStateTracker {
    modifier_masks_by_key: HashMap<u32, u32>,
    unicode_to_keycode: HashMap<u16, UnicodeKeyMapping>,
    pressed_keys: HashSet<u32>,
    depressed_mods: u32,
    locked_mods: u32,
    caps_lock_mask: u32,
    num_lock_mask: u32,
    scroll_lock_mask: u32,
    kana_lock_mask: u32,
}

impl KeyboardStateTracker {
    pub(super) fn new(keymap_data: &[u8]) -> Result<Self> {
        let keymap = compile_xkb_keymap(keymap_data)?;

        Ok(Self {
            modifier_masks_by_key: build_modifier_masks_by_key(&keymap),
            unicode_to_keycode: build_unicode_to_keycode(&keymap),
            pressed_keys: HashSet::new(),
            depressed_mods: 0,
            locked_mods: 0,
            caps_lock_mask: locked_mask_for_key(&keymap, KEY_CAPSLOCK),
            num_lock_mask: locked_mask_for_key(&keymap, KEY_NUMLOCK),
            scroll_lock_mask: locked_mask_for_key(&keymap, KEY_SCROLLLOCK),
            kana_lock_mask: locked_mask_for_key(&keymap, KEY_KATAKANAHIRAGANA),
        })
    }

    pub(super) fn unicode_to_evdev(&self, code_point: u16) -> Option<UnicodeKeyMapping> {
        self.unicode_to_keycode.get(&code_point).copied()
    }

    pub(super) fn key(&mut self, evdev_key: u32, pressed: bool) {
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

    pub(super) fn synchronize_locks(&mut self, flags: SynchronizeFlags) {
        self.locked_mods = self.locked_mods_from_flags(flags);
    }

    pub(super) fn send_modifiers(&self, vk: &ZwpVirtualKeyboardV1) {
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

fn build_unicode_to_keycode(keymap: &xkb::Keymap) -> HashMap<u16, UnicodeKeyMapping> {
    let mut map = HashMap::new();

    for keycode_raw in keymap.min_keycode().raw()..=keymap.max_keycode().raw() {
        let keycode = xkb::Keycode::new(keycode_raw);
        let evdev_key = keycode_raw - XKB_KEYCODE_OFFSET;
        let num_layouts = keymap.num_layouts_for_key(keycode);

        for layout in 0..num_layouts {
            let num_levels = keymap.num_levels_for_key(keycode, layout);
            for level in 0..num_levels {
                let syms = keymap.key_get_syms_by_level(keycode, layout, level);
                for sym in syms {
                    let ch = xkb::keysym_to_utf32(*sym);
                    if ch > 0 && ch <= u32::from(u16::MAX) {
                        map.entry(ch as u16).or_insert(UnicodeKeyMapping {
                            evdev_key,
                            needs_shift: level == 1, // level 1 = Shift
                        });
                    }
                }
            }
        }
    }

    map
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
pub(super) fn generate_xkb_keymap() -> Result<Vec<u8>> {
    generate_xkb_keymap_from_names(&XkbKeymapNames::default())
}

pub(super) fn generate_xkb_keymap_from_names(names: &XkbKeymapNames) -> Result<Vec<u8>> {
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = xkb::Keymap::new_from_names(
        &context,
        "", // rules: system default
        "", // model: system default
        names.layout.as_deref().unwrap_or(""),
        names.variant.as_deref().unwrap_or(""),
        names.options.clone(),
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .context("Failed to compile XKB keymap")?;
    let mut keymap_data = keymap
        .get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)
        .into_bytes();
    if keymap_data.is_empty() {
        bail!("XKB keymap generation returned empty string");
    }
    // XKB v1 format requires NUL-terminated string
    if keymap_data.last() != Some(&0) {
        keymap_data.push(0);
    }
    Ok(keymap_data)
}

pub(super) fn create_keymap_fd(keymap: &[u8]) -> Result<OwnedFd> {
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
    if unsafe { libc::lseek(fd.as_raw_fd(), 0, libc::SEEK_SET) } < 0 {
        bail!("lseek failed on keymap memfd");
    }
    Ok(fd)
}

#[cfg(test)]
mod tests {
    use super::{
        generate_xkb_keymap_from_names, xkb_names_for_rdp_keyboard_layout, KeyboardStateTracker,
        XkbKeymapNames,
    };

    #[test]
    fn generated_keymap_honors_non_us_layout_names() {
        let keymap = generate_xkb_keymap_from_names(&XkbKeymapNames {
            layout: Some("de".into()),
            ..Default::default()
        })
        .expect("German keymap compiles");
        let tracker = KeyboardStateTracker::new(&keymap).expect("generated keymap loads");

        assert_eq!(tracker.unicode_to_evdev('z' as u16).unwrap().evdev_key, 21);
        assert_eq!(tracker.unicode_to_evdev('y' as u16).unwrap().evdev_key, 44);
    }

    #[test]
    fn rdp_keyboard_layout_maps_supported_hkl_to_xkb_names() {
        let names =
            xkb_names_for_rdp_keyboard_layout(0xe0010411).expect("Japanese HKL is supported");

        assert_eq!(names.layout.as_deref(), Some("jp"));
        assert_eq!(names.variant, None);
        assert_eq!(names.options, None);
    }

    #[test]
    fn rdp_keyboard_layout_returns_none_for_unknown_hkl() {
        assert_eq!(xkb_names_for_rdp_keyboard_layout(0x0000ffff), None);
    }

    #[test]
    fn rdp_keyboard_layout_generated_keymap_affects_unicode_lookup() {
        let names = xkb_names_for_rdp_keyboard_layout(0x00000407).expect("German HKL is supported");
        let keymap = generate_xkb_keymap_from_names(&names).expect("German keymap compiles");
        let tracker = KeyboardStateTracker::new(&keymap).expect("generated keymap loads");

        assert_eq!(tracker.unicode_to_evdev('z' as u16).unwrap().evdev_key, 21);
        assert_eq!(tracker.unicode_to_evdev('y' as u16).unwrap().evdev_key, 44);
    }
}
