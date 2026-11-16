//! Monitors that switch along with a session started from the menu. The button first moves the chosen
//! monitors to the peer's inputs and only then hands over the keyboard and mouse; when that session
//! ends by any path (the peer's own "back" button, the chord, the edge, a dropped link, a display
//! topology change, the worker stopping), the monitors go back to this Mac's inputs.
//!
//! `Kvm` is the policy and holds no OS state.

use crate::forward::WayBack;
use crate::ddc::{self, DdcError, Readback, Reply};
use crate::status::{CheckState, InspectStatus, MonitorCheck};
use std::collections::HashMap;
use std::fmt::Debug;
use std::time::Duration;

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
    Probe { uuid: String, check: crate::status::MonitorCheck },
    Inspect(crate::status::InspectStatus),
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

/// A display handle stays on its platform DDC thread. The only I/O here is through this trait.
pub trait DdcDisplay {
    fn get(&self, vcp: u8) -> Result<Reply, DdcError>;
    fn set(&self, vcp: u8, value: u16) -> Result<(), DdcError>;
    fn capabilities(&self) -> Result<String, DdcError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenFailure { NotFound, NoChannel, Other }

pub trait DdcOpener {
    type Display: DdcDisplay;
    type Error: Debug;
    fn open(&mut self, uuid: &str) -> Result<Self::Display, Self::Error>;
    fn failure(&self, error: &Self::Error) -> OpenFailure;
    fn local_name(&self) -> &'static str;
    fn local_name_possessive(&self) -> &'static str;
    fn no_channel_reason(&self) -> &'static str;
    fn retain_away_handles(&self) -> bool { false }
}

/// Shared DDC command flow. A caller injects pauses so hardware tests never wait or write VCP.
pub struct DdcRunner<O: DdcOpener> {
    opener: O,
    caps: HashMap<String, Option<Vec<u16>>>,
    held: HashMap<String, O::Display>,
    pause: fn(Duration),
}

impl<O: DdcOpener> DdcRunner<O> {
    pub fn new(opener: O, pause: fn(Duration)) -> Self {
        Self { opener, caps: HashMap::new(), held: HashMap::new(), pause }
    }

    pub fn run(&mut self, cmd: Cmd) -> Done {
        match cmd {
            Cmd::Away { peer, target } => {
                let (result, out) = self.away(&target);
                Done::Away { peer, result, out }
            }
            Cmd::Home { peer, target } => {
                let (result, out) = self.home(&target);
                Done::Home { peer, result, out }
            }
            Cmd::Probe { peer, monitor } => {
                let (state, reason) = self.probe(&monitor);
                Done::Probe { uuid: monitor.uuid, check: MonitorCheck { peer, state, reason } }
            }
            Cmd::Inspect { uuid } => Done::Inspect(self.inspect(uuid)),
        }
    }

    fn inspect(&mut self, uuid: String) -> InspectStatus {
        let d = match self.opener.open(&uuid) {
            Ok(d) => d,
            Err(e) => return InspectStatus { uuid, inputs: None, current: None, reason: Some(format!("cannot open the monitor: {e:?}")) },
        };
        let mut reasons = Vec::new();
        let current = ddc::confirm(|| d.get(ddc::VCP_INPUT).map(|r| r.current), u16::MAX, 3, || (self.pause)(ddc::CONFIRM_INTERVAL / 4));
        let current = match current {
            Readback::Mismatch(v) => Some(v),
            other => { reasons.push(format!("current input unreadable: {other:?}")); None }
        };
        if !self.caps.contains_key(&uuid) {
            match d.capabilities() {
                Ok(s) => { self.caps.insert(uuid.clone(), ddc::input_sources(&s)); }
                Err(e) => reasons.push(format!("capabilities unreadable: {e:?}")),
            }
        }
        let inputs = self.caps.get(&uuid).cloned().flatten();
        InspectStatus { uuid, inputs, current, reason: (!reasons.is_empty()).then(|| reasons.join("; ")) }
    }

