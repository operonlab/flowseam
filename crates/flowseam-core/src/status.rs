use crate::proto::Kind;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// `driven_by`: the peer driving this machine's keyboard and mouse right now, for the menu's way back.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct LinkStatus {
    pub written_ms: u64,
    pub peers: Vec<PeerStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driven_by: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PeerStatus { pub name: String, pub up: bool, pub kind: Option<Kind>, pub remote: Option<String>, pub since_ms: u64, pub reason: Option<String> }

/// The monitor switched along with a menu-started session, written by the controller's worker.
/// One writer per file: the links own `status.toml`, the worker's main loop owns this one.
/// `uuid` and `home` name the monitor while it is out, so the window can send it home after the worker
/// is gone without trusting a config that may have been edited since.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct KvmStatus {
    pub written_ms: u64,
    pub phase: KvmPhase,
    pub peer: Option<String>,
    pub error: Option<String>,
    /// The monitors out right now and the input each goes home to, so the window can send them home after
    /// the worker is gone without trusting a config that may have been edited since.
    #[serde(default)]
    pub out: Vec<OutMonitor>,
    /// Every monitor in the config the worker runs with, by peer. The window gates its buttons on these,
    /// not on its own unsaved form, so it offers what the worker accepts.
    #[serde(default)]
    pub monitors: Vec<MonitorState>,
    #[serde(default)]
    pub trial: Option<TrialStatus>,
    /// How long a settings test shows the candidate input, for the window to say before it starts.
    #[serde(default)]
    pub trial_hold_ms: u64,
    #[serde(default)]
    pub inspect: Option<InspectStatus>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct OutMonitor { pub uuid: String, pub home: u16 }

/// One configured monitor of a peer, as the worker running that config sees it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct MonitorState {
    pub peer: String,
    pub uuid: String,
    /// A settings test confirmed its input shows the peer.
    pub verified: bool,
    /// The last probe; `None` until the first one finishes.
    pub check: Option<MonitorCheck>,
    /// Whether the peer reported being attached to it. Information only: a monitor may cut power to an
    /// input it is not showing, and then the peer cannot see it until it switches.
    pub peer_sees: Option<bool>,
}

