//! What a worker keeps beside the KVM policy for kvm.toml (the window's monitor rows, probes, inspect and
//! test answers), and the steps that run the policy. Shared by every worker that switches monitors; the DDC
//! backend and the input switch are the caller's.

use std::collections::{BTreeMap, HashMap};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use crate::config::{Config, DisplaySwitch};
use crate::forward::{SwitchTo, WayBack};
use crate::kvm::{self, Cmd, Done, Kvm, Phase, Step};
use crate::menu::MenuCmd;
use crate::proto::MonitorId;
use crate::status::{CheckState, InspectStatus, KvmPhase, KvmStatus, MonitorCheck, MonitorState, OutMonitor, TrialMonitor, TrialStatus};

/// What each linked peer says it is attached to; the same type as `flowseam_net::links::PeerMonitors`.
pub type PeerReports = Arc<Mutex<HashMap<String, Option<Vec<MonitorId>>>>>;

/// How long a stop waits for the monitor to land home. Inside the window's 6 s before it ends the worker; if
/// it runs out, kvm.toml still says the monitor is out and the window sends it home itself.
pub const KVM_STOP_LIMIT: Duration = Duration::from_secs(4);

/// Waits, at most `limit`, for an unfinished switch or return to land.
pub fn finish_kvm(kvm: &mut Kvm, rx: Option<&Receiver<Done>>, limit: Duration) {
    let deadline = Instant::now() + limit;
    while !matches!(kvm.phase(), Phase::Idle) {
        let Some(rx) = rx else { break };
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(done) => {
                crate::log::line(&format!("flowseam worker: monitor {done:?}"));
                kvm.done(done);
            }
            Err(_) => {
                crate::log::line(&format!("flowseam worker: the monitors' return is not confirmed {limit:?} into the stop; the window takes over"));
                break;
            }
        }
    }
}

/// Hands input to a peer (`Forwarder::switch_to`).
pub type SwitchInput<'a> = dyn FnMut(&str, WayBack) -> SwitchTo + 'a;

/// How long a test shows the candidate input: long enough to recognize a desktop, short enough that the
/// person waits for it rather than doing something else.
pub const TRIAL_HOLD_MS: u64 = 8000;

/// The settings' commands. Both read or move only the monitor, and only between switches; a test also
/// needs input here, since the person must be looking at this Mac to judge it.
pub fn settings_command(cmd: MenuCmd<'_>, local: bool, kvm: &mut Kvm, side: &mut KvmSide, ddc: Option<&Sender<Cmd>>, switch: &mut SwitchInput<'_>) {
    if !matches!(kvm.phase(), Phase::Idle) {
        kvm.last_error = Some(format!("busy: {:?}", kvm.phase()));
        return;
    }
    match cmd {
        MenuCmd::Inspect(uuid) => {
            let sent = ddc.is_some_and(|tx| tx.send(Cmd::Inspect { uuid: uuid.to_string() }).is_ok());
            if !sent {
                side.inspect = Some(InspectStatus { uuid: uuid.to_string(), inputs: None, current: None, reason: Some("no DDC thread".into()) });
            }
        }
        MenuCmd::Test { token, monitors, peer } => {
            if !local {
                kvm.last_error = Some("a test needs the keyboard and mouse on this Mac".into());
                return;
            }
            side.trial_id = token;
            let target = kvm::Target { monitors: monitors.iter().map(|&(uuid, input, home)| kvm::Monitor { uuid: uuid.to_string(), input, home }).collect() };
            let steps = kvm.trial(peer, target, TRIAL_HOLD_MS);
            run_kvm(kvm, steps, ddc, switch);
        }
        MenuCmd::Switch { .. } | MenuCmd::Input(_) | MenuCmd::Return(_) | MenuCmd::Home => {}
    }
}

