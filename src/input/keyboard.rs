use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use anyhow::{bail, Context, Result};
use ironrdp_pdu::input::fast_path::SynchronizeFlags;
use xkbcommon::xkb;

/// `xkb_state_update_latched_locked`: the server-side path for out-of-band
/// latched/locked changes to a state otherwise driven by
/// `xkb_state_update_key` (mixing `xkb_state_update_mask` into such a state
/// is explicitly unsupported). The symbol appeared in libxkbcommon 1.10 and
/// the xkbcommon crate does not bind it yet, so it is resolved at runtime to
/// keep the binary linking against older releases. The type below mirrors
/// the documented C declaration in xkbcommon/xkbcommon.h (stable public
/// API): xkb_mod_mask_t is uint32_t, layouts are int32_t, and Rust bool is
/// ABI-compatible with C bool.
type UpdateLatchedLockedFn = unsafe extern "C" fn(
    state: *mut xkb::ffi::xkb_state,
    affect_latched_mods: u32,
    latched_mods: u32,
    affect_latched_layout: bool,
    latched_layout: i32,
    affect_locked_mods: u32,
    locked_mods: u32,
    affect_locked_layout: bool,
    locked_layout: i32,
) -> u32;

fn lookup_update_latched_locked() -> Option<UpdateLatchedLockedFn> {
    let ptr = unsafe {
        libc::dlsym(
            libc::RTLD_DEFAULT,
            c"xkb_state_update_latched_locked".as_ptr(),
        )
    };
    if ptr.is_null() {
        None
    } else {
        // SAFETY: the symbol comes from the linked libxkbcommon and has the
        // documented signature above.
        Some(unsafe { std::mem::transmute::<*mut libc::c_void, UpdateLatchedLockedFn>(ptr) })
    }
}

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
    let variant = match keyboard_layout {
        0x00010405 => Some("qwerty"),
        _ => None,
    };

    Some(XkbKeymapNames {
        layout: Some(layout.to_owned()),
        variant: variant.map(str::to_owned),
        ..Default::default()
    })
}

/// Evdev keycode + required modifier (e.g. Shift) for a Unicode character.
#[derive(Clone, Copy)]
pub(super) struct UnicodeKeyMapping {
    pub(super) evdev_key: u32,
    pub(super) needs_shift: bool,
}

/// Modifier and layout snapshot in the shape of
/// `zwp_virtual_keyboard_v1.modifiers`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct KeyboardModifierState {
    pub(super) depressed: u32,
    pub(super) latched: u32,
    pub(super) locked: u32,
    pub(super) group: u32,
}

/// Replica of the compositor's XKB state for the virtual keyboard.
///
/// A single `xkb_state` is the source of truth. Raw keys go through the
/// server-side `xkb_state_update_key` path — group toggle options such as
/// `grp:alt_shift_toggle` therefore switch the replica in lockstep with the
/// compositor processing the same key stream. Out-of-band changes (RDP lock
/// synchronization, external layout switches) go through the server-side
/// `xkb_state_update_latched_locked` path against that same state.
pub(super) struct KeyboardStateTracker {
    unicode_to_keycode: HashMap<u16, UnicodeKeyMapping>,
    layout_names: Vec<String>,
    xkb_state: xkb::State,
    update_latched_locked: Option<UpdateLatchedLockedFn>,
    caps_lock_mask: u32,
    num_lock_mask: u32,
    scroll_lock_mask: u32,
    kana_lock_mask: u32,
}

