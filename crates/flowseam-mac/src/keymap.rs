//! USB HID usage ↔ macOS virtual keycode (`kVK_*`, Carbon `Events.h`).
//!
//! Layout-independent: HID names physical key positions, and so does kVK.
//! ponytail: a character-layout-aware path (TIS lookup) would be needed if a
//! non-US layout is ever observed mismatching; add it then, not before.

use flowseam_core::proto::HID_LIMIT;

/// HID usage → macOS keycode. `None` means this table cannot express the key;
/// callers must surface that, not drop it silently.
pub fn hid_to_keycode(hid: u16) -> Option<u16> {
    Some(match hid {
        0x04 => 0x00, // a
        0x05 => 0x0B, // b
        0x06 => 0x08, // c
        0x07 => 0x02, // d
        0x08 => 0x0E, // e
        0x09 => 0x03, // f
        0x0A => 0x05, // g
        0x0B => 0x04, // h
        0x0C => 0x22, // i
        0x0D => 0x26, // j
        0x0E => 0x28, // k
        0x0F => 0x25, // l
        0x10 => 0x2E, // m
        0x11 => 0x2D, // n
        0x12 => 0x1F, // o
        0x13 => 0x23, // p
        0x14 => 0x0C, // q
        0x15 => 0x0F, // r
        0x16 => 0x01, // s
        0x17 => 0x11, // t
        0x18 => 0x20, // u
        0x19 => 0x09, // v
        0x1A => 0x0D, // w
        0x1B => 0x07, // x
        0x1C => 0x10, // y
        0x1D => 0x06, // z
        0x1E => 0x12, // 1
        0x1F => 0x13, // 2
        0x20 => 0x14, // 3
        0x21 => 0x15, // 4
        0x22 => 0x17, // 5
        0x23 => 0x16, // 6
        0x24 => 0x1A, // 7
        0x25 => 0x1C, // 8
        0x26 => 0x19, // 9
        0x27 => 0x1D, // 0
        0x28 => 0x24, // Return
        0x29 => 0x35, // Escape
        0x2A => 0x33, // Delete (backspace)
        0x2B => 0x30, // Tab
        0x2C => 0x31, // Space
        0x2D => 0x1B, // -
        0x2E => 0x18, // =
        0x2F => 0x21, // [
        0x30 => 0x1E, // ]
        0x31 => 0x2A, // backslash
        0x33 => 0x29, // ;
        0x34 => 0x27, // '
        0x35 => 0x32, // `
        0x36 => 0x2B, // ,
        0x37 => 0x2F, // .
        0x38 => 0x2C, // /
        0x39 => 0x39, // CapsLock
        0x3A => 0x7A, // F1
        0x3B => 0x78, // F2
        0x3C => 0x63, // F3
        0x3D => 0x76, // F4
        0x3E => 0x60, // F5
        0x3F => 0x61, // F6
        0x40 => 0x62, // F7
        0x41 => 0x64, // F8
        0x42 => 0x65, // F9
        0x43 => 0x6D, // F10
        0x44 => 0x67, // F11
        0x45 => 0x6F, // F12
        0x68 => 0x69, // F13
        0x69 => 0x6B, // F14
        0x6A => 0x71, // F15
        0x6B => 0x6A, // F16
        0x6C => 0x40, // F17
        0x6D => 0x4F, // F18
        0x6E => 0x50, // F19
        0x6F => 0x5A, // F20
        0x53 => 0x47, // Keypad Clear (NumLock position)
        0x54 => 0x4B, // Keypad /
        0x55 => 0x43, // Keypad *
        0x56 => 0x4E, // Keypad -
        0x57 => 0x45, // Keypad +
        0x58 => 0x4C, // Keypad Enter
        0x59 => 0x53, // Keypad 1
        0x5A => 0x54, // Keypad 2
        0x5B => 0x55, // Keypad 3
        0x5C => 0x56, // Keypad 4
        0x5D => 0x57, // Keypad 5
        0x5E => 0x58, // Keypad 6
        0x5F => 0x59, // Keypad 7
        0x60 => 0x5B, // Keypad 8
        0x61 => 0x5C, // Keypad 9
        0x62 => 0x52, // Keypad 0
        0x63 => 0x41, // Keypad .
        0x67 => 0x51, // Keypad =
        0x64 => 0x0A, // ISO key left of Z (kVK_ISO_Section)
        0x65 => 0x6E, // Application / context menu
        0x7F => 0x4A, // Mute
        0x80 => 0x48, // Volume up
        0x81 => 0x49, // Volume down
        0x87 => 0x5E, // JIS Ro (underscore)
        0x89 => 0x5D, // JIS Yen
        0x49 => 0x72, // Insert → kVK_Help (macOS's own mapping of the Insert key)
        0x4A => 0x73, // Home
        0x4B => 0x74, // PageUp
        0x4C => 0x75, // Forward delete
        0x4D => 0x77, // End
        0x4E => 0x79, // PageDown
        0x4F => 0x7C, // Right
        0x50 => 0x7B, // Left
        0x51 => 0x7D, // Down
        0x52 => 0x7E, // Up
        0xE0 => 0x3B, // Left Control
        0xE1 => 0x38, // Left Shift
        0xE2 => 0x3A, // Left Option
        0xE3 => 0x37, // Left Command
        0xE4 => 0x3E, // Right Control
        0xE5 => 0x3C, // Right Shift
        0xE6 => 0x3D, // Right Option
        0xE7 => 0x36, // Right Command
        _ => return None,
    })
}

