//! Mac shortcuts on a PC peer (`KeyMap::Mac`). A one-to-one modifier swap cannot do it: Command is Ctrl
//! for ⌘C, Alt for ⌘Tab and no modifier at all for ⌘← (Home); ⌃⌘Q is Win+L. So every modifier is held
//! back when pressed, and the next key decides which modifiers the peer gets with it.
//!
//! The table follows Microsoft's "Moving from Mac to Windows" shortcut table
//! (learn.microsoft.com/windows/dev-environment/mac-to-windows, checked 2026-11-05), plus text navigation:
//!
//! | Mac                    | peer                          |
//! |------------------------|-------------------------------|
//! | ⌘ + key, ⌃ + key       | Ctrl + key                    |
//! | ⌥ + key                | Alt + key                     |
//! | ⌘Tab, ⌘⇧Tab            | Alt+Tab, Alt+Shift+Tab (Alt held while ⌘ is) |
//! | ⌘Q                     | Alt+F4                        |
//! | ⌘⇧Z                    | Ctrl+Y                        |
//! | ⌘← ⌘→ (⇧)              | Home, End (Shift)             |
//! | ⌘↑ ⌘↓ (⇧)              | Ctrl+Home, Ctrl+End (Shift)   |
//! | ⌥← ⌥→ ⌥↑ ⌥↓ (⇧), ⌥⌫, ⌥⌦ | Ctrl + that key (Shift)       |
//! | ⌘Space                 | Win+S (search)                |
//! | ⌃Space                 | Win+Space (input source)      |
//! | ⌃⌘Space                | Win+. (emoji)                 |
//! | ⌃⌘Q                    | Win+L (lock)                  |
//! | ⌘M, ⌘H                 | Win+↓ (minimize)              |
//! | ⌘⌥M                    | Win+M (minimize all)          |
//! | ⌘F3, ⌘⌥D               | Win+D (show desktop)          |
//! | ⌃⌘F                    | Win+↑ (maximize)              |
//! | ⌃↑                     | Win+Tab (task view)           |
//! | ⌃← ⌃→                  | Win+Ctrl+← → (virtual desktops) |
//! | ⌘⇧3                    | Win+PrintScreen               |
//! | ⌘⇧4, ⌘⇧5, ⌃⌘⇧4         | Win+Shift+S (snip)            |
//! | ⌃⌘⇧3                   | PrintScreen (to clipboard)    |
//! | ⌘⌥F                    | Ctrl+H (replace)              |
//! | ⌘⌥Esc                  | Ctrl+Shift+Esc (Task Manager) |
//! | ⌘, ⌥, ⌃, ⇧ + click or wheel | Ctrl, Alt, Ctrl, Shift held for it |
//!
//! A modifier tapped alone: ⌥ is the Windows key (Start; the PC keycap in that place), ⇧ and ⌃ go
//! through as taps (Shift switches the Chinese IME), ⌘ sends nothing, as on a Mac.

use std::collections::{BTreeMap, BTreeSet};

const LCTRL: u16 = 0xE0;
const LSHIFT: u16 = 0xE1;
const LALT: u16 = 0xE2;
const LGUI: u16 = 0xE3;
const RCTRL: u16 = 0xE4;
const RSHIFT: u16 = 0xE5;
const RALT: u16 = 0xE6;
const RGUI: u16 = 0xE7;
const D: u16 = 0x07;
const F: u16 = 0x09;
const H: u16 = 0x0B;
const L: u16 = 0x0F;
const M: u16 = 0x10;
const Q: u16 = 0x14;
const S: u16 = 0x16;
const Y: u16 = 0x1C;
const Z: u16 = 0x1D;
const N3: u16 = 0x20;
const N4: u16 = 0x21;
const N5: u16 = 0x22;
const ESC: u16 = 0x29;
const BACKSPACE: u16 = 0x2A;
const TAB: u16 = 0x2B;
const SPACE: u16 = 0x2C;
const PERIOD: u16 = 0x37;
const F3: u16 = 0x3C;
const F4: u16 = 0x3D;
const PRINT_SCREEN: u16 = 0x46;
const HOME: u16 = 0x4A;
const DELETE: u16 = 0x4C;
const END: u16 = 0x4D;
const RIGHT: u16 = 0x4F;
const LEFT: u16 = 0x50;
const DOWN: u16 = 0x51;
const UP: u16 = 0x52;