impl KeyboardStateTracker {
    pub(super) fn new(keymap_data: &[u8]) -> Result<Self> {
        let keymap = compile_xkb_keymap(keymap_data)?;

        Ok(Self {
            unicode_to_keycode: build_unicode_to_keycode(&keymap),
            layout_names: build_layout_names(&keymap),
            xkb_state: xkb::State::new(&keymap),
            update_latched_locked: lookup_update_latched_locked(),
            caps_lock_mask: locked_mask_for_key(&keymap, KEY_CAPSLOCK),
            num_lock_mask: locked_mask_for_key(&keymap, KEY_NUMLOCK),
            scroll_lock_mask: locked_mask_for_key(&keymap, KEY_SCROLLLOCK),
            kana_lock_mask: locked_mask_for_key(&keymap, KEY_KATAKANAHIRAGANA),
        })
    }

    pub(super) fn unicode_to_evdev(&self, code_point: u16) -> Option<UnicodeKeyMapping> {
        self.unicode_to_keycode.get(&code_point).copied()
    }

    /// Resolve an XKB layout display name (e.g. "Ukrainian") to its group
    /// index in this keymap.
    pub(super) fn layout_index_by_name(&self, name: &str) -> Option<u32> {
        self.layout_names
            .iter()
            .position(|layout| layout == name)
            .map(|index| index as u32)
    }

    /// Feed a key event through the replica. Returns true when the modifier
    /// or group state visible to the compositor changed.
    pub(super) fn key(&mut self, evdev_key: u32, pressed: bool) -> bool {
        let before = self.modifier_state();
        let direction = if pressed {
            xkb::KeyDirection::Down
        } else {
            xkb::KeyDirection::Up
        };
        self.xkb_state
            .update_key(xkb::Keycode::new(evdev_key + XKB_KEYCODE_OFFSET), direction);
        self.modifier_state() != before
    }

    /// Force lock modifiers to the client's view (RDP `Synchronize`).
    /// Returns true when the state changed.
    pub(super) fn synchronize_locks(&mut self, flags: SynchronizeFlags) -> bool {
        let target = self.locked_mods_from_flags(flags);

        if self.update_latched_locked.is_some() {
            let affect = self.caps_lock_mask
                | self.num_lock_mask
                | self.scroll_lock_mask
                | self.kana_lock_mask;
            return self.apply_latched_locked(affect, target, false, 0);
        }

        // Fallback for libxkbcommon < 1.10: locks are key toggles, so drive
        // them through the server-side key path of the same state.
        let before = self.modifier_state();
        for (mask, key) in [
            (self.caps_lock_mask, KEY_CAPSLOCK),
            (self.num_lock_mask, KEY_NUMLOCK),
            (self.scroll_lock_mask, KEY_SCROLLLOCK),
            (self.kana_lock_mask, KEY_KATAKANAHIRAGANA),
        ] {
            if mask != 0 && (before.locked ^ target) & mask != 0 {
                self.key(key, true);
                self.key(key, false);
            }
        }
        self.modifier_state() != before
    }

    /// Whether external layout switches can be applied. Requires the
    /// libxkbcommon >= 1.10 server-side latched/locked update path.
    pub(super) fn supports_locked_layout(&self) -> bool {
        self.update_latched_locked.is_some()
    }

    /// Lock a layout group (external layout switch). Group toggles processed
    /// by `key` continue relative to the new group. Returns true when the
    /// state changed.
    pub(super) fn set_locked_group(&mut self, group: u32) -> bool {
        self.apply_latched_locked(0, 0, true, group as i32)
    }

