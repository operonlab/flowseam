//! Pure conversion between the GUI's editable state and `flowseam_core::config::Config`.
//! No egui types here so it can be tested without a window.

use flowseam_core::config::{Config, ConfigError, DisplaySwitch, Edge, GestureMap, KeyMap, Peer, Switch};
use flowseam_core::gesture::Habit;

pub const EDGES: [Edge; 4] = [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom];

/// GUI label for the side a peer sits on.
pub fn edge_label(e: Edge) -> &'static str {
    match e {
        Edge::Left => crate::i18n::tr("左"),
        Edge::Right => crate::i18n::tr("右"),
        Edge::Top => crate::i18n::tr("上"),
        Edge::Bottom => crate::i18n::tr("下"),
    }
}

/// A switch chord as macOS writes shortcuts: `ctrl+alt+cmd+s` is ⌃⌥⌘S.
pub fn chord_symbols(chord: &str) -> String {
    chord
        .split('+')
        .map(|p| match p.trim().to_ascii_lowercase().as_str() {
            "ctrl" | "control" => "⌃".to_string(),
            "alt" | "opt" | "option" => "⌥".to_string(),
            "cmd" | "command" | "super" => "⌘".to_string(),
            "shift" => "⇧".to_string(),
            k => k.to_uppercase(),
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerRow {
    pub name: String,
    pub addr: String,
    pub edge: Edge,
    /// 64 hex chars once paired; empty before.
    pub fingerprint: String,
    pub keys: KeyMap,
    pub gestures: GestureMap,
    /// The monitors that switch with this peer, edited in the settings' monitor rows.
    pub displays: Vec<DisplaySwitch>,
}

/// Choices in the order the GUI lists them; the default first.
pub const KEY_MAPS: [KeyMap; 3] = [KeyMap::Position, KeyMap::Mac, KeyMap::None];

/// The key maps a peer row offers. A PC keyboard has no Mac map: the Windows worker would downgrade it.
pub fn key_maps(windows: bool) -> &'static [KeyMap] {
    if windows { &[KeyMap::Position, KeyMap::None] } else { &KEY_MAPS }
}

/// What a row holding `k` shows: on Windows a stored `Mac` map is what the worker runs as `Position`.
pub fn shown_keys(k: KeyMap, windows: bool) -> KeyMap {
    if windows && k == KeyMap::Mac { KeyMap::Position } else { k }
}

pub fn keys_label_for(k: KeyMap, windows: bool) -> &'static str {
    if !windows {
        return keys_label(k);
    }
    match shown_keys(k, true) {
        KeyMap::None => crate::i18n::tr("不轉換（對方也是 PC）"),
        _ => crate::i18n::tr("依鍵盤位置（Alt=⌘、Win=⌥）"),
    }
}

/// Windows does not capture trackpad gestures, so there is nothing to map.
pub fn shows_gesture_map(windows: bool) -> bool {
    !windows
}

/// The chord as keycaps read it: Mac symbols on a Mac; on a PC `cmd` in the config is the Windows key.
pub fn chord_display(chord: &str, windows: bool) -> String {
    if !windows {
        return chord_symbols(chord);
    }
    chord
        .split('+')
        .map(|p| match p.trim().to_ascii_lowercase().as_str() {
            "ctrl" | "control" => "Ctrl".to_string(),
            "alt" | "opt" | "option" => "Alt".to_string(),
            "cmd" | "command" | "gui" | "win" | "super" => "Win".to_string(),
            "shift" => "Shift".to_string(),
            k => k.to_uppercase(),
        })
        .collect::<Vec<_>>()
        .join("+")
}

/// GUI label for a key mapping mode.
pub fn keys_label(k: KeyMap) -> &'static str {
    match k {
        KeyMap::Position => crate::i18n::tr("依鍵盤位置（⌥=Win、⌘=Alt）"),
        KeyMap::Mac => crate::i18n::tr("依 Mac 快捷鍵（⌘C 複製、⌘Tab 切換、單按 ⌥ 開始）"),
        KeyMap::None => crate::i18n::tr("不轉換（對方也是 Mac）"),
    }
}

/// GUI label for a gesture habit.
pub fn habit_label(h: Habit) -> &'static str {
    match h {
        Habit::Mac => crate::i18n::tr("依 Mac 手勢（下滑＝所有視窗）"),
        Habit::Windows => crate::i18n::tr("依 Windows 觸控板（下滑＝顯示桌面）"),
    }
}

