//! User configuration: the one schema shared by the GUI, the worker and pairing.
//! Path: `~/.config/flowseam/config.toml`. A missing file means defaults, so a
//! fresh install starts with no peers and never switches.

use std::fs;
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::gesture::Habit;

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
    /// What this Mac's trackpad gestures do on a PC peer.
    #[serde(default, skip_serializing_if = "GestureMap::is_linked")]
    pub gestures: GestureMap,
    /// Monitors this machine can switch to the peer's input along with a switch from the menu.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub displays: Vec<DisplaySwitch>,
}

/// MCCS input-source values (VCP 0x60) for one monitor. `uuid` is the CoreGraphics display UUID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplaySwitch {
    pub uuid: String,
    /// Input that shows the peer.
    pub input: u16,
    /// Input that shows this machine.
    pub home: u16,
    /// A person saw `input` show the peer in the window's test. Hand-written configs start unconfirmed.
    #[serde(default)]
    pub verified: bool,
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

/// Which habit a Mac's trackpad gestures follow on a PC. They differ in one motion: swiping down shows the
/// windows, as App Exposé does, or the desktop, as a Windows touchpad does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GestureMap {
    /// The habit the key mapping keeps: Mac shortcuts keep the Mac's, keys by position the PC's.
    #[default]
    Linked,
    Mac,
    Windows,
}

impl GestureMap {
    pub fn is_linked(&self) -> bool {
        *self == GestureMap::Linked
    }

    pub fn habit(self, keys: KeyMap) -> Habit {
        match (self, keys) {
            (GestureMap::Mac, _) => Habit::Mac,
            (GestureMap::Windows, _) => Habit::Windows,
            (GestureMap::Linked, KeyMap::Position) => Habit::Windows,
            (GestureMap::Linked, KeyMap::Mac | KeyMap::None) => Habit::Mac,
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

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ConfigDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    /// Same name, different addr or fingerprint: the link must be re-established.
    pub relinked: Vec<String>,
    /// Same name, same addr and fingerprint, different edge or key map: only the forwarder changes.
    pub respecced: Vec<String>,
    pub switch_changed: bool,
    pub listen_changed: bool,
}

impl ConfigDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.relinked.is_empty() && self.respecced.is_empty() && !self.switch_changed && !self.listen_changed
    }
}

pub fn diff(old: &Config, new: &Config) -> ConfigDiff {
    let old_peers: std::collections::BTreeMap<_, _> = old.peers.iter().map(|p| (p.name.as_str(), p)).collect();
    let new_peers: std::collections::BTreeMap<_, _> = new.peers.iter().map(|p| (p.name.as_str(), p)).collect();
    let mut result = ConfigDiff { switch_changed: old.switch != new.switch, listen_changed: old.listen != new.listen, ..ConfigDiff::default() };
    for (name, peer) in &new_peers {
        match old_peers.get(name) {
            None => result.added.push((*name).into()),
            Some(previous) if previous.addr != peer.addr || previous.fingerprint != peer.fingerprint => result.relinked.push((*name).into()),
            Some(previous) if previous.edge != peer.edge || previous.keys != peer.keys => result.respecced.push((*name).into()),
            _ => {}
        }
    }
    for name in old_peers.keys() {
        if !new_peers.contains_key(name) {
            result.removed.push((*name).into());
        }
    }
    result
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
            for (i, d) in p.displays.iter().enumerate() {
                // 0 is no MCCS input; the same input both ways would switch nothing.
                if d.input == 0 || d.home == 0 || d.input == d.home {
                    errs.push(format!("peer {:?} 的螢幕 {} 輸入設定不完整（對方 {}、這台 {}）", p.name, d.uuid, d.input, d.home));
                }
                if p.displays[..i].iter().any(|e| e.uuid == d.uuid) {
                    errs.push(format!("peer {:?} 的螢幕 {} 重複", p.name, d.uuid));
                }
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
    ///
    /// A file an older Flowseam rewrote has lost what that build did not know: its monitors and gesture
    /// mapping. Those come back from the copy `save` keeps beside it (`extras_path`).
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        match fs::read_to_string(path) {
            Ok(text) => {
                let mut cfg: Config = toml::from_str(&text).map_err(|e| ConfigError::Parse(e.to_string()))?;
                let marked = toml::from_str::<Marker>(&text).is_ok_and(|m| m.format >= FORMAT);
                if !marked {
                    restore_extras(&mut cfg, &extras_path(path));
                }
                Ok(cfg)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(ConfigError::Io(e)),
        }
    }

    /// Validate, then write atomically (tmp file + rename). An invalid config is never written.
    ///
    /// The file carries `format`, which an older build drops when it rewrites it; the monitors and gesture
    /// mappings also go to `extras_path` first, for `load` to put back after such a rewrite.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        self.validate_draft()?;
        let body = toml::to_string_pretty(self).map_err(|e| ConfigError::Parse(e.to_string()))?;
        // A top-level key, so it goes before the first table.
        let text = format!("format = {FORMAT}\n{body}");
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(ConfigError::Io)?;
        }
        save_extras(self, &extras_path(path))?;
        write_atomic(path, &text)
    }
}