    fn probe(&mut self, m: &Monitor) -> (CheckState, Option<String>) {
        let d = match self.opener.open(&m.uuid) {
            Ok(d) => d,
            Err(e) => return match self.opener.failure(&e) {
                OpenFailure::NotFound => (CheckState::Problem, Some(format!("monitor {} is not connected to this {}", m.uuid, self.opener.local_name()))),
                OpenFailure::NoChannel => (CheckState::Problem, Some(self.opener.no_channel_reason().into())),
                OpenFailure::Other => (CheckState::Unknown, Some(format!("cannot open the monitor: {e:?}"))),
            },
        };
        let mut last = None;
        let answered = (0..3).any(|i| {
            if i > 0 { (self.pause)(ddc::CONFIRM_INTERVAL / 4); }
            d.get(ddc::VCP_INPUT).map_err(|e| last = Some(e)).is_ok()
        });
        if !answered { return (CheckState::Unknown, Some(format!("the monitor does not answer DDC: {last:?}"))); }
        if !self.caps.contains_key(&m.uuid) {
            match d.capabilities() {
                Ok(s) => { self.caps.insert(m.uuid.clone(), ddc::input_sources(&s)); }
                Err(e) => return (CheckState::Unlisted, Some(format!("the monitor's capabilities could not be read: {e:?}"))),
            }
        }
        match self.caps.get(&m.uuid).cloned().flatten() {
            Some(list) if !list.is_empty() => {
                let names = list.iter().map(|v| ddc::input_name(*v)).collect::<Vec<_>>().join(", ");
                for (what, v) in [("the peer's", m.input), (self.opener.local_name_possessive(), m.home)] {
                    if !list.contains(&v) {
                        return (CheckState::Problem, Some(format!("the monitor has no input {} ({what}); it has {names}", ddc::input_name(v))));
                    }
                }
                (CheckState::Ok, None)
            }
            _ => (CheckState::Unlisted, Some("the monitor does not list its inputs".into())),
        }
    }

    fn open_switch(&mut self, m: &Monitor) -> Result<O::Display, String> {
        self.opener.open(&m.uuid).map_err(|e| format!("monitor {}: {e:?}", m.uuid))
    }

    /// Write all displays before any readback; slow monitors get the full shared confirmation window.
    fn set_all(&self, ds: &[(&Monitor, &O::Display)], value: fn(&Monitor) -> u16) -> Vec<Result<Readback, DdcError>> {
        let sets: Vec<Result<(), DdcError>> = ds.iter().map(|(m, d)| d.set(ddc::VCP_INPUT, value(m))).collect();
        let pending: Vec<usize> = (0..ds.len()).filter(|&i| sets[i].is_ok()).collect();
        let wants: Vec<u16> = pending.iter().map(|&i| value(ds[i].0)).collect();
        let reads = ddc::confirm_all(|k| ds[pending[k]].1.get(ddc::VCP_INPUT).map(|r| r.current), &wants, ddc::CONFIRM_ATTEMPTS, || (self.pause)(ddc::CONFIRM_INTERVAL));
        let mut reads = reads.into_iter();
        sets.into_iter().map(|s| s.map(|()| reads.next().expect("one read per write that went out"))).collect()
    }

    fn home_all(&mut self, opened: &mut [(&Monitor, O::Display)], retained: &[bool]) -> Vec<(Monitor, String)> {
        let mut left: Vec<usize> = (0..opened.len()).collect();
        let mut why = Vec::new();
        for round in 0..2 {
            if left.is_empty() { break; }
            let ds: Vec<(&Monitor, &O::Display)> = left.iter().map(|&i| (opened[i].0, &opened[i].1)).collect();
            let results = self.set_all(&ds, |m| m.home);
            drop(ds);
            why.clear();
            let mut next = Vec::new();
            for (&i, r) in left.iter().zip(results) {
                let m = opened[i].0;
                if !matches!(r, Ok(Readback::Confirmed(_))) {
                    why.push(format!("monitor {} did not return to {}: {r:?}", m.uuid, ddc::input_name(m.home)));
                    next.push(i);
                }
            }
            if round == 0 {
                for &i in &next {
                    if retained[i] {
                        if let Ok(fresh) = self.opener.open(&opened[i].0.uuid) { opened[i].1 = fresh; }
                    }
                }
            }
            left = next;
        }
        left.into_iter().map(|i| opened[i].0.clone()).zip(why).collect()
    }