/// The choice a gesture habit is when picked by hand.
pub fn chosen(h: Habit) -> GestureMap {
    match h {
        Habit::Mac => GestureMap::Mac,
        Habit::Windows => GestureMap::Windows,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiState {
    pub listen: String,
    pub dwell_ms: u64,
    pub arm_dist: i32,
    pub boundary_px: i32,
    pub chord: String,
    pub peers: Vec<PeerRow>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Controller,
    Driven,
}

/// Every machine can drive peers now, so the controls are shown everywhere; what differs by platform is only
/// how they read (`keys_label_for`, `chord_display`) and the gesture row (`shows_gesture_map`). The role
/// decides one thing: whether a monitor may still show this PC while nobody drives it (`back_to`).
pub fn role() -> Role {
    if cfg!(windows) {
        Role::Driven
    } else {
        Role::Controller
    }
}

const SHARING_MARK: &str = "sharing-on";

const MONITOR_SKIPS: &str = "monitor-skips";

/// Monitors unticked from a peer's switch button in the tray, as (peer, monitor UUID). A monitor is in the
/// switch unless it was unticked, so one added later switches too. A file that cannot be read is an error
/// for the caller to show; a missing one is the empty set.
pub fn monitor_skips(dir: &std::path::Path) -> std::io::Result<std::collections::BTreeSet<(String, String)>> {
    let text = match std::fs::read_to_string(dir.join(MONITOR_SKIPS)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
        r => r?,
    };
    // One `<uuid> <peer>` per line: the peer last, since a name may hold spaces.
    Ok(text.lines().filter_map(|l| l.split_once(' ')).map(|(u, p)| (p.to_string(), u.to_string())).filter(|(p, u)| !p.is_empty() && !u.is_empty()).collect())
}

pub fn save_monitor_skips(dir: &std::path::Path, skips: &std::collections::BTreeSet<(String, String)>) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let text: String = skips.iter().map(|(p, u)| format!("{u} {p}\n")).collect();
    std::fs::write(dir.join(MONITOR_SKIPS), text)
}

/// Whether sharing was on when it was last switched. A machine that restarts, or an app that was updated,
/// comes back sharing: a driven machine has nobody at its own keyboard to switch it back on.
pub fn sharing_remembered(dir: &std::path::Path) -> bool {
    dir.join(SHARING_MARK).is_file()
}

pub fn remember_sharing(dir: &std::path::Path, on: bool) -> std::io::Result<()> {
    let mark = dir.join(SHARING_MARK);
    if on {
        std::fs::create_dir_all(dir)?;
        std::fs::write(mark, b"on\n")
    } else {
        match std::fs::remove_file(mark) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            r => r,
        }
    }
}

impl UiState {
    pub fn from_config(c: &Config) -> Self {
        UiState {
            listen: c.listen.clone(),
            dwell_ms: c.switch.dwell_ms,
            arm_dist: c.switch.arm_dist,
            boundary_px: c.switch.boundary_px,
            chord: c.switch.chord.clone(),
            peers: c
                .peers
                .iter()
                .map(|p| PeerRow { name: p.name.clone(), addr: p.addr.clone(), edge: p.edge, fingerprint: p.fingerprint.clone(), keys: p.keys, gestures: p.gestures, displays: p.displays.clone() })
                .collect(),
        }
    }

    pub fn to_config(&self) -> Config {
        Config {
            listen: self.listen.trim().to_string(),
            switch: Switch {
                dwell_ms: self.dwell_ms,
                arm_dist: self.arm_dist,
                chord: self.chord.trim().to_string(),
                boundary_px: self.boundary_px,
            },
            peers: self
                .peers
                .iter()
                .map(|p| Peer {
                    name: p.name.trim().to_string(),
                    addr: p.addr.trim().to_string(),
                    edge: p.edge,
                    fingerprint: p.fingerprint.clone(),
                    keys: p.keys,
                    gestures: p.gestures,
                    displays: p.displays.clone(),
                })
                .collect(),
        }
    }

    /// Every problem the user must fix before saving: all of `Config::validate` plus the chord check.
    pub fn problems(&self) -> Vec<String> {
        let mut out = match self.to_config().validate_draft() {
            Ok(()) => Vec::new(),
            Err(ConfigError::Invalid(list)) => list,
            Err(other) => vec![other.to_string()],
        };
        if let Err(e) = check_chord(&self.chord) {
            out.push(e);
        }
        out
    }