/// macOS keycode → HID usage (inverse of [`hid_to_keycode`]).
pub fn keycode_to_hid(keycode: u16) -> Option<u16> {
    (0..HID_LIMIT).find(|&h| hid_to_keycode(h) == Some(keycode))
}

/// Modifier HID usages 0xE0..=0xE7.
pub fn is_modifier_hid(hid: u16) -> bool {
    (0xE0..=0xE7).contains(&hid)
}

/// Modifier keycodes whose press state arrives as `FlagsChanged` (Command,
/// Shift, Option, Control and their right-hand twins). CapsLock is handled
/// separately because it is a toggle.
pub fn is_modifier_keycode(keycode: u16) -> bool {
    matches!(keycode, 0x36..=0x38 | 0x3A..=0x3E)
}

pub const KC_CAPS_LOCK: u16 = 0x39;
/// kVK_Function: the fn / Globe key.
pub const KC_FN: u16 = 0x3F;
pub const KC_ISO_SECTION: u16 = 0x0A;
pub const KC_GRAVE: u16 = 0x32;

/// The HID usage for where the key sits. On an ISO keyboard macOS names the top-left key (§)
/// kVK_ISO_Section and the key left of Z kVK_ANSI_Grave; by position those are HID Grave (0x35, ` ~ on
/// a PC) and Non-US Backslash (0x64, \ | on a PC), the reverse of what the table gives them.
pub fn by_position(kc: u16, hid: u16, iso: bool) -> u16 {
    match (iso, kc) {
        (true, KC_ISO_SECTION) => 0x35,
        (true, KC_GRAVE) => 0x64,
        _ => hid,
    }
}

/// The inverse, for injecting: the keycode that presses the key at `hid`'s position on a keyboard that is
/// ISO when `iso`. `kc` is what the table gives.
pub fn keycode_by_position(hid: u16, kc: u16, iso: bool) -> u16 {
    match (iso, hid) {
        (true, 0x35) => KC_ISO_SECTION,
        (true, 0x64) => KC_GRAVE,
        _ => kc,
    }
}

