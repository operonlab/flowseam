//! Monitors that switch along with a session started from the menu. The button first moves the chosen
//! monitors to the peer's inputs and only then hands over the keyboard and mouse; when that session
//! ends by any path (the peer's own "back" button, the chord, the edge, a dropped link, a display
//! topology change, the worker stopping), the monitors go back to this Mac's inputs.
//!
//! `Kvm` is the policy and holds no OS state. `mac::spawn` runs the DDC side on one thread of its
//! own: the display handle cannot leave the thread that opened it, and a single owner keeps this
//! process to one writer on the I²C bus (two writers at once read back garbage).

use flowseam_core::forward::WayBack;

/// One monitor of a peer, from the config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Monitor {
    pub uuid: String,
    /// Input that shows the peer.
    pub input: u16,
    /// Input that shows this Mac.
    pub home: u16,
}

/// The monitors one switch moves together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub monitors: Vec<Monitor>,
}

impl Target {
    pub fn one(m: Monitor) -> Self {
        Self { monitors: vec![m] }
    }
}

/// `ms` plus those of `more` not in it already, by UUID.
fn union(mut ms: Vec<Monitor>, more: &[Monitor]) -> Vec<Monitor> {
    for m in more {
        if !ms.iter().any(|x| x.uuid == m.uuid) {
            ms.push(m.clone());
        }
    }
    ms
}

/// Work for the DDC thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    Away { peer: String, target: Target },
    Home { peer: String, target: Target },
    /// Is the monitor connected here, answering DDC, and does it have both inputs? Switches nothing.
    Probe { peer: String, monitor: Monitor },
    /// Read a monitor's inputs and the one showing now, for the settings. Switches nothing.
    Inspect { uuid: String },
}

/// The DDC thread's answer to one `Cmd`. A switch moves all of its monitors or none: an `Away` that
/// failed has already sent every one of them home. `out` names the monitors whose return was not
/// confirmed, which may still show the peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Done {
    Away { peer: String, result: Result<(), String>, out: Vec<Monitor> },
    Home { peer: String, result: Result<(), String>, out: Vec<Monitor> },
    Probe { uuid: String, check: flowseam_core::status::MonitorCheck },
    Inspect(flowseam_core::status::InspectStatus),
}

/// What the worker must do next, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Ddc(Cmd),
    /// Hand input to this peer now (`Forwarder::switch_to`), with the way back its request planned.
    SwitchInput(String, WayBack),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    Idle,
    /// The monitors are being moved to the peer; input is still here.
    Switching { peer: String, target: Target },
    /// The monitors show the peer and the peer has input.
    Away { peer: String, target: Target },
    /// The session ended, or a test's time ran out; the monitors are on their way home. `trial` is the
    /// monitors of the test this return ends, if it ends one.
    Returning { peer: String, target: Target, trial: Option<Target> },
    /// A test from the settings: the monitors move to their candidate inputs together, input stays here.
    Trying { peer: String, target: Target, hold_ms: u64 },
    /// The monitors show the candidates for `hold_ms` from `since_ms` (set on the first tick), then go home.
    Shown { peer: String, target: Target, hold_ms: u64, since_ms: Option<u64> },
}

/// How a test from the settings ended: `Ok` when every monitor confirmed its candidate input and came back.
/// They switch together or not at all, so one failure is the whole test's. Whether each input showed the
/// right computer is the person's answer, not this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrialResult {
    pub peer: String,
    pub monitors: Vec<Monitor>,
    pub result: Result<(), String>,
}

#[derive(Debug, Default)]
pub struct Kvm {
    phase: Option<Phase>,
    /// Why the last switch or return did not complete, for the menu. Cleared by the next request.
    pub last_error: Option<String>,
    /// The last finished test. Cleared when the next one starts.
    pub last_trial: Option<TrialResult>,
    /// Monitors whose return failed, and the peer they were switched to: they may still show it. They
    /// count as out, and every later return takes them along.
    // ponytail: no timed retry between sessions; a monitor that went dark to this Mac when it left cannot
    // be reached until it comes back, so add one only if a bus collision ever strands a monitor.
    stranded: Option<(String, Vec<Monitor>)>,
    /// This Mac was sending input to a peer at the last tick: an input-only session ending is also a time
    /// to try a stranded monitor again, since the person is back at this Mac.
    was_driving: bool,
    /// The way back the switch in flight hands input over with, set by each request that starts one: once
    /// the monitors have moved, they may no longer be listed here.
    way_back: WayBack,
}

impl Kvm {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn phase(&self) -> &Phase {
        self.phase.as_ref().unwrap_or(&Phase::Idle)
    }

    /// Monitors an earlier process left out (its kvm.toml still lists them): they count as stranded here.
    /// That process's session is over, so the caller sends them home with `return_stranded` once it can.
    pub fn adopt(&mut self, peer: String, out: Vec<Monitor>) {
        self.strand(peer, &out);
    }

    /// Between switches, sends home the monitors whose return failed, if any.
    pub fn return_stranded(&mut self) -> Vec<Step> {
        match (self.phase(), self.stranded.clone()) {
            (Phase::Idle, Some((peer, ms))) => self.go_returning(peer, Target { monitors: ms }, None),
            _ => Vec::new(),
        }
    }