    fn stuck_text(stuck: &[(Monitor, String)]) -> String {
        stuck.iter().map(|(_, w)| w.as_str()).collect::<Vec<_>>().join("; ")
    }

    fn away(&mut self, t: &Target) -> (Result<(), String>, Vec<Monitor>) {
        let mut opened = Vec::new();
        for m in &t.monitors {
            match self.open_switch(m) {
                Ok(d) => opened.push((m, d)),
                Err(e) => return (Err(e), Vec::new()),
            }
        }
        let ds: Vec<(&Monitor, &O::Display)> = opened.iter().map(|(m, d)| (*m, d)).collect();
        let results = self.set_all(&ds, |m| m.input);
        let failed: Vec<String> = ds.iter().zip(&results)
            .filter(|(_, r)| !matches!(r, Ok(Readback::Confirmed(_))))
            .map(|((m, _), r)| format!("monitor {} did not switch to {}: {r:?}", m.uuid, ddc::input_name(m.input)))
            .collect();
        if failed.is_empty() {
            if self.opener.retain_away_handles() {
                for (m, d) in opened { self.held.insert(m.uuid.clone(), d); }
            }
            return (Ok(()), Vec::new());
        }
        drop(ds);
        let retained = vec![false; opened.len()];
        let stuck = self.home_all(&mut opened, &retained);
        let back = if stuck.is_empty() { "all sent home".to_string() } else { Self::stuck_text(&stuck) };
        let out: Vec<Monitor> = stuck.into_iter().map(|(m, _)| m).collect();
        if self.opener.retain_away_handles() {
            for (m, d) in opened {
                if out.iter().any(|s| s.uuid == m.uuid) { self.held.insert(m.uuid.clone(), d); }
            }
        }
        (Err(format!("{}; {back}", failed.join("; "))), out)
    }