/// Press state of a modifier key from a `FlagsChanged` event's flags word.
///
/// Uses the device-dependent (left/right) bits, so releasing one of two held
/// Shift keys reads as "still down" for the other side only. Bit values are
/// the IOKit `NX_DEVICE*KEYMASK` constants.
/// Returns `None` when `keycode` is not a left/right modifier.
pub fn modifier_down(keycode: u16, flags: u64) -> Option<bool> {
    let bit: u64 = match keycode {
        0x3B => 0x0000_0001, // left control
        0x3E => 0x0000_2000, // right control
        0x38 => 0x0000_0002, // left shift
        0x3C => 0x0000_0004, // right shift
        0x37 => 0x0000_0008, // left command
        0x36 => 0x0000_0010, // right command
        0x3A => 0x0000_0020, // left option
        0x3D => 0x0000_0040, // right option
        _ => return None,
    };
    Some(flags & bit != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors_match_carbon_kvk_table() {
        assert_eq!(hid_to_keycode(0x04), Some(0x00), "a");
        assert_eq!(hid_to_keycode(0x29), Some(0x35), "escape");
        assert_eq!(hid_to_keycode(0x2C), Some(0x31), "space");
        assert_eq!(hid_to_keycode(0x52), Some(0x7E), "up arrow");
        assert_eq!(hid_to_keycode(0xE0), Some(0x3B), "left control");
        assert_eq!(hid_to_keycode(0xE3), Some(0x37), "left command");
        assert_eq!(hid_to_keycode(0xE7), Some(0x36), "right command");
    }

    #[test]
    fn unmappable_hid_is_none_not_a_default() {
        assert_eq!(hid_to_keycode(0x00), None, "reserved usage");
        assert_eq!(hid_to_keycode(0x46), None, "PrintScreen has no entry; must surface");
        assert_eq!(hid_to_keycode(0xFF), None);
    }

    #[test]
    fn forward_and_inverse_agree_and_keycodes_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for hid in 0..HID_LIMIT {
            if let Some(kc) = hid_to_keycode(hid) {
                assert!(seen.insert(kc), "keycode {kc:#x} reached from two HIDs");
                assert_eq!(keycode_to_hid(kc), Some(hid), "round trip for HID {hid:#x}");
            }
        }
        assert_eq!(seen.len(), 116, "table size changed; update deliberately");
    }

    /// Keys a Mac keyboard has that the table once lacked; each one leaked into the local app while a
    /// peer was driven (measured for keypad 0 and +). Keycodes from Carbon `Events.h`.
    #[test]
    fn function_row_keypad_and_extra_keys_are_mapped() {
        for (kc, hid, name) in [
            (0x69, 0x68, "F13"), (0x5A, 0x6F, "F20"), (0x52, 0x62, "keypad 0"), (0x45, 0x57, "keypad +"),
            (0x4C, 0x58, "keypad enter"), (0x51, 0x67, "keypad ="), (0x47, 0x53, "keypad clear"),
            (0x0A, 0x64, "ISO section"), (0x6E, 0x65, "context menu"), (0x48, 0x80, "volume up"),
            (0x4A, 0x7F, "mute"), (0x5D, 0x89, "JIS yen"),
        ] {
            assert_eq!(keycode_to_hid(kc), Some(hid), "{name}");
        }
    }

    /// The keycaps of an ISO Mac keyboard: § top-left lands as ` ~, ` left of Z lands as \ |. An ANSI
    /// keyboard has ` top-left and no key left of Z, so it keeps the table.
    #[test]
    fn iso_keys_land_by_position() {
        assert_eq!(by_position(KC_ISO_SECTION, keycode_to_hid(KC_ISO_SECTION).unwrap(), true), 0x35);
        assert_eq!(by_position(KC_GRAVE, keycode_to_hid(KC_GRAVE).unwrap(), true), 0x64);
        assert_eq!(by_position(KC_GRAVE, keycode_to_hid(KC_GRAVE).unwrap(), false), 0x35, "ANSI top-left");
        assert_eq!(by_position(0x00, 0x04, true), 0x04, "other keys untouched");
    }

    /// Injecting on an ISO Mac presses the key at the same position the sender's key was: § for Grave,
    /// the key left of Z for Non-US Backslash. Without it, an ISO Mac driving an ISO Mac typed § as `.
    #[test]
    fn injection_on_iso_presses_the_same_position() {
        for hid in [0x35, 0x64] {
            let kc = keycode_by_position(hid, hid_to_keycode(hid).unwrap(), true);
            assert_eq!(by_position(kc, keycode_to_hid(kc).unwrap(), true), hid, "HID {hid:#04x}");
        }
        assert_eq!(keycode_by_position(0x35, hid_to_keycode(0x35).unwrap(), false), KC_GRAVE, "ANSI keeps the table");
    }

    #[test]
    fn modifier_hid_range_and_keycode_range() {
        assert!(is_modifier_hid(0xE0) && is_modifier_hid(0xE7));
        assert!(!is_modifier_hid(0xDF) && !is_modifier_hid(0xE8));
        assert!(is_modifier_keycode(0x37) && is_modifier_keycode(0x3E));
        assert!(!is_modifier_keycode(KC_CAPS_LOCK), "CapsLock is a toggle, not FlagsChanged-held");
        assert!(!is_modifier_keycode(0x00));
    }

    #[test]
    fn modifier_down_reads_device_bits_per_side() {
        // Left shift held, right shift not.
        assert_eq!(modifier_down(0x38, 0x0002), Some(true));
        assert_eq!(modifier_down(0x3C, 0x0002), Some(false), "right shift is not left shift");
        // Both shifts held, left released: right still reads down.
        assert_eq!(modifier_down(0x38, 0x0004), Some(false));
        assert_eq!(modifier_down(0x3C, 0x0004), Some(true));
        assert_eq!(modifier_down(0x00, u64::MAX), None, "not a modifier keycode");
    }
}

/// HID keyboard-page usages for media keys the page itself lacks. 0xE8.. is reserved in the page; this
/// is the assignment Linux's HID layer uses for it.
pub const HID_PLAY_PAUSE: u16 = 0xE8;
pub const HID_PREVIOUS: u16 = 0xEA;
pub const HID_NEXT: u16 = 0xEB;