impl MonitorState {
    pub fn blocked(&self) -> Option<Blocked> {
        switch_blocked(self.check.as_ref())
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum KvmPhase { #[default] Idle, Switching, Away, Returning, Testing }

/// A finished test from the settings. `ok`: every monitor confirmed its candidate input and came back
/// home (they switch together or not at all); whether each input showed the right computer is the person's
/// answer. A status from a build that tested one monitor at a time reads with no monitors.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct TrialStatus {
    pub peer: String,
    #[serde(default)]
    pub monitors: Vec<TrialMonitor>,
    pub ok: bool,
    pub reason: Option<String>,
    pub id: u64,
}

/// One monitor of a test: the candidate input it showed and this Mac's, which it came back to.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct TrialMonitor { pub uuid: String, pub input: u16, pub home: u16 }

/// What the worker read from one monitor for the settings: its inputs and the one showing now.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct InspectStatus { pub uuid: String, pub inputs: Option<Vec<u16>>, pub current: Option<u16>, pub reason: Option<String> }

/// What the controller's probe found about a peer's monitor.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CheckState {
    /// Connected here, answers DDC, and lists both inputs.
    Ok,
    /// Connected and answers DDC, but does not list its inputs: only a test shows the input is there.
    Unlisted,
    /// Rules the switch out: not connected here, or an input the monitor does not have.
    Problem,
    /// The probe could not finish (no DDC answer): nothing known either way.
    Unknown,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct MonitorCheck { pub peer: String, pub state: CheckState, pub reason: Option<String> }

/// Why a configured monitor switch may not be offered, or `None` when it may: a probe that found the
/// monitor usable. A monitor nobody tested switches too, on the person's word that the input is the
/// peer's; the menu marks it unconfirmed. The worker and the window both decide with this, so the menu
/// and the refusal agree.
pub fn switch_blocked(check: Option<&MonitorCheck>) -> Option<Blocked> {
    match check.map(|c| (c.state, c.reason.clone())) {
        None => Some(Blocked::NotChecked),
        Some((CheckState::Problem, r)) => Some(Blocked::Problem(r.unwrap_or_default())),
        Some((CheckState::Unknown, r)) => Some(Blocked::Unknown(r.unwrap_or_default())),
        Some((CheckState::Ok | CheckState::Unlisted, _)) => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocked { NotChecked, Problem(String), Unknown(String) }

/// Whether a peer says it is attached to `mine`, the monitor this machine would switch: `Some(true)` it is,
/// `Some(false)` it named only other monitors, `None` it said nothing (protocol below 5, or an empty list).
/// A serial of 0 on either side is a monitor that does not report one, so it matches any.
pub fn peer_on_monitor(mine: Option<crate::proto::MonitorId>, theirs: Option<&[crate::proto::MonitorId]>) -> Option<bool> {
    let (mine, theirs) = (mine?, theirs.filter(|t| !t.is_empty())?);
    Some(theirs.iter().any(|t| t.vendor == mine.vendor && t.product == mine.product && (mine.serial == 0 || t.serial == 0 || t.serial == mine.serial)))
}


pub fn status_path(config: &Path) -> PathBuf { config.with_file_name("status.toml") }

pub fn kvm_path(config: &Path) -> PathBuf { config.with_file_name("kvm.toml") }

pub fn write(path: &Path, s: &LinkStatus) -> io::Result<()> {
    write_toml(path, s)
}

pub fn write_kvm(path: &Path, s: &KvmStatus) -> io::Result<()> {
    write_toml(path, s)
}

pub fn read_kvm(path: &Path) -> io::Result<KvmStatus> {
    toml::from_str(&fs::read_to_string(path)?).map_err(io::Error::other)
}

fn write_toml<T: Serialize>(path: &Path, s: &T) -> io::Result<()> {
    let text = toml::to_string(s).map_err(io::Error::other)?;
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
    }
    fs::rename(tmp, path)
}

pub fn read(path: &Path) -> io::Result<LinkStatus> {
    toml::from_str(&fs::read_to_string(path)?).map_err(io::Error::other)
}

pub fn now_ms() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64 }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_missing_file() {
        let path = std::env::temp_dir().join(format!("flowseam-status-{}-{}.toml", std::process::id(), now_ms()));
        assert_eq!(read(&path).unwrap_err().kind(), io::ErrorKind::NotFound);
        let status = LinkStatus { written_ms: 42, peers: vec![PeerStatus { name: "Mac".into(), up: true, kind: Some(Kind::WiFi), remote: Some("192.168.1.2:24800".into()), since_ms: 40, reason: None }], driven_by: None };
        write(&path, &status).unwrap();
        assert_eq!(read(&path).unwrap(), status);
        let driven = LinkStatus { driven_by: Some("mini".into()), ..status };
        write(&path, &driven).unwrap();
        assert_eq!(read(&path).unwrap(), driven);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_monitor_switch_needs_a_usable_probe_but_no_test() {
        let c = |state, reason: Option<&str>| MonitorCheck { peer: "pc".into(), state, reason: reason.map(str::to_string) };
        assert_eq!(switch_blocked(None), Some(Blocked::NotChecked));
        assert_eq!(switch_blocked(Some(&c(CheckState::Problem, Some("not connected")))), Some(Blocked::Problem("not connected".into())));
        assert_eq!(switch_blocked(Some(&c(CheckState::Unknown, Some("no answer")))), Some(Blocked::Unknown("no answer".into())));
        assert_eq!(switch_blocked(Some(&c(CheckState::Ok, None))), None);
        assert_eq!(switch_blocked(Some(&c(CheckState::Unlisted, None))), None);
    }

    #[test]
    fn a_peer_reports_whether_it_is_attached_to_the_monitor() {
        use crate::proto::MonitorId;
        let mon = MonitorId { vendor: 0x09D1, product: 0x7955, serial: 0x1234 };
        let other = MonitorId { vendor: 0x0693, product: 0xA714, serial: 1 };
        assert_eq!(peer_on_monitor(Some(mon), Some(&[other, mon])), Some(true));
        assert_eq!(peer_on_monitor(Some(mon), Some(&[other])), Some(false));
        assert_eq!(peer_on_monitor(Some(mon), Some(&[MonitorId { serial: 0, ..mon }])), Some(true), "no serial on one side");
        assert_eq!(peer_on_monitor(Some(mon), Some(&[MonitorId { serial: 9, ..mon }])), Some(false), "a twin with another serial");
        assert_eq!(peer_on_monitor(Some(mon), None), None, "an older peer says nothing");
        assert_eq!(peer_on_monitor(Some(mon), Some(&[])), None);
        assert_eq!(peer_on_monitor(None, Some(&[mon])), None);
    }

    /// The peer not seeing a monitor never blocks it: a monitor may power down the inputs it is not showing.
    #[test]
    fn what_the_peer_sees_does_not_block_a_switch() {
        let ok = MonitorCheck { peer: "pc".into(), state: CheckState::Ok, reason: None };
        let m = MonitorState { peer: "pc".into(), uuid: "U".into(), verified: true, check: Some(ok), peer_sees: Some(false) };
        assert_eq!(m.blocked(), None);
        assert_eq!(MonitorState { verified: false, ..m.clone() }.blocked(), None, "untested switches too");
        assert_eq!(MonitorState { check: None, ..m }.blocked(), Some(Blocked::NotChecked));
    }

    /// A status written by a build without `driven_by` still reads.
    #[test]
    fn an_older_status_without_driven_by_reads_as_none() {
        let s: LinkStatus = toml::from_str("written_ms = 5\npeers = []\n").unwrap();
        assert_eq!(s.driven_by, None);
    }

    #[test]
    fn kvm_status_round_trips_with_and_without_optionals() {
        let path = std::env::temp_dir().join(format!("flowseam-kvm-{}-{}.toml", std::process::id(), now_ms()));
        for s in [
            KvmStatus {
                written_ms: 7,
                phase: KvmPhase::Away,
                peer: Some("pc".into()),
                out: vec![OutMonitor { uuid: "U".into(), home: 18 }, OutMonitor { uuid: "V".into(), home: 19 }],
                monitors: vec![
                    MonitorState { peer: "pc".into(), uuid: "U".into(), verified: true, check: Some(MonitorCheck { peer: "pc".into(), state: CheckState::Ok, reason: None }), peer_sees: Some(true) },
                    MonitorState { peer: "pc".into(), uuid: "V".into(), verified: false, check: None, peer_sees: None },
                ],
                ..Default::default()
            },
            KvmStatus {
                written_ms: 9,
                phase: KvmPhase::Testing,
                trial_hold_ms: 8000,
                trial: Some(TrialStatus { peer: "pc".into(), monitors: vec![TrialMonitor { uuid: "U".into(), input: 15, home: 18 }, TrialMonitor { uuid: "V".into(), input: 17, home: 19 }], ok: true, reason: None, id: 3 }),
                inspect: Some(InspectStatus { uuid: "U".into(), inputs: Some(vec![15, 17, 18]), current: Some(18), reason: None }),
                ..Default::default()
            },
            KvmStatus { written_ms: 8, error: Some("monitor did not switch".into()), ..Default::default() },
        ] {
            write_kvm(&path, &s).unwrap();
            assert_eq!(read_kvm(&path).unwrap(), s);
        }
        assert!(fs::read_to_string(&path).unwrap().contains("phase = \"idle\""));
        // Left by a worker that tested one monitor at a time: still a status, with no monitors to ask about.
        fs::write(&path, "written_ms = 1\nphase = \"idle\"\n[trial]\npeer = \"pc\"\nuuid = \"U\"\ninput = 15\nhome = 18\nok = true\nid = 3\n").unwrap();
        assert_eq!(read_kvm(&path).unwrap().trial.map(|t| t.monitors), Some(vec![]));
        fs::remove_file(path).unwrap();
    }
}