    /// Record a pairing result on the row it was started for. Pairing runs in the background, so
    /// the row may have been removed or its address edited meanwhile: both are errors, never a
    /// write to whichever row now sits at `idx`.
    /// Whether the fingerprint changed: pairing again with a computer that is already paired gives the same one,
    /// and then there is nothing new to save.
    pub fn apply_pairing(&mut self, idx: usize, addr: &str, fp: &flowseam_net::identity::Fingerprint) -> Result<bool, String> {
        let row = self.peers.get_mut(idx).ok_or_else(|| crate::i18n::tr("配對期間這台裝置被移除了，請重新配對").to_string())?;
        if row.addr.trim() != addr {
            return Err(crate::i18n::trf("配對期間位址從 {} 改成 {}，請重新配對", &[&addr, &row.addr.trim()]));
        }
        let hex = flowseam_net::identity::to_hex(fp);
        let changed = row.fingerprint != hex;
        row.fingerprint = hex;
        Ok(changed)
    }

    /// One label per row for the pairing picker, distinct even when names repeat or are empty.
    pub fn pairing_labels(&self) -> Vec<String> {
        let base: Vec<String> = self
            .peers
            .iter()
            .map(|p| {
                let name = if p.name.trim().is_empty() { crate::i18n::tr("（未命名）") } else { p.name.trim() };
                if p.addr.trim().is_empty() { name.to_string() } else { format!("{name} · {}", p.addr.trim()) }
            })
            .collect();
        base.iter()
            .enumerate()
            .map(|(i, b)| if base.iter().filter(|o| *o == b).count() > 1 { format!("{b} #{}", i + 1) } else { b.clone() })
            .collect()
    }
}

/// The first 8 characters of a fingerprint for display; shorter strings are shown whole.
pub fn fingerprint_short(fp: &str) -> &str {
    fp.char_indices().nth(8).map_or(fp, |(i, _)| &fp[..i])
}

/// Modifier names shown in messages; the parser also takes control/option/command/gui/win/super.
const MODIFIERS: [&str; 5] = ["ctrl", "alt", "opt", "cmd", "shift"];