    /// A "back" asked for from either side: `monitors`, every one configured for `peer`, go home whether or
    /// not this process switched them (one may have changed input by itself while this Mac was down).
    /// `None` while a switch or return is under way: the caller asks again once it is `Idle`.
    pub fn bring_home(&mut self, peer: &str, monitors: Vec<Monitor>) -> Option<Vec<Step>> {
        if !matches!(self.phase(), Phase::Idle) {
            return None;
        }
        if monitors.is_empty() {
            return Some(self.return_stranded());
        }
        self.last_error = None;
        Some(self.go_returning(peer.into(), Target { monitors }, None))
    }

    /// The monitors out right now and the peer they were moved for: what a process that outlives this
    /// one must send home.
    pub fn out(&self) -> Option<(String, Vec<Monitor>)> {
        let (peer, ms) = match self.phase() {
            Phase::Idle => (self.stranded.as_ref()?.0.clone(), Vec::new()),
            Phase::Switching { peer, target } | Phase::Away { peer, target } | Phase::Returning { peer, target, .. } => (peer.clone(), target.monitors.clone()),
            Phase::Trying { peer, target, .. } | Phase::Shown { peer, target, .. } => (peer.clone(), target.monitors.clone()),
        };
        Some((peer, union(ms, self.stranded.as_ref().map_or(&[], |(_, s)| s.as_slice()))))
    }

    /// The settings' test: show each monitor's candidate input for `hold_ms`, all at once, then go home.
    /// Input never leaves.
    pub fn trial(&mut self, peer: &str, target: Target, hold_ms: u64) -> Vec<Step> {
        if !matches!(self.phase(), Phase::Idle) {
            self.last_error = Some(format!("busy: {:?}", self.phase()));
            return Vec::new();
        }
        self.last_error = None;
        self.last_trial = None;
        self.phase = Some(Phase::Trying { peer: peer.into(), target: target.clone(), hold_ms });
        vec![Step::Ddc(Cmd::Away { peer: peer.into(), target })]
    }

    /// The menu button for `peer`. Without monitors it is a plain input switch, and its way back is open.
    pub fn request(&mut self, peer: &str, target: Option<Target>, way_back: WayBack) -> Vec<Step> {
        if !matches!(self.phase(), Phase::Idle) {
            self.last_error = Some(format!("busy: {:?}", self.phase()));
            return Vec::new();
        }
        self.last_error = None;
        match target.filter(|t| !t.monitors.is_empty()) {
            None => vec![Step::SwitchInput(peer.into(), WayBack::Open)],
            Some(target) => {
                self.way_back = way_back;
                self.phase = Some(Phase::Switching { peer: peer.into(), target: target.clone() });
                vec![Step::Ddc(Cmd::Away { peer: peer.into(), target })]
            }
        }
    }

    pub fn done(&mut self, d: Done) -> Vec<Step> {
        match (self.phase.take().unwrap_or(Phase::Idle), d) {
            (Phase::Switching { peer, target }, Done::Away { peer: p, result, out }) if p == peer => match result {
                Ok(()) => {
                    self.unstrand(&target.monitors);
                    self.phase = Some(Phase::Away { peer: peer.clone(), target });
                    vec![Step::SwitchInput(peer, self.way_back)]
                }
                Err(e) => {
                    self.last_error = Some(e);
                    self.retry_home(peer, out)
                }
            },
            (Phase::Trying { peer, target, hold_ms }, Done::Away { peer: p, result, out }) if p == peer => match result {
                Ok(()) => {
                    self.unstrand(&target.monitors);
                    self.phase = Some(Phase::Shown { peer, target, hold_ms, since_ms: None });
                    Vec::new()
                }
                Err(e) => {
                    self.last_trial = Some(TrialResult { peer: peer.clone(), monitors: target.monitors, result: Err(e) });
                    self.retry_home(peer, out)
                }
            },
            (Phase::Returning { peer, target, trial }, Done::Home { peer: p, result, out }) if p == peer => {
                self.unstrand(&target.monitors);
                if let Some(t) = trial {
                    // The test's own monitors decide it; a stranded one taken along does not.
                    let back = !out.iter().any(|o| t.monitors.iter().any(|m| m.uuid == o.uuid));
                    let result = if back { Ok(()) } else { result.clone() };
                    self.last_trial = Some(TrialResult { peer: peer.clone(), monitors: t.monitors, result });
                }
                // A return after a failed switch keeps that failure's message unless it fails too.
                if let Err(e) = result {
                    self.last_error = Some(e);
                    self.strand(peer, &out);
                }
                Vec::new()
            }
            (phase, _) => {
                // Not the answer this phase waits for (an `Away` overtaken by a stop, or a probe, which the
                // worker reads): keep waiting.
                self.phase = Some(phase);
                Vec::new()
            }
        }
    }