/// The chosen monitors of `peer`, if each is in the config the worker runs with and its last probe found
/// it usable; an untested one switches too. One that is not refuses the whole switch: switching only the
/// rest would leave the person in front of a screen they did not choose.
pub fn kvm_ready(side: &KvmSide, peer: &str, uuids: &[&str]) -> Result<kvm::Target, String> {
    let mut monitors: Vec<kvm::Monitor> = Vec::new();
    for &uuid in uuids {
        let (_, d) = side.monitors.iter().find(|(p, d)| p == peer && d.uuid == uuid).ok_or_else(|| format!("monitor {uuid} is not one of {peer}'s"))?;
        if side.openable.is_some_and(|ok| !ok(uuid)) {
            return Err(format!("monitor switch to {peer} is not available: monitor {uuid} cannot be identified reliably"));
        }
        if let Some(b) = side.state(peer, d).blocked() {
            return Err(format!("monitor switch to {peer} is not available: monitor {uuid}: {b:?}"));
        }
        if !monitors.iter().any(|m| m.uuid == uuid) {
            monitors.push(kvm::Monitor { uuid: d.uuid.clone(), input: d.input, home: d.home });
        }
    }
    Ok(kvm::Target { monitors })
}

/// Queues a "back" for `peer` once: asked twice before it runs, it still runs once.
pub fn want_home(wanted: &mut Vec<String>, peer: &str) {
    if !wanted.iter().any(|p| p == peer) {
        wanted.push(peer.to_string());
    }
}

/// Queues a probe of every configured monitor on the DDC thread; one that cannot be queued is recorded
/// as unknown rather than left looking fine.
pub fn probe_monitors(monitors: &[(String, DisplaySwitch)], ddc: Option<&Sender<Cmd>>, checks: &mut BTreeMap<(String, String), MonitorCheck>) {
    for (peer, d) in monitors {
        let monitor = kvm::Monitor { uuid: d.uuid.clone(), input: d.input, home: d.home };
        let sent = ddc.is_some_and(|tx| tx.send(Cmd::Probe { peer: peer.clone(), monitor }).is_ok());
        if !sent {
            checks.insert((peer.clone(), d.uuid.clone()), MonitorCheck { peer: peer.clone(), state: CheckState::Unknown, reason: Some("no DDC thread".into()) });
        }
    }
}

/// Runs the KVM policy's steps in order. A command the DDC thread cannot take is answered as failed,
/// and whatever the policy does about that runs too.
pub fn run_kvm(kvm: &mut Kvm, steps: Vec<Step>, ddc: Option<&Sender<Cmd>>, switch: &mut SwitchInput<'_>) {
    let mut queue = std::collections::VecDeque::from(steps);
    while let Some(step) = queue.pop_front() {
        match step {
            Step::SwitchInput(peer, way_back) => {
                let r = switch(&peer, way_back);
                crate::log::line(&format!("flowseam worker: menu switch to {peer}: {r:?}"));
            }
            Step::Ddc(cmd) => {
                crate::log::line(&format!("flowseam worker: monitor command {cmd:?}"));
                let unsent = match ddc {
                    Some(tx) => tx.send(cmd).err().map(|e| e.0),
                    None => Some(cmd),
                };
                if let Some(cmd) = unsent {
                    let why = "no DDC thread".to_string();
                    queue.extend(kvm.done(match cmd {
                        Cmd::Away { peer, .. } => Done::Away { peer, result: Err(why), out: Vec::new() },
                        // Nothing was written, so whatever was out is still out.
                        Cmd::Home { peer, target } => Done::Home { peer, result: Err(why), out: target.monitors },
                        Cmd::Probe { peer, monitor } => Done::Probe { uuid: monitor.uuid, check: MonitorCheck { peer, state: CheckState::Unknown, reason: Some(why) } },
                        Cmd::Inspect { uuid } => Done::Inspect(InspectStatus { uuid, inputs: None, current: None, reason: Some(why) }),
                    }));
                }
            }
        }
    }
}

/// What the worker keeps beside the policy for kvm.toml: the configured monitors, probe results, the
/// settings' last inspect, and the number of the current test, so the window asks its question once per test.
#[derive(Default)]
pub struct KvmSide {
    /// Every monitor in the config the worker runs with, and its peer, in config order.
    pub monitors: Vec<(String, DisplaySwitch)>,
    /// What the last probe found, by peer and monitor UUID.
    pub checks: BTreeMap<(String, String), MonitorCheck>,
    pub inspect: Option<InspectStatus>,
    pub trial_id: u64,
    /// What each linked peer says it is attached to; filled by the links.
    pub peer_monitors: Option<PeerReports>,
    /// The EDID identity of each monitor here, by UUID; refreshed with every probe round.
    pub edids: HashMap<String, crate::proto::MonitorId>,
    /// Whether the DDC backend can open a monitor by this id safely; `None` accepts every id.
    pub openable: Option<fn(&str) -> bool>,
}