/// Written into every config.toml this build saves. A build from before it does not know the key and
/// drops it when it rewrites the file: that is how `load` tells such a rewrite from the person's own edit.
const FORMAT: u32 = 1;

#[derive(Deserialize)]
struct Marker {
    #[serde(default)]
    format: u32,
}

/// What an older build drops from config.toml, per peer.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Extras {
    #[serde(default)]
    peers: Vec<PeerExtras>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PeerExtras {
    name: String,
    #[serde(default, skip_serializing_if = "GestureMap::is_linked")]
    gestures: GestureMap,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    displays: Vec<DisplaySwitch>,
}

/// The copy of the monitors and gesture mappings kept beside `config`.
pub fn extras_path(config: &Path) -> std::path::PathBuf {
    config.with_file_name("peer-extras.toml")
}

fn save_extras(cfg: &Config, path: &Path) -> Result<(), ConfigError> {
    let peers: Vec<PeerExtras> = cfg
        .peers
        .iter()
        .filter(|p| !p.displays.is_empty() || !p.gestures.is_linked())
        .map(|p| PeerExtras { name: p.name.clone(), gestures: p.gestures, displays: p.displays.clone() })
        .collect();
    if peers.is_empty() {
        // Nothing to keep. A stale copy would bring back monitors removed since.
        return match fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(ConfigError::Io(e)),
            _ => Ok(()),
        };
    }
    let body = toml::to_string_pretty(&Extras { peers }).map_err(|e| ConfigError::Parse(e.to_string()))?;
    write_atomic(path, &format!("# Kept by Flowseam: what an older version drops from config.toml, restored from here.\n{body}"))
}

/// Puts back, into peers that have lost them, the monitors and gesture mapping kept for them by name. A peer
/// that still has its own keeps them: only a rewrite by an older build gets here, and it empties them.
fn restore_extras(cfg: &mut Config, path: &Path) {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            crate::log::line(&format!("flowseam: {} unreadable, monitors not restored: {e}", path.display()));
            return;
        }
    };
    let extras: Extras = match toml::from_str(&text) {
        Ok(x) => x,
        Err(e) => {
            // Moved aside rather than left for the next save to replace: it may be the only copy.
            let aside = path.with_extension("toml.unreadable");
            let moved = fs::rename(path, &aside).map_or_else(|e| format!("not moved: {e}"), |_| format!("moved to {}", aside.display()));
            crate::log::line(&format!("flowseam: {} unreadable, monitors not restored ({moved}): {e}", path.display()));
            return;
        }
    };
    for p in &mut cfg.peers {
        let Some(kept) = extras.peers.iter().find(|k| k.name == p.name) else { continue };
        if p.displays.is_empty() && !kept.displays.is_empty() {
            p.displays = kept.displays.clone();
            crate::log::line(&format!("flowseam: restored {} monitor(s) of {} that an older version dropped", p.displays.len(), p.name));
        }
        if p.gestures.is_linked() && !kept.gestures.is_linked() {
            p.gestures = kept.gestures;
        }
    }
}