    /// Every main-loop turn, after the steps returned so far have run. `driving` is the peer this
    /// Mac is sending input to, if any. A session that is no longer live sends the monitors home, and so
    /// does a test whose time is up; any session ending, input-only too, takes a stranded monitor along.
    pub fn tick(&mut self, driving: Option<&str>, now_ms: u64) -> Vec<Step> {
        let ended = std::mem::replace(&mut self.was_driving, driving.is_some()) && driving.is_none();
        if let Some(Phase::Shown { since_ms: since @ None, .. }) = &mut self.phase {
            *since = Some(now_ms);
        }
        match self.phase() {
            Phase::Away { peer, .. } if driving != Some(peer.as_str()) => self.go_home(),
            Phase::Shown { hold_ms, since_ms: Some(since), .. } if now_ms.saturating_sub(*since) >= *hold_ms => self.go_home(),
            Phase::Idle if ended => self.return_stranded(),
            _ => Vec::new(),
        }
    }

    /// The worker is stopping: whatever is in flight, and any monitor whose return failed, ends at home.
    /// The DDC thread runs a switch's commands in order, so a `Home` queued behind an unfinished `Away`
    /// still lands last.
    pub fn stop(&mut self) -> Vec<Step> {
        match self.phase() {
            Phase::Switching { .. } | Phase::Away { .. } | Phase::Trying { .. } | Phase::Shown { .. } => self.go_home(),
            Phase::Idle => self.return_stranded(),
            Phase::Returning { .. } => Vec::new(),
        }
    }

    /// After a failed `Away`: one more round for the monitors whose return was not confirmed, as a return,
    /// so the stop path and the window's fallback see monitors that may still be out.
    fn retry_home(&mut self, peer: String, out: Vec<Monitor>) -> Vec<Step> {
        if out.is_empty() {
            return Vec::new();
        }
        self.go_returning(peer, Target { monitors: out }, None)
    }

    fn go_home(&mut self) -> Vec<Step> {
        match self.phase.take() {
            Some(Phase::Switching { peer, target }) | Some(Phase::Away { peer, target }) => self.go_returning(peer, target, None),
            Some(Phase::Trying { peer, target, .. }) | Some(Phase::Shown { peer, target, .. }) => self.go_returning(peer, target.clone(), Some(target)),
            other => {
                self.phase = other;
                Vec::new()
            }
        }
    }

    /// Sends `target` home, and any stranded monitor with it.
    fn go_returning(&mut self, peer: String, target: Target, trial: Option<Target>) -> Vec<Step> {
        let target = Target { monitors: union(target.monitors, self.stranded.as_ref().map_or(&[], |(_, s)| s.as_slice())) };
        self.phase = Some(Phase::Returning { peer: peer.clone(), target: target.clone(), trial });
        vec![Step::Ddc(Cmd::Home { peer, target })]
    }

    fn strand(&mut self, peer: String, out: &[Monitor]) {
        if out.is_empty() {
            return;
        }
        let had = self.stranded.take().map(|(_, s)| s).unwrap_or_default();
        self.stranded = Some((peer, union(had, out)));
    }

    fn unstrand(&mut self, back: &[Monitor]) {
        if let Some((peer, s)) = self.stranded.take() {
            let left: Vec<Monitor> = s.into_iter().filter(|m| !back.iter().any(|b| b.uuid == m.uuid)).collect();
            if !left.is_empty() {
                self.stranded = Some((peer, left));
            }
        }
    }
}

/// The DDC thread's next command: a switch or a return before any probe or inspect queued ahead of it,
/// so a click never waits behind a round of checks; otherwise in order.
pub fn next_cmd(queue: &mut std::collections::VecDeque<Cmd>) -> Option<Cmd> {
    let urgent = queue.iter().position(|c| matches!(c, Cmd::Away { .. } | Cmd::Home { .. })).unwrap_or(0);
    queue.remove(urgent)
}

#[cfg(target_os = "macos")]
pub mod mac {
    use super::{Cmd, Done, Monitor, Target};
    use crate::ddc::{self, mac::Display, mac::OpenError, DdcError, Readback};
    use flowseam_core::status::{CheckState, InspectStatus, MonitorCheck};
    use std::collections::HashMap;
    use std::sync::mpsc::{channel, Receiver, Sender};
    use std::thread;

    /// Input lists read so far, by monitor UUID. Only successful reads are kept: a read lost to another
    /// writer on the bus is tried again at the next probe.
    type Caps = HashMap<String, Option<Vec<u16>>>;

    /// Starts the DDC thread. It ends when the `Cmd` sender is dropped.
    pub fn spawn() -> std::io::Result<(Sender<Cmd>, Receiver<Done>)> {
        let (cmd_tx, cmd_rx) = channel::<Cmd>();
        let (done_tx, done_rx) = channel::<Done>();
        thread::Builder::new().name("flowseam-ddc".into()).spawn(move || {
            let mut caps = Caps::new();
            let mut queue = std::collections::VecDeque::new();
            loop {
                if queue.is_empty() {
                    match cmd_rx.recv() {
                        Ok(cmd) => queue.push_back(cmd),
                        Err(_) => break,
                    }
                }
                queue.extend(cmd_rx.try_iter());
                let Some(cmd) = super::next_cmd(&mut queue) else { continue };
                if done_tx.send(run(cmd, &mut caps)).is_err() {
                    break;
                }
            }
        })?;
        Ok((cmd_tx, done_rx))
    }