pub fn kvm_view(kvm: &Kvm, side: &KvmSide) -> KvmStatus {
    let phase = match kvm.phase() {
        Phase::Idle => KvmPhase::Idle,
        Phase::Switching { .. } => KvmPhase::Switching,
        Phase::Away { .. } => KvmPhase::Away,
        Phase::Returning { trial: None, .. } => KvmPhase::Returning,
        Phase::Returning { trial: Some(_), .. } | Phase::Trying { .. } | Phase::Shown { .. } => KvmPhase::Testing,
    };
    let out = kvm.out();
    let trial = kvm.last_trial.as_ref().map(|t| TrialStatus {
        peer: t.peer.clone(),
        monitors: t.monitors.iter().map(|m| TrialMonitor { uuid: m.uuid.clone(), input: m.input, home: m.home }).collect(),
        ok: t.result.is_ok(),
        reason: t.result.clone().err(),
        id: side.trial_id,
    });
    KvmStatus {
        written_ms: 0,
        phase,
        peer: out.as_ref().map(|(p, _)| p.clone()),
        error: kvm.last_error.clone(),
        out: out.map(|(_, ms)| ms.into_iter().map(|m| OutMonitor { uuid: m.uuid, home: m.home }).collect()).unwrap_or_default(),
        monitors: side.states(),
        trial,
        trial_hold_ms: TRIAL_HOLD_MS,
        inspect: side.inspect.clone(),
    }
}

/// The policy this worker starts with: monitors the previous run's kvm.toml still lists as out (its return,
/// and the window's after it, did not land) are adopted before this worker replaces the file; otherwise
/// nothing would ever try them again.
pub fn kvm_start(path: Option<&std::path::Path>) -> Kvm {
    let mut kvm = Kvm::new();
    let Some(prev) = path.and_then(|p| crate::status::read_kvm(p).ok()) else { return kvm };
    if !prev.out.is_empty() {
        crate::log::line(&format!("flowseam worker: {} monitor(s) still out from the previous run: {:?}", prev.out.len(), prev.out));
        kvm.adopt(prev.peer.unwrap_or_default(), prev.out.into_iter().map(|o| kvm::Monitor { uuid: o.uuid, input: o.home, home: o.home }).collect());
    }
    kvm
}

/// Writes kvm.toml when what it says changes; the first call always writes, replacing a previous run's file.
pub fn write_kvm_status(kvm: &Kvm, side: &KvmSide, path: Option<&std::path::Path>, written: &mut Option<KvmStatus>) {
    let view = kvm_view(kvm, side);
    if written.as_ref() == Some(&view) {
        return;
    }
    let Some(path) = path else { return };
    let stamped = KvmStatus { written_ms: crate::status::now_ms(), ..view.clone() };
    match crate::status::write_kvm(path, &stamped) {
        Ok(()) => *written = Some(view),
        Err(e) => crate::log::line(&format!("flowseam worker: kvm status write: {e}")),
    }
}

impl KvmSide {
    pub fn new(cfg: &Config, peer_monitors: Option<PeerReports>) -> Self {
        let monitors = cfg.peers.iter().flat_map(|p| p.displays.iter().map(|d| (p.name.clone(), d.clone()))).collect();
        Self { monitors, peer_monitors, ..Self::default() }
    }

    /// One configured monitor as it stands. Whether the peer sees it is reported, never a reason to refuse:
    /// a monitor may power down the inputs it is not showing, so the peer only sees it once it switches.
    pub fn state(&self, peer: &str, d: &DisplaySwitch) -> MonitorState {
        let reported: Option<Vec<crate::proto::MonitorId>> = self.peer_monitors.as_ref().and_then(|m| m.lock().unwrap_or_else(|e| e.into_inner()).get(peer).cloned()).flatten();
        MonitorState {
            peer: peer.to_string(),
            uuid: d.uuid.clone(),
            verified: d.verified,
            check: self.checks.get(&(peer.to_string(), d.uuid.clone())).cloned(),
            peer_sees: crate::status::peer_on_monitor(self.edids.get(&d.uuid).copied(), reported.as_deref()),
        }
    }