/// A media key from a system-defined event (`NSSystemDefined`, subtype 8 = aux control buttons):
/// `data1` is the NX key type in bits 16..32 and the state in bits 8..16 (0x0A down, 0x0B up).
/// Volume, mute and transport map to keyboard usages; brightness and the rest have no peer meaning
/// and return `None`, so they stay with this Mac.
pub fn aux_key(subtype: i16, data1: i64) -> Option<(u16, bool)> {
    const NX_SUBTYPE_AUX_CONTROL_BUTTONS: i16 = 8;
    if subtype != NX_SUBTYPE_AUX_CONTROL_BUTTONS {
        return None;
    }
    let down = match (data1 >> 8) & 0xFF {
        0x0A => true,
        0x0B => false,
        _ => return None,
    };
    let hid = match (data1 >> 16) & 0xFFFF {
        0 => 0x80,                 // NX_KEYTYPE_SOUND_UP
        1 => 0x81,                 // NX_KEYTYPE_SOUND_DOWN
        7 => 0x7F,                 // NX_KEYTYPE_MUTE
        16 => HID_PLAY_PAUSE,      // NX_KEYTYPE_PLAY
        17 | 19 => HID_NEXT,       // NX_KEYTYPE_NEXT, NX_KEYTYPE_FAST
        18 | 20 => HID_PREVIOUS,   // NX_KEYTYPE_PREVIOUS, NX_KEYTYPE_REWIND
        _ => return None,
    };
    Some((hid, down))
}

#[cfg(test)]
mod aux_tests {
    use super::*;

    fn ev(key: i64, state: i64) -> i64 {
        (key << 16) | (state << 8)
    }

    #[test]
    fn media_keys_decode_with_their_press_state() {
        assert_eq!(aux_key(8, ev(0, 0x0A)), Some((0x80, true)), "volume up down");
        assert_eq!(aux_key(8, ev(0, 0x0B)), Some((0x80, false)), "volume up up");
        assert_eq!(aux_key(8, ev(1, 0x0A)), Some((0x81, true)));
        assert_eq!(aux_key(8, ev(7, 0x0A)), Some((0x7F, true)), "mute");
        assert_eq!(aux_key(8, ev(16, 0x0A)), Some((HID_PLAY_PAUSE, true)));
        assert_eq!(aux_key(8, ev(20, 0x0B)), Some((HID_PREVIOUS, false)), "rewind is previous");
        assert_eq!(aux_key(8, ev(19, 0x0A)), Some((HID_NEXT, true)), "fast is next");
    }

    #[test]
    fn brightness_other_subtypes_and_odd_states_stay_local() {
        assert_eq!(aux_key(8, ev(2, 0x0A)), None, "brightness up");
        assert_eq!(aux_key(8, ev(3, 0x0A)), None, "brightness down");
        assert_eq!(aux_key(7, ev(0, 0x0A)), None, "subtype 7 is mouse buttons");
        assert_eq!(aux_key(8, ev(0, 0x00)), None, "neither down nor up");
    }
}

/// CGEventFlags aggregate bits for a flowseam_core modifier set.
/// Device-independent: Left and Right both set the same bit.
pub fn flags_for(mods: flowseam_core::proto::ModifierSet) -> u64 {
    use flowseam_core::proto::ModifierSet;
    let mut f = 0u64;
    if mods.contains(ModifierSet::CAPS) {
        f |= 0x0001_0000;
    }
    if mods.contains(ModifierSet::SHIFT) {
        f |= 0x0002_0000;
    }
    if mods.contains(ModifierSet::CTRL) {
        f |= 0x0004_0000;
    }
    if mods.contains(ModifierSet::ALT) {
        f |= 0x0008_0000;
    }
    if mods.contains(ModifierSet::GUI) {
        f |= 0x0010_0000;
    }
    f
}

#[cfg(test)]
mod flag_tests {
    use super::*;
    use flowseam_core::proto::ModifierSet;

    #[test]
    fn flags_follow_the_cgeventflags_aggregate_bits() {
        assert_eq!(flags_for(ModifierSet(0)), 0);
        assert_eq!(flags_for(ModifierSet(ModifierSet::SHIFT)), 0x0002_0000);
        assert_eq!(flags_for(ModifierSet(ModifierSet::GUI)), 0x0010_0000);
        let all = ModifierSet(ModifierSet::SHIFT | ModifierSet::CTRL | ModifierSet::ALT | ModifierSet::GUI | ModifierSet::CAPS);
        assert_eq!(flags_for(all), 0x001F_0000);
    }
}
