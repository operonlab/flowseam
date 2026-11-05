//! User configuration: the one schema shared by the GUI, the worker and pairing.
//! Path: `~/.config/flowseam/config.toml`. A missing file means defaults, so a
//! fresh install starts with no peers and never switches.

use std::fs;
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Which edge of the local display leads to a peer. Crossing it (after dwell
/// or the chord) hands input to that peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl From<Edge> for crate::proto::Edge {
    fn from(e: Edge) -> Self {
        match e {
            Edge::Left => Self::Left,
            Edge::Right => Self::Right,
            Edge::Top => Self::Top,
            Edge::Bottom => Self::Bottom,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Peer {
    pub name: String,
    /// `host:port` of the peer's listener.
    pub addr: String,
    pub edge: Edge,
    /// SHA-256 of the peer's TLS certificate, 64 lowercase hex chars. Set by pairing.
    pub fingerprint: String,
    /// How this Mac's keys land on the peer.
    #[serde(default)]
    pub keys: KeyMap,
}

/// How a Mac keyboard's modifiers land on a peer. Applied by the controller to everything it sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyMap {
    /// By position, as a PC keyboard has its keys: Option is the Windows key, Command is Alt.
    #[default]
    Position,
    /// By Mac habit: ⌘C copies, ⌘Tab switches apps, ⌘Q quits, ⌥← moves by word. Whole shortcuts are
    /// translated, not single keys (`mac_shortcuts`), so [`KeyMap::apply`] leaves this mode's keys alone.
    Mac,
    /// Unchanged, for a peer that is also a Mac.
    None,
}

impl KeyMap {
    /// The HID usage to send for a key this Mac pressed, for the key-by-key modes. Each is a permutation
    /// of the modifiers, so two held keys never collapse into one. `Mac` passes through here: the
    /// controller translates its shortcuts whole.
    pub fn apply(self, hid: u16) -> u16 {
        const LALT: u16 = 0xE2;
        const LGUI: u16 = 0xE3;
        const RALT: u16 = 0xE6;
        const RGUI: u16 = 0xE7;
        match (self, hid) {
            (KeyMap::Position, LALT) => LGUI,
            (KeyMap::Position, LGUI) => LALT,
            (KeyMap::Position, RALT) => RGUI,
            (KeyMap::Position, RGUI) => RALT,
            _ => hid,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Switch {
    /// How long the cursor must stay on the boundary before the switch fires.
    pub dwell_ms: u64,
    /// Chebyshev distance (px) the cursor must travel away from the boundary
    /// before the dwell timer re-arms.
    pub arm_dist: i32,
    /// Manual switch chord, e.g. `ctrl+alt+cmd+s`. Parsing is the backend's job.
    pub chord: String,
    /// Width (px) of the boundary strip inside the display edge.
    pub boundary_px: i32,
}

/// Widest boundary strip. The receiver lands the cursor `ENTRY_INSET` px inside its edge, so the strip
/// must stay well short of that or the cursor starts inside the return strip and bounces straight back.
pub const BOUNDARY_PX_MAX: i32 = crate::geometry::ENTRY_INSET - 8;

impl Default for Switch {
    fn default() -> Self {
        Self { dwell_ms: 200, arm_dist: 4, chord: "ctrl+alt+cmd+s".into(), boundary_px: 2 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Local listener address, `host:port`.
    pub listen: String,
    pub switch: Switch,
    pub peers: Vec<Peer>,
}

impl Default for Config {
    fn default() -> Self {
        Self { listen: "0.0.0.0:24800".into(), switch: Switch::default(), peers: Vec::new() }
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Parse(String),
    Invalid(Vec<String>),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "config io: {e}"),
            ConfigError::Parse(m) => write!(f, "config parse: {m}"),
            ConfigError::Invalid(list) => write!(f, "config invalid: {}", list.join("; ")),
        }
    }
}

impl Config {
    /// Structure only: a peer may still be unpaired (empty fingerprint). A non-empty fingerprint must be well-formed.
    /// Every problem, not just the first, so the GUI can list them together.
    pub fn validate_draft(&self) -> Result<(), ConfigError> {
        let mut errs = Vec::new();
        if !(50..=5000).contains(&self.switch.dwell_ms) {
            errs.push(format!("dwell_ms {} 超出 50..=5000", self.switch.dwell_ms));
        }
        if !(1..=64).contains(&self.switch.arm_dist) {
            errs.push(format!("arm_dist {} 超出 1..=64", self.switch.arm_dist));
        }
        if !(1..=BOUNDARY_PX_MAX).contains(&self.switch.boundary_px) {
            errs.push(format!("boundary_px {} 超出 1..={BOUNDARY_PX_MAX}", self.switch.boundary_px));
        }
        if self.switch.chord.trim().is_empty() {
            errs.push("chord 不可為空".into());
        }
        if !is_socket_addr(&self.listen) {
            errs.push(format!("listen {:?} 不是 host:port", self.listen));
        }
        let mut seen_names = Vec::new();
        for p in &self.peers {
            if p.name.trim().is_empty() {
                errs.push("peer name 不可為空".into());
            }
            if seen_names.contains(&p.name) {
                errs.push(format!("peer name {:?} 重複", p.name));
            }
            seen_names.push(p.name.clone());
            if !is_socket_addr(&p.addr) {
                errs.push(format!("peer {:?} addr {:?} 不是 host:port", p.name, p.addr));
            }
            if !p.fingerprint.is_empty() && !is_fingerprint(&p.fingerprint) {
                errs.push(format!("peer {:?} fingerprint 須為 64 個小寫 hex", p.name));
            }
        }
        // 一條邊只能通往一台 peer，否則切換目標不唯一。
        for (i, a) in self.peers.iter().enumerate() {
            if self.peers[i + 1..].iter().any(|b| b.edge == a.edge) {
                errs.push(format!("邊 {:?} 有兩台 peer", a.edge));
            }
        }
        if errs.is_empty() { Ok(()) } else { Err(ConfigError::Invalid(errs)) }
    }

    /// Names of peers that have no valid fingerprint yet.
    pub fn unpaired(&self) -> Vec<&str> {
        self.peers.iter().filter(|p| !is_fingerprint(&p.fingerprint)).map(|p| p.name.as_str()).collect()
    }

    /// Draft rules plus: every peer is paired. Required before the worker may start.
    pub fn validate_for_run(&self) -> Result<(), ConfigError> {
        let mut errs = match self.validate_draft() {
            Ok(()) => Vec::new(),
            Err(ConfigError::Invalid(l)) => l,
            Err(e) => return Err(e),
        };
        for name in self.unpaired() {
            let msg = format!("peer {name:?} 尚未配對（fingerprint 缺或無效）");
            if !errs.iter().any(|e| e.contains(&format!("peer {name:?} fingerprint"))) {
                errs.push(msg);
            }
        }
        if errs.is_empty() { Ok(()) } else { Err(ConfigError::Invalid(errs)) }
    }

    /// Missing file → defaults. Any other failure is an error, never a silent default.
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        match fs::read_to_string(path) {
            Ok(text) => {
                let cfg: Config = toml::from_str(&text).map_err(|e| ConfigError::Parse(e.to_string()))?;
                Ok(cfg)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(ConfigError::Io(e)),
        }
    }

    /// Validate, then write atomically (tmp file + rename). An invalid config is never written.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        self.validate_draft()?;
        let text = toml::to_string_pretty(self).map_err(|e| ConfigError::Parse(e.to_string()))?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(ConfigError::Io)?;
        }
        let tmp = path.with_extension("toml.tmp");
        {
            let mut f = fs::File::create(&tmp).map_err(ConfigError::Io)?;
            f.write_all(text.as_bytes()).map_err(ConfigError::Io)?;
            f.sync_all().map_err(ConfigError::Io)?;
        }
        fs::rename(&tmp, path).map_err(ConfigError::Io)
    }
}

fn is_socket_addr(s: &str) -> bool {
    s.parse::<std::net::SocketAddr>().is_ok()
}

fn is_fingerprint(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(name: &str, edge: Edge) -> Peer {
        Peer { name: name.into(), addr: "192.168.1.20:24800".into(), edge, fingerprint: "ab".repeat(32), keys: KeyMap::default() }
    }

    /// A config written before the setting existed still loads, with the default mapping.
    #[test]
    fn a_peer_without_a_key_mapping_loads_as_position() {
        let text = "listen = \"0.0.0.0:24800\"\n[[peers]]\nname = \"win\"\naddr = \"10.0.0.2:24800\"\nedge = \"right\"\nfingerprint = \"" .to_string()
            + &"ab".repeat(32) + "\"\n";
        let c: Config = toml::from_str(&text).unwrap();
        assert_eq!(c.peers[0].keys, KeyMap::Position);
        let back: Config = toml::from_str(&toml::to_string_pretty(&Config { peers: vec![Peer { keys: KeyMap::Mac, ..c.peers[0].clone() }], ..c }).unwrap()).unwrap();
        assert_eq!(back.peers[0].keys, KeyMap::Mac, "the choice survives a save");
    }

    /// By position (the default): the key where a PC keyboard has its Windows key (Option) is the
    /// Windows key, and Command, where a PC has Alt, is Alt. Both sides, left and right.
    #[test]
    fn position_mapping_follows_the_pc_keycaps() {
        assert_eq!(KeyMap::Position.apply(0xE2), 0xE3, "left Option is the Windows key");
        assert_eq!(KeyMap::Position.apply(0xE3), 0xE2, "left Command is Alt");
        assert_eq!(KeyMap::Position.apply(0xE6), 0xE7);
        assert_eq!(KeyMap::Position.apply(0xE7), 0xE6);
        assert_eq!(KeyMap::Position.apply(0xE0), 0xE0, "Control stays Control");
        assert_eq!(KeyMap::Position.apply(0x06), 0x06, "letters are untouched");
    }

    /// `Mac` is translated whole by the controller; key by key it changes nothing.
    #[test]
    fn mac_and_none_leave_keys_alone_key_by_key() {
        for hid in [0xE0, 0xE2, 0xE3, 0xE7, 0x06] {
            assert_eq!(KeyMap::Mac.apply(hid), hid);
            assert_eq!(KeyMap::None.apply(hid), hid);
        }
    }

    /// Each mode only permutes keys: two held keys can never arrive as one.
    #[test]
    fn every_mode_is_a_permutation() {
        for m in [KeyMap::Position, KeyMap::Mac, KeyMap::None] {
            let mut seen = std::collections::HashSet::new();
            for hid in 0u16..256 {
                assert!(seen.insert(m.apply(hid)), "{m:?} sends {:#04x} for two keys", m.apply(hid));
            }
        }
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("kmcfg-{}-{name}", std::process::id())).join("config.toml")
    }

    #[test]
    fn missing_file_is_defaults_not_error() {
        let p = tmp("missing");
        let _ = fs::remove_dir_all(p.parent().unwrap());
        assert_eq!(Config::load(&p).unwrap(), Config::default());
    }

    #[test]
    fn round_trip_preserves_boundaries_and_switch() {
        let p = tmp("roundtrip");
        let cfg = Config {
            listen: "0.0.0.0:24800".into(),
            switch: Switch { dwell_ms: 350, arm_dist: 6, chord: "ctrl+alt+cmd+s".into(), boundary_px: 3 },
            peers: vec![peer("mini", Edge::Right), peer("air", Edge::Left)],
        };
        cfg.save(&p).unwrap();
        assert_eq!(Config::load(&p).unwrap(), cfg);
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn validate_reports_every_problem_at_once() {
        let mut cfg = Config::default();
        cfg.switch.dwell_ms = 1;
        cfg.peers = vec![
            Peer { name: "a".into(), addr: "nope".into(), edge: Edge::Left, fingerprint: "short".into(), keys: KeyMap::default() },
            peer("b", Edge::Left),
        ];
        let msg = match cfg.validate_draft() {
            Err(ConfigError::Invalid(list)) => list.join("|"),
            other => panic!("應為 Invalid，實得 {other:?}"),
        };
        assert!(msg.contains("dwell_ms"), "{msg}");
        assert!(msg.contains("addr"), "{msg}");
        assert!(msg.contains("fingerprint"), "{msg}");
        assert!(msg.contains("兩台"), "{msg}");
    }

    #[test]
    fn boundary_strip_must_stay_narrower_than_the_entry_inset() {
        let mut cfg = Config::default();
        cfg.switch.boundary_px = crate::geometry::ENTRY_INSET + 1;
        assert!(cfg.validate_draft().is_err(), "a strip wider than the entry inset bounces the cursor straight back");
        cfg.switch.boundary_px = BOUNDARY_PX_MAX;
        assert!(cfg.validate_draft().is_ok());
        cfg.switch.boundary_px = BOUNDARY_PX_MAX + 1;
        assert!(cfg.validate_draft().is_err());
    }

    #[test]
    fn draft_allows_unpaired_but_run_rejects_and_names_them() {
        let mut cfg = Config::default();
        let mut a = peer("a", Edge::Left);
        a.fingerprint = String::new();
        cfg.peers = vec![a, peer("b", Edge::Right)];
        assert!(cfg.validate_draft().is_ok());
        let msg = match cfg.validate_for_run() {
            Err(ConfigError::Invalid(l)) => l.join("|"),
            other => panic!("{other:?}"),
        };
        assert!(msg.contains("\"a\"") && msg.contains("尚未配對"), "{msg}");
        assert!(!msg.contains("\"b\""), "{msg}");
        let p = tmp("draft");
        cfg.save(&p).unwrap();
        assert_eq!(Config::load(&p).unwrap(), cfg);
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn draft_still_rejects_malformed_nonempty_fingerprint() {
        let mut cfg = Config::default();
        let mut a = peer("a", Edge::Left);
        a.fingerprint = "short".into();
        cfg.peers = vec![a];
        assert!(cfg.validate_draft().is_err());
        assert!(cfg.validate_for_run().is_err());
    }

    #[test]
    fn invalid_config_is_never_written() {
        let p = tmp("invalid");
        let mut cfg = Config::default();
        cfg.switch.arm_dist = 0;
        assert!(cfg.save(&p).is_err());
        assert!(!p.exists());
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn malformed_toml_is_parse_error_not_default() {
        let p = tmp("malformed");
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, "dwell_ms = [").unwrap();
        assert!(matches!(Config::load(&p), Err(ConfigError::Parse(_))));
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn config_edge_maps_to_proto_edge() {
        use crate::proto;
        assert_eq!(proto::Edge::from(Edge::Left), proto::Edge::Left);
        assert_eq!(proto::Edge::from(Edge::Right), proto::Edge::Right);
        assert_eq!(proto::Edge::from(Edge::Top), proto::Edge::Top);
        assert_eq!(proto::Edge::from(Edge::Bottom), proto::Edge::Bottom);
    }
}