    /// Every monitor the config has for `peer`.
    pub fn monitors_of(&self, peer: &str) -> Vec<kvm::Monitor> {
        self.monitors.iter().filter(|(p, _)| p == peer).map(|(_, d)| kvm::Monitor { uuid: d.uuid.clone(), input: d.input, home: d.home }).collect()
    }

    pub fn states(&self) -> Vec<MonitorState> {
        self.monitors.iter().map(|(p, d)| self.state(p, d)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mon(uuid: &str) -> kvm::Monitor {
        kvm::Monitor { uuid: uuid.into(), input: 15, home: 18 }
    }
    fn target() -> kvm::Target {
        kvm::Target { monitors: vec![mon("U"), mon("V")] }
    }

    #[test]
    fn a_test_needs_input_here_and_reports_under_its_own_number() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut k = Kvm::new();
        let mut side = KvmSide::default();
        let test = MenuCmd::Test { token: 42, monitors: vec![("U", 15, 18)], peer: "pc" };
        settings_command(test, false, &mut k, &mut side, Some(&tx), &mut |_, _| panic!("no input moves in a test"));
        assert!(rx.try_recv().is_err() && k.last_error.is_some(), "not while input is away");
        let test = MenuCmd::Test { token: 42, monitors: vec![("U", 15, 18), ("V", 15, 18)], peer: "pc" };
        settings_command(test, true, &mut k, &mut side, Some(&tx), &mut |_, _| panic!("no input moves in a test"));
        assert_eq!(rx.try_recv(), Ok(Cmd::Away { peer: "pc".into(), target: kvm::Target { monitors: vec![mon("U"), mon("V")] } }), "both in one switch");
        assert_eq!((side.trial_id, kvm_view(&k, &side).phase), (42, KvmPhase::Testing));
        settings_command(MenuCmd::Inspect("U"), true, &mut k, &mut side, Some(&tx), &mut |_, _| SwitchTo::Switched);
        assert!(rx.try_recv().is_err(), "nothing else while a test runs");
    }

    fn cfg_with(verified: [bool; 2]) -> Config {
        let mut cfg = Config::default();
        cfg.peers.push(crate::config::Peer {
            name: "pc".into(),
            addr: "192.168.1.2:24800".into(),
            edge: crate::config::Edge::Right,
            fingerprint: "ab".repeat(32),
            keys: Default::default(),
            gestures: Default::default(),
            displays: vec![
                DisplaySwitch { uuid: "U".into(), input: 15, home: 18, verified: verified[0] },
                DisplaySwitch { uuid: "V".into(), input: 15, home: 18, verified: verified[1] },
            ],
        });
        cfg
    }

    fn checked(cfg: &Config, state: CheckState) -> KvmSide {
        let mut side = KvmSide::new(cfg, None);
        for u in ["U", "V"] {
            side.checks.insert(("pc".into(), u.into()), MonitorCheck { peer: "pc".into(), state, reason: Some("why".into()) });
        }
        side
    }

    #[test]
    fn a_monitor_switch_waits_for_a_usable_probe_of_every_chosen_monitor() {
        let both = cfg_with([true, true]);
        assert_eq!(kvm_ready(&checked(&both, CheckState::Ok), "pc", &["U", "V"]), Ok(target()));
        assert_eq!(kvm_ready(&checked(&both, CheckState::Ok), "pc", &["V"]), Ok(kvm::Target::one(mon("V"))), "one of them alone");
        assert_eq!(kvm_ready(&checked(&both, CheckState::Ok), "pc", &["U", "U"]), Ok(kvm::Target::one(mon("U"))), "named twice, switched once");
        let one_unverified = checked(&cfg_with([true, false]), CheckState::Ok);
        assert_eq!(kvm_ready(&one_unverified, "pc", &["U", "V"]), Ok(target()), "an untested monitor switches too");
        assert_eq!(kvm_ready(&one_unverified, "pc", &["U"]), Ok(kvm::Target::one(mon("U"))));
        let one_down = { let mut s = checked(&cfg_with([true, false]), CheckState::Ok); s.checks.insert(("pc".into(), "V".into()), MonitorCheck { peer: "pc".into(), state: CheckState::Problem, reason: Some("gone".into()) }); s };
        let e = kvm_ready(&one_down, "pc", &["U", "V"]).unwrap_err();
        assert!(e.contains("monitor V") && e.contains("gone"), "one unusable monitor refuses the whole switch and is named: {e}");
        assert!(kvm_ready(&checked(&both, CheckState::Problem), "pc", &["U"]).unwrap_err().contains("why"));
        assert!(kvm_ready(&KvmSide::new(&both, None), "pc", &["U"]).unwrap_err().contains("NotChecked"));
        assert!(kvm_ready(&checked(&both, CheckState::Ok), "other", &["U"]).is_err(), "another peer's name");
        assert!(kvm_ready(&checked(&both, CheckState::Ok), "pc", &["W"]).unwrap_err().contains("not one of"));
    }

    /// Some monitors power down the inputs they are not showing, so the peer cannot see one until it
    /// switches: what the peer reports is shown, never a reason to refuse.
    #[test]
    fn what_the_peer_reports_is_shown_but_does_not_block_the_switch() {
        use crate::proto::MonitorId;
        let mine = MonitorId { vendor: 0x09D1, product: 0x7955, serial: 0x1234 };
        let reports = PeerReports::default();
        let mut side = checked(&cfg_with([true, true]), CheckState::Ok);
        side.peer_monitors = Some(reports.clone());
        side.edids.insert("U".into(), mine);
        assert_eq!(side.states()[0].peer_sees, None, "no report yet");
        reports.lock().unwrap().insert("pc".into(), Some(vec![MonitorId { vendor: 0x0693, product: 0xA714, serial: 1 }]));
        assert_eq!(side.states()[0].peer_sees, Some(false));
        assert_eq!(kvm_ready(&side, "pc", &["U", "V"]), Ok(target()));
        reports.lock().unwrap().insert("pc".into(), Some(vec![mine]));
        assert_eq!(kvm_view(&Kvm::new(), &side).monitors[0].peer_sees, Some(true), "kvm.toml says the same");
    }

    #[test]
    fn every_configured_monitor_is_probed_and_an_unsent_probe_is_unknown() {
        let (tx, rx) = std::sync::mpsc::channel();
        let side = KvmSide::new(&cfg_with([true, true]), None);
        let mut checks = BTreeMap::new();
        probe_monitors(&side.monitors, Some(&tx), &mut checks);
        assert_eq!(rx.try_recv(), Ok(Cmd::Probe { peer: "pc".into(), monitor: mon("U") }));
        assert_eq!(rx.try_recv(), Ok(Cmd::Probe { peer: "pc".into(), monitor: mon("V") }));
        assert!(checks.is_empty());
        probe_monitors(&side.monitors, None, &mut checks);
        assert_eq!(checks[&("pc".to_string(), "V".to_string())].state, CheckState::Unknown);
        assert_eq!(checks.len(), 2);
    }

    #[test]
    fn a_menu_switch_reaches_the_ddc_thread_and_input_follows_its_answer() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut switched = Vec::new();
        let mut k = Kvm::new();
        let steps = k.request("pc", Some(target()), WayBack::Closed);
        run_kvm(&mut k, steps, Some(&tx), &mut |p, w| { switched.push((p.to_string(), w)); SwitchTo::Switched });
        assert_eq!(rx.try_recv(), Ok(Cmd::Away { peer: "pc".into(), target: target() }));
        assert!(switched.is_empty(), "input waits for the monitors");
        let steps = k.done(Done::Away { peer: "pc".into(), result: Ok(()), out: vec![] });
        run_kvm(&mut k, steps, Some(&tx), &mut |p, w| { switched.push((p.to_string(), w)); SwitchTo::Switched });
        assert_eq!(switched, [("pc".to_string(), WayBack::Closed)], "input follows with the way back its request planned");
    }

