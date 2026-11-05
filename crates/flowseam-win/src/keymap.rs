//! USB HID usage (page 0x07, the wire key space) to the PC/AT set-1 scan code Windows injects with
//! `KEYEVENTF_SCANCODE`. Scan codes, not virtual keys, so the user's Windows keyboard layout applies
//! exactly as it would to a physical key.

/// A set-1 scan code, and whether it carries the `E0` prefix (`KEYEVENTF_EXTENDEDKEY`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Scan {
    pub code: u16,
    pub extended: bool,
}

const fn s(code: u16) -> Option<Scan> {
    Some(Scan { code, extended: false })
}
const fn e(code: u16) -> Option<Scan> {
    Some(Scan { code, extended: true })
}

/// `None` for usages Windows has no single scan code for (Pause is a multi-byte sequence) or that do
/// not exist on PC keyboards. The caller reports those instead of guessing.
pub fn hid_to_scan(hid: u16) -> Option<Scan> {
    match hid {
        // Letters a..z
        0x04 => s(0x1E),
        0x05 => s(0x30),
        0x06 => s(0x2E),
        0x07 => s(0x20),
        0x08 => s(0x12),
        0x09 => s(0x21),
        0x0A => s(0x22),
        0x0B => s(0x23),
        0x0C => s(0x17),
        0x0D => s(0x24),
        0x0E => s(0x25),
        0x0F => s(0x26),
        0x10 => s(0x32),
        0x11 => s(0x31),
        0x12 => s(0x18),
        0x13 => s(0x19),
        0x14 => s(0x10),
        0x15 => s(0x13),
        0x16 => s(0x1F),
        0x17 => s(0x14),
        0x18 => s(0x16),
        0x19 => s(0x2F),
        0x1A => s(0x11),
        0x1B => s(0x2D),
        0x1C => s(0x15),
        0x1D => s(0x2C),
        // Digits 1..9, 0
        0x1E => s(0x02),
        0x1F => s(0x03),
        0x20 => s(0x04),
        0x21 => s(0x05),
        0x22 => s(0x06),
        0x23 => s(0x07),
        0x24 => s(0x08),
        0x25 => s(0x09),
        0x26 => s(0x0A),
        0x27 => s(0x0B),
        0x28 => s(0x1C), // Enter
        0x29 => s(0x01), // Escape
        0x2A => s(0x0E), // Backspace
        0x2B => s(0x0F), // Tab
        0x2C => s(0x39), // Space
        0x2D => s(0x0C), // - _
        0x2E => s(0x0D), // = +
        0x2F => s(0x1A), // [ {
        0x30 => s(0x1B), // ] }
        0x31 => s(0x2B), // \ |
        0x32 => s(0x2B), // Non-US # ~ (same physical position on ISO boards)
        0x33 => s(0x27), // ; :
        0x34 => s(0x28), // ' "
        0x35 => s(0x29), // ` ~
        0x36 => s(0x33), // , <
        0x37 => s(0x34), // . >
        0x38 => s(0x35), // / ?
        0x39 => s(0x3A), // Caps Lock
        // F1..F12
        0x3A => s(0x3B),
        0x3B => s(0x3C),
        0x3C => s(0x3D),
        0x3D => s(0x3E),
        0x3E => s(0x3F),
        0x3F => s(0x40),
        0x40 => s(0x41),
        0x41 => s(0x42),
        0x42 => s(0x43),
        0x43 => s(0x44),
        0x44 => s(0x57),
        0x45 => s(0x58),
        0x46 => e(0x37), // Print Screen
        0x47 => s(0x46), // Scroll Lock
        0x48 => None,    // Pause: E1 1D 45, no single scan code
        0x49 => e(0x52), // Insert
        0x4A => e(0x47), // Home
        0x4B => e(0x49), // Page Up
        0x4C => e(0x53), // Delete
        0x4D => e(0x4F), // End
        0x4E => e(0x51), // Page Down
        0x4F => e(0x4D), // Right
        0x50 => e(0x4B), // Left
        0x51 => e(0x50), // Down
        0x52 => e(0x48), // Up
        0x53 => s(0x45), // Num Lock (hooks report it extended, but SendInput of E0 45 is no key)
        0x54 => e(0x35), // Keypad /
        0x55 => s(0x37), // Keypad *
        0x56 => s(0x4A), // Keypad -
        0x57 => s(0x4E), // Keypad +
        0x58 => e(0x1C), // Keypad Enter
        0x59 => s(0x4F), // Keypad 1
        0x5A => s(0x50), // Keypad 2
        0x5B => s(0x51), // Keypad 3
        0x5C => s(0x4B), // Keypad 4
        0x5D => s(0x4C), // Keypad 5
        0x5E => s(0x4D), // Keypad 6
        0x5F => s(0x47), // Keypad 7
        0x60 => s(0x48), // Keypad 8
        0x61 => s(0x49), // Keypad 9
        0x62 => s(0x52), // Keypad 0
        0x63 => s(0x53), // Keypad .
        0x64 => s(0x56), // Non-US \ | (ISO key left of Z)
        0x65 => e(0x5D), // Application / Menu
        0x67 => s(0x59), // Keypad = (Mac keypad)
        // F13..F24
        0x68 => s(0x64),
        0x69 => s(0x65),
        0x6A => s(0x66),
        0x6B => s(0x67),
        0x6C => s(0x68),
        0x6D => s(0x69),
        0x6E => s(0x6A),
        0x6F => s(0x6B),
        0x70 => s(0x6C),
        0x71 => s(0x6D),
        0x72 => s(0x6E),
        0x73 => s(0x76),
        // Volume (the keyboard page's own usages; Windows reads them as the media volume keys)
        0x7F => e(0x20), // Mute
        0x80 => e(0x30), // Volume Up
        0x81 => e(0x2E), // Volume Down
        // Media transport (keyboard-page 0xE8.., the Linux assignment flowseam_mac uses)
        0xE8 => e(0x22), // Play/Pause
        0xEA => e(0x10), // Previous track
        0xEB => e(0x19), // Next track
        // Japanese / international
        0x87 => s(0x73), // International1 (Ro)
        0x88 => s(0x70), // International2 (Katakana/Hiragana)
        0x89 => s(0x7D), // International3 (Yen)
        0x8A => s(0x79), // International4 (Henkan)
        0x8B => s(0x7B), // International5 (Muhenkan)
        // Modifiers
        0xE0 => s(0x1D), // Left Ctrl
        0xE1 => s(0x2A), // Left Shift
        0xE2 => s(0x38), // Left Alt
        0xE3 => e(0x5B), // Left GUI (Windows key)
        0xE4 => e(0x1D), // Right Ctrl
        0xE5 => s(0x36), // Right Shift
        0xE6 => e(0x38), // Right Alt (AltGr)
        0xE7 => e(0x5C), // Right GUI
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_letter_digit_and_modifier_has_a_scan_code() {
        for hid in (0x04..=0x27).chain(0xE0..=0xE7) {
            assert!(hid_to_scan(hid).is_some(), "HID {hid:#04x} unmapped");
        }
    }

    #[test]
    fn spot_checks_against_the_set1_table() {
        assert_eq!(hid_to_scan(0x04), s(0x1E), "A");
        assert_eq!(hid_to_scan(0x1D), s(0x2C), "Z");
        assert_eq!(hid_to_scan(0x27), s(0x0B), "0");
        assert_eq!(hid_to_scan(0x28), s(0x1C), "Enter");
        assert_eq!(hid_to_scan(0x58), e(0x1C), "Keypad Enter is the extended Enter");
        assert_eq!(hid_to_scan(0x52), e(0x48), "Up is extended; Keypad 8 is not");
        assert_eq!(hid_to_scan(0x60), s(0x48));
        assert_eq!(hid_to_scan(0xE6), e(0x38), "AltGr is extended Alt");
        assert_eq!(hid_to_scan(0x80), e(0x30), "Volume Up is the extended 0x30");
        assert_eq!(hid_to_scan(0x7F), e(0x20), "Mute");
        // Measured: SendInput of E0 45 arrived as virtual key 0xFF (no key) and NumLock stayed off.
        assert_eq!(hid_to_scan(0x53), s(0x45), "NumLock is injected without the E0 prefix");
    }

    /// Every key the Mac side can capture must have a scan code here, or it is swallowed on the Mac
    /// and lost on Windows. Mirrors flowseam_mac's table; PrintScreen and friends have no Mac key.
    #[test]
    fn every_key_a_mac_keyboard_sends_has_a_scan_code() {
        let mac_side = (0x04..=0x31).chain(0x33..=0x45).chain(0x49..=0x52).chain(0x53..=0x65)
            .chain(0x67..=0x6F).chain([0x7F, 0x80, 0x81, 0x87, 0x89, 0xE8, 0xEA, 0xEB]).chain(0xE0..=0xE7);
        for hid in mac_side {
            assert!(hid_to_scan(hid).is_some(), "HID {hid:#04x} unmapped");
        }
    }

    /// Two different keys must not inject the same scan code, or one of them types the other.
    /// The only intended overlap is `\` and Non-US `#`, which share a physical position.
    #[test]
    fn no_two_keys_share_a_scan_code_except_the_iso_hash_key() {
        let mut seen = std::collections::HashMap::new();
        for hid in 0u16..=0xFF {
            if let Some(sc) = hid_to_scan(hid) {
                if let Some(prev) = seen.insert(sc, hid) {
                    assert_eq!((prev, hid), (0x31, 0x32), "HID {prev:#04x} and {hid:#04x} share {sc:?}");
                }
            }
        }
    }
}