    fn home(&mut self, t: &Target) -> (Result<(), String>, Vec<Monitor>) {
        let mut opened = Vec::new();
        let mut retained = Vec::new();
        let mut stuck = Vec::new();
        for m in &t.monitors {
            let old = self.held.remove(&m.uuid);
            let was_retained = old.is_some();
            let display = old.map_or_else(|| self.open_switch(m), Ok);
            match display {
                Ok(d) if d.get(ddc::VCP_INPUT).is_ok_and(|r| r.current == m.home) => {}
                Ok(d) => { opened.push((m, d)); retained.push(was_retained); }
                Err(e) => stuck.push((m.clone(), e)),
            }
        }
        let failed = self.home_all(&mut opened, &retained);
        if self.opener.retain_away_handles() {
            for (m, d) in opened {
                if failed.iter().any(|(s, _)| s.uuid == m.uuid) { self.held.insert(m.uuid.clone(), d); }
            }
        }
        stuck.extend(failed);
        if stuck.is_empty() { return (Ok(()), Vec::new()); }
        (Err(Self::stuck_text(&stuck)), stuck.into_iter().map(|(m, _)| m).collect())
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

#[cfg(test)]
mod ddc_tests {
    use super::*;
    use std::cell::RefCell;
    use std::sync::{Arc, Mutex};

    thread_local! { static PAUSES: RefCell<Vec<Duration>> = const { RefCell::new(Vec::new()) }; }
    fn count_pause(duration: Duration) { PAUSES.with(|pauses| pauses.borrow_mut().push(duration)); }

    #[derive(Default)]
    struct State {
        current: u16,
        wanted: u16,
        failed_reads: usize,
        reads: usize,
        capability_reads: usize,
        never_away: bool,
        never_home: bool,
        caps: &'static str,
        writes: Vec<u16>,
    }
    #[derive(Clone)]
    struct FakeDisplay(Arc<Mutex<State>>);
    impl DdcDisplay for FakeDisplay {
        fn get(&self, _: u8) -> Result<Reply, DdcError> {
            let mut s = self.0.lock().unwrap();
            s.reads += 1;
            if s.failed_reads > 0 {
                s.failed_reads -= 1;
                return Err(DdcError::Io(-1));
            }
            if !((s.never_away && s.wanted == 15) || (s.never_home && s.wanted == 18)) { s.current = s.wanted; }
            Ok(Reply { current: s.current, max: 21 })
        }
        fn set(&self, _: u8, value: u16) -> Result<(), DdcError> {
            let mut s = self.0.lock().unwrap();
            s.writes.push(value);
            s.wanted = value;
            Ok(())
        }
        fn capabilities(&self) -> Result<String, DdcError> {
            let mut s = self.0.lock().unwrap();
            s.capability_reads += 1;
            Ok(s.caps.into())
        }
    }
    struct FakeOpener(HashMap<String, FakeDisplay>);
    impl DdcOpener for FakeOpener {
        type Display = FakeDisplay;
        type Error = &'static str;
        fn open(&mut self, id: &str) -> Result<FakeDisplay, Self::Error> {
            self.0.get(id).cloned().ok_or(match id { "no-channel" => "no-channel", "other" => "other", _ => "missing" })
        }
        fn failure(&self, error: &Self::Error) -> OpenFailure {
            match *error { "missing" => OpenFailure::NotFound, "no-channel" => OpenFailure::NoChannel, _ => OpenFailure::Other }
        }
        fn local_name(&self) -> &'static str { "Mac" }
        fn local_name_possessive(&self) -> &'static str { "this Mac's" }
        fn no_channel_reason(&self) -> &'static str { "no channel" }
    }
    fn display(failed_reads: usize, never_away: bool) -> FakeDisplay {
        FakeDisplay(Arc::new(Mutex::new(State { current: 18, wanted: 18, failed_reads, never_away, caps: "(vcp(60(0f 12)))", ..Default::default() })))
    }
    fn monitor(id: &str) -> Monitor { Monitor { uuid: id.into(), input: 15, home: 18 } }
    fn runner(displays: &[(&str, &FakeDisplay)]) -> DdcRunner<FakeOpener> {
        DdcRunner::new(FakeOpener(displays.iter().map(|(id, d)| ((*id).into(), (*d).clone())).collect()), |_| {})
    }

    #[test]
    fn delayed_readback_uses_the_full_confirmation_window() {
        let d = display(5, false);
        let mut r = runner(&[("A", &d)]);
        let Done::Away { result, .. } = r.run(Cmd::Away { peer: "p".into(), target: Target::one(monitor("A")) }) else { panic!("away") };
        assert_eq!(result, Ok(()));
        assert_eq!(d.0.lock().unwrap().writes, [15]);
    }

    #[test]
    fn confirmation_uses_exactly_twelve_reads_and_eleven_one_second_pauses() {
        for (failed_reads, confirmed) in [(11, true), (12, false)] {
            PAUSES.with(|pauses| pauses.borrow_mut().clear());
            let d = display(failed_reads, false);
            let mut r = runner(&[("A", &d)]);
            r.pause = count_pause;
            let monitor = monitor("A");
            let result = r.set_all(&[(&monitor, &d)], |m| m.input);
            assert_eq!(matches!(result.as_slice(), [Ok(Readback::Confirmed(15))]), confirmed);
            assert_eq!(d.0.lock().unwrap().reads, 12);
            PAUSES.with(|pauses| assert_eq!(pauses.borrow().as_slice(), &[ddc::CONFIRM_INTERVAL; 11]));
        }
    }

    #[test]
    fn one_unconfirmed_monitor_sends_every_monitor_home() {
        let a = display(0, false);
        let b = display(0, true);
        let mut r = runner(&[("A", &a), ("B", &b)]);
        let Done::Away { result, out, .. } = r.run(Cmd::Away { peer: "p".into(), target: Target { monitors: vec![monitor("A"), monitor("B")] } }) else { panic!("away") };
        assert!(result.unwrap_err().contains("all sent home"));
        assert!(out.is_empty());
        assert_eq!(a.0.lock().unwrap().writes, [15, 18]);
        assert_eq!(b.0.lock().unwrap().writes, [15, 18]);
    }

    #[test]
    fn one_unopenable_monitor_prevents_every_write() {
        let a = display(0, false);
        let mut r = runner(&[("A", &a)]);
        let Done::Away { result, .. } = r.run(Cmd::Away { peer: "p".into(), target: Target { monitors: vec![monitor("A"), monitor("missing")] } }) else { panic!("away") };
        assert!(result.is_err());
        assert!(a.0.lock().unwrap().writes.is_empty());
    }

    #[test]
    fn home_does_not_write_an_input_that_is_already_home() {
        let a = display(0, false);
        let mut r = runner(&[("A", &a)]);
        let Done::Home { result, .. } = r.run(Cmd::Home { peer: "p".into(), target: Target::one(monitor("A")) }) else { panic!("home") };
        assert_eq!(result, Ok(()));
        assert!(a.0.lock().unwrap().writes.is_empty());
    }

    #[test]
    fn two_unconfirmed_home_rounds_report_the_monitor_out() {
        let a = display(0, false);
        {
            let mut s = a.0.lock().unwrap();
            s.current = 15;
            s.wanted = 15;
            s.never_home = true;
        }
        let mut r = runner(&[("A", &a)]);
        let Done::Home { result, out, .. } = r.run(Cmd::Home { peer: "p".into(), target: Target::one(monitor("A")) }) else { panic!("home") };
        assert!(result.is_err());
        assert_eq!(out, vec![monitor("A")]);
        let s = a.0.lock().unwrap();
        assert_eq!(s.writes, [18, 18]);
        assert_eq!(s.reads, 1 + 2 * ddc::CONFIRM_ATTEMPTS as usize);
    }

    #[test]
    fn probe_distinguishes_open_failures_and_failed_reads() {
        let unreadable = display(3, false);
        let mut r = runner(&[("unreadable", &unreadable)]);
        for (id, state) in [
            ("missing", CheckState::Problem),
            ("no-channel", CheckState::Problem),
            ("other", CheckState::Unknown),
            ("unreadable", CheckState::Unknown),
        ] {
            let Done::Probe { check, .. } = r.run(Cmd::Probe { peer: "p".into(), monitor: monitor(id) }) else { panic!("probe") };
            assert_eq!(check.state, state, "{id}");
            assert!(check.reason.is_some(), "{id}");
        }
        assert_eq!(unreadable.0.lock().unwrap().reads, 3);
        assert_eq!(unreadable.0.lock().unwrap().capability_reads, 0);
    }

    #[test]
    fn probe_caches_capabilities_and_reports_a_missing_input() {
        let listed = display(0, false);
        let absent = display(0, false);
        absent.0.lock().unwrap().caps = "(vcp(60(0f)))";
        let mut r = runner(&[("listed", &listed), ("absent", &absent)]);
        for _ in 0..2 {
            let Done::Probe { check, .. } = r.run(Cmd::Probe { peer: "p".into(), monitor: monitor("listed") }) else { panic!("probe") };
            assert_eq!(check.state, CheckState::Ok);
        }
        assert_eq!(listed.0.lock().unwrap().capability_reads, 1);
        let Done::Probe { check, .. } = r.run(Cmd::Probe { peer: "p".into(), monitor: monitor("absent") }) else { panic!("probe") };
        assert_eq!(check.state, CheckState::Problem);
        assert!(check.reason.unwrap().contains("no input"));
        assert_eq!(absent.0.lock().unwrap().capability_reads, 1);
    }

    #[test]
    fn inspect_reads_the_current_input_three_times() {
        let a = display(0, false);
        let mut r = runner(&[("A", &a)]);
        let Done::Inspect(status) = r.run(Cmd::Inspect { uuid: "A".into() }) else { panic!("inspect") };
        assert_eq!(status.current, Some(18));
        assert_eq!(status.inputs, Some(vec![15, 18]));
        assert_eq!(a.0.lock().unwrap().reads, 3);
        assert_eq!(a.0.lock().unwrap().capability_reads, 1);
    }

    #[test]
    fn retained_handle_returns_a_display_that_vanished_after_switching() {
        struct Vanishing(FakeDisplay);
        impl DdcOpener for Vanishing {
            type Display = FakeDisplay;
            type Error = &'static str;
            fn open(&mut self, _: &str) -> Result<FakeDisplay, Self::Error> {
                if self.0.0.lock().unwrap().current == 15 { Err("not listed") } else { Ok(self.0.clone()) }
            }
            fn failure(&self, _: &Self::Error) -> OpenFailure { OpenFailure::NotFound }
            fn local_name(&self) -> &'static str { "PC" }
            fn local_name_possessive(&self) -> &'static str { "this PC's" }
            fn no_channel_reason(&self) -> &'static str { "no channel" }
            fn retain_away_handles(&self) -> bool { true }
        }
        let a = display(0, false);
        let mut r = DdcRunner::new(Vanishing(a.clone()), |_| {});
        let target = Target::one(monitor("A"));
        assert!(matches!(r.run(Cmd::Away { peer: "p".into(), target: target.clone() }), Done::Away { result: Ok(()), .. }));
        assert!(matches!(r.run(Cmd::Home { peer: "p".into(), target }), Done::Home { result: Ok(()), .. }));
        assert_eq!(a.0.lock().unwrap().writes, [15, 18]);
    }

    #[test]
    fn stale_retained_handle_reopens_once_before_the_second_home_round() {
        #[derive(Default)]
        struct Shared { opens: usize, current: u16, writes: Vec<(usize, u16)> }
        struct StaleDisplay { handle: usize, shared: Arc<Mutex<Shared>> }
        impl DdcDisplay for StaleDisplay {
            fn get(&self, _: u8) -> Result<Reply, DdcError> {
                Ok(Reply { current: self.shared.lock().unwrap().current, max: 21 })
            }
            fn set(&self, _: u8, value: u16) -> Result<(), DdcError> {
                let mut s = self.shared.lock().unwrap();
                s.writes.push((self.handle, value));
                if self.handle == 0 && value == 18 { return Err(DdcError::Io(-1)); }
                s.current = value;
                Ok(())
            }
            fn capabilities(&self) -> Result<String, DdcError> { Ok(String::new()) }
        }
        struct StaleOpener(Arc<Mutex<Shared>>);
        impl DdcOpener for StaleOpener {
            type Display = StaleDisplay;
            type Error = &'static str;
            fn open(&mut self, _: &str) -> Result<StaleDisplay, Self::Error> {
                let mut s = self.0.lock().unwrap();
                let handle = s.opens;
                s.opens += 1;
                Ok(StaleDisplay { handle, shared: self.0.clone() })
            }
            fn failure(&self, _: &Self::Error) -> OpenFailure { OpenFailure::Other }
            fn local_name(&self) -> &'static str { "PC" }
            fn local_name_possessive(&self) -> &'static str { "this PC's" }
            fn no_channel_reason(&self) -> &'static str { "no channel" }
            fn retain_away_handles(&self) -> bool { true }
        }
        let shared = Arc::new(Mutex::new(Shared { current: 18, ..Default::default() }));
        let mut runner = DdcRunner::new(StaleOpener(shared.clone()), |_| {});
        let target = Target::one(monitor("A"));
        assert!(matches!(runner.run(Cmd::Away { peer: "p".into(), target: target.clone() }), Done::Away { result: Ok(()), .. }));
        let Done::Home { result, out, .. } = runner.run(Cmd::Home { peer: "p".into(), target }) else { panic!("home") };
        assert_eq!(result, Ok(()));
        assert!(out.is_empty());
        let s = shared.lock().unwrap();
        assert_eq!(s.opens, 2, "one initial open and one stale-handle retry");
        assert_eq!(s.writes, [(0, 15), (0, 18), (1, 18)]);
    }
}