/// Key events (HID usage, down) to send to the peer.
pub type Out = Vec<(u16, bool)>;

/// The Mac modifiers held, by kind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Held {
    cmd: bool,
    opt: bool,
    ctrl: bool,
    shift: bool,
}

/// Peer modifiers, as the set of their (left-hand) HID usages.
type Mods = BTreeSet<u16>;

#[derive(Debug, Default)]
pub struct MacShortcuts {
    /// Modifier keys held on this Mac. A set, so a repeated down is not a second press.
    mods: BTreeSet<u16>,
    /// The modifier pressed first while none were held, until anything else happens: a bare tap.
    tap: Option<u16>,
    /// Peer modifiers held for the current shortcut.
    chord: Mods,
    /// Keys down on the peer, counted: ⌘Q's F4 and a real F4 are one key there, which goes up only when
    /// the last of them is let go.
    down: BTreeMap<u16, u8>,
    /// For each key of this Mac held down, the key it pressed on the peer, so its release matches.
    held_keys: BTreeMap<u16, u16>,
}

impl MacShortcuts {
    pub fn new() -> Self {
        Self::default()
    }

    /// A translator for a session that starts with `held` already down on this Mac (modifiers first).
    /// None of them is a tap: releasing an Option held while crossing must not open Start.
    pub fn with_held(held: &[u16]) -> Self {
        let mut s = Self::new();
        for &h in held {
            s.key(h, true);
        }
        s.tap = None;
        s
    }

    /// What to send for one key event of this Mac.
    pub fn key(&mut self, hid: u16, down: bool) -> Out {
        let mut out = Out::new();
        if (LCTRL..=RGUI).contains(&hid) {
            if down {
                if self.mods.insert(hid) {
                    self.tap = if self.mods.len() == 1 && self.held_keys.is_empty() { Some(hid) } else { None };
                }
            } else if self.mods.remove(&hid) {
                if self.tap.take() == Some(hid) {
                    self.bare_tap(hid, &mut out);
                }
                self.drop_unheld(&mut out);
            }
            return out;
        }
        self.tap = None;
        if down {
            if let Some(&sent) = self.held_keys.get(&hid) {
                // Auto-repeat: the same key again, under the meaning it went down with.
                out.push((sent, true));
                return out;
            }
            let (mods, sent) = decide(self.held(), hid);
            self.set_chord(mods, &mut out);
            self.held_keys.insert(hid, sent);
            self.press(sent, &mut out);
        } else if let Some(sent) = self.held_keys.remove(&hid) {
            // A release with no press seen (the switch chord's own key) has nothing to let go of.
            self.release(sent, &mut out);
        }
        out
    }

    /// Before a click or a wheel turn: the held modifiers in their plain meaning (⌘-click selects more,
    /// ⇧-click selects a range, ⌘-wheel zooms). A shortcut already decided keeps its modifiers: after ⌘Tab
    /// a click with ⌘ still held is Alt+click, which picks a window in the switcher.
    pub fn pointer(&mut self) -> Out {
        let mut out = Out::new();
        self.tap = None;
        if self.chord.is_empty() && !self.mods.is_empty() {
            self.set_chord(plain(self.held()), &mut out);
        }
        out
    }

    /// Every key held down on the peer.
    pub fn pressed(&self) -> Vec<u16> {
        self.down.keys().copied().collect()
    }

