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