    /// Runs one command on this thread, for a caller with no DDC thread (the GUI after a worker died).
    pub fn run_now(cmd: Cmd) -> Done {
        run(cmd, &mut Caps::new())
    }

    fn run(cmd: Cmd, caps: &mut Caps) -> Done {
        match cmd {
            Cmd::Away { peer, target } => {
                let (result, out) = away(&target);
                Done::Away { peer, result, out }
            }
            Cmd::Home { peer, target } => {
                let (result, out) = home(&target);
                Done::Home { peer, result, out }
            }
            Cmd::Probe { peer, monitor } => {
                let (state, reason) = probe(&monitor, caps);
                Done::Probe { uuid: monitor.uuid, check: MonitorCheck { peer, state, reason } }
            }
            Cmd::Inspect { uuid } => Done::Inspect(inspect(uuid, caps)),
        }
    }

    fn inspect(uuid: String, caps: &mut Caps) -> InspectStatus {
        let d = match ddc::mac::open(&uuid) {
            Ok(d) => d,
            Err(e) => return InspectStatus { uuid, inputs: None, current: None, reason: Some(format!("cannot open the monitor: {e:?}")) },
        };
        let mut reasons = Vec::new();
        let current = ddc::confirm(|| d.get(ddc::VCP_INPUT).map(|r| r.current), u16::MAX, 3, || thread::sleep(ddc::CONFIRM_INTERVAL / 4));
        let current = match current {
            Readback::Mismatch(v) => Some(v),
            other => {
                reasons.push(format!("current input unreadable: {other:?}"));
                None
            }
        };
        if !caps.contains_key(&uuid) {
            match d.capabilities() {
                Ok(s) => {
                    caps.insert(uuid.clone(), ddc::input_sources(&s));
                }
                Err(e) => reasons.push(format!("capabilities unreadable: {e:?}")),
            }
        }
        let inputs = caps.get(&uuid).cloned().flatten();
        InspectStatus { uuid, inputs, current, reason: (!reasons.is_empty()).then(|| reasons.join("; ")) }
    }

    fn probe(m: &Monitor, caps: &mut Caps) -> (CheckState, Option<String>) {
        let d = match ddc::mac::open(&m.uuid) {
            Ok(d) => d,
            Err(OpenError::NotFound) => return (CheckState::Problem, Some(format!("monitor {} is not connected to this Mac", m.uuid))),
            Err(OpenError::NoAvService) => return (CheckState::Problem, Some("the monitor has no DDC channel (built-in, virtual or AirPlay)".into())),
            Err(e) => return (CheckState::Unknown, Some(format!("cannot open the monitor: {e:?}"))),
        };
        let mut last = None;
        let answered = (0..3).any(|i| {
            if i > 0 {
                thread::sleep(ddc::CONFIRM_INTERVAL / 4);
            }
            d.get(ddc::VCP_INPUT).map_err(|e| last = Some(e)).is_ok()
        });
        if !answered {
            return (CheckState::Unknown, Some(format!("the monitor does not answer DDC: {last:?}")));
        }
        if !caps.contains_key(&m.uuid) {
            match d.capabilities() {
                Ok(s) => {
                    caps.insert(m.uuid.clone(), ddc::input_sources(&s));
                }
                Err(e) => return (CheckState::Unlisted, Some(format!("the monitor's capabilities could not be read: {e:?}"))),
            }
        }
        match caps.get(&m.uuid).cloned().flatten() {
            Some(list) if !list.is_empty() => {
                let names = list.iter().map(|v| ddc::input_name(*v)).collect::<Vec<_>>().join(", ");
                for (what, v) in [("the peer's", m.input), ("this Mac's", m.home)] {
                    if !list.contains(&v) {
                        return (CheckState::Problem, Some(format!("the monitor has no input {} ({what}); it has {names}", ddc::input_name(v))));
                    }
                }
                (CheckState::Ok, None)
            }
            _ => (CheckState::Unlisted, Some("the monitor does not list its inputs".into())),
        }
    }

    // ponytail: opens the display afresh for every command (an IORegistry walk, milliseconds), so a
    // replugged monitor needs no cache invalidation; cache the handle if switching ever needs to be faster.
    fn open(m: &Monitor) -> Result<Display, String> {
        ddc::mac::open(&m.uuid).map_err(|e| format!("monitor {}: {e:?}", m.uuid))
    }