fn write_atomic(path: &Path, text: &str) -> Result<(), ConfigError> {
    let tmp = path.with_extension("toml.tmp");
    {
        let mut f = fs::File::create(&tmp).map_err(ConfigError::Io)?;
        f.write_all(text.as_bytes()).map_err(ConfigError::Io)?;
        f.sync_all().map_err(ConfigError::Io)?;
    }
    fs::rename(&tmp, path).map_err(ConfigError::Io)
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
        Peer { name: name.into(), addr: "192.168.1.20:24800".into(), edge, fingerprint: "ab".repeat(32), keys: KeyMap::default(), gestures: Default::default(), displays: Vec::new() }
    }

    #[test]
    fn identical_configs_have_no_diff() {
        let cfg = Config { peers: vec![peer("b", Edge::Right)], ..Config::default() };
        assert_eq!(diff(&cfg, &cfg), ConfigDiff::default());
        assert!(diff(&cfg, &cfg).is_empty());
    }

    #[test]
    fn diff_classifies_and_sorts_each_kind_of_change() {
        let old = Config { peers: vec![peer("z-remove", Edge::Left), peer("a-remove", Edge::Left), peer("z-link", Edge::Left), peer("a-link", Edge::Left), peer("z-spec", Edge::Left), peer("a-spec", Edge::Left)], ..Config::default() };
        let mut new = old.clone();
        new.peers.retain(|p| !p.name.ends_with("remove"));
        new.peers.push(peer("z-add", Edge::Left));
        new.peers.push(peer("a-add", Edge::Left));
        new.peers.iter_mut().find(|p| p.name == "z-link").unwrap().addr = "192.168.1.21:24800".into();
        let both = new.peers.iter_mut().find(|p| p.name == "a-link").unwrap();
        both.fingerprint = "cd".repeat(32);
        both.keys = KeyMap::Mac;
        new.peers.iter_mut().find(|p| p.name == "z-spec").unwrap().edge = Edge::Right;
        new.peers.iter_mut().find(|p| p.name == "a-spec").unwrap().keys = KeyMap::None;
        new.switch.dwell_ms += 1;
        new.listen = "0.0.0.0:24801".into();
        let got = diff(&old, &new);
        assert_eq!(got, ConfigDiff { added: vec!["a-add".into(), "z-add".into()], removed: vec!["a-remove".into(), "z-remove".into()], relinked: vec!["a-link".into(), "z-link".into()], respecced: vec!["a-spec".into(), "z-spec".into()], switch_changed: true, listen_changed: true });
        assert!(!got.is_empty());
    }

    #[test]
    fn renamed_peer_is_removed_and_added_even_with_the_same_link() {
        let old = Config { peers: vec![peer("old", Edge::Right)], ..Config::default() };
        let mut new = old.clone();
        new.peers[0].name = "new".into();
        assert_eq!(diff(&old, &new), ConfigDiff { added: vec!["new".into()], removed: vec!["old".into()], ..ConfigDiff::default() });
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

    #[test]
    fn an_incomplete_or_repeated_monitor_does_not_save() {
        let mut cfg = Config::default();
        let d = |uuid: &str, input, home| DisplaySwitch { uuid: uuid.into(), input, home, verified: false };
        cfg.peers = vec![Peer { displays: vec![d("one", 15, 0)], ..peer("b", Edge::Left) }];
        assert!(cfg.validate_draft().is_err(), "home 0: this machine's input was never read");
        cfg.peers[0].displays = vec![d("one", 15, 15)];
        assert!(cfg.validate_draft().is_err(), "the same input both ways");
        cfg.peers[0].displays = vec![d("one", 15, 18), d("one", 17, 18)];
        assert!(cfg.validate_draft().is_err(), "one monitor twice for one peer");
        cfg.peers[0].displays = vec![d("one", 15, 18), d("two", 17, 19)];
        assert!(cfg.validate_draft().is_ok());
    }

    #[test]
    fn gestures_load_linked_and_only_a_choice_is_written() {
        let text = "[[peers]]\nname = \"pc\"\naddr = \"192.168.1.20:24800\"\nedge = \"right\"\nfingerprint = \"".to_string() + &"ab".repeat(32) + "\"\n";
        let cfg: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.peers[0].gestures, GestureMap::Linked, "a config from before the choice existed");
        assert!(!toml::to_string_pretty(&cfg).unwrap().contains("gestures"), "linked stays out of the file");
        for g in [GestureMap::Mac, GestureMap::Windows] {
            let chosen = Config { peers: vec![Peer { gestures: g, ..cfg.peers[0].clone() }], ..cfg.clone() };
            let back: Config = toml::from_str(&toml::to_string_pretty(&chosen).unwrap()).unwrap();
            assert_eq!(back.peers[0].gestures, g);
        }
    }

    #[test]
    fn linked_gestures_keep_the_habit_of_the_key_mapping() {
        use GestureMap::*;
        assert_eq!(Linked.habit(KeyMap::Position), Habit::Windows, "keys by position: a PC keyboard habit");
        assert_eq!(Linked.habit(KeyMap::Mac), Habit::Mac);
        assert_eq!(Linked.habit(KeyMap::None), Habit::Mac);
        for keys in [KeyMap::Position, KeyMap::Mac, KeyMap::None] {
            assert_eq!(Mac.habit(keys), Habit::Mac, "a choice overrides {keys:?}");
            assert_eq!(Windows.habit(keys), Habit::Windows, "a choice overrides {keys:?}");
        }
    }

    #[test]
    fn a_peer_without_displays_loads_with_none() {
        let text = "[[peers]]\nname = \"mini\"\naddr = \"192.168.1.20:24800\"\nedge = \"right\"\nfingerprint = \"".to_string() + &"ab".repeat(32) + "\"\n";
        let cfg: Config = toml::from_str(&text).unwrap();
        assert!(cfg.peers[0].displays.is_empty());
    }

    #[test]
    fn a_peers_displays_load_and_survive_save() {
        let text = "[[peers]]\nname = \"mini\"\naddr = \"192.168.1.20:24800\"\nedge = \"right\"\nfingerprint = \"".to_string()
            + &"ab".repeat(32)
            + "\"\n[[peers.displays]]\nuuid = \"one\"\ninput = 15\nhome = 17\n[[peers.displays]]\nuuid = \"two\"\ninput = 17\nhome = 19\nverified = true\n";
        let cfg: Config = toml::from_str(&text).unwrap();
        assert_eq!(
            cfg.peers[0].displays,
            [DisplaySwitch { uuid: "one".into(), input: 15, home: 17, verified: false }, DisplaySwitch { uuid: "two".into(), input: 17, home: 19, verified: true }],
            "unconfirmed unless the file says so"
        );
        let p = tmp("display-roundtrip");
        cfg.save(&p).unwrap();
        assert_eq!(Config::load(&p).unwrap(), cfg);
        let _ = fs::remove_dir_all(p.parent().unwrap());
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

    fn with_monitors() -> Config {
        Config {
            peers: vec![
                Peer { displays: vec![DisplaySwitch { uuid: "one".into(), input: 15, home: 17, verified: true }], gestures: GestureMap::Mac, ..peer("pc", Edge::Right) },
                peer("mac", Edge::Left),
            ],
            ..Default::default()
        }
    }

    /// What a build from before monitors and gesture mappings writes back: the same peers without them,
    /// and no `format`.
    fn old_build_rewrite(cfg: &Config, path: &Path) {
        let mut old = cfg.clone();
        for p in &mut old.peers {
            p.displays.clear();
            p.gestures = GestureMap::Linked;
        }
        fs::write(path, toml::to_string_pretty(&old).unwrap()).unwrap();
    }

    #[test]
    fn monitors_an_older_build_drops_come_back() {
        let p = tmp("extras-restore");
        let cfg = with_monitors();
        cfg.save(&p).unwrap();
        assert!(fs::read_to_string(&p).unwrap().starts_with("format = 1\n"));
        assert!(fs::read_to_string(extras_path(&p)).unwrap().contains("one"), "the copy beside it");
        old_build_rewrite(&cfg, &p);
        assert_eq!(Config::load(&p).unwrap(), cfg, "monitors and gesture mapping restored");
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn monitors_removed_here_stay_removed() {
        let p = tmp("extras-removed");
        let mut cfg = with_monitors();
        cfg.save(&p).unwrap();
        cfg.peers[0].displays.clear();
        cfg.peers[0].gestures = GestureMap::Linked;
        cfg.save(&p).unwrap();
        assert!(!extras_path(&p).exists(), "nothing left to keep");
        assert_eq!(Config::load(&p).unwrap(), cfg);

        // A copy left over, but the file is this build's own: what it says stands.
        fs::write(extras_path(&p), "[[peers]]\nname = \"pc\"\n[[peers.displays]]\nuuid = \"stale\"\ninput = 1\nhome = 2\n").unwrap();
        assert_eq!(Config::load(&p).unwrap(), cfg, "marked: the copy is not consulted");
        let _ = fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn a_config_from_before_the_copy_keeps_its_monitors() {
        let p = tmp("extras-legacy");
        let cfg = with_monitors();
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, toml::to_string_pretty(&cfg).unwrap()).unwrap();
        assert_eq!(Config::load(&p).unwrap(), cfg, "no format and no copy: the file as it is");

        // A peer that still has monitors of its own keeps them, even unmarked.
        fs::write(extras_path(&p), "[[peers]]\nname = \"pc\"\n[[peers.displays]]\nuuid = \"other\"\ninput = 1\nhome = 2\n").unwrap();
        assert_eq!(Config::load(&p).unwrap().peers[0].displays, cfg.peers[0].displays);
        fs::write(extras_path(&p), "not toml [").unwrap();
        assert_eq!(Config::load(&p).unwrap(), cfg, "an unreadable copy restores nothing and fails nothing");
        let aside = extras_path(&p).with_extension("toml.unreadable");
        assert_eq!(fs::read_to_string(&aside).unwrap(), "not toml [", "and is kept aside, not left for a save to replace");
        let _ = fs::remove_dir_all(p.parent().unwrap());
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
            Peer { name: "a".into(), addr: "nope".into(), edge: Edge::Left, fingerprint: "short".into(), keys: KeyMap::default(), gestures: Default::default(), displays: Vec::new() },
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
