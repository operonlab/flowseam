//! Pure conversion between the GUI's editable state and `flowseam_core::config::Config`.
//! No egui types here so it can be tested without a window.

use flowseam_core::config::{Config, ConfigError, Edge, KeyMap, Peer, Switch};

pub const EDGES: [Edge; 4] = [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom];

/// GUI label for the side a peer sits on.
pub fn edge_label(e: Edge) -> &'static str {
    match e {
        Edge::Left => "左",
        Edge::Right => "右",
        Edge::Top => "上",
        Edge::Bottom => "下",
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
}

/// Choices in the order the GUI lists them; the default first.
pub const KEY_MAPS: [KeyMap; 3] = [KeyMap::Position, KeyMap::Mac, KeyMap::None];

/// GUI label for a key mapping mode.
pub fn keys_label(k: KeyMap) -> &'static str {
    match k {
        KeyMap::Position => "依鍵盤位置（⌥=Win、⌘=Alt）",
        KeyMap::Mac => "依 Mac 快捷鍵（⌘C 複製、⌘Tab 切換、單按 ⌥ 開始）",
        KeyMap::None => "不轉換（對方也是 Mac）",
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
                .map(|p| PeerRow { name: p.name.clone(), addr: p.addr.clone(), edge: p.edge, fingerprint: p.fingerprint.clone(), keys: p.keys })
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
    pub fn apply_pairing(&mut self, idx: usize, addr: &str, fp: &flowseam_net::identity::Fingerprint) -> Result<(), String> {
        let row = self.peers.get_mut(idx).ok_or_else(|| "配對期間這台裝置被移除了，請重新配對".to_string())?;
        if row.addr.trim() != addr {
            return Err(format!("配對期間位址從 {addr} 改成 {}，請重新配對", row.addr.trim()));
        }
        row.fingerprint = flowseam_net::identity::to_hex(fp);
        Ok(())
    }

    /// One label per row for the pairing picker, distinct even when names repeat or are empty.
    pub fn pairing_labels(&self) -> Vec<String> {
        let base: Vec<String> = self
            .peers
            .iter()
            .map(|p| {
                let name = if p.name.trim().is_empty() { "（未命名）" } else { p.name.trim() };
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

const MODIFIERS: [&str; 5] = ["ctrl", "alt", "opt", "cmd", "shift"];

// TODO: flowseam_mac has no chord parser yet; this is a syntax-only check. Replace with the backend's
// parser once it exists so the GUI and the worker cannot disagree on what is valid.
pub fn check_chord(s: &str) -> Result<(), String> {
    let tokens: Vec<String> = s.split('+').map(|t| t.trim().to_ascii_lowercase()).collect();
    if tokens.iter().any(|t| t.is_empty()) {
        return Err(format!("chord {s:?} 含空的片段"));
    }
    let (key, mods) = tokens.split_last().expect("split 至少回傳一段");
    if MODIFIERS.contains(&key.as_str()) {
        return Err(format!("chord {s:?} 最後一段必須是按鍵，不是修飾鍵"));
    }
    if mods.is_empty() {
        return Err(format!("chord {s:?} 至少要一個修飾鍵（{}）", MODIFIERS.join("/")));
    }
    for (i, m) in mods.iter().enumerate() {
        if !MODIFIERS.contains(&m.as_str()) {
            return Err(format!("chord {s:?} 的 {m:?} 不是修飾鍵（{}）", MODIFIERS.join("/")));
        }
        if mods[..i].contains(m) {
            return Err(format!("chord {s:?} 的 {m:?} 重複"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Config {
        Config {
            listen: "0.0.0.0:24800".into(),
            switch: Switch { dwell_ms: 350, arm_dist: 6, chord: "ctrl+alt+cmd+s".into(), boundary_px: 3 },
            peers: vec![
                Peer { name: "mini".into(), addr: "192.168.1.20:24800".into(), edge: Edge::Right, fingerprint: "ab".repeat(32), keys: KeyMap::Position },
                Peer { name: "air".into(), addr: "192.168.1.21:24800".into(), edge: Edge::Left, fingerprint: "cd".repeat(32), keys: KeyMap::Mac },
            ],
        }
    }

    #[test]
    fn config_ui_config_round_trip() {
        let c = sample();
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
    fn chord_shapes() {
        assert!(check_chord("ctrl+alt+cmd+s").is_ok());
        assert!(check_chord("Ctrl + Shift + F1").is_ok());
        for bad in ["", "s", "ctrl", "ctrl+alt", "ctrl+ctrl+s", "foo+s", "ctrl++s", "+s"] {
            assert!(check_chord(bad).is_err(), "{bad:?} 應被拒");
        }
    }

    #[test]
    fn pairing_writes_hex_fingerprint_to_that_row_only() {
        let mut ui = UiState::from_config(&sample());
        ui.peers[1].fingerprint.clear();
        let before = ui.peers[0].fingerprint.clone();
        ui.apply_pairing(1, "192.168.1.21:24800", &[0x0f; 32]).unwrap();
        assert_eq!(ui.peers[1].fingerprint, "0f".repeat(32));
        assert_eq!(ui.peers[0].fingerprint, before);
        assert!(ui.problems().is_empty());
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
        ui.peers.push(PeerRow { name: " ".into(), addr: String::new(), edge: Edge::Top, fingerprint: String::new(), keys: KeyMap::Mac });
        ui.peers.push(PeerRow { name: String::new(), addr: String::new(), edge: Edge::Bottom, fingerprint: String::new(), keys: KeyMap::Mac });
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