    /// Writes every monitor's `value` first, then reads them back together. A write that failed is not read.
    fn set_all(ds: &[(&Monitor, &Display)], value: fn(&Monitor) -> u16) -> Vec<Result<Readback, DdcError>> {
        let sets: Vec<Result<(), DdcError>> = ds.iter().map(|(m, d)| d.set(ddc::VCP_INPUT, value(m))).collect();
        let pending: Vec<usize> = (0..ds.len()).filter(|&i| sets[i].is_ok()).collect();
        let wants: Vec<u16> = pending.iter().map(|&i| value(ds[i].0)).collect();
        let reads = ddc::confirm_all(|k| ds[pending[k]].1.get(ddc::VCP_INPUT).map(|r| r.current), &wants, ddc::CONFIRM_ATTEMPTS, || thread::sleep(ddc::CONFIRM_INTERVAL));
        let mut reads = reads.into_iter();
        sets.into_iter().map(|s| s.map(|()| reads.next().expect("one read per write that went out"))).collect()
    }

    /// Sends every monitor in `ds` home, with a second round for those that did not confirm (a write can
    /// be lost to another writer on the bus). Returns the ones still not home, and why.
    fn home_all(ds: &[(&Monitor, &Display)]) -> Vec<(Monitor, String)> {
        let mut left: Vec<(&Monitor, &Display)> = ds.to_vec();
        let mut why = Vec::new();
        for _ in 0..2 {
            if left.is_empty() {
                break;
            }
            let results = set_all(&left, |m| m.home);
            why.clear();
            let mut next = Vec::new();
            for (&(m, d), r) in left.iter().zip(results) {
                if !matches!(r, Ok(Readback::Confirmed(_))) {
                    why.push(format!("monitor {} did not return to {}: {r:?}", m.uuid, ddc::input_name(m.home)));
                    next.push((m, d));
                }
            }
            left = next;
        }
        left.into_iter().map(|(m, _)| m.clone()).zip(why).collect()
    }

    fn stuck_text(stuck: &[(Monitor, String)]) -> String {
        stuck.iter().map(|(_, w)| w.as_str()).collect::<Vec<_>>().join("; ")
    }

    /// All the monitors switch or none stays switched: input stays here when one did not, so every one
    /// goes home, those that did switch too. Returns the result and the monitors whose return failed.
    fn away(t: &Target) -> (Result<(), String>, Vec<Monitor>) {
        // Every one is opened before any is written: one that cannot be reached fails the switch while
        // nothing has moved.
        let mut opened = Vec::new();
        for m in &t.monitors {
            match open(m) {
                Ok(d) => opened.push((m, d)),
                Err(e) => return (Err(e), Vec::new()),
            }
        }
        let ds: Vec<(&Monitor, &Display)> = opened.iter().map(|(m, d)| (*m, d)).collect();
        let results = set_all(&ds, |m| m.input);
        let failed: Vec<String> = ds
            .iter()
            .zip(&results)
            .filter(|(_, r)| !matches!(r, Ok(Readback::Confirmed(_))))
            .map(|((m, _), r)| format!("monitor {} did not switch to {}: {r:?}", m.uuid, ddc::input_name(m.input)))
            .collect();
        if failed.is_empty() {
            return (Ok(()), Vec::new());
        }
        let stuck = home_all(&ds);
        let back = if stuck.is_empty() { "all sent home".to_string() } else { stuck_text(&stuck) };
        (Err(format!("{}; {back}", failed.join("; "))), stuck.into_iter().map(|(m, _)| m).collect())
    }