    fn held(&self) -> Held {
        let has = |a: u16, b: u16| self.mods.contains(&a) || self.mods.contains(&b);
        Held { cmd: has(LGUI, RGUI), opt: has(LALT, RALT), ctrl: has(LCTRL, RCTRL), shift: has(LSHIFT, RSHIFT) }
    }

    fn bare_tap(&mut self, hid: u16, out: &mut Out) {
        let k = match hid {
            LALT | RALT => LGUI,
            LSHIFT | RSHIFT => LSHIFT,
            LCTRL | RCTRL => LCTRL,
            _ => return,
        };
        self.press(k, out);
        self.release(k, out);
    }

    /// After a Mac modifier is let go: Shift goes with Shift; Ctrl, Alt and Win go once no ⌘ ⌥ ⌃ is held.
    fn drop_unheld(&mut self, out: &mut Out) {
        let h = self.held();
        let keep: Mods = self.chord.iter().copied().filter(|&m| if m == LSHIFT { h.shift } else { h.cmd || h.opt || h.ctrl }).collect();
        self.set_chord(keep, out);
    }

    /// Make the peer's shortcut modifiers `to`: release those not in it, then press the new ones.
    fn set_chord(&mut self, to: Mods, out: &mut Out) {
        for m in self.chord.difference(&to).copied().collect::<Vec<_>>() {
            self.release(m, out);
        }
        for m in to.difference(&self.chord).copied().collect::<Vec<_>>() {
            self.press(m, out);
        }
        self.chord = to;
    }

    fn press(&mut self, k: u16, out: &mut Out) {
        let n = self.down.entry(k).or_insert(0);
        *n += 1;
        if *n == 1 {
            out.push((k, true));
        }
    }

    fn release(&mut self, k: u16, out: &mut Out) {
        if let Some(n) = self.down.get_mut(&k) {
            *n -= 1;
            if *n == 0 {
                self.down.remove(&k);
                out.push((k, false));
            }
        }
    }
}

/// The modifiers a Mac shortcut has on the peer when nothing special applies.
fn plain(h: Held) -> Mods {
    let mut m = Mods::new();
    if h.cmd || h.ctrl {
        m.insert(LCTRL);
    }
    if h.opt {
        m.insert(LALT);
    }
    if h.shift {
        m.insert(LSHIFT);
    }
    m
}

/// The peer modifiers and key for `key` pressed under the held Mac modifiers.
fn decide(h: Held, key: u16) -> (Mods, u16) {
    let mods = |ks: &[u16]| -> Mods { ks.iter().copied().collect() };
    // Shift as held, added to a special shortcut that keeps it.
    let with_shift = |mut m: Mods| {
        if h.shift {
            m.insert(LSHIFT);
        }
        m
    };
    let Held { cmd, opt, ctrl, shift } = h;
    let special = match (cmd, opt, ctrl, shift, key) {
        (true, false, false, _, TAB) => Some((with_shift(mods(&[LALT])), TAB)),
        (true, false, false, false, Q) => Some((mods(&[LALT]), F4)),
        (true, false, false, true, Z) => Some((mods(&[LCTRL]), Y)),
        (true, false, false, _, LEFT) => Some((with_shift(mods(&[])), HOME)),
        (true, false, false, _, RIGHT) => Some((with_shift(mods(&[])), END)),
        (true, false, false, _, UP) => Some((with_shift(mods(&[LCTRL])), HOME)),
        (true, false, false, _, DOWN) => Some((with_shift(mods(&[LCTRL])), END)),
        (true, false, false, false, SPACE) => Some((mods(&[LGUI]), S)),
        (true, false, false, false, M | H) => Some((mods(&[LGUI]), DOWN)),
        (true, false, false, false, F3) => Some((mods(&[LGUI]), D)),
        (true, false, false, true, N3) => Some((mods(&[LGUI]), PRINT_SCREEN)),
        (true, false, false, true, N4 | N5) => Some((mods(&[LGUI, LSHIFT]), S)),
        (true, true, false, false, M) => Some((mods(&[LGUI]), M)),
        (true, true, false, false, D) => Some((mods(&[LGUI]), D)),
        (true, true, false, false, F) => Some((mods(&[LCTRL]), H)),
        (true, true, false, false, ESC) => Some((mods(&[LCTRL, LSHIFT]), ESC)),
        (true, false, true, false, Q) => Some((mods(&[LGUI]), L)),
        (true, false, true, false, SPACE) => Some((mods(&[LGUI]), PERIOD)),
        (true, false, true, false, F) => Some((mods(&[LGUI]), UP)),
        (true, false, true, true, N3) => Some((mods(&[]), PRINT_SCREEN)),
        (true, false, true, true, N4) => Some((mods(&[LGUI, LSHIFT]), S)),
        (false, false, true, false, UP) => Some((mods(&[LGUI]), TAB)),
        (false, false, true, false, LEFT | RIGHT) => Some((mods(&[LGUI, LCTRL]), key)),
        (false, false, true, false, SPACE) => Some((mods(&[LGUI]), SPACE)),
        (false, true, false, _, LEFT | RIGHT | UP | DOWN | BACKSPACE | DELETE) => Some((with_shift(mods(&[LCTRL])), key)),
        _ => None,
    };
    special.unwrap_or_else(|| (plain(h), key))
}

