//! Held-key and modifier tracking (design §6).
//!
//! State is a set of pressed HID usages, not a counter: a duplicate down
//! (OS auto-repeat) followed by one up leaves nothing behind. Modifier bits
//! are derived from the pressed set on every query, so they cannot go stale.
//! CapsLock is toggle state and never enters the pressed set.

use crate::proto::{ModifierSet, HID_LIMIT};

pub const HID_CAPS_LOCK: u16 = 0x39;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyError {
    HidOutOfRange(u16),
}

/// Modifier HIDs 0xE0..=0xE7 map to four modifier bits, each for left and right.
pub fn modifier_bit(hid: u16) -> Option<u8> {
    match hid {
        0xE0 | 0xE4 => Some(ModifierSet::CTRL),
        0xE1 | 0xE5 => Some(ModifierSet::SHIFT),
        0xE2 | 0xE6 => Some(ModifierSet::ALT),
        0xE3 | 0xE7 => Some(ModifierSet::GUI),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KeyTracker {
    pressed: [u64; 4],
    caps_on: bool,
    caps_held: bool,
}

impl KeyTracker {
    pub fn new() -> Self {
        Self::default()
    }

    fn is_pressed(&self, hid: u16) -> bool {
        self.pressed[(hid / 64) as usize] & (1u64 << (hid % 64)) != 0
    }

    /// Apply a key edge. Returns whether the held set changed.
    pub fn set_key(&mut self, hid: u16, down: bool) -> Result<bool, KeyError> {
        if hid >= HID_LIMIT {
            return Err(KeyError::HidOutOfRange(hid));
        }
        if hid == HID_CAPS_LOCK {
            if down && !self.caps_held {
                self.caps_on = !self.caps_on;
            }
            self.caps_held = down;
            return Ok(false);
        }
        let word = &mut self.pressed[(hid / 64) as usize];
        let bit = 1u64 << (hid % 64);
        let before = *word & bit != 0;
        if down {
            *word |= bit;
        } else {
            *word &= !bit;
        }
        Ok(before != down)
    }

    /// Derived modifier state: held modifier keys plus the CapsLock toggle.
    pub fn mods(&self) -> ModifierSet {
        let mut m = 0u8;
        for hid in 0xE0..=0xE7u16 {
            if self.is_pressed(hid) {
                m |= modifier_bit(hid).unwrap_or(0);
            }
        }
        if self.caps_on {
            m |= ModifierSet::CAPS;
        }
        ModifierSet(m)
    }

    /// Pressed HID usages, sorted ascending (the `Enter.pressed` payload).
    pub fn snapshot(&self) -> Vec<u16> {
        (0..HID_LIMIT).filter(|&h| self.is_pressed(h)).collect()
    }

    /// Release everything held and return the keys that were released.
    /// The CapsLock toggle is state, not a held key, so it is left alone.
    pub fn release_all(&mut self) -> Vec<u16> {
        let released = self.snapshot();
        self.pressed = [0; 4];
        self.caps_held = false;
        released
    }

    /// Rebuild from a `ModSync`: replaces the held set and the CapsLock toggle.
    pub fn sync(&mut self, mods: ModifierSet, pressed: &[u16]) -> Result<(), KeyError> {
        if let Some(&bad) = pressed.iter().find(|&&h| h >= HID_LIMIT) {
            return Err(KeyError::HidOutOfRange(bad));
        }
        self.pressed = [0; 4];
        for &h in pressed {
            self.pressed[(h / 64) as usize] |= 1u64 << (h % 64);
        }
        self.caps_on = mods.contains(ModifierSet::CAPS);
        // caps_held is the physical key, not peer state: leave it alone so an
        // auto-repeat down after sync does not toggle the synced state again.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LCTRL: u16 = 0xE0;
    const LSHIFT: u16 = 0xE1;
    const LALT: u16 = 0xE2;
    const LGUI: u16 = 0xE3;
    const RSHIFT: u16 = 0xE5;
    const KEY_A: u16 = 0x04;

    #[test]
    fn release_all_leaves_no_residue() {
        let mut t = KeyTracker::new();
        t.set_key(LCTRL, true).unwrap();
        t.set_key(KEY_A, true).unwrap();
        assert!(t.mods().contains(ModifierSet::CTRL));
        let released = t.release_all();
        assert_eq!(released, vec![KEY_A, LCTRL]);
        assert!(t.snapshot().is_empty());
        assert_eq!(t.mods(), ModifierSet(0));
    }

    #[test]
    fn up_without_matching_down_is_a_noop() {
        let mut t = KeyTracker::new();
        assert_eq!(t.set_key(KEY_A, false), Ok(false));
        assert_eq!(t.set_key(LCTRL, false), Ok(false));
        assert_eq!(t.mods(), ModifierSet(0));
        assert!(t.snapshot().is_empty());
    }

    #[test]
    fn auto_repeat_down_does_not_need_two_ups() {
        // A counter-based tracker would leave Ctrl held here.
        let mut t = KeyTracker::new();
        assert_eq!(t.set_key(LCTRL, true), Ok(true));
        assert_eq!(t.set_key(LCTRL, true), Ok(false), "repeat reports no change");
        assert_eq!(t.set_key(LCTRL, false), Ok(true));
        assert_eq!(t.mods(), ModifierSet(0));
    }

    #[test]
    fn left_and_right_modifiers_are_independent() {
        let mut t = KeyTracker::new();
        t.set_key(LSHIFT, true).unwrap();
        t.set_key(RSHIFT, true).unwrap();
        t.set_key(LSHIFT, false).unwrap();
        assert!(t.mods().contains(ModifierSet::SHIFT), "right shift still held");
        t.set_key(RSHIFT, false).unwrap();
        assert_eq!(t.mods(), ModifierSet(0));
    }

    #[test]
    fn caps_lock_toggles_on_down_edge_and_is_never_pressed() {
        let mut t = KeyTracker::new();
        t.set_key(HID_CAPS_LOCK, true).unwrap();
        assert!(t.mods().contains(ModifierSet::CAPS));
        assert!(t.snapshot().is_empty(), "CapsLock is toggle state, not a held key");
        t.set_key(HID_CAPS_LOCK, false).unwrap();
        assert!(t.mods().contains(ModifierSet::CAPS), "release does not toggle");
        t.set_key(HID_CAPS_LOCK, true).unwrap();
        assert!(!t.mods().contains(ModifierSet::CAPS), "second press toggles off");
        // Auto-repeat down while held must not toggle again.
        t.set_key(HID_CAPS_LOCK, true).unwrap();
        assert!(!t.mods().contains(ModifierSet::CAPS));
    }

    #[test]
    fn release_all_keeps_caps_toggle() {
        let mut t = KeyTracker::new();
        t.set_key(HID_CAPS_LOCK, true).unwrap();
        t.set_key(HID_CAPS_LOCK, false).unwrap();
        t.release_all();
        assert!(t.mods().contains(ModifierSet::CAPS));
        assert_eq!(t.mods().0 & !ModifierSet::CAPS, 0);
    }

    #[test]
    fn snapshot_is_sorted_and_covers_word_boundaries() {
        let mut t = KeyTracker::new();
        for h in [200u16, 3, 64, 63, 0] {
            t.set_key(h, true).unwrap();
        }
        assert_eq!(t.snapshot(), vec![0, 3, 63, 64, 200]);
    }

    #[test]
    fn out_of_range_hid_is_rejected() {
        let mut t = KeyTracker::new();
        assert_eq!(t.set_key(256, true), Err(KeyError::HidOutOfRange(256)));
        assert!(t.snapshot().is_empty());
    }

    #[test]
    fn sync_while_caps_physically_held_does_not_retoggle_on_repeat() {
        let mut t = KeyTracker::new();
        t.set_key(HID_CAPS_LOCK, true).unwrap(); // caps on, key down
        t.sync(ModifierSet(0), &[]).unwrap(); // peer says caps off; key still physically down
        t.set_key(HID_CAPS_LOCK, true).unwrap(); // OS auto-repeat down
        assert!(!t.mods().contains(ModifierSet::CAPS), "auto-repeat must not toggle caps back on");
    }

    #[test]
    fn sync_replaces_state_wholesale() {
        let mut t = KeyTracker::new();
        t.set_key(KEY_A, true).unwrap();
        t.sync(ModifierSet(ModifierSet::ALT | ModifierSet::CAPS), &[LALT, LGUI]).unwrap();
        assert_eq!(t.snapshot(), vec![LALT, LGUI], "stale KEY_A must be gone");
        assert!(t.mods().contains(ModifierSet::ALT));
        assert!(t.mods().contains(ModifierSet::GUI));
        assert!(t.mods().contains(ModifierSet::CAPS));
        assert!(t.sync(ModifierSet(0), &[300]).is_err());
    }
}