    #[test]
    fn without_a_ddc_thread_a_switch_fails_and_says_why() {
        let mut k = Kvm::new();
        let steps = k.request("pc", Some(target()), WayBack::Open);
        run_kvm(&mut k, steps, None, &mut |_, _| panic!("input must not move"));
        assert_eq!(k.phase(), &Phase::Idle);
        assert_eq!(k.last_error.as_deref(), Some("no DDC thread"));
    }

    #[test]
    fn kvm_status_is_written_once_per_change() {
        let path = std::env::temp_dir().join(format!("flowseam-kvm-worker-{}-{}.toml", std::process::id(), crate::status::now_ms()));
        let mut written = None;
        let mut k = Kvm::new();
        let side = KvmSide::new(&cfg_with([true, false]), None);
        write_kvm_status(&k, &side, Some(&path), &mut written);
        let first = crate::status::read_kvm(&path).unwrap();
        assert_eq!(first.phase, KvmPhase::Idle);
        assert_eq!(first.monitors.iter().map(|m| (m.uuid.as_str(), m.verified)).collect::<Vec<_>>(), [("U", true), ("V", false)], "the window gates on what the worker runs");
        assert_eq!(first.trial_hold_ms, TRIAL_HOLD_MS);
        std::fs::remove_file(&path).unwrap();
        write_kvm_status(&k, &side, Some(&path), &mut written);
        assert!(!path.exists(), "unchanged: not rewritten");
        k.request("pc", Some(target()), WayBack::Open);
        write_kvm_status(&k, &side, Some(&path), &mut written);
        let s = crate::status::read_kvm(&path).unwrap();
        assert_eq!((s.phase, s.peer.as_deref()), (KvmPhase::Switching, Some("pc")));
        assert_eq!(s.out, [OutMonitor { uuid: "U".into(), home: 18 }, OutMonitor { uuid: "V".into(), home: 18 }], "names every monitor for the window's fallback");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn monitors_the_previous_run_left_out_are_adopted_and_stay_listed() {
        let path = std::env::temp_dir().join(format!("flowseam-kvm-adopt-{}-{}.toml", std::process::id(), crate::status::now_ms()));
        assert_eq!(kvm_start(Some(&path)).out(), None, "no previous file");
        let prev = KvmStatus { written_ms: 1, peer: Some("pc".into()), out: vec![OutMonitor { uuid: "V".into(), home: 18 }], ..Default::default() };
        crate::status::write_kvm(&path, &prev).unwrap();
        let mut k = kvm_start(Some(&path));
        let mut written = None;
        write_kvm_status(&k, &KvmSide::default(), Some(&path), &mut written);
        assert_eq!(crate::status::read_kvm(&path).unwrap().out, prev.out, "this run's kvm.toml keeps naming it");
        assert_eq!(k.stop(), vec![Step::Ddc(Cmd::Home { peer: "pc".into(), target: kvm::Target { monitors: vec![kvm::Monitor { uuid: "V".into(), input: 18, home: 18 }] } })]);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_back_brings_every_monitor_of_that_peer_and_is_asked_once() {
        let side = KvmSide::new(&cfg_with([true, false]), None);
        assert_eq!(side.monitors_of("pc"), target().monitors, "unconfirmed ones too: going home needs no confirmation");
        assert_eq!(side.monitors_of("air"), vec![]);
        let mut wanted = Vec::new();
        want_home(&mut wanted, "pc");
        want_home(&mut wanted, "pc");
        want_home(&mut wanted, "air");
        assert_eq!(wanted, ["pc", "air"]);
    }

    #[test]
    fn an_unstable_monitor_id_refuses_the_switch_even_when_it_probed_fine() {
        let mut side = checked(&cfg_with([true, true]), CheckState::Ok);
        side.openable = Some(|id| id != "U");
        let e = kvm_ready(&side, "pc", &["V", "U"]).unwrap_err();
        assert!(e.contains("monitor U") && e.contains("reliably"), "{e}");
        assert_eq!(kvm_ready(&side, "pc", &["V"]), Ok(kvm::Target::one(mon("V"))), "the stable one alone is fine");
    }

    #[test]
    fn a_stop_waits_for_the_return_only_up_to_its_limit() {
        use std::sync::mpsc::channel;
        let (tx, rx) = channel();
        let mut k = Kvm::new();
        k.request("pc", Some(target()), WayBack::Open);
        k.done(Done::Away { peer: "pc".into(), result: Ok(()), out: vec![] });
        k.stop();
        assert!(matches!(k.phase(), Phase::Returning { .. }));
        let t0 = Instant::now();
        finish_kvm(&mut k, Some(&rx), Duration::from_millis(200));
        assert!(t0.elapsed() < Duration::from_secs(2), "unanswered: gives up at the limit");
        assert!(matches!(k.phase(), Phase::Returning { .. }), "and says nothing landed");
        tx.send(Done::Home { peer: "pc".into(), result: Ok(()), out: vec![] }).unwrap();
        finish_kvm(&mut k, Some(&rx), Duration::from_secs(5));
        assert_eq!(k.phase(), &Phase::Idle, "an answer inside the limit lands it");
    }
}