#[cfg(test)]
mod tests {
    use super::*;

    const C: u16 = 0x06;
    /// (Mac modifiers, Mac key, peer modifiers, peer key, what it does)
    type Row = (&'static [u16], u16, &'static [u16], u16, &'static str);

    /// Feed (key, down) pairs; return everything sent.
    fn run(s: &mut MacShortcuts, keys: &[(u16, bool)]) -> Out {
        keys.iter().flat_map(|&(k, d)| s.key(k, d)).collect()
    }

    /// Press the modifiers, tap `key`, release the modifiers in reverse.
    fn chord(s: &mut MacShortcuts, mods: &[u16], key: u16) -> Out {
        let mut ev: Vec<(u16, bool)> = mods.iter().map(|&m| (m, true)).collect();
        ev.extend([(key, true), (key, false)]);
        ev.extend(mods.iter().rev().map(|&m| (m, false)));
        run(s, &ev)
    }

    /// The peer's view of one shortcut: the keys held when `key` goes down, and that key.
    fn lands_as(mods: &[u16], key: u16) -> (Vec<u16>, u16) {
        let mut s = MacShortcuts::new();
        for &m in mods {
            s.key(m, true);
        }
        let out = s.key(key, true);
        let (&(sent, d), _) = out.split_last().expect("the key itself is sent");
        assert!(d);
        let mut held = s.pressed();
        held.retain(|&k| k != sent);
        (held, sent)
    }

    #[test]
    fn microsofts_mac_to_windows_table() {
        let rows: &[Row] = &[
            (&[LGUI], C, &[LCTRL], C, "copy"),
            (&[LGUI], 0x19, &[LCTRL], 0x19, "paste"),
            (&[LGUI], 0x1A, &[LCTRL], 0x1A, "close tab"),
            (&[LCTRL, LGUI], Q, &[LGUI], L, "lock"),
            (&[LGUI], F3, &[LGUI], D, "show desktop"),
            (&[LGUI, LALT], M, &[LGUI], M, "minimize all"),
            (&[LGUI], SPACE, &[LGUI], S, "search"),
            (&[LGUI], Q, &[LALT], F4, "quit"),
            (&[LGUI], TAB, &[LALT], TAB, "switch apps"),
            (&[LCTRL], UP, &[LGUI], TAB, "show open windows"),
            (&[LCTRL], LEFT, &[LCTRL, LGUI], LEFT, "previous desktop"),
            (&[LCTRL], RIGHT, &[LCTRL, LGUI], RIGHT, "next desktop"),
            (&[LGUI, LSHIFT], N3, &[LGUI], PRINT_SCREEN, "screenshot"),
            (&[LGUI, LSHIFT], N4, &[LSHIFT, LGUI], S, "capture region"),
            (&[LCTRL, LGUI], SPACE, &[LGUI], PERIOD, "emoji"),
        ];
        for &(mac, key, mods, sent, what) in rows {
            assert_eq!(lands_as(mac, key), (mods.to_vec(), sent), "{what}");
        }
    }

    #[test]
    fn text_navigation_and_the_rest_of_the_table() {
        let rows: &[Row] = &[
            (&[LGUI], LEFT, &[], HOME, "line start"),
            (&[LGUI, LSHIFT], RIGHT, &[LSHIFT], END, "select to line end"),
            (&[LGUI], DOWN, &[LCTRL], END, "document end"),
            (&[LALT], LEFT, &[LCTRL], LEFT, "word left"),
            (&[LALT, LSHIFT], RIGHT, &[LCTRL, LSHIFT], RIGHT, "select word right"),
            (&[LALT], BACKSPACE, &[LCTRL], BACKSPACE, "delete word"),
            (&[LGUI, LSHIFT], Z, &[LCTRL], Y, "redo"),
            (&[LGUI], M, &[LGUI], DOWN, "minimize"),
            (&[LGUI, LALT], D, &[LGUI], D, "show desktop"),
            (&[LCTRL, LGUI], F, &[LGUI], UP, "maximize"),
            (&[LGUI, LALT], ESC, &[LCTRL, LSHIFT], ESC, "task manager"),
            (&[LCTRL], SPACE, &[LGUI], SPACE, "input source"),
            (&[LALT], C, &[LALT], C, "Option + other key is Alt"),
            (&[LCTRL], C, &[LCTRL], C, "a terminal's ⌃C"),
            (&[LSHIFT], C, &[LSHIFT], C, "a capital"),
        ];
        for &(mac, key, mods, sent, what) in rows {
            assert_eq!(lands_as(mac, key), (mods.to_vec(), sent), "{what}");
        }
    }

    #[test]
    fn command_tab_holds_alt_while_command_is_held() {
        let mut s = MacShortcuts::new();
        let out = run(&mut s, &[(LGUI, true), (TAB, true), (TAB, false), (TAB, true), (TAB, false), (LGUI, false)]);
        assert_eq!(out, [(LALT, true), (TAB, true), (TAB, false), (TAB, true), (TAB, false), (LALT, false)]);
    }

    /// ⌘⇧Tab then letting go of ⇧ keeps going forward: Shift up, Alt still held.
    #[test]
    fn letting_go_of_shift_keeps_the_rest_of_the_shortcut() {
        let mut s = MacShortcuts::new();
        let out = run(&mut s, &[(LGUI, true), (LSHIFT, true), (TAB, true), (TAB, false), (LSHIFT, false)]);
        assert_eq!(out, [(LSHIFT, true), (LALT, true), (TAB, true), (TAB, false), (LSHIFT, false)]);
        assert_eq!(s.pressed(), [LALT]);
        assert_eq!(s.key(LGUI, false), [(LALT, false)]);
    }

    /// ⌘Tab then ⌘C without letting go of ⌘: the switcher closes (Alt up) before Ctrl+C.
    #[test]
    fn command_changes_meaning_between_keys_while_held() {
        let mut s = MacShortcuts::new();
        let out = run(&mut s, &[(LGUI, true), (TAB, true), (TAB, false), (C, true), (C, false), (LGUI, false)]);
        assert_eq!(out, [(LALT, true), (TAB, true), (TAB, false), (LALT, false), (LCTRL, true), (C, true), (C, false), (LCTRL, false)]);
    }

    #[test]
    fn a_bare_option_is_the_windows_key_shift_and_control_go_through_command_sends_nothing() {
        let mut s = MacShortcuts::new();
        assert_eq!(run(&mut s, &[(LALT, true), (LALT, false)]), [(LGUI, true), (LGUI, false)], "Start");
        assert_eq!(run(&mut s, &[(RSHIFT, true), (RSHIFT, false)]), [(LSHIFT, true), (LSHIFT, false)], "IME switch");
        assert_eq!(run(&mut s, &[(LCTRL, true), (LCTRL, false)]), [(LCTRL, true), (LCTRL, false)]);
        assert!(run(&mut s, &[(LGUI, true), (LGUI, false)]).is_empty());
        assert!(s.pressed().is_empty());
    }

    #[test]
    fn a_modifier_held_when_the_session_starts_is_not_a_tap() {
        let mut s = MacShortcuts::with_held(&[LALT]);
        assert!(s.pressed().is_empty());
        assert!(s.key(LALT, false).is_empty(), "no Start menu");
    }

    /// A modifier that took part in a shortcut, or was pressed with another, is not a tap.
    #[test]
    fn only_a_modifier_alone_is_a_tap() {
        let mut s = MacShortcuts::new();
        let out = chord(&mut s, &[LALT], C);
        assert_eq!(out, [(LALT, true), (C, true), (C, false), (LALT, false)]);
        assert!(run(&mut s, &[(LALT, true), (LGUI, true), (LGUI, false), (LALT, false)]).is_empty());
    }

    #[test]
    fn a_key_is_released_as_what_it_pressed_whatever_the_order() {
        let mut s = MacShortcuts::new();
        let out = run(&mut s, &[(LGUI, true), (Q, true), (LGUI, false), (Q, false)]);
        assert_eq!(out, [(LALT, true), (F4, true), (LALT, false), (F4, false)]);
        assert!(s.pressed().is_empty());
    }

    /// ⌘Q's F4 and a real F4 are one key on the peer: it goes up when the last one does.
    #[test]
    fn two_mac_keys_on_one_peer_key_release_it_last() {
        let mut s = MacShortcuts::new();
        let out = run(&mut s, &[(LGUI, true), (Q, true), (LGUI, false), (F4, true), (Q, false)]);
        assert_eq!(out, [(LALT, true), (F4, true), (LALT, false)], "F4 still held for the real F4");
        assert_eq!(s.key(F4, false), [(F4, false)]);
    }

    /// Auto-repeat resends the held key as it went down; repeated modifier downs are not new presses.
    #[test]
    fn repeats_keep_their_first_meaning() {
        let mut s = MacShortcuts::new();
        let out = run(&mut s, &[(Q, true), (LGUI, true), (LGUI, true), (Q, true), (Q, false), (LGUI, false)]);
        assert_eq!(out, [(Q, true), (Q, true), (Q, false)]);
        assert!(s.pressed().is_empty());
    }

    /// The switch chord's own key went down locally and was swallowed; its release reaches no one.
    #[test]
    fn a_release_without_a_press_sends_nothing() {
        let mut s = MacShortcuts::new();
        assert!(s.key(0x16, false).is_empty());
    }

    #[test]
    fn a_click_or_wheel_holds_the_plain_modifiers_for_it() {
        let mut s = MacShortcuts::new();
        s.key(LGUI, true);
        s.key(LSHIFT, true);
        assert_eq!(s.pointer(), [(LCTRL, true), (LSHIFT, true)]);
        assert!(s.pointer().is_empty(), "already held");
        assert_eq!(run(&mut s, &[(LSHIFT, false), (LGUI, false)]), [(LSHIFT, false), (LCTRL, false)]);
        assert!(s.pointer().is_empty(), "nothing held");
    }

    #[test]
    fn left_and_right_command_are_one_command() {
        let mut s = MacShortcuts::new();
        let out = run(&mut s, &[(LGUI, true), (RGUI, true), (C, true), (C, false), (LGUI, false)]);
        assert_eq!(out, [(LCTRL, true), (C, true), (C, false)], "right ⌘ still held");
        assert_eq!(s.key(RGUI, false), [(LCTRL, false)]);
    }
}
