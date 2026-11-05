//! Emergency exit chord (design §7/§8-4): Ctrl + Option + Cmd + Esc.
//!
//! Decision: extra modifiers (e.g. Shift) do not block the chord. The point of
//! an emergency key is that it fires; a stray held key should not defeat it.
//! The check is stateless, so it fires on every Esc down while the three
//! modifiers are held. Releasing the grab twice is harmless.

use crate::proto::ModifierSet;

pub const HID_ESCAPE: u16 = 0x29;

pub fn is_emergency(mods: ModifierSet, hid: u16, down: bool) -> bool {
    down && hid == HID_ESCAPE
        && mods.contains(ModifierSet::CTRL)
        && mods.contains(ModifierSet::ALT)
        && mods.contains(ModifierSet::GUI)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modifiers::KeyTracker;

    const CTRL: u8 = ModifierSet::CTRL;
    const ALT: u8 = ModifierSet::ALT;
    const GUI: u8 = ModifierSet::GUI;

    #[test]
    fn full_chord_fires_on_escape_down() {
        assert!(is_emergency(ModifierSet(CTRL | ALT | GUI), HID_ESCAPE, true));
    }

    #[test]
    fn each_missing_modifier_blocks_it() {
        assert!(!is_emergency(ModifierSet(ALT | GUI), HID_ESCAPE, true));
        assert!(!is_emergency(ModifierSet(CTRL | GUI), HID_ESCAPE, true));
        assert!(!is_emergency(ModifierSet(CTRL | ALT), HID_ESCAPE, true));
    }

    #[test]
    fn escape_up_and_other_keys_do_not_fire() {
        assert!(!is_emergency(ModifierSet(CTRL | ALT | GUI), HID_ESCAPE, false));
        assert!(!is_emergency(ModifierSet(CTRL | ALT | GUI), 0x04, true));
    }

    #[test]
    fn extra_shift_does_not_defeat_the_chord() {
        assert!(is_emergency(ModifierSet(CTRL | ALT | GUI | ModifierSet::SHIFT), HID_ESCAPE, true));
    }

    #[test]
    fn right_side_modifiers_count_via_the_tracker() {
        // Right Ctrl (0xE4), right Alt (0xE6), right GUI (0xE7) through the real tracker.
        let mut t = KeyTracker::new();
        for hid in [0xE4u16, 0xE6, 0xE7] {
            t.set_key(hid, true).unwrap();
        }
        assert!(is_emergency(t.mods(), HID_ESCAPE, true));
    }

    #[test]
    fn releasing_one_modifier_before_escape_disarms_it() {
        let mut t = KeyTracker::new();
        for hid in [0xE0u16, 0xE2, 0xE3] {
            t.set_key(hid, true).unwrap();
        }
        t.set_key(0xE2, false).unwrap(); // Option lifted
        assert!(!is_emergency(t.mods(), HID_ESCAPE, true));
    }
}