    pub(super) fn modifier_state(&self) -> KeyboardModifierState {
        KeyboardModifierState {
            depressed: self.xkb_state.serialize_mods(xkb::STATE_MODS_DEPRESSED),
            latched: self.xkb_state.serialize_mods(xkb::STATE_MODS_LATCHED),
            locked: self.xkb_state.serialize_mods(xkb::STATE_MODS_LOCKED),
            group: self.xkb_state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE),
        }
    }

    fn apply_latched_locked(
        &mut self,
        affect_locked_mods: u32,
        locked_mods: u32,
        affect_locked_layout: bool,
        locked_layout: i32,
    ) -> bool {
        let Some(update) = self.update_latched_locked else {
            return false;
        };
        let before = self.modifier_state();
        unsafe {
            update(
                self.xkb_state.get_raw_ptr(),
                0,
                0,
                false,
                0,
                affect_locked_mods,
                locked_mods,
                affect_locked_layout,
                locked_layout,
            );
        }
        self.modifier_state() != before
    }

    #[cfg(test)]
    pub(super) fn without_locked_layout_support(mut self) -> Self {
        self.update_latched_locked = None;
        self
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

fn build_layout_names(keymap: &xkb::Keymap) -> Vec<String> {
    (0..keymap.num_layouts())
        .map(|layout| keymap.layout_get_name(layout).to_owned())
        .collect()
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
        generate_xkb_keymap, generate_xkb_keymap_from_names, xkb_names_for_rdp_keyboard_layout,
        KeyboardStateTracker, SynchronizeFlags, XkbKeymapNames,
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
    fn rdp_keyboard_layout_preserves_czech_qwerty_variant() {
        let names =
            xkb_names_for_rdp_keyboard_layout(0x00010405).expect("Czech QWERTY HKL is supported");

        assert_eq!(names.layout.as_deref(), Some("cz"));
        assert_eq!(names.variant.as_deref(), Some("qwerty"));
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

    #[test]
    fn rdp_keyboard_layout_czech_qwerty_generated_keymap_affects_unicode_lookup() {
        let names =
            xkb_names_for_rdp_keyboard_layout(0x00010405).expect("Czech QWERTY HKL is supported");
        let keymap = generate_xkb_keymap_from_names(&names).expect("Czech QWERTY keymap compiles");
        let tracker = KeyboardStateTracker::new(&keymap).expect("generated keymap loads");

        assert_eq!(tracker.unicode_to_evdev('y' as u16).unwrap().evdev_key, 21);
        assert_eq!(tracker.unicode_to_evdev('z' as u16).unwrap().evdev_key, 44);
    }

    fn toggle_keymap() -> Vec<u8> {
        generate_xkb_keymap_from_names(&XkbKeymapNames {
            layout: Some("us,ua".into()),
            options: Some("grp:alt_shift_toggle".into()),
            ..Default::default()
        })
        .expect("multi-layout keymap compiles")
    }

    // 56 = KEY_LEFTALT, 42 = KEY_LEFTSHIFT
    fn press_alt_shift(tracker: &mut KeyboardStateTracker) {
        tracker.key(56, true);
        tracker.key(42, true);
        tracker.key(42, false);
        tracker.key(56, false);
    }

    #[test]
    fn modifier_state_preserves_active_keyboard_group() {
        let keymap = generate_xkb_keymap_from_names(&XkbKeymapNames {
            layout: Some("cz,us".into()),
            variant: Some("qwerty,".into()),
            ..Default::default()
        })
        .expect("multi-layout keymap compiles");
        let mut tracker = KeyboardStateTracker::new(&keymap).expect("generated keymap loads");
        if !tracker.supports_locked_layout() {
            eprintln!("skipping: libxkbcommon lacks xkb_state_update_latched_locked");
            return;
        }

        assert!(tracker.set_locked_group(1));

        assert_eq!(tracker.modifier_state().group, 1);
    }

    #[test]
    fn layout_index_by_name_resolves_group_indices() {
        let tracker = KeyboardStateTracker::new(&toggle_keymap()).expect("keymap loads");

        assert_eq!(tracker.layout_index_by_name("English (US)"), Some(0));
        assert_eq!(tracker.layout_index_by_name("Ukrainian"), Some(1));
        assert_eq!(tracker.layout_index_by_name("German"), None);
    }

    #[test]
    fn alt_shift_toggles_layout_group() {
        let mut tracker = KeyboardStateTracker::new(&toggle_keymap()).expect("keymap loads");

        press_alt_shift(&mut tracker);
        assert_eq!(tracker.modifier_state().group, 1);

        press_alt_shift(&mut tracker);
        assert_eq!(tracker.modifier_state().group, 0);
    }

    #[test]
    fn external_group_switch_composes_with_alt_shift_toggle() {
        let mut tracker = KeyboardStateTracker::new(&toggle_keymap()).expect("keymap loads");
        if !tracker.supports_locked_layout() {
            eprintln!("skipping: libxkbcommon lacks xkb_state_update_latched_locked");
            return;
        }

        assert!(tracker.set_locked_group(1));
        assert_eq!(tracker.modifier_state().group, 1);

        press_alt_shift(&mut tracker);
        assert_eq!(tracker.modifier_state().group, 0);
    }

    #[test]
    fn synchronize_locks_preserves_locked_group() {
        let mut tracker = KeyboardStateTracker::new(&toggle_keymap()).expect("keymap loads");
        if !tracker.supports_locked_layout() {
            eprintln!("skipping: libxkbcommon lacks xkb_state_update_latched_locked");
            return;
        }
        tracker.set_locked_group(1);

        assert!(tracker.synchronize_locks(SynchronizeFlags::CAPS_LOCK));

        let state = tracker.modifier_state();
        assert_eq!(state.group, 1);
        assert_ne!(state.locked & tracker.caps_lock_mask, 0);

        assert!(tracker.synchronize_locks(SynchronizeFlags::empty()));
        let state = tracker.modifier_state();
        assert_eq!(state.group, 1);
        assert_eq!(state.locked & tracker.caps_lock_mask, 0);
    }

    #[test]
    fn synchronize_locks_falls_back_to_lock_key_toggles() {
        let mut tracker = KeyboardStateTracker::new(&toggle_keymap())
            .expect("keymap loads")
            .without_locked_layout_support();
        tracker.key(56, true); // hold Alt across the sync to prove state survives

        assert!(tracker.synchronize_locks(SynchronizeFlags::CAPS_LOCK));
        let state = tracker.modifier_state();
        assert_ne!(state.locked & tracker.caps_lock_mask, 0);
        assert_ne!(state.depressed, 0);

        assert!(tracker.synchronize_locks(SynchronizeFlags::empty()));
        assert_eq!(tracker.modifier_state().locked & tracker.caps_lock_mask, 0);
    }

    #[test]
    fn set_locked_group_degrades_without_latched_locked_support() {
        let mut tracker = KeyboardStateTracker::new(&toggle_keymap())
            .expect("keymap loads")
            .without_locked_layout_support();

        assert!(!tracker.set_locked_group(1));
        assert_eq!(tracker.modifier_state().group, 0);
    }

    #[test]
    fn caps_lock_key_and_synchronize_share_one_state() {
        let mut tracker = KeyboardStateTracker::new(&toggle_keymap()).expect("keymap loads");

        // 58 = KEY_CAPSLOCK: the key path locks caps inside the XKB state...
        tracker.key(58, true);
        tracker.key(58, false);
        assert_ne!(tracker.modifier_state().locked & tracker.caps_lock_mask, 0);

        // ...and the RDP Synchronize path clears it on the same state.
        tracker.synchronize_locks(SynchronizeFlags::empty());
        assert_eq!(tracker.modifier_state().locked & tracker.caps_lock_mask, 0);
    }

    #[test]
    fn normal_key_does_not_report_modifier_state_change() {
        let keymap = generate_xkb_keymap().expect("default keymap compiles");
        let mut tracker = KeyboardStateTracker::new(&keymap).expect("generated keymap loads");

        assert!(!tracker.key(30, true));
        assert!(!tracker.key(30, false));
    }

    #[test]
    fn modifier_key_reports_modifier_state_change() {
        let keymap = generate_xkb_keymap().expect("default keymap compiles");
        let mut tracker = KeyboardStateTracker::new(&keymap).expect("generated keymap loads");

        assert!(tracker.key(42, true));
        assert!(tracker.key(42, false));
    }
}