/// Validates with the parser the worker runs, so the window and the worker cannot disagree on a chord.
pub fn check_chord(s: &str) -> Result<(), String> {
    use flowseam_core::chord::{parse, ChordError};
    let q = format!("{s:?}");
    parse(s).map(|_| ()).map_err(|e| match e {
        ChordError::Empty => crate::i18n::trf("chord {} 含空的片段", &[&q]),
        ChordError::UnknownToken(t) if t.is_empty() => crate::i18n::trf("chord {} 含空的片段", &[&q]),
        ChordError::UnknownToken(t) | ChordError::TwoKeys(t, _) => crate::i18n::trf("chord {} 的 {} 不是修飾鍵（{}）", &[&q, &format!("{t:?}"), &MODIFIERS.join("/")]),
        ChordError::NoKey => crate::i18n::trf("chord {} 最後一段必須是按鍵，不是修飾鍵", &[&q]),
        ChordError::NoModifier => crate::i18n::trf("chord {} 至少要一個修飾鍵（{}）", &[&q, &MODIFIERS.join("/")]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Config {
        Config {
            listen: "0.0.0.0:24800".into(),
            switch: Switch { dwell_ms: 350, arm_dist: 6, chord: "ctrl+alt+cmd+s".into(), boundary_px: 3 },
            peers: vec![
                Peer { name: "mini".into(), addr: "192.168.1.20:24800".into(), edge: Edge::Right, fingerprint: "ab".repeat(32), keys: KeyMap::Position, gestures: Default::default(), displays: Vec::new() },
                Peer { name: "air".into(), addr: "192.168.1.21:24800".into(), edge: Edge::Left, fingerprint: "cd".repeat(32), keys: KeyMap::Mac, gestures: GestureMap::Windows, displays: Vec::new() },
            ],
        }
    }

    #[test]
    fn config_ui_config_round_trip() {
        let c = sample();
        assert_eq!(UiState::from_config(&c).to_config(), c);
    }

    #[test]
    fn a_save_from_the_window_keeps_the_peer_monitors() {
        let mut c = sample();
        c.peers[1].displays = vec![DisplaySwitch { uuid: "U".into(), input: 15, home: 18, verified: true }, DisplaySwitch { uuid: "V".into(), input: 17, home: 19, verified: false }];
        assert_eq!(UiState::from_config(&c).to_config(), c);
    }

    #[test]
    fn to_config_trims_user_typing() {
        let mut ui = UiState::from_config(&sample());
        ui.peers[0].name = "  mini ".into();
        ui.chord = " ctrl+alt+cmd+s ".into();
        let c = ui.to_config();
        assert_eq!(c.peers[0].name, "mini");
        assert_eq!(c.switch.chord, "ctrl+alt+cmd+s");
    }

    #[test]
    fn valid_state_has_no_problems() {
        assert!(UiState::from_config(&sample()).problems().is_empty());
    }

    #[test]
    fn same_edge_twice_is_reported_alongside_other_errors() {
        let mut ui = UiState::from_config(&sample());
        ui.peers[1].edge = Edge::Right;
        ui.dwell_ms = 10;
        let all = ui.problems().join("|");
        assert!(all.contains("兩台"), "{all}");
        assert!(all.contains("dwell_ms"), "{all}");
    }

    #[test]
    fn bad_chord_is_reported_with_other_errors() {
        let mut ui = UiState::from_config(&sample());
        ui.chord = "ctrl+".into();
        ui.arm_dist = 0;
        let all = ui.problems().join("|");
        assert!(all.contains("chord"), "{all}");
        assert!(all.contains("arm_dist"), "{all}");
    }

    #[test]
    fn a_win_chord_does_not_block_save() {
        let mut ui = UiState::from_config(&sample());
        ui.chord = "ctrl+alt+win+s".into();
        assert!(!ui.problems().iter().any(|p| p.contains("chord")));
    }

    #[test]
    fn chord_shapes() {
        assert!(check_chord("ctrl+alt+cmd+s").is_ok());
        assert!(check_chord("Ctrl + Shift + F1").is_ok());
        for bad in ["", "s", "ctrl", "ctrl+alt", "foo+s", "ctrl++s", "+s"] {
            assert!(check_chord(bad).is_err(), "{bad:?} 應被拒");
        }
    }

    #[test]
    fn pairing_writes_hex_fingerprint_to_that_row_only() {
        let mut ui = UiState::from_config(&sample());
        ui.peers[1].fingerprint.clear();
        let before = ui.peers[0].fingerprint.clone();
        assert!(ui.apply_pairing(1, "192.168.1.21:24800", &[0x0f; 32]).unwrap(), "a new fingerprint is a change");
        assert_eq!(ui.peers[1].fingerprint, "0f".repeat(32));
        assert_eq!(ui.peers[0].fingerprint, before);
        assert!(ui.problems().is_empty());
    }

    /// Pairing again with a computer that kept its identity: the window must not ask for a Save that is greyed out.
    #[test]
    fn pairing_again_with_the_same_fingerprint_is_not_a_change() {
        let mut ui = UiState::from_config(&sample());
        ui.peers[1].fingerprint = "0f".repeat(32);
        assert!(!ui.apply_pairing(1, "192.168.1.21:24800", &[0x0f; 32]).unwrap());
        assert!(ui.apply_pairing(1, "192.168.1.21:24800", &[0x1e; 32]).unwrap());
    }

    #[test]
    fn pairing_unknown_row_is_error() {
        let mut ui = UiState::from_config(&sample());
        assert!(ui.apply_pairing(9, "192.168.1.21:24800", &[0; 32]).is_err());
    }

    #[test]
    fn pairing_result_never_lands_on_a_row_that_changed_meanwhile() {
        // Row 0 removed while pairing row 1: row 1 is now at index 0 with another address.
        let mut ui = UiState::from_config(&sample());
        let started_for = ui.peers[0].addr.clone();
        ui.peers.remove(0);
        let before = ui.peers[0].fingerprint.clone();
        assert!(ui.apply_pairing(0, &started_for, &[0x0f; 32]).is_err());
        assert_eq!(ui.peers[0].fingerprint, before);
        // Same row, address edited.
        let mut ui = UiState::from_config(&sample());
        ui.peers[1].addr = "192.168.1.99:24800".into();
        assert!(ui.apply_pairing(1, "192.168.1.21:24800", &[0x0f; 32]).is_err());
        assert_eq!(ui.peers[1].fingerprint, "cd".repeat(32));
    }

    #[test]
    fn pairing_labels_tell_every_row_apart() {
        let mut ui = UiState::from_config(&sample());
        ui.peers[1].name = "mini".into();
        ui.peers[1].addr = ui.peers[0].addr.clone();
        ui.peers.push(PeerRow { name: " ".into(), addr: String::new(), edge: Edge::Top, fingerprint: String::new(), keys: KeyMap::Mac, gestures: Default::default(), displays: Vec::new() });
        ui.peers.push(PeerRow { name: String::new(), addr: String::new(), edge: Edge::Bottom, fingerprint: String::new(), keys: KeyMap::Mac, gestures: Default::default(), displays: Vec::new() });
        let labels = ui.pairing_labels();
        let distinct: std::collections::BTreeSet<_> = labels.iter().collect();
        assert_eq!(distinct.len(), labels.len(), "{labels:?}");
        assert_eq!(labels[0], "mini · 192.168.1.20:24800 #1");
    }

    #[test]
    fn short_fingerprint_never_panics() {
        assert_eq!(fingerprint_short(&"ab".repeat(32)), "abababab");
        assert_eq!(fingerprint_short("abc"), "abc");
        assert_eq!(fingerprint_short(""), "");
        assert_eq!(fingerprint_short("指紋指紋指紋指紋指紋"), "指紋指紋指紋指紋");
    }

    #[test]
    fn chords_read_as_mac_shortcut_symbols() {
        assert_eq!(chord_symbols("ctrl+alt+cmd+s"), "⌃⌥⌘S");
        assert_eq!(chord_symbols("shift+cmd+f1"), "⇧⌘F1");
    }
}

#[cfg(test)]
mod sharing_tests {
    use super::*;

    #[test]
    fn sharing_is_remembered_across_starts_until_switched_off() {
        let dir = std::env::temp_dir().join(format!("flowseam-sharing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!sharing_remembered(&dir), "a fresh install starts with sharing off");
        remember_sharing(&dir, true).unwrap();
        assert!(sharing_remembered(&dir));
        remember_sharing(&dir, false).unwrap();
        assert!(!sharing_remembered(&dir));
        remember_sharing(&dir, false).unwrap(); // switching off twice is fine
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unticked_monitors_are_remembered_per_peer() {
        let dir = std::env::temp_dir().join(format!("flowseam-skips-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(monitor_skips(&dir).unwrap().is_empty(), "a fresh install ticks every monitor");
        let skips: std::collections::BTreeSet<_> = [("my pc".to_string(), "U-1".to_string()), ("air".to_string(), "V".to_string())].into();
        save_monitor_skips(&dir, &skips).unwrap();
        assert_eq!(monitor_skips(&dir).unwrap(), skips, "a peer name with a space survives");
        save_monitor_skips(&dir, &Default::default()).unwrap();
        assert!(monitor_skips(&dir).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod platform_tests {
    use super::*;

    #[test]
    fn windows_offers_no_mac_key_map() {
        assert_eq!(key_maps(false), &[KeyMap::Position, KeyMap::Mac, KeyMap::None]);
        assert_eq!(key_maps(true), &[KeyMap::Position, KeyMap::None]);
    }

    #[test]
    fn a_stored_mac_map_shows_as_position_on_windows() {
        assert_eq!(shown_keys(KeyMap::Mac, true), KeyMap::Position);
        assert_eq!(shown_keys(KeyMap::Mac, false), KeyMap::Mac);
        assert_eq!(shown_keys(KeyMap::None, true), KeyMap::None);
    }

    #[test]
    fn key_labels_follow_the_keyboard_in_front_of_you() {
        for k in [KeyMap::Position, KeyMap::Mac, KeyMap::None] {
            assert_eq!(keys_label_for(k, false), keys_label(k), "the Mac labels do not change");
        }
        assert!(keys_label_for(KeyMap::Position, true).contains("Alt=⌘"));
        assert!(keys_label_for(KeyMap::None, true).contains("PC"));
        assert_eq!(keys_label_for(KeyMap::Mac, true), keys_label_for(KeyMap::Position, true));
    }

    #[test]
    fn only_the_mac_has_gesture_rows() {
        assert!(shows_gesture_map(false));
        assert!(!shows_gesture_map(true));
    }

    #[test]
    fn chord_accepts_every_modifier_spelling_the_worker_parses() {
        for ok in ["ctrl+alt+win+s", "ctrl+super+s", "control+command+s", "option+gui+f1"] {
            assert!(check_chord(ok).is_ok(), "{ok:?}");
        }
        for bad in ["ctrl+", "ctrl+banana", "ctrl+a+b", "win", "s"] {
            assert!(check_chord(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn windows_names_cover_win_and_gui() {
        assert_eq!(chord_display("ctrl+alt+win+s", true), "Ctrl+Alt+Win+S");
        assert_eq!(chord_display("ctrl+gui+s", true), "Ctrl+Win+S");
    }

    #[test]
    fn chord_reads_in_the_names_on_the_keyboard() {
        assert_eq!(chord_display("ctrl+alt+cmd+s", false), "⌃⌥⌘S");
        assert_eq!(chord_display("ctrl+alt+cmd+s", true), "Ctrl+Alt+Win+S");
        assert_eq!(chord_display("shift+opt+f1", true), "Shift+Alt+F1");
    }
}