    /// Every monitor that can be reached goes home, even when another cannot. One that already shows this
    /// Mac is left alone: selecting the input it shows makes some monitors blank for a moment.
    fn home(t: &Target) -> (Result<(), String>, Vec<Monitor>) {
        let mut opened = Vec::new();
        let mut stuck = Vec::new();
        for m in &t.monitors {
            match open(m) {
                Ok(d) if d.get(ddc::VCP_INPUT).is_ok_and(|r| r.current == m.home) => {}
                Ok(d) => opened.push((m, d)),
                Err(e) => stuck.push((m.clone(), e)),
            }
        }
        let ds: Vec<(&Monitor, &Display)> = opened.iter().map(|(m, d)| (*m, d)).collect();
        stuck.extend(home_all(&ds));
        if stuck.is_empty() {
            return (Ok(()), Vec::new());
        }
        (Err(stuck_text(&stuck)), stuck.into_iter().map(|(m, _)| m).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(uuid: &str) -> Monitor {
        Monitor { uuid: uuid.into(), input: 15, home: 18 }
    }
    fn t() -> Target {
        Target { monitors: vec![m("U"), m("V")] }
    }
    fn away() -> Cmd {
        Cmd::Away { peer: "pc".into(), target: t() }
    }
    fn home() -> Cmd {
        Cmd::Home { peer: "pc".into(), target: t() }
    }
    fn home_of(ms: &[&str]) -> Cmd {
        Cmd::Home { peer: "pc".into(), target: Target { monitors: ms.iter().map(|u| m(u)).collect() } }
    }
    fn ok_away() -> Done {
        Done::Away { peer: "pc".into(), result: Ok(()), out: vec![] }
    }
    fn ok_home() -> Done {
        Done::Home { peer: "pc".into(), result: Ok(()), out: vec![] }
    }
    fn switched() -> Kvm {
        let mut k = Kvm::new();
        k.request("pc", Some(t()), WayBack::Open);
        k.done(ok_away());
        k
    }

    #[test]
    fn a_peer_without_monitors_is_a_plain_input_switch() {
        let mut k = Kvm::new();
        assert_eq!(k.request("pc", None, WayBack::Open), vec![Step::SwitchInput("pc".into(), WayBack::Open)]);
        assert_eq!(k.request("pc", Some(Target { monitors: vec![] }), WayBack::Open), vec![Step::SwitchInput("pc".into(), WayBack::Open)], "no monitor chosen");
        assert_eq!(k.phase(), &Phase::Idle);
    }

    #[test]
    fn the_monitors_move_first_and_input_follows_only_once_they_are_confirmed() {
        let mut k = Kvm::new();
        assert_eq!(k.request("pc", Some(t()), WayBack::Open), vec![Step::Ddc(away())], "one command for all the monitors");
        assert_eq!(k.tick(None, 0), vec![], "input is still here while the monitors switch");
        assert_eq!(k.done(ok_away()), vec![Step::SwitchInput("pc".into(), WayBack::Open)]);
        assert_eq!(k.tick(Some("pc"), 0), vec![]);
        assert!(matches!(k.phase(), Phase::Away { .. }));
    }

    #[test]
    fn the_way_back_planned_at_the_request_goes_with_the_input() {
        let mut k = Kvm::new();
        k.request("pc", Some(t()), WayBack::Closed);
        // A second click while the first is in flight is refused and plans nothing.
        assert_eq!(k.request("pc", Some(t()), WayBack::Open), vec![]);
        assert_eq!(k.done(ok_away()), vec![Step::SwitchInput("pc".into(), WayBack::Closed)]);
        // The next switch plans its own.
        k.tick(None, 0);
        k.done(Done::Home { peer: "pc".into(), result: Ok(()), out: vec![] });
        k.request("pc", Some(t()), WayBack::Open);
        assert_eq!(k.done(ok_away()), vec![Step::SwitchInput("pc".into(), WayBack::Open)]);
        // Moving no monitor, nothing is out of sight: the way back stays open.
        assert_eq!(Kvm::new().request("pc", None, WayBack::Closed), vec![Step::SwitchInput("pc".into(), WayBack::Open)]);
    }

    #[test]
    fn monitors_that_did_not_switch_keep_input_here() {
        let mut k = Kvm::new();
        k.request("pc", Some(t()), WayBack::Open);
        assert_eq!(k.done(Done::Away { peer: "pc".into(), result: Err("no".into()), out: vec![] }), vec![]);
        assert_eq!(k.phase(), &Phase::Idle);
        assert_eq!(k.last_error.as_deref(), Some("no"));
        assert_eq!(k.out(), None, "all came home");
    }

    #[test]
    fn any_end_of_the_session_sends_every_monitor_home_once() {
        let mut k = switched();
        assert_eq!(k.tick(None, 0), vec![Step::Ddc(home())]);
        assert_eq!(k.tick(None, 0), vec![], "only once");
        assert_eq!(k.done(ok_home()), vec![]);
        assert_eq!(k.phase(), &Phase::Idle);
    }

    #[test]
    fn driving_another_peer_also_ends_the_session() {
        let mut k = switched();
        assert_eq!(k.tick(Some("other"), 0), vec![Step::Ddc(home())]);
    }

    #[test]
    fn a_switch_input_that_did_not_take_is_caught_on_the_next_tick() {
        // The worker ran SwitchInput but the link was down, so input never left.
        let mut k = switched();
        assert_eq!(k.tick(None, 0), vec![Step::Ddc(home())]);
    }

    #[test]
    fn stopping_mid_switch_queues_home_behind_the_away() {
        let mut k = Kvm::new();
        k.request("pc", Some(t()), WayBack::Open);
        assert_eq!(k.stop(), vec![Step::Ddc(home())]);
        assert_eq!(k.done(ok_away()), vec![], "the overtaken Away must not hand over input");
        assert_eq!(k.done(ok_home()), vec![]);
        assert_eq!(k.phase(), &Phase::Idle);
    }

    #[test]
    fn a_request_while_busy_is_refused_and_said_so() {
        let mut k = switched();
        assert_eq!(k.request("pc", Some(t()), WayBack::Open), vec![]);
        assert!(k.last_error.as_deref().unwrap_or("").starts_with("busy"));
        assert!(matches!(k.phase(), Phase::Away { .. }));
    }

    #[test]
    fn the_monitors_count_as_out_from_the_request_until_their_return_lands() {
        let mut k = Kvm::new();
        assert_eq!(k.out(), None);
        k.request("pc", Some(t()), WayBack::Open);
        assert_eq!(k.out(), Some(("pc".into(), t().monitors)), "switching: they may already have moved");
        k.done(ok_away());
        k.tick(None, 0);
        assert_eq!(k.out(), Some(("pc".into(), t().monitors)), "returning: not home until confirmed");
        k.done(ok_home());
        assert_eq!(k.out(), None);
    }

    /// A monitor whose return failed may still show the peer: it stays out, the next return takes it along,
    /// and so does the stop path, until one confirms it.
    #[test]
    fn a_monitor_whose_return_failed_stays_out_until_a_later_return_lands() {
        let mut k = switched();
        k.tick(None, 0);
        k.done(Done::Home { peer: "pc".into(), result: Err("V stuck".into()), out: vec![m("V")] });
        assert_eq!(k.phase(), &Phase::Idle);
        assert_eq!(k.last_error.as_deref(), Some("V stuck"));
        assert_eq!(k.out(), Some(("pc".into(), vec![m("V")])), "kvm.toml keeps naming it for the window's fallback");
        assert_eq!(k.stop(), vec![Step::Ddc(home_of(&["V"]))], "the stop path tries it again");
        k.done(ok_home());
        assert_eq!(k.out(), None);

        let mut k = switched();
        k.tick(None, 0);
        k.done(Done::Home { peer: "pc".into(), result: Err("V stuck".into()), out: vec![m("V")] });
        k.request("pc", Some(Target::one(m("U"))), WayBack::Open);
        k.done(ok_away());
        assert_eq!(k.out(), Some(("pc".into(), vec![m("U"), m("V")])));
        assert_eq!(k.tick(None, 0), vec![Step::Ddc(home_of(&["U", "V"]))], "the next return takes it along");
        k.done(ok_home());
        assert_eq!(k.out(), None);
    }

    #[test]
    fn an_input_only_session_ending_takes_a_stranded_monitor_home() {
        let mut k = switched();
        k.tick(None, 0);
        k.done(Done::Home { peer: "pc".into(), result: Err("V stuck".into()), out: vec![m("V")] });
        assert_eq!(k.request("air", None, WayBack::Open), vec![Step::SwitchInput("air".into(), WayBack::Open)]);
        assert_eq!(k.tick(Some("air"), 0), vec![], "nothing while the session runs");
        assert_eq!(k.tick(None, 0), vec![Step::Ddc(home_of(&["V"]))], "the person is back here");
        assert_eq!(k.tick(None, 0), vec![], "once");
        k.done(ok_home());
        assert_eq!(k.out(), None);
        assert_eq!(Kvm::new().tick(None, 0), vec![], "nothing stranded: nothing to do");
    }

    #[test]
    fn a_back_from_either_side_sends_every_configured_monitor_home_once_idle() {
        let mut k = switched();
        assert_eq!(k.bring_home("pc", t().monitors), None, "a session is out: asked again once it is over");
        k.tick(None, 0);
        k.done(ok_home());
        assert_eq!(k.bring_home("pc", t().monitors), Some(vec![Step::Ddc(home())]));
        assert_eq!(k.out(), Some(("pc".into(), t().monitors)), "counted out until the return lands");
        k.done(ok_home());
        assert_eq!(k.out(), None);
        let mut k = Kvm::new();
        assert_eq!(k.bring_home("pc", vec![]), Some(vec![]), "a peer without monitors: nothing to do");
        k.adopt("pc".into(), vec![m("V")]);
        assert_eq!(k.bring_home("pc", vec![m("U")]), Some(vec![Step::Ddc(home_of(&["U", "V"]))]), "a stranded one goes along");
    }

    #[test]
    fn monitors_an_earlier_process_left_out_are_taken_home_by_this_one() {
        let mut k = Kvm::new();
        k.adopt("pc".into(), vec![m("V")]);
        assert_eq!(k.out(), Some(("pc".into(), vec![m("V")])));
        assert_eq!(k.return_stranded(), vec![Step::Ddc(home_of(&["V"]))], "at start, not only at a later session's end");
        k.done(ok_home());
        assert_eq!(k.return_stranded(), vec![], "once home, nothing more");
        let mut k = Kvm::new();
        k.adopt("pc".into(), vec![m("V")]);
        assert_eq!(k.stop(), vec![Step::Ddc(home_of(&["V"]))]);
        let mut k = Kvm::new();
        k.adopt("pc".into(), vec![]);
        assert_eq!(k.out(), None, "an empty list adopts nothing");
    }

    #[test]
    fn a_monitor_that_switches_again_is_no_longer_stranded() {
        let mut k = switched();
        k.tick(None, 0);
        k.done(Done::Home { peer: "pc".into(), result: Err("V stuck".into()), out: vec![m("V")] });
        k.request("pc", Some(t()), WayBack::Open);
        k.done(ok_away());
        assert_eq!(k.out(), Some(("pc".into(), t().monitors)), "listed once");
    }

    #[test]
    fn a_test_shows_the_input_for_its_time_never_moves_input_and_comes_home() {
        let mut k = Kvm::new();
        let one = |c: fn(String, Target) -> Cmd| c("pc".into(), Target::one(m("U")));
        assert_eq!(k.trial("pc", Target::one(m("U")), 8000), vec![Step::Ddc(one(|peer, target| Cmd::Away { peer, target }))]);
        assert_eq!(k.done(ok_away()), vec![], "no input handover in a test");
        assert_eq!(k.tick(None, 1000), vec![], "the clock starts on the first tick");
        assert_eq!(k.tick(None, 8999), vec![]);
        assert_eq!(k.tick(None, 9000), vec![Step::Ddc(one(|peer, target| Cmd::Home { peer, target }))]);
        assert_eq!(k.out(), Some(("pc".into(), vec![m("U")])));
        k.done(ok_home());
        assert_eq!(k.phase(), &Phase::Idle);
        assert_eq!(k.last_trial, Some(TrialResult { peer: "pc".into(), monitors: vec![m("U")], result: Ok(()) }));
    }

    #[test]
    fn a_test_of_several_monitors_shows_them_together_and_reports_on_all() {
        let mut k = Kvm::new();
        assert_eq!(k.trial("pc", t(), 8000), vec![Step::Ddc(away())], "one switch for every monitor");
        k.done(ok_away());
        k.tick(None, 0);
        assert_eq!(k.out(), Some(("pc".into(), t().monitors)), "both are out while shown");
        assert_eq!(k.tick(None, 8000), vec![Step::Ddc(home())], "and go home together");
        k.done(ok_home());
        assert_eq!(k.last_trial, Some(TrialResult { peer: "pc".into(), monitors: t().monitors, result: Ok(()) }));

        // One of them not confirmed home fails the test, whichever it is.
        for stuck in ["U", "V"] {
            let mut k = Kvm::new();
            k.trial("pc", t(), 0);
            k.done(ok_away());
            k.tick(None, 0);
            k.done(Done::Home { peer: "pc".into(), result: Err(format!("{stuck} stuck")), out: vec![m(stuck)] });
            assert_eq!(k.last_trial.map(|r| r.result), Some(Err(format!("{stuck} stuck"))), "{stuck}");
        }
    }

    #[test]
    fn a_test_whose_monitor_did_not_switch_reports_it() {
        let mut k = Kvm::new();
        k.trial("pc", Target::one(m("U")), 8000);
        k.done(Done::Away { peer: "pc".into(), result: Err("mismatch".into()), out: vec![] });
        assert_eq!(k.phase(), &Phase::Idle);
        assert_eq!(k.last_trial.map(|r| r.result), Some(Err("mismatch".into())));
    }

    #[test]
    fn a_test_that_also_returns_a_stranded_monitor_reports_on_its_own_monitor() {
        let mut k = switched();
        k.tick(None, 0);
        k.done(Done::Home { peer: "pc".into(), result: Err("V stuck".into()), out: vec![m("V")] });
        k.trial("pc", Target::one(m("U")), 0);
        k.done(ok_away());
        k.tick(None, 0);
        k.done(Done::Home { peer: "pc".into(), result: Err("V stuck".into()), out: vec![m("V")] });
        assert_eq!(k.last_trial.as_ref().map(|r| r.result.clone()), Some(Ok(())), "U came back");
        assert_eq!(k.out(), Some(("pc".into(), vec![m("V")])));
    }

    #[test]
    fn a_stop_mid_test_sends_the_monitor_home() {
        let mut k = Kvm::new();
        k.trial("pc", Target::one(m("U")), 8000);
        k.done(ok_away());
        assert_eq!(k.stop(), vec![Step::Ddc(home_of(&["U"]))]);
    }

    #[test]
    fn a_session_switch_never_reports_as_a_test() {
        let mut k = switched();
        k.tick(None, 0);
        k.done(ok_home());
        assert_eq!(k.last_trial, None);
        assert!(k.trial("pc", Target::one(m("U")), 1).len() == 1 && k.request("pc", Some(t()), WayBack::Open).is_empty(), "one at a time");
    }

    #[test]
    fn a_failed_switch_whose_return_did_not_confirm_tries_those_monitors_again_and_they_stay_out_until_then() {
        for start in [|k: &mut Kvm| k.request("pc", Some(t()), WayBack::Open), |k: &mut Kvm| k.trial("pc", Target::one(m("V")), 8000)] {
            let mut k = Kvm::new();
            start(&mut k);
            assert_eq!(k.done(Done::Away { peer: "pc".into(), result: Err("no".into()), out: vec![m("V")] }), vec![Step::Ddc(home_of(&["V"]))], "only the one not home");
            assert_eq!(k.out(), Some(("pc".into(), vec![m("V")])), "the stop path and the window's fallback still see it");
            assert_eq!(k.stop(), vec![], "already going home");
            k.done(ok_home());
            assert_eq!(k.phase(), &Phase::Idle);
            assert_eq!(k.out(), None);
            assert_eq!(k.last_error.as_deref().or(k.last_trial.as_ref().and_then(|r| r.result.as_ref().err().map(String::as_str))), Some("no"), "the failure stays reported");
        }
    }

    #[test]
    fn a_click_is_not_queued_behind_checks() {
        let probe = |p: &str| Cmd::Probe { peer: p.into(), monitor: m("U") };
        let mut q = std::collections::VecDeque::from([probe("a"), Cmd::Inspect { uuid: "U".into() }, away(), probe("b"), home()]);
        assert_eq!(next_cmd(&mut q), Some(away()));
        assert_eq!(next_cmd(&mut q), Some(home()));
        assert_eq!(next_cmd(&mut q), Some(probe("a")), "the rest in order");
        assert_eq!(q.len(), 2);
    }
}
