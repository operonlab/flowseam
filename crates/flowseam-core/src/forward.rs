//! Forwarding state machine: which machine owns the keyboard and mouse right now.
//!
//! Pure logic. Time is injected (`now_ms`), OS effects go through [`InputSink`],
//! network sends go through [`Outbox`]. One owner drives it; local capture events,
//! cursor samples, peer messages and link up/down all come in as method calls.
//!
//! Modes:
//! - `Local`: input stays here. Dwell in a peer's exit strip (or the chord) hands over.
//! - `RemoteOut`: we grabbed local input and stream it to one peer.
//! - `RemoteIn`: a peer drives this machine; its messages are injected through the sink.
//! - `Relay`: a peer drives this machine through us into one of our peers.
//!
//! Edge convention: `Enter.edge` and `Leave.edge` both name an edge of the
//! *recipient's* screen: where the cursor appears on the recipient.
//!
//! Sessions are identified by `seq`: the controller bumps it on every `Enter`, and every
//! later message of that session carries it. A message whose seq is not the live
//! session's is dropped.

use std::collections::{BTreeSet, HashMap};

use crate::backend::{CapturedEvent, Disposition};
use crate::chord::ChordError;
use crate::config::{Config, GestureMap, KeyMap};
use crate::emergency::is_emergency;
use crate::geometry::{center_point, clamp_inside, clamp_to_displays, contains, entry_point, exit_frac, in_exit_strip};
use crate::gesture::{GestureKeys, Habit, Platform, Stroke};
use crate::mac_shortcuts::MacShortcuts;
use crate::modifiers::KeyTracker;
use crate::proto::{Edge, Gesture, ModifierSet, Msg, Point, Reason, Rect, CENTERED_PROTO_VERSION, CLOSED_PROTO_VERSION, GESTURE_PROTO_VERSION, PRIMARY_BUTTON};
use crate::switch::{decide, SwitchEvent, SwitchPolicy, SwitchState};

/// Effects on this machine. The real implementation wraps the OS layer; tests record.
pub trait InputSink {
    fn motion_rel(&mut self, dx: i32, dy: i32);
    fn button(&mut self, button: u8, down: bool);
    fn key(&mut self, hid: u16, down: bool, repeat: bool);
    fn wheel(&mut self, dx: i32, dy: i32);
    fn warp(&mut self, p: Point);
    /// `true`: swallow local input and freeze the cursor. Idempotent.
    fn set_grab(&mut self, on: bool);
    fn hide(&mut self);
    fn show(&mut self);
    /// `false` detaches the physical mouse from the cursor (injected motion only).
    fn set_association(&mut self, connected: bool);
    /// The primary button went down or up on this machine: pressed here while input stays local, or
    /// injected by the peer driving us. A file drag starts on a press and is dropped on a release.
    fn primary(&mut self, _down: bool) {}
    /// A file drag started on this machine during the current press of the primary button. While that
    /// button is held from a press made here, the cursor only leaves for a file drag: a window drag, a
    /// selection or a slider would otherwise be left waiting here for a release it never gets. The default
    /// (no drag support) keeps the old behaviour of leaving regardless.
    fn drag_started_here(&mut self) -> bool {
        true
    }
    /// The cursor is about to leave for `peer`, on every crossing, button held or not: a drag started here
    /// during the current press goes with it, and one received here but not dropped goes back. Called before
    /// the switch takes effect and before any held button is released here.
    fn drag_away(&mut self, _peer: &str) {}
}

/// Messages to a named peer. Must not block (channel send).
pub trait Outbox {
    fn send(&mut self, peer: &str, msg: Msg);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerSpec {
    pub name: String,
    /// Edge of our screen that leads to this peer.
    pub edge: Edge,
    /// How our keys land there; applied to every key this side sends that peer.
    pub keys: KeyMap,
    /// What our trackpad gestures do there, when it is a PC.
    pub gestures: GestureMap,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FwdConfig {
    pub peers: Vec<PeerSpec>,
    pub policy: SwitchPolicy,
    pub boundary_px: i32,
}

impl FwdConfig {
    pub fn from_config(cfg: &Config) -> Result<Self, ChordError> {
        Ok(Self {
            peers: cfg.peers.iter().map(|p| PeerSpec { name: p.name.clone(), edge: p.edge.into(), keys: p.keys, gestures: p.gestures }).collect(),
            policy: SwitchPolicy::from_config(&cfg.switch)?,
            boundary_px: cfg.switch.boundary_px,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Local,
    RemoteOut { peer: String, seq: u32, exit_pos: Point },
    RemoteIn { peer: String, seq: u32, edge: Edge, pos: Point, ret: SwitchState },
    Relay { from: String, from_seq: u32, edge: Edge, to: String, to_seq: u32, exit_pos: Point, keys: BTreeSet<u16> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchTo { Switched, UnknownPeer, NotLocal, PeerDown }

pub fn opposite(e: Edge) -> Edge {
    match e {
        Edge::Left => Edge::Right,
        Edge::Right => Edge::Left,
        Edge::Top => Edge::Bottom,
        Edge::Bottom => Edge::Top,
    }
}

const FRAC_MID: u16 = 0x8000;
const TAKEOVER_PX: u64 = 8;
const TAKEOVER_WINDOW_MS: u64 = 300;
/// A hold for a menu switch outlasts any real monitor switch (two monitors measured 3.4 s); one never
/// lifted lapses here, so the edges cannot stay dead.
const SWITCH_HOLD_LIMIT_MS: u64 = 15_000;

/// Where the cursor appears on the peer: next to the edge it came through, or, for a switch that came
/// through no edge (the menu, the chord), in the middle of the display on that side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Landing {
    Edge,
    Center,
}

/// Whether the edge toward the controller ends a session. `Closed` when a menu switch moved every display
/// of the controller to this session's machine: that edge then leads to a machine nobody can see.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WayBack {
    #[default]
    Open,
    Closed,
}

pub struct Forwarder<S: InputSink, O: Outbox> {
    cfg: FwdConfig,
    displays: Vec<Rect>,
    sink: S,
    out: O,
    mode: Mode,
    /// Physical keys on this machine (tap view).
    keys: KeyTracker,
    sw: SwitchState,
    up: BTreeSet<String>,
    next_seq: u32,
    /// Newest accepted `Enter.seq` per peer. Cleared when that peer's link drops.
    last_enter: HashMap<String, u32>,
    inj_keys: BTreeSet<u16>,
    inj_btn: BTreeSet<u8>,
    last_pos: Point,
    /// Shortcut translation for the peer being driven when its map is `KeyMap::Mac`; fresh per session.
    mac_keys: MacShortcuts,
    /// Protocol version each linked peer speaks, as negotiated.
    speaks: HashMap<String, u16>,
    /// Gestures as shortcuts, for this session's peer or for this machine; fresh per session.
    gesture_keys: GestureKeys,
    /// The primary button is down from a press made here while input stayed local.
    held_here: bool,
    /// The primary button is physically down (tap view, any mode).
    primary_down: bool,
    /// Control left with a press made here still down: this machine saw that press, and its release goes
    /// to the peer, so this machine owes itself a release when control comes back.
    crossed_holding: bool,
    takeover_motion: Option<(u64, u64)>,
    /// Since when a menu switch has held input here (`hold_for_switch`).
    switch_hold: Option<u64>,
    /// The way back of the session this machine is driven in, as its `Enter` set it; a relay keeps it.
    way_back: WayBack,
}

impl<S: InputSink, O: Outbox> Forwarder<S, O> {
    pub fn new(cfg: FwdConfig, displays: Vec<Rect>, sink: S, out: O) -> Self {
        let start = Point { x: 0, y: 0 };
        Self {
            cfg,
            displays,
            sink,
            out,
            mode: Mode::Local,
            keys: KeyTracker::new(),
            sw: SwitchState::new(start),
            up: BTreeSet::new(),
            next_seq: 0,
            last_enter: HashMap::new(),
            inj_keys: BTreeSet::new(),
            inj_btn: BTreeSet::new(),
            last_pos: start,
            mac_keys: MacShortcuts::new(),
            speaks: HashMap::new(),
            gesture_keys: GestureKeys::new(),
            held_here: false,
            primary_down: false,
            crossed_holding: false,
            takeover_motion: None,
            switch_hold: None,
            way_back: WayBack::Open,
        }
    }

    pub fn mode(&self) -> &Mode {
        &self.mode
    }
    pub fn sink(&self) -> &S {
        &self.sink
    }
    pub fn sink_mut(&mut self) -> &mut S {
        &mut self.sink
    }
    pub fn out(&self) -> &O {
        &self.out
    }
    /// What `switch_to(peer)` would do now, without doing it: a monitor switched ahead of the input
    /// must not be switched for a peer the input cannot reach.
    pub fn can_switch_to(&self, peer: &str) -> SwitchTo {
        if !self.cfg.peers.iter().any(|p| p.name == peer) {
            return SwitchTo::UnknownPeer;
        }
        if !matches!(self.mode, Mode::Local) {
            return SwitchTo::NotLocal;
        }
        if !self.up.contains(peer) {
            return SwitchTo::PeerDown;
        }
        SwitchTo::Switched
    }

    /// Hands input to `peer` now, as the switch chord would, from wherever the cursor is. `Closed` asks the
    /// peer to keep its edge back here from ending the session, for when it shows on every display here.
    pub fn switch_to(&mut self, peer: &str, way_back: WayBack) -> SwitchTo {
        let r = self.can_switch_to(peer);
        if let (SwitchTo::Switched, Some(idx)) = (r, self.cfg.peers.iter().position(|p| p.name == peer)) {
            self.enter_out(idx, self.last_pos, None, Landing::Center, way_back);
        }
        r
    }

    /// While a menu switch moves the monitors, input stays here and must not leave by itself: the edge or
    /// the chord would hand it to some peer, and the switch, finding this machine busy, would send the
    /// monitors back. `switch_to` still hands it over. The caller holds for as long as the monitors are
    /// on their way; a hold never lifted lapses after `SWITCH_HOLD_LIMIT_MS`.
    pub fn hold_for_switch(&mut self, on: bool, now_ms: u64) {
        if !on {
            self.switch_hold = None;
        } else if self.switch_hold.is_none() {
            self.switch_hold = Some(now_ms);
        }
    }

    fn holding(&self, now_ms: u64) -> bool {
        self.switch_hold.is_some_and(|since| now_ms.saturating_sub(since) < SWITCH_HOLD_LIMIT_MS)
    }

    /// Ends whatever session this machine is part of, the way the emergency chord does. Returns
    /// whether there was one.
    pub fn release_control(&mut self) -> bool {
        let active = !matches!(self.mode, Mode::Local);
        self.emergency();
        active
    }

    pub fn set_config(&mut self, cfg: FwdConfig) {
        if let Mode::Relay { from, from_seq, edge, to, to_seq, exit_pos, keys } = self.mode.clone() {
            let changed = |name: &str| self.cfg.peers.iter().find(|p| p.name == name) != cfg.peers.iter().find(|p| p.name == name);
            if changed(&to) || changed(&from) {
                if self.up.contains(&to) {
                    self.out.send(&to, Msg::Release { seq: to_seq, reason: Reason::TopologyChange });
                }
                self.resume_remote_in(from, from_seq, edge, exit_pos, keys);
            }
        }
        let active = match &self.mode {
            Mode::RemoteOut { peer, seq, .. } | Mode::RemoteIn { peer, seq, .. } => Some((peer.clone(), *seq)),
            Mode::Relay { .. } => None,
            Mode::Local => None,
        };
        if let Some((peer, seq)) = active {
            let old = self.cfg.peers.iter().find(|p| p.name == peer);
            let new = cfg.peers.iter().find(|p| p.name == peer);
            if old != new {
                let was_up = self.up.contains(&peer);
                let speaks = self.speaks.get(&peer).copied();
                // The link stays up, so the other end must hear that the switch is over: otherwise it keeps
                // driving us, or keeps the keys we injected held down.
                if was_up {
                    self.out.send(&peer, Msg::Release { seq, reason: Reason::TopologyChange });
                }
                self.drop_peer(&peer, "its settings changed");
                if was_up && new.is_some() {
                    // Same link, so the same protocol: only a relink would tell it again.
                    if let Some(v) = speaks {
                        self.speaks.insert(peer.clone(), v);
                    }
                    self.up.insert(peer);
                }
            }
        }
        self.up.retain(|peer| cfg.peers.iter().any(|p| &p.name == peer));
        self.last_enter.retain(|peer, _| cfg.peers.iter().any(|p| &p.name == peer));
        if self.cfg.policy != cfg.policy {
            self.sw = SwitchState::new(self.last_pos);
            if let Mode::RemoteIn { pos, ret, .. } = &mut self.mode {
                *ret = SwitchState::new(*pos);
            }
        }
        self.cfg = cfg;
    }

    /// A real topology change ends any session that spans it. `RemoteOut` releases the peer and brings the
    /// cursor home onto a display that still exists; `RemoteIn` pulls the virtual cursor back onto one.
    /// Callers refresh periodically, so an identical list is not a change.
    pub fn set_displays(&mut self, d: Vec<Rect>) {
        if d == self.displays {
            return;
        }
        self.displays = d;
        match self.mode.clone() {
            Mode::RemoteOut { peer, seq, exit_pos } => {
                self.out.send(&peer, Msg::Release { seq, reason: Reason::TopologyChange });
                let at = clamp_inside(&self.displays, exit_pos).unwrap_or(exit_pos);
                self.restore_local(at, "this machine's displays changed");
            }
            Mode::RemoteIn { pos, .. } => {
                if let Some(p) = clamp_inside(&self.displays, pos).filter(|p| *p != pos) {
                    if let Mode::RemoteIn { pos, .. } = &mut self.mode {
                        *pos = p;
                    }
                    self.sink.warp(p);
                }
            }
            Mode::Relay { from, from_seq, edge, to, to_seq, exit_pos, keys } => {
                self.out.send(&to, Msg::Release { seq: to_seq, reason: Reason::TopologyChange });
                let at = clamp_inside(&self.displays, exit_pos).unwrap_or(exit_pos);
                self.resume_remote_in(from, from_seq, edge, at, keys);
            }
            Mode::Local => {}
        }
    }

    pub fn link_up(&mut self, peer: &str) {
        self.up.insert(peer.to_string());
    }

    /// The protocol version the link to `peer` negotiated. Unknown means the oldest: a message it cannot
    /// decode would end the link.
    pub fn peer_speaks(&mut self, peer: &str, ver: u16) {
        self.speaks.insert(peer.to_string(), ver);
    }

    /// A dropped link frees whatever it held: RemoteOut returns local at once, RemoteIn
    /// releases every injected key and button.
    pub fn link_down(&mut self, peer: &str) {
        self.drop_peer(peer, "the link dropped");
    }

    fn drop_peer(&mut self, peer: &str, why: &str) {
        self.up.remove(peer);
        self.last_enter.remove(peer);
        self.speaks.remove(peer);
        match self.mode.clone() {
            Mode::RemoteOut { peer: p, exit_pos, .. } if p == peer => self.restore_local(exit_pos, why),
            Mode::RemoteIn { peer: p, .. } if p == peer => self.leave_remote_in(why),
            Mode::Relay { from, to, to_seq, exit_pos, .. } if from == peer => {
                self.out.send(&to, Msg::Release { seq: to_seq, reason: Reason::Shutdown });
                self.end_relay_local(exit_pos);
            }
            Mode::Relay { from, from_seq, edge, to, exit_pos, keys, .. } if to == peer => {
                self.resume_remote_in(from, from_seq, edge, exit_pos, keys);
            }
            _ => {}
        }
    }

    /// The grab for a switch could not be taken: tell the peer and return to Local.
    pub fn abort_switch(&mut self) {
        if let Mode::RemoteOut { peer, seq, exit_pos } = self.mode.clone() {
            let edge = self.cfg.peers.iter().find(|x| x.name == peer).map(|x| x.edge).unwrap_or(Edge::Right);
            self.out.send(&peer, Msg::Leave { seq, edge, frac: FRAC_MID });
            self.restore_local(exit_pos, "the cursor could not be grabbed");
        }
    }

    /// Cursor sample from the tap (Local mode drives dwell from it).
    pub fn on_cursor(&mut self, pos: Point, now_ms: u64) {
        self.last_pos = pos;
        if !matches!(self.mode, Mode::Local) {
            return;
        }
        let target = self.peer_at(pos);
        // Held without a file drag: the edge does not count, so the dwell starts only after the release.
        let held = target.is_some() && self.held_here && !self.sink.drag_started_here();
        let holding = self.holding(now_ms);
        let fire = decide(
            &self.cfg.policy,
            &mut self.sw,
            SwitchEvent::Cursor { pos, in_boundary: target.is_some() && !held && !holding },
            now_ms,
        );
        if let (true, Some(i)) = (fire, target) {
            self.enter_out(i, pos, None, Landing::Edge, WayBack::Open);
        }
    }

    /// Periodic call. Both dwells complete on the clock: the OS sends no events while the mouse rests, so a
    /// cursor parked at an edge would otherwise never switch out (Local) or never come home (RemoteIn).
    pub fn tick(&mut self, now_ms: u64) {
        if matches!(self.mode, Mode::Local) {
            self.on_cursor(self.last_pos, now_ms);
        }
        self.check_return(now_ms);
    }

    /// A resting cursor needs the clock only while an edge dwell can advance.
    pub fn needs_tick(&self) -> bool {
        match &self.mode {
            Mode::Local => self.peer_at(self.last_pos).is_some() || self.sw.dwell_start.is_some(),
            Mode::RemoteIn { edge, pos, ret, .. } => ret.dwell_start.is_some()
                || (self.way_back == WayBack::Open && in_exit_strip(&self.displays, *pos, *edge, self.cfg.boundary_px))
                || self.relay_at().is_some(),
            Mode::RemoteOut { .. } | Mode::Relay { .. } => false,
        }
    }

    pub fn on_captured(&mut self, ev: CapturedEvent, now_ms: u64) -> Disposition {
        if let CapturedEvent::Key { hid, down } = ev {
            if self.keys.set_key(hid, down).is_err() {
                return Disposition::Swallow;
            }
            if is_emergency(self.keys.mods(), hid, down) {
                self.emergency();
                return Disposition::Swallow;
            }
        }
        if self.takeover_trigger(&ev, now_ms) {
            // A key that takes over is sent below as itself; in the sync too, it would be pressed twice.
            let trigger = match ev {
                CapturedEvent::Key { hid, .. } => Some(hid),
                _ => None,
            };
            self.take_over(trigger);
        }
        if let CapturedEvent::Button { button: PRIMARY_BUTTON, down } = ev {
            let local = matches!(self.mode, Mode::Local);
            self.primary_down = down;
            self.held_here = down && local;
            if local {
                self.sink.primary(down);
            }
        }
        if let CapturedEvent::Key { hid, down } = ev {
            if matches!(self.mode, Mode::Local) {
                return self.local_key(hid, down, now_ms);
            }
        }
        match self.mode.clone() {
            Mode::RemoteOut { peer, seq, exit_pos } => {
                // The switch chord also comes home: a way back that does not depend on the peer.
                if let CapturedEvent::Key { hid, down } = ev {
                    let mods = self.keys.mods();
                    if decide(&self.cfg.policy, &mut self.sw, SwitchEvent::Key { hid, down, mods }, now_ms) {
                        self.out.send(&peer, Msg::Release { seq, reason: Reason::UserEscape });
                        self.restore_local(exit_pos, "the switch chord here");
                        return Disposition::Swallow;
                    }
                }
                let keys = self.cfg.peers.iter().find(|p| p.name == peer).map_or(KeyMap::None, |p| p.keys);
                let mac = keys == KeyMap::Mac;
                if matches!(ev, CapturedEvent::Key { down: true, .. } | CapturedEvent::Button { down: true, .. }) {
                    self.gesture_keys.other_input();
                }
                let key_msg = |(hid, down): (u16, bool)| Msg::Key { seq, hid, down, repeat: false };
                let mut msgs: Vec<Msg> = match ev {
                    CapturedEvent::Key { hid, down } if mac => self.mac_keys.key(hid, down).into_iter().map(key_msg).collect(),
                    CapturedEvent::Key { hid, down } => vec![key_msg((keys.apply(hid), down))],
                    CapturedEvent::Button { .. } | CapturedEvent::Wheel { .. } if mac => {
                        self.mac_keys.pointer().into_iter().map(key_msg).collect()
                    }
                    _ => Vec::new(),
                };
                match ev {
                    CapturedEvent::Button { button, down } => msgs.push(Msg::Button { seq, button, down }),
                    CapturedEvent::Motion { dx, dy } => msgs.push(Msg::MotionRel { seq, dx, dy }),
                    CapturedEvent::Wheel { dx, dy } => msgs.push(Msg::Wheel { seq, dx, dy }),
                    CapturedEvent::Gesture(g) if !modifier_held(&self.keys.snapshot()) => {
                        msgs.extend(self.gesture_msgs(&peer, seq, g));
                    }
                    CapturedEvent::Gesture(g) => {
                        crate::log::line(&format!("flowseam: gesture {g:?} not sent to {peer}: a modifier is held"));
                    }
                    CapturedEvent::Key { .. } => {}
                }
                for m in msgs {
                    self.out.send(&peer, m);
                }
                Disposition::Swallow
            }
            _ => Disposition::Pass,
        }
    }

    fn takeover_trigger(&mut self, ev: &CapturedEvent, now_ms: u64) -> bool {
        if !matches!(self.mode, Mode::RemoteIn { .. } | Mode::Relay { .. }) {
            return false;
        }
        match *ev {
            CapturedEvent::Button { down: true, .. } | CapturedEvent::Wheel { .. } | CapturedEvent::Gesture(_) => true,
            // Hold the emergency chord's modifier prefix until Esc can take priority.
            CapturedEvent::Key { hid, down: true } => !(0xE0..=0xE7).contains(&hid),
            CapturedEvent::Motion { dx, dy } => {
                let delta = u64::from(dx.unsigned_abs()) + u64::from(dy.unsigned_abs());
                let (start, total) = match self.takeover_motion {
                    Some((start, total)) if now_ms >= start && now_ms - start <= TAKEOVER_WINDOW_MS => (start, total + delta),
                    _ => (now_ms, delta),
                };
                self.takeover_motion = Some((start, total));
                total >= TAKEOVER_PX
            }
            _ => false,
        }
    }

    /// Local input on this machine ends the peer's control. Relaying, this machine goes on driving the far peer
    /// in the same session, with its own held keys (`skip_key` is the key that triggered it, sent on its own).
    fn take_over(&mut self, skip_key: Option<u16>) {
        match self.mode.clone() {
            Mode::RemoteIn { peer, seq, .. } => {
                self.out.send(&peer, Msg::Release { seq, reason: Reason::UserEscape });
                self.leave_remote_in("input here took over");
            }
            Mode::Relay { from, from_seq, to, to_seq, exit_pos, .. } => {
                self.out.send(&from, Msg::Release { seq: from_seq, reason: Reason::UserEscape });
                let map = self.cfg.peers.iter().find(|p| p.name == to).map_or(KeyMap::None, |p| p.keys);
                let mut held: Vec<u16> = self.keys.snapshot().into_iter().filter(|h| Some(*h) != skip_key).collect();
                held.sort_by_key(|h| !(0xE0..=0xE7).contains(h));
                let pressed: Vec<u16> = if map == KeyMap::Mac {
                    self.mac_keys = MacShortcuts::with_held(&held);
                    self.mac_keys.pressed()
                } else {
                    held.into_iter().map(|h| map.apply(h)).collect()
                };
                let mods = mods_of(&pressed, self.keys.mods().contains(ModifierSet::CAPS));
                self.out.send(&to, Msg::ModSync { seq: to_seq, mods, pressed });
                self.held_here = false;
                self.crossed_holding = false;
                self.sink.set_grab(true);
                self.mode = Mode::RemoteOut { peer: to, seq: to_seq, exit_pos };
            }
            _ => {}
        }
        self.takeover_motion = None;
    }

    fn local_key(&mut self, hid: u16, down: bool, now_ms: u64) -> Disposition {
        let target = self.peer_at(self.last_pos).or_else(|| self.first_up());
        let Some(i) = target else { return Disposition::Pass };
        let mods = self.keys.mods();
        let before = self.sw;
        if decide(&self.cfg.policy, &mut self.sw, SwitchEvent::Key { hid, down, mods }, now_ms) {
            if self.holding(now_ms) {
                // A chord that switched nothing leaves the edge armed as it was.
                self.sw = before;
                crate::log::line("flowseam: switch chord ignored: a menu switch is moving the monitors");
                return Disposition::Swallow;
            }
            let pos = self.last_pos;
            // The chord key is swallowed here; it must not arrive as a held key and type on the peer.
            self.enter_out(i, pos, Some(hid), Landing::Center, WayBack::Open);
            return Disposition::Swallow;
        }
        Disposition::Pass
    }

    pub fn on_msg(&mut self, peer: &str, msg: Msg, now_ms: u64) {
        // The peer we relay into only drives the session's end. Its Enter still reaches on_enter, which
        // refuses it as Busy: unanswered, that peer would keep its keyboard and mouse grabbed.
        if matches!(&self.mode, Mode::Relay { to, .. } if to == peer)
            && !matches!(&msg, Msg::Leave { .. } | Msg::Release { .. } | Msg::Enter { .. } | Msg::EnterCentered { .. } | Msg::EnterCenteredClosed { .. }) {
            return;
        }
        if self.forward_relay(peer, &msg) {
            return;
        }
        match msg {
            Msg::Enter { seq, edge, frac, pressed, .. } => self.on_enter(peer, seq, edge, frac, &pressed, Landing::Edge, WayBack::Open),
            Msg::EnterCentered { seq, edge, frac, pressed, .. } => {
                self.on_enter(peer, seq, edge, frac, &pressed, Landing::Center, WayBack::Open);
            }
            Msg::EnterCenteredClosed { seq, edge, frac, pressed, .. } => {
                self.on_enter(peer, seq, edge, frac, &pressed, Landing::Center, WayBack::Closed);
            }
            Msg::Leave { seq, frac, .. } => self.on_leave(peer, seq, frac),
            Msg::Release { seq, reason } => self.on_release(peer, seq, reason),
            Msg::ModSync { seq, pressed, .. } if self.live_in(peer, seq) => self.sync_injected(&pressed),
            Msg::MotionRel { seq, dx, dy } => {
                if self.live_in(peer, seq) {
                    self.sink.motion_rel(dx, dy);
                    self.move_virtual(dx, dy);
                    self.check_return(now_ms);
                }
            }
            Msg::Button { seq, button, down } => {
                if self.live_in(peer, seq) {
                    if down {
                        self.inj_btn.insert(button);
                        self.gesture_keys.other_input();
                    } else {
                        self.inj_btn.remove(&button);
                    }
                    self.sink.button(button, down);
                    if button == PRIMARY_BUTTON {
                        self.sink.primary(down);
                    }
                }
            }
            Msg::Wheel { seq, dx, dy } => {
                if self.live_in(peer, seq) {
                    self.sink.wheel(dx, dy);
                }
            }
            Msg::Key { seq, hid, down, repeat } if self.live_in(peer, seq) => {
                if down {
                    self.gesture_keys.other_input();
                }
                self.inject_key(hid, down, repeat);
            }
            Msg::Gesture { seq, gesture } if self.live_in(peer, seq) && !modifier_held(&self.inj_keys) => {
                // ponytail: shortcuts here too, so a pinch zooms in steps and a swipe does not follow the
                // fingers; replaying the gesture events themselves needs the macOS 27 field-4205 payload (iss).
                let strokes = self.gesture_keys.strokes(gesture, Platform::here());
                crate::log::line(&format!("flowseam: gesture {gesture:?} from {peer}: {strokes:?}"));
                for s in strokes {
                    match s {
                        Stroke::Key(hid, down) => self.inject_key(hid, down, false),
                        Stroke::Wheel(n) => self.sink.wheel(0, n),
                    }
                }
            }
            Msg::Gesture { gesture, .. } => crate::log::line(&format!(
                "flowseam: gesture {gesture:?} from {peer} ignored: not its live session, or a modifier is held"
            )),
            _ => {}
        }
    }

    /// What carries gesture `g` to `peer` in session `seq`: the gesture itself for a Mac that decodes it,
    /// its shortcut for a PC, nothing for an older Mac. Callers drop a gesture made while a modifier is held
    /// (`modifier_held`).
    fn gesture_msgs(&mut self, peer: &str, seq: u32, g: Gesture) -> Vec<Msg> {
        let found = self.cfg.peers.iter().find(|p| p.name == peer);
        let keys = found.map_or(KeyMap::None, |p| p.keys);
        // `None` is the map for a peer that is also a Mac; the other two are for a PC.
        if keys == KeyMap::None {
            let speaks = self.speaks.get(peer).copied();
            if speaks.is_some_and(|v| v >= GESTURE_PROTO_VERSION) {
                crate::log::line(&format!("flowseam: gesture {g:?} to {peer}"));
                return vec![Msg::Gesture { seq, gesture: g }];
            }
            // An older Mac may be relaying to a PC: Mac shortcuts would land there as other keys (⌃← as a
            // word jump, ⌘Space as the input method), so it gets nothing, as before gestures were carried.
            crate::log::line(&format!("flowseam: gesture {g:?} not sent to {peer}: it speaks protocol {speaks:?}"));
            return Vec::new();
        }
        let habit = found.map_or(Habit::Mac, |p| p.gestures.habit(keys));
        let strokes = self.gesture_keys.strokes(g, Platform::Pc(habit));
        crate::log::line(&format!("flowseam: gesture {g:?} to {peer} as {strokes:?}"));
        strokes.into_iter().map(|s| match s {
            Stroke::Key(hid, down) => Msg::Key { seq, hid, down, repeat: false },
            Stroke::Wheel(n) => Msg::Wheel { seq, dx: 0, dy: n },
        }).collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn on_enter(&mut self, peer: &str, seq: u32, edge: Edge, frac: u16, pressed: &[u16], landing: Landing, way_back: WayBack) {
        if let Some(&last) = self.last_enter.get(peer) {
            if (seq.wrapping_sub(last) as i32) <= 0 {
                return; // a duplicate of a session already handled
            }
        }
        // The same peer starting a newer session means it considers the old one over.
        if let Mode::Relay { from, to, to_seq, exit_pos, .. } = &self.mode {
            if from == peer {
                let (to, to_seq, exit_pos) = (to.clone(), *to_seq, *exit_pos);
                self.out.send(&to, Msg::Release { seq: to_seq, reason: Reason::TopologyChange });
                self.end_relay_local(exit_pos);
            }
        }
        if matches!(&self.mode, Mode::RemoteIn { peer: p, .. } if p == peer) {
            self.leave_remote_in("it started a newer session");
        }
        if !self.cfg.peers.iter().any(|p| p.name == peer) || !matches!(self.mode, Mode::Local) {
            // The sender has already hidden its cursor and grabbed its keyboard and mouse. Unanswered, it
            // would keep them until a restart; refused, it comes home at once.
            self.out.send(peer, Msg::Release { seq, reason: Reason::Busy });
            return;
        }
        self.last_enter.insert(peer.to_string(), seq);
        let land = match landing {
            Landing::Edge => entry_point(&self.displays, edge, frac),
            Landing::Center => center_point(&self.displays, edge, frac),
        };
        let Ok(pos) = land else {
            self.out.send(peer, Msg::Leave { seq, edge: opposite(edge), frac });
            return;
        };
        self.sink.warp(pos);
        self.sink.set_association(false);
        self.sync_injected(pressed);
        self.gesture_keys = GestureKeys::new();
        self.mode = Mode::RemoteIn { peer: peer.to_string(), seq, edge, pos, ret: SwitchState::new(pos) };
        self.takeover_motion = None;
        self.way_back = way_back;
        if way_back == WayBack::Closed {
            crate::log::line(&format!(
                "flowseam: {peer}'s displays all show this machine: its edge back is closed until this session ends (its chord, this machine's menu, or the emergency chord)"
            ));
        }
    }

    /// Make the injected key set equal `pressed`: release what the controller no longer holds, press the rest.
    fn sync_injected(&mut self, pressed: &[u16]) {
        let held: BTreeSet<u16> = pressed.iter().copied().collect();
        for k in self.inj_keys.clone().difference(&held) {
            self.inject_key(*k, false, false);
        }
        for &k in &held {
            if !self.inj_keys.contains(&k) {
                self.inject_key(k, true, false);
            }
        }
    }

    /// `Release` ends the session from either side, whatever the reason. Same effect as `Leave`, but it
    /// carries no landing position, so a controller comes home where it left.
    fn on_release(&mut self, peer: &str, seq: u32, reason: Reason) {
        let why = format!("it released the session ({reason:?})");
        match self.mode.clone() {
            Mode::RemoteOut { peer: p, seq: s, exit_pos } if p == peer && s == seq => self.restore_local(exit_pos, &why),
            Mode::RemoteIn { peer: p, seq: s, .. } if p == peer && s == seq => self.leave_remote_in(&why),
            Mode::Relay { from, from_seq, to, to_seq, exit_pos, edge, keys } if to == peer && to_seq == seq => {
                self.resume_remote_in(from, from_seq, edge, exit_pos, keys);
            }
            Mode::Relay { from, from_seq, to, to_seq, exit_pos, .. } if from == peer && from_seq == seq => {
                self.out.send(&to, Msg::Release { seq: to_seq, reason: Reason::UserEscape });
                self.end_relay_local(exit_pos);
            }
            _ => {}
        }
    }

    fn on_leave(&mut self, peer: &str, seq: u32, frac: u16) {
        match self.mode.clone() {
            Mode::RemoteOut { peer: p, seq: s, exit_pos } if p == peer && s == seq => {
                let edge = self.cfg.peers.iter().find(|x| x.name == p).map(|x| x.edge);
                let at = edge
                    .and_then(|e| entry_point(&self.displays, e, frac).ok())
                    .unwrap_or(exit_pos);
                self.restore_local(at, "it handed control back (Leave: its edge or its emergency chord)");
            }
            Mode::RemoteIn { peer: p, seq: s, .. } if p == peer && s == seq => self.leave_remote_in("it let go (Leave)"),
            Mode::Relay { from, from_seq, edge, to, to_seq, exit_pos, keys } if to == peer && to_seq == seq => {
                let at = self.cfg.peers.iter().find(|p| p.name == to)
                    .and_then(|p| entry_point(&self.displays, p.edge, frac).ok()).unwrap_or(exit_pos);
                self.resume_remote_in(from, from_seq, edge, at, keys);
            }
            Mode::Relay { from, from_seq, to, to_seq, exit_pos, .. } if from == peer && from_seq == seq => {
                self.out.send(&to, Msg::Release { seq: to_seq, reason: Reason::UserEscape });
                self.end_relay_local(exit_pos);
            }
            _ => {}
        }
    }

    fn live_in(&self, peer: &str, seq: u32) -> bool {
        matches!(&self.mode, Mode::RemoteIn { peer: p, seq: s, .. } if p == peer && *s == seq)
    }

    /// Relay traffic keeps the controller's raw keys so they can be restored here on return.
    fn forward_relay(&mut self, peer: &str, msg: &Msg) -> bool {
        let Mode::Relay { from, from_seq, to, to_seq, keys, .. } = &mut self.mode else { return false };
        let incoming_seq = match msg {
            Msg::MotionRel { seq, .. } | Msg::Button { seq, .. } | Msg::Wheel { seq, .. }
            | Msg::Key { seq, .. } | Msg::ModSync { seq, .. } | Msg::Gesture { seq, .. } => *seq,
            _ => return false,
        };
        if from != peer || incoming_seq != *from_seq {
            return false;
        }
        let (to, seq) = (to.clone(), *to_seq);
        let map = self.cfg.peers.iter().find(|p| p.name == to).map_or(KeyMap::None, |p| p.keys);
        let key_msg = |(hid, down): (u16, bool)| Msg::Key { seq, hid, down, repeat: false };
        if matches!(msg, Msg::Key { down: true, .. } | Msg::Button { down: true, .. }) {
            self.gesture_keys.other_input();
        }
        match msg {
            Msg::Gesture { gesture, .. } => {
                if !modifier_held(keys.iter()) {
                    for m in self.gesture_msgs(&to, seq, *gesture) {
                        self.out.send(&to, m);
                    }
                }
            }
            Msg::MotionRel { dx, dy, .. } => self.out.send(&to, Msg::MotionRel { seq, dx: *dx, dy: *dy }),
            Msg::Button { .. } | Msg::Wheel { .. } if map == KeyMap::Mac => {
                for m in self.mac_keys.pointer().into_iter().map(key_msg) {
                    self.out.send(&to, m);
                }
                match msg {
                    Msg::Button { button, down, .. } => self.out.send(&to, Msg::Button { seq, button: *button, down: *down }),
                    Msg::Wheel { dx, dy, .. } => self.out.send(&to, Msg::Wheel { seq, dx: *dx, dy: *dy }),
                    _ => unreachable!(),
                }
            }
            Msg::Button { button, down, .. } => self.out.send(&to, Msg::Button { seq, button: *button, down: *down }),
            Msg::Wheel { dx, dy, .. } => self.out.send(&to, Msg::Wheel { seq, dx: *dx, dy: *dy }),
            Msg::Key { hid, down, repeat, .. } => {
                if *down { keys.insert(*hid); } else { keys.remove(hid); }
                if map == KeyMap::Mac {
                    for (hid, down) in self.mac_keys.key(*hid, *down) {
                        self.out.send(&to, Msg::Key { seq, hid, down, repeat: *repeat });
                    }
                } else {
                    self.out.send(&to, Msg::Key { seq, hid: map.apply(*hid), down: *down, repeat: *repeat });
                }
            }
            Msg::ModSync { pressed, .. } => {
                *keys = pressed.iter().copied().collect();
                let mut held: Vec<u16> = keys.iter().copied().collect();
                held.sort_by_key(|h| !(0xE0..=0xE7).contains(h));
                let mapped = if map == KeyMap::Mac {
                    self.mac_keys = MacShortcuts::with_held(&held);
                    self.mac_keys.pressed()
                } else {
                    held.into_iter().map(|h| map.apply(h)).collect()
                };
                self.out.send(&to, Msg::ModSync { seq, mods: mods_of(&mapped, false), pressed: mapped });
            }
            _ => return false,
        }
        true
    }

    fn inject_key(&mut self, hid: u16, down: bool, repeat: bool) {
        if down {
            self.inj_keys.insert(hid);
        } else {
            self.inj_keys.remove(&hid);
        }
        self.sink.key(hid, down, repeat);
    }

    fn move_virtual(&mut self, dx: i32, dy: i32) {
        let displays = &self.displays;
        if let Mode::RemoteIn { pos, .. } = &mut self.mode {
            // The same rule the sinks use to place the injected cursor (`PostedCursor`), so this position
            // and the real cursor cross the seams between displays together and stop at the same edges.
            *pos = clamp_to_displays(displays, Point { x: pos.x.saturating_add(dx), y: pos.y.saturating_add(dy) });
        }
    }

    fn check_return(&mut self, now_ms: u64) {
        // A press injected here and still held keeps the cursor here unless it started a file drag. A drag
        // that crossed in already held left no injected press, so it can always be carried back.
        let held = self.inj_btn.contains(&PRIMARY_BUTTON) && !self.sink.drag_started_here();
        let relay = self.relay_at();
        let Mode::RemoteIn { peer, seq, edge, pos, ret } = &mut self.mode else { return };
        let returning = self.way_back == WayBack::Open && in_exit_strip(&self.displays, *pos, *edge, self.cfg.boundary_px);
        let in_boundary = (!held && returning) || (relay.is_some() && self.inj_btn.is_empty());
        if !decide(&self.cfg.policy, ret, SwitchEvent::Cursor { pos: *pos, in_boundary }, now_ms) {
            return;
        }
        let (peer, seq, edge, pos) = (peer.clone(), *seq, *edge, *pos);
        if !returning {
            // Reached only with no injected button held (`in_boundary` above): files do not relay, so even
            // a file drag stays here until its button is released.
            if let Some(i) = relay { self.enter_relay(peer, seq, edge, i, pos); }
            return;
        }
        // The controller's Enter carries no buttons, so a drag that arrived here with the button already
        // down left no trace in `inj_btn`: the hook runs on every crossing and the sink decides.
        self.sink.drag_away(&peer);
        let frac = frac_at(&self.displays, pos, edge);
        self.out.send(&peer, Msg::Leave { seq, edge: opposite(edge), frac });
        self.leave_remote_in("the cursor went back through the edge");
    }

    fn enter_relay(&mut self, from: String, from_seq: u32, edge: Edge, idx: usize, pos: Point) {
        let spec = self.cfg.peers[idx].clone();
        self.next_seq = self.next_seq.wrapping_add(1);
        let to_seq = self.next_seq;
        let keys = self.inj_keys.clone();
        let mut held: Vec<u16> = keys.iter().copied().collect();
        held.sort_by_key(|h| !(0xE0..=0xE7).contains(h));
        self.mac_keys = MacShortcuts::new();
        self.gesture_keys = GestureKeys::new();
        let pressed = if spec.keys == KeyMap::Mac {
            self.mac_keys = MacShortcuts::with_held(&held);
            self.mac_keys.pressed()
        } else {
            held.into_iter().map(|h| spec.keys.apply(h)).collect()
        };
        // ponytail: Caps is omitted until the wire carries toggle state; on_enter ignores Enter.mods.
        self.out.send(&spec.name, Msg::Enter {
            seq: to_seq, edge: opposite(spec.edge), frac: frac_at(&self.displays, pos, spec.edge),
            mods: mods_of(&pressed, false), pressed,
        });
        for k in std::mem::take(&mut self.inj_keys) { self.sink.key(k, false, false); }
        for b in std::mem::take(&mut self.inj_btn) { self.sink.button(b, false); }
        self.sink.hide();
        self.mode = Mode::Relay { from, from_seq, edge, to: spec.name, to_seq, exit_pos: pos, keys };
        self.takeover_motion = None;
    }

    fn resume_remote_in(&mut self, from: String, seq: u32, edge: Edge, pos: Point, keys: BTreeSet<u16>) {
        let mut ret = SwitchState::new(pos);
        ret.armed = false;
        ret.anchor = Some(pos);
        self.mode = Mode::RemoteIn { peer: from, seq, edge, pos, ret };
        self.takeover_motion = None;
        self.sink.warp(pos);
        self.sink.show();
        self.sync_injected(&keys.into_iter().collect::<Vec<_>>());
    }

    fn end_relay_local(&mut self, at: Point) {
        self.sink.show();
        self.sink.set_association(true);
        self.go_local(at);
    }

    fn enter_out(&mut self, idx: usize, pos: Point, skip_key: Option<u16>, landing: Landing, way_back: WayBack) {
        let spec = self.cfg.peers[idx].clone();
        self.crossed_holding = self.held_here;
        self.held_here = false;
        self.sink.drag_away(&spec.name);
        self.next_seq = self.next_seq.wrapping_add(1);
        let seq = self.next_seq;
        let mut held: Vec<u16> = self.keys.snapshot().into_iter().filter(|h| Some(*h) != skip_key).collect();
        self.mac_keys = MacShortcuts::new();
        self.gesture_keys = GestureKeys::new();
        let pressed: Vec<u16> = if spec.keys == KeyMap::Mac {
            // Modifiers first, so a key held while crossing is translated under them.
            held.sort_by_key(|h| !(0xE0..=0xE7).contains(h));
            self.mac_keys = MacShortcuts::with_held(&held);
            self.mac_keys.pressed()
        } else {
            held.into_iter().map(|h| spec.keys.apply(h)).collect()
        };
        let (edge, frac) = (opposite(spec.edge), frac_at(&self.displays, pos, spec.edge));
        let mods = mods_of(&pressed, self.keys.mods().contains(ModifierSet::CAPS));
        let speaks = self.speaks.get(&spec.name).copied();
        let msg = match landing {
            Landing::Center if way_back == WayBack::Closed && speaks.is_some_and(|v| v >= CLOSED_PROTO_VERSION) => {
                Msg::EnterCenteredClosed { seq, edge, frac, mods, pressed }
            }
            Landing::Center if speaks.is_some_and(|v| v >= CENTERED_PROTO_VERSION) => {
                if way_back == WayBack::Closed {
                    crate::log::line(&format!(
                        "flowseam: {} speaks protocol {speaks:?}: its edge back here stays open, though every display here shows it",
                        spec.name
                    ));
                }
                Msg::EnterCentered { seq, edge, frac, mods, pressed }
            }
            Landing::Center => {
                crate::log::line(&format!(
                    "flowseam: {} speaks protocol {speaks:?}: its cursor lands at the edge, not in the middle{}",
                    spec.name,
                    if way_back == WayBack::Closed { ", and its edge back here stays open" } else { "" }
                ));
                Msg::Enter { seq, edge, frac, mods, pressed }
            }
            Landing::Edge => Msg::Enter { seq, edge, frac, mods, pressed },
        };
        self.out.send(&spec.name, msg);
        self.sink.set_grab(true);
        self.sink.hide();
        self.mode = Mode::RemoteOut { peer: spec.name, seq, exit_pos: pos };
    }

    /// Any mode to Local. A peer that is driving us or being driven by us is told with `Leave`.
    fn emergency(&mut self) {
        match self.mode.clone() {
            Mode::RemoteOut { peer, seq, exit_pos } => {
                let edge = self.cfg.peers.iter().find(|x| x.name == peer).map(|x| x.edge).unwrap_or(Edge::Right);
                self.out.send(&peer, Msg::Leave { seq, edge, frac: FRAC_MID });
                self.restore_local(exit_pos, "the emergency chord or a release here");
            }
            Mode::RemoteIn { peer, seq, edge, .. } => {
                self.out.send(&peer, Msg::Leave { seq, edge: opposite(edge), frac: FRAC_MID });
                self.leave_remote_in("the emergency chord or a release here");
            }
            Mode::Relay { from, from_seq, edge, to, to_seq, exit_pos, .. } => {
                self.out.send(&to, Msg::Release { seq: to_seq, reason: Reason::UserEscape });
                self.out.send(&from, Msg::Leave { seq: from_seq, edge: opposite(edge), frac: FRAC_MID });
                self.end_relay_local(exit_pos);
            }
            Mode::Local => {}
        }
    }

    /// RemoteOut to Local. `why` goes to the log: which of the many ways a session ends was this one.
    fn restore_local(&mut self, at: Point, why: &str) {
        if let Mode::RemoteOut { peer, .. } = &self.mode {
            crate::log::line(&format!("flowseam: control back from {peer}: {why}"));
        }
        self.sink.set_grab(false);
        self.sink.show();
        self.sink.warp(at);
        // Without this release the system here keeps the button down: clicks do nothing and a drag loop
        // eats the keys, until someone posts an up. Still held now: the real release comes, and passes.
        if std::mem::take(&mut self.crossed_holding) && !self.primary_down {
            self.sink.button(PRIMARY_BUTTON, false);
        }
        self.go_local(at);
    }

    /// RemoteIn to Local; `why` goes to the log, as for `restore_local`.
    fn leave_remote_in(&mut self, why: &str) {
        if let Mode::RemoteIn { peer, .. } = &self.mode {
            crate::log::line(&format!("flowseam: control returned to {peer}: {why}"));
        }
        for k in std::mem::take(&mut self.inj_keys) {
            self.sink.key(k, false, false);
        }
        for b in std::mem::take(&mut self.inj_btn) {
            self.sink.button(b, false);
        }
        self.sink.set_association(true);
        let at = match &self.mode {
            Mode::RemoteIn { pos, .. } => *pos,
            _ => self.last_pos,
        };
        self.go_local(at);
    }

    /// Back to Local, disarmed at `at`: the cursor is usually parked in a strip and must
    /// leave it and move `arm_dist` before dwell can switch again.
    fn go_local(&mut self, at: Point) {
        self.mode = Mode::Local;
        self.last_pos = at;
        self.sw = SwitchState::new(at);
        self.sw.armed = false;
        self.sw.anchor = Some(at);
    }

    fn peer_at(&self, pos: Point) -> Option<usize> {
        self.cfg.peers.iter().position(|p| {
            self.up.contains(&p.name) && in_exit_strip(&self.displays, pos, p.edge, self.cfg.boundary_px)
        })
    }

    fn relay_at(&self) -> Option<usize> {
        let Mode::RemoteIn { peer, edge, pos, .. } = &self.mode else { return None };
        self.cfg.peers.iter().position(|p| p.name != *peer && p.edge != *edge && self.up.contains(&p.name)
            && in_exit_strip(&self.displays, *pos, p.edge, self.cfg.boundary_px))
    }

    fn first_up(&self) -> Option<usize> {
        self.cfg.peers.iter().position(|p| self.up.contains(&p.name))
    }
}

/// A gesture made while one of these is held does nothing: its shortcut would press and release that
/// modifier under the user's own hold, and the peer would lose it (or the shortcut would gain it).
fn modifier_held<'a>(keys: impl IntoIterator<Item = &'a u16>) -> bool {
    keys.into_iter().any(|k| (0xE0..=0xE7).contains(k))
}

/// The modifier set a peer sees for these held usages (after mapping), plus CapsLock's toggle.
fn mods_of(pressed: &[u16], caps: bool) -> ModifierSet {
    let mut m = if caps { ModifierSet::CAPS } else { 0 };
    for &h in pressed {
        m |= match h {
            0xE0 | 0xE4 => ModifierSet::CTRL,
            0xE1 | 0xE5 => ModifierSet::SHIFT,
            0xE2 | 0xE6 => ModifierSet::ALT,
            0xE3 | 0xE7 => ModifierSet::GUI,
            _ => 0,
        };
    }
    ModifierSet(m)
}

/// Fraction along `edge` for a cursor at `pos`, projected onto the boundary line first
/// (the cursor is only in the strip, not on the edge pixel). Middle when it has no exit there.
fn frac_at(displays: &[Rect], pos: Point, edge: Edge) -> u16 {
    let Some(r) = displays.iter().find(|r| contains(r, pos)) else { return FRAC_MID };
    let on_edge = match edge {
        Edge::Left => Point { x: r.x, y: pos.y },
        Edge::Right => Point { x: r.x + r.w - 1, y: pos.y },
        Edge::Top => Point { x: pos.x, y: r.y },
        Edge::Bottom => Point { x: pos.x, y: r.y + r.h - 1 },
    };
    exit_frac(displays, on_edge, edge).unwrap_or(FRAC_MID)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{ModifierSet, Reason};

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Fx {
        Motion(i32, i32),
        Button(u8, bool),
        Key(u16, bool),
        Wheel(i32, i32),
        Warp(Point),
        Grab(bool),
        Hide,
        Show,
        Assoc(bool),
        Primary(bool),
        DragAway(String),
    }

    /// `.1`: pretend no file drag started during the press (a window or selection drag).
    #[derive(Default)]
    struct Rec(Vec<Fx>, bool);
    impl InputSink for Rec {
        fn drag_started_here(&mut self) -> bool {
            !self.1
        }
        fn motion_rel(&mut self, dx: i32, dy: i32) {
            self.0.push(Fx::Motion(dx, dy));
        }
        fn button(&mut self, b: u8, d: bool) {
            self.0.push(Fx::Button(b, d));
        }
        fn key(&mut self, h: u16, d: bool, _r: bool) {
            self.0.push(Fx::Key(h, d));
        }
        fn wheel(&mut self, dx: i32, dy: i32) {
            self.0.push(Fx::Wheel(dx, dy));
        }
        fn warp(&mut self, p: Point) {
            self.0.push(Fx::Warp(p));
        }
        fn set_grab(&mut self, on: bool) {
            self.0.push(Fx::Grab(on));
        }
        fn hide(&mut self) {
            self.0.push(Fx::Hide);
        }
        fn show(&mut self) {
            self.0.push(Fx::Show);
        }
        fn set_association(&mut self, c: bool) {
            self.0.push(Fx::Assoc(c));
        }
        fn primary(&mut self, d: bool) {
            self.0.push(Fx::Primary(d));
        }
        fn drag_away(&mut self, peer: &str) {
            self.0.push(Fx::DragAway(peer.into()));
        }
    }

    #[derive(Default)]
    struct Mail(Vec<(String, Msg)>);
    impl Outbox for Mail {
        fn send(&mut self, peer: &str, msg: Msg) {
            self.0.push((peer.to_string(), msg));
        }
    }

    const W: i32 = 1440;
    const H: i32 = 900;
    const CTRL: u16 = 0xE0;
    const ALT: u16 = 0xE2;
    const GUI: u16 = 0xE3;
    const ESC: u16 = 0x29;

    fn screen() -> Vec<Rect> {
        vec![Rect { x: 0, y: 0, w: W, h: H }]
    }
    fn cfg(edge: Edge) -> FwdConfig {
        FwdConfig {
            peers: vec![PeerSpec { name: "b".into(), edge, keys: KeyMap::None, gestures: Default::default() }],
            policy: SwitchPolicy::default(),
            boundary_px: 2,
        }
    }
    fn fwd(edge: Edge) -> Forwarder<Rec, Mail> {
        let mut f = Forwarder::new(cfg(edge), screen(), Rec::default(), Mail::default());
        f.link_up("b");
        f
    }
    fn right_edge() -> Point {
        Point { x: W - 1, y: 450 }
    }
    fn left_edge() -> Point {
        Point { x: 0, y: 450 }
    }
    /// Dwell at `p` from t0 until the switch fires.
    fn dwell(f: &mut Forwarder<Rec, Mail>, p: Point, t0: u64) {
        f.on_cursor(p, t0);
        f.on_cursor(p, t0 + 200);
    }
    fn go_out(f: &mut Forwarder<Rec, Mail>) -> u32 {
        dwell(f, right_edge(), 0);
        match f.mode() {
            Mode::RemoteOut { seq, .. } => *seq,
            m => panic!("expected RemoteOut, got {m:?}"),
        }
    }

    #[test]
    fn tick_is_needed_only_at_a_live_edge_or_active_dwell() {
        let mut f = fwd(Edge::Right);
        f.on_cursor(Point { x: W / 2, y: H / 2 }, 0);
        assert!(!f.needs_tick(), "local middle");
        f.on_cursor(right_edge(), 1);
        assert!(f.needs_tick(), "linked local edge");
        f.mode = Mode::RemoteOut { peer: "b".into(), seq: 1, exit_pos: right_edge() };
        assert!(!f.needs_tick(), "remote out");

        let mut f = fwd(Edge::Left);
        go_in(&mut f, 7, vec![]);
        assert!(!f.needs_tick(), "remote in middle");
        f.on_msg("b", Msg::MotionRel { seq: 7, dx: -1000, dy: 0 }, 10);
        assert!(f.needs_tick(), "remote in exit strip");
    }

    #[test]
    fn a_resting_dwell_needs_a_tick_before_it_completes() {
        let mut local = fwd(Edge::Right);
        local.on_cursor(right_edge(), 0);
        assert!(local.needs_tick());
        local.tick(200);
        assert!(matches!(local.mode(), Mode::RemoteOut { .. }));

        let mut incoming = fwd(Edge::Left);
        go_in(&mut incoming, 7, vec![]);
        incoming.on_msg("b", Msg::MotionRel { seq: 7, dx: -1000, dy: 0 }, 1000);
        assert!(incoming.needs_tick());
        incoming.tick(1200);
        assert!(matches!(incoming.mode(), Mode::Local));
    }
    /// Receiver (peer to the left of the sender) with an active session `seq`.
    fn go_in(f: &mut Forwarder<Rec, Mail>, seq: u32, pressed: Vec<u16>) {
        f.on_msg(
            "b",
            Msg::Enter { seq, edge: Edge::Left, frac: 0x8000, mods: ModifierSet(0), pressed },
            0,
        );
        assert!(matches!(f.mode(), Mode::RemoteIn { .. }), "{:?}", f.mode());
    }
    fn key(f: &mut Forwarder<Rec, Mail>, hid: u16, down: bool) -> Disposition {
        f.on_captured(CapturedEvent::Key { hid, down }, 0)
    }
    fn emergency_chord(f: &mut Forwarder<Rec, Mail>) -> Disposition {
        for h in [CTRL, ALT, GUI] {
            key(f, h, true);
        }
        key(f, ESC, true)
    }
    fn msgs(f: &Forwarder<Rec, Mail>) -> Vec<Msg> {
        f.out().0.iter().map(|(_, m)| m.clone()).collect()
    }

    const LEFT: u8 = PRIMARY_BUTTON;

    /// Driven from the left, the cursor reaches a second display on the right, stops at its far edge,
    /// and still comes home through the left edge of the first one.
    #[test]
    fn a_driven_cursor_crosses_the_seam_between_displays() {
        let two = vec![Rect { x: 0, y: 0, w: W, h: H }, Rect { x: W, y: 100, w: 1920, h: 1080 }];
        let mut f = Forwarder::new(cfg(Edge::Left), two, Rec::default(), Mail::default());
        f.link_up("b");
        go_in(&mut f, 1, vec![]);
        let pos = |f: &Forwarder<Rec, Mail>| match f.mode() {
            Mode::RemoteIn { pos, .. } => *pos,
            m => panic!("expected RemoteIn, got {m:?}"),
        };
        let start = pos(&f);
        f.on_msg("b", Msg::MotionRel { seq: 1, dx: 2000, dy: 0 }, 10);
        assert_eq!(pos(&f), Point { x: start.x + 2000, y: start.y }, "stopped at the seam");
        f.on_msg("b", Msg::MotionRel { seq: 1, dx: 5000, dy: 5000 }, 20);
        assert_eq!(pos(&f), Point { x: W + 1920 - 1, y: 100 + 1080 - 1 }, "the far corner of the second display");
        f.on_msg("b", Msg::MotionRel { seq: 1, dx: -5000, dy: -800 }, 30);
        assert_eq!(pos(&f).x, 0, "back across the seam to the left edge");
        f.tick(300);
        assert!(matches!(f.mode(), Mode::Local), "{:?}", f.mode());
        assert!(matches!(msgs(&f).last(), Some(Msg::Leave { seq: 1, .. })), "{:?}", msgs(&f));
    }

    /// A press here that reaches the edge carries its drag along, before the grab freezes this side; the
    /// release then happens on the peer, so nothing is reported as dropped here.
    #[test]
    fn a_drag_out_is_announced_before_the_switch_and_its_drop_belongs_to_the_peer() {
        let mut f = fwd(Edge::Right);
        assert_eq!(f.on_captured(CapturedEvent::Button { button: LEFT, down: true }, 0), Disposition::Pass);
        dwell(&mut f, right_edge(), 0);
        assert!(matches!(f.mode(), Mode::RemoteOut { .. }));
        assert_eq!(f.sink().0, vec![Fx::Primary(true), Fx::DragAway("b".into()), Fx::Grab(true), Fx::Hide]);
        f.on_captured(CapturedEvent::Button { button: LEFT, down: false }, 300);
        assert!(!f.sink().0.contains(&Fx::Primary(false)), "the drop happens on the peer, not here");
        // Other buttons are not reported as presses; the crossing is reported all the same.
        let mut g = fwd(Edge::Right);
        g.on_captured(CapturedEvent::Button { button: 1, down: true }, 0);
        dwell(&mut g, right_edge(), 0);
        assert_eq!(g.sink().0, vec![Fx::DragAway("b".into()), Fx::Grab(true), Fx::Hide]);
    }

    /// Held from a press here without a file drag (a window dragged to the edge): the cursor stays until
    /// the release, then dwells out as usual. Nothing is left waiting for a release it never gets.
    #[test]
    fn a_held_press_without_a_file_drag_waits_at_the_edge_until_released() {
        let mut f = fwd(Edge::Right);
        f.sink_mut().1 = true;
        f.on_captured(CapturedEvent::Button { button: LEFT, down: true }, 0);
        for t in (0..=2000).step_by(50) {
            f.on_cursor(right_edge(), t);
        }
        assert!(matches!(f.mode(), Mode::Local), "left with the window drag still held");
        f.on_captured(CapturedEvent::Button { button: LEFT, down: false }, 2000);
        f.tick(2100);
        assert!(matches!(f.mode(), Mode::Local), "the dwell starts at the release, not before");
        f.tick(2400);
        assert!(matches!(f.mode(), Mode::RemoteOut { .. }), "{:?}", f.mode());
    }

    /// A release is not a cursor event: a press let go at the edge starts its dwell only on the clock, so a
    /// cursor resting in the strip needs ticks before any dwell runs.
    #[test]
    fn a_cursor_resting_in_the_strip_needs_ticks_before_its_dwell_starts() {
        let mut f = fwd(Edge::Right);
        f.sink_mut().1 = true;
        f.on_captured(CapturedEvent::Button { button: LEFT, down: true }, 0);
        f.on_cursor(right_edge(), 0);
        f.on_captured(CapturedEvent::Button { button: LEFT, down: false }, 100);
        assert!(f.sw.dwell_start.is_none(), "no dwell yet at the release");
        assert!(f.needs_tick());
        f.tick(150);
        f.tick(500);
        assert!(matches!(f.mode(), Mode::RemoteOut { .. }), "{:?}", f.mode());
    }

    /// A press made here that crossed (a drag) had its release go to the peer. Coming home, this machine
    /// still has the button down unless it is told otherwise: clicks and keys stop working here.
    #[test]
    fn a_press_that_crossed_is_released_here_when_control_comes_back() {
        // Every way home: the peer's Leave, the peer's Release, the link dropping, the switch chord.
        type Home = fn(&mut Forwarder<Rec, Mail>, u32);
        let homes: [(&str, Home); 4] = [
            ("leave", |f, seq| f.on_msg("b", Msg::Leave { seq, edge: Edge::Right, frac: 0x8000 }, 500)),
            ("release", |f, seq| f.on_msg("b", Msg::Release { seq, reason: Reason::UserEscape }, 500)),
            ("link down", |f, _| f.link_down("b")),
            ("emergency", |f, _| {
                emergency_chord(f);
            }),
        ];
        for (how, home) in homes {
            let mut f = fwd(Edge::Right);
            f.on_captured(CapturedEvent::Button { button: LEFT, down: true }, 0);
            let seq = go_out(&mut f);
            f.on_captured(CapturedEvent::Button { button: LEFT, down: false }, 300); // goes to the peer
            f.sink_mut().0.clear();
            home(&mut f, seq);
            assert!(matches!(f.mode(), Mode::Local), "{how}: {:?}", f.mode());
            assert_eq!(f.sink().0.iter().filter(|e| **e == Fx::Button(LEFT, false)).count(), 1, "{how}: {:?}", f.sink().0);
            // Once: going out and back again without a press owes nothing.
            f.link_up("b");
            f.on_cursor(Point { x: W / 2, y: 450 }, 600);
            let seq = match f.mode() {
                Mode::Local => {
                    dwell(&mut f, right_edge(), 700);
                    match f.mode() {
                        Mode::RemoteOut { seq, .. } => *seq,
                        m => panic!("{how}: {m:?}"),
                    }
                }
                m => panic!("{how}: {m:?}"),
            };
            f.sink_mut().0.clear();
            f.on_msg("b", Msg::Leave { seq, edge: Edge::Right, frac: 0x8000 }, 1000);
            assert!(!f.sink().0.contains(&Fx::Button(LEFT, false)), "{how}: {:?}", f.sink().0);
        }
    }

    /// Still held when control comes back: the real release is on its way and reaches the system, so no
    /// release is made up (it would end the drag early).
    #[test]
    fn a_press_still_held_on_the_way_home_is_left_to_the_real_release() {
        let mut f = fwd(Edge::Right);
        f.on_captured(CapturedEvent::Button { button: LEFT, down: true }, 0);
        let seq = go_out(&mut f);
        f.sink_mut().0.clear();
        f.on_msg("b", Msg::Leave { seq, edge: Edge::Right, frac: 0x8000 }, 500);
        assert!(!f.sink().0.contains(&Fx::Button(LEFT, false)), "{:?}", f.sink().0);
        assert_eq!(f.on_captured(CapturedEvent::Button { button: LEFT, down: false }, 600), Disposition::Pass);
    }

    /// A press made on the peer that comes home still held (a drag from the peer) may go back out: only a
    /// press made here waits at the edge.
    #[test]
    fn a_press_made_on_the_peer_can_be_carried_back_out() {
        let mut f = fwd(Edge::Right);
        f.sink_mut().1 = true;
        let seq = go_out(&mut f);
        f.on_captured(CapturedEvent::Button { button: LEFT, down: true }, 300);
        f.on_msg("b", Msg::Leave { seq, edge: Edge::Right, frac: 0x8000 }, 400);
        assert!(matches!(f.mode(), Mode::Local));
        f.on_cursor(Point { x: W / 2, y: 450 }, 500); // leave the strip to re-arm
        dwell(&mut f, right_edge(), 600);
        assert!(matches!(f.mode(), Mode::RemoteOut { .. }), "{:?}", f.mode());
    }

    /// The same on the driven side: a press injected here and still held keeps the cursor here.
    #[test]
    fn a_driven_side_keeps_a_held_press_without_a_file_drag() {
        let mut f = fwd(Edge::Left);
        f.sink_mut().1 = true;
        go_in(&mut f, 1, vec![]);
        f.on_msg("b", Msg::Button { seq: 1, button: LEFT, down: true }, 10);
        f.on_msg("b", Msg::MotionRel { seq: 1, dx: -5000, dy: 0 }, 20);
        f.tick(1500);
        assert!(matches!(f.mode(), Mode::RemoteIn { .. }), "returned with the press still held");
        f.on_msg("b", Msg::Button { seq: 1, button: LEFT, down: false }, 1500);
        f.tick(1600);
        f.tick(2000);
        assert!(matches!(f.mode(), Mode::Local), "{:?}", f.mode());
    }

    /// A drag that crossed in with the button already down (the controller's Enter carries no buttons) and
    /// is carried back out must still be handed back, so the staged files are not left waiting here.
    #[test]
    fn a_drag_carried_back_without_any_injected_press_is_still_handed_back() {
        let mut f = fwd(Edge::Left);
        go_in(&mut f, 1, vec![]);
        f.on_msg("b", Msg::MotionRel { seq: 1, dx: -5000, dy: 0 }, 30);
        f.tick(400);
        assert!(matches!(f.mode(), Mode::Local), "{:?}", f.mode());
        assert!(f.sink().0.contains(&Fx::DragAway("b".into())), "{:?}", f.sink().0);
    }

    /// The driven side reports injected primary presses, and a cursor that dwells back out with the
    /// button held takes the drag home before the held button is let go here.
    #[test]
    fn a_driven_side_reports_drops_and_hands_a_held_drag_back_before_releasing() {
        let mut f = fwd(Edge::Left);
        go_in(&mut f, 1, vec![]);
        f.on_msg("b", Msg::Button { seq: 1, button: LEFT, down: false }, 10);
        assert!(f.sink().0.ends_with(&[Fx::Button(LEFT, false), Fx::Primary(false)]), "{:?}", f.sink().0);
        f.on_msg("b", Msg::Button { seq: 1, button: LEFT, down: true }, 20);
        assert!(f.sink().0.ends_with(&[Fx::Button(LEFT, true), Fx::Primary(true)]));
        f.on_msg("b", Msg::MotionRel { seq: 1, dx: -5000, dy: 0 }, 30);
        f.tick(400);
        assert!(matches!(f.mode(), Mode::Local), "{:?}", f.mode());
        let fx = &f.sink().0;
        let away = fx.iter().position(|e| *e == Fx::DragAway("b".into())).expect("drag handed back");
        let release = fx.iter().rposition(|e| *e == Fx::Button(LEFT, false)).unwrap();
        assert!(away < release, "{fx:?}");
    }

    /// Back home with the button still held from the peer: letting go here is a drop here.
    #[test]
    fn a_release_after_coming_home_is_a_drop_here() {
        let mut f = fwd(Edge::Right);
        go_out(&mut f);
        f.on_captured(CapturedEvent::Button { button: LEFT, down: true }, 300);
        emergency_chord(&mut f);
        assert!(matches!(f.mode(), Mode::Local));
        f.on_captured(CapturedEvent::Button { button: LEFT, down: false }, 400);
        assert_eq!(f.sink().0.last(), Some(&Fx::Primary(false)));
    }

    /// A machine that cannot take a session (it is driving another, or being driven) must say so. The
    /// sender has already hidden its cursor and grabbed its keyboard and mouse; with no answer it keeps them
    /// until a restart.
    #[test]
    fn an_enter_that_cannot_be_taken_is_refused_so_the_sender_comes_home() {
        let mut busy = fwd(Edge::Right);
        go_out(&mut busy); // driving its own peer
        let before = busy.out().0.len();
        busy.on_msg("b", Msg::Enter { seq: 77, edge: Edge::Left, frac: 0x8000, mods: ModifierSet(0), pressed: vec![] }, 10);
        assert!(matches!(busy.mode(), Mode::RemoteOut { .. }), "the busy machine keeps its own session");
        let replies: Vec<&Msg> = busy.out().0[before..].iter().map(|(_, m)| m).collect();
        assert!(matches!(replies[..], [Msg::Release { seq: 77, .. }]), "{replies:?}");
        // The sender, on that reply, is home.
        let mut sender = fwd(Edge::Right);
        let seq = go_out(&mut sender);
        let reply = match replies[0] { Msg::Release { reason, .. } => Msg::Release { seq, reason: *reason }, _ => unreachable!() };
        sender.on_msg("b", reply, 20);
        assert!(matches!(sender.mode(), Mode::Local));
        // An Enter from a machine this one does not know is refused the same way.
        let mut stranger = fwd(Edge::Right);
        stranger.on_msg("nobody", Msg::Enter { seq: 5, edge: Edge::Left, frac: 0x8000, mods: ModifierSet(0), pressed: vec![] }, 0);
        assert!(matches!(stranger.out().0.last(), Some((p, Msg::Release { seq: 5, .. })) if p == "nobody"));
    }

    /// The peer driving us starts a newer session (it came home and went out again before our side saw the
    /// end): that one is taken, with whatever the old one held let go first, instead of being refused.
    #[test]
    fn a_newer_session_from_the_driving_peer_replaces_the_old_one() {
        let mut f = fwd(Edge::Left);
        go_in(&mut f, 1, vec![]);
        f.on_msg("b", Msg::Key { seq: 1, hid: 0x04, down: true, repeat: false }, 5);
        f.on_msg("b", Msg::Enter { seq: 2, edge: Edge::Left, frac: 0x8000, mods: ModifierSet(0), pressed: vec![] }, 10);
        assert!(matches!(f.mode(), Mode::RemoteIn { seq: 2, .. }), "{:?}", f.mode());
        assert!(f.sink().0.contains(&Fx::Key(0x04, false)), "the old session's key is released");
        assert!(!msgs(&f).iter().any(|m| matches!(m, Msg::Release { .. })), "not refused");
    }

    /// The switch chord also brings control home, so there is a known way back when the peer never hands it
    /// back (it cannot reach its exit edge, or it is not answering).
    #[test]
    fn the_switch_chord_brings_control_home_from_the_peer() {
        let mut f = fwd(Edge::Right);
        let seq = go_out(&mut f);
        for h in [CTRL, ALT, GUI] {
            key(&mut f, h, true);
        }
        assert_eq!(key(&mut f, crate::switch::HID_S, true), Disposition::Swallow, "the chord key never reaches the peer");
        assert!(matches!(f.mode(), Mode::Local), "{:?}", f.mode());
        assert!(msgs(&f).iter().any(|m| matches!(m, Msg::Release { seq: s, .. } if *s == seq)), "{:?}", msgs(&f));
        assert!(!msgs(&f).iter().any(|m| matches!(m, Msg::Key { hid: crate::switch::HID_S, .. })));
    }

    #[test]
    fn dwell_in_strip_switches_out_and_sends_enter() {
        let mut f = fwd(Edge::Right);
        f.on_cursor(right_edge(), 0);
        assert!(matches!(f.mode(), Mode::Local), "touching alone must not switch");
        f.on_cursor(right_edge(), 200);
        assert!(matches!(f.mode(), Mode::RemoteOut { seq: 1, .. }));
        let m = msgs(&f);
        let Msg::Enter { seq, edge, frac, pressed, .. } = &m[0] else { panic!("{m:?}") };
        assert_eq!((*seq, *edge), (1, Edge::Left));
        assert!((0x7000..0x9000).contains(frac), "mid-height cursor, got {frac}");
        assert!(pressed.is_empty());
        assert_eq!(f.sink().0, vec![Fx::DragAway("b".into()), Fx::Grab(true), Fx::Hide]);
    }

    /// macOS sends no events once the mouse stops, so a user who pushes to the edge and rests there gives
    /// exactly one sample in the strip. The dwell has to complete on the clock (tick), not on more events.
    #[test]
    fn resting_at_the_edge_switches_on_the_clock_without_more_events() {
        let mut f = fwd(Edge::Right);
        f.on_cursor(right_edge(), 0);
        f.tick(150);
        assert!(matches!(f.mode(), Mode::Local), "dwell not reached yet");
        f.tick(250);
        assert!(matches!(f.mode(), Mode::RemoteOut { .. }), "{:?}", f.mode());
    }

    #[test]
    fn coming_home_into_the_strip_does_not_bounce_back_out_on_the_clock() {
        let mut f = fwd(Edge::Right);
        go_out(&mut f); // left from right_edge(), which is inside the strip
        emergency_chord(&mut f); // home at the exit point, still in the strip
        assert!(matches!(f.mode(), Mode::Local));
        for t in (1000..=5000).step_by(50) {
            f.tick(t);
        }
        assert!(matches!(f.mode(), Mode::Local), "bounced back out with no user motion");
    }

    #[test]
    fn peer_without_link_never_switches() {
        let mut f = Forwarder::new(cfg(Edge::Right), screen(), Rec::default(), Mail::default());
        dwell(&mut f, right_edge(), 0);
        assert!(matches!(f.mode(), Mode::Local));
        assert!(f.out().0.is_empty() && f.sink().0.is_empty());
    }

    #[test]
    fn switch_to_connected_peer_enters_remote_out() {
        let mut f = fwd(Edge::Right);
        assert_eq!(f.switch_to("b", WayBack::Open), SwitchTo::Switched);
        assert!(matches!(f.mode(), Mode::RemoteOut { peer, .. } if peer == "b"));
        assert!(matches!(f.out().0.last(), Some((peer, Msg::Enter { .. })) if peer == "b"));
    }

    fn chord(f: &mut Forwarder<Rec, Mail>) -> Disposition {
        for h in [CTRL, ALT, GUI] {
            key(f, h, true);
        }
        key(f, 0x16, true)
    }

    #[test]
    fn a_switch_through_no_edge_asks_for_a_centered_landing_where_the_peer_reads_it() {
        // From the menu bar icon, top right: the old Enter put the PC's cursor 24 px from the way back.
        let menu = Point { x: W - 200, y: 10 };
        let mut f = fwd(Edge::Right);
        f.peer_speaks("b", CENTERED_PROTO_VERSION);
        f.on_cursor(menu, 0);
        assert_eq!(f.switch_to("b", WayBack::Open), SwitchTo::Switched);
        assert!(matches!(msgs(&f).last(), Some(Msg::EnterCentered { edge: Edge::Left, .. })), "{:?}", msgs(&f));
        assert!(matches!(f.mode(), Mode::RemoteOut { exit_pos, .. } if *exit_pos == menu));

        let mut c = fwd(Edge::Right);
        c.peer_speaks("b", CENTERED_PROTO_VERSION);
        assert_eq!(chord(&mut c), Disposition::Swallow);
        let Some(Msg::EnterCentered { pressed, .. }) = msgs(&c).pop() else { panic!("{:?}", msgs(&c)) };
        assert_eq!(pressed, vec![CTRL, ALT, GUI]);

        // Through the edge it stays an edge entry.
        let mut e = fwd(Edge::Right);
        e.peer_speaks("b", CENTERED_PROTO_VERSION);
        go_out(&mut e);
        assert!(matches!(msgs(&e).last(), Some(Msg::Enter { .. })), "{:?}", msgs(&e));

        // A peer that cannot decode it, or whose version is unknown, gets the plain Enter it understands.
        for speaks in [Some(CENTERED_PROTO_VERSION - 1), None] {
            let mut old = fwd(Edge::Right);
            if let Some(v) = speaks {
                old.peer_speaks("b", v);
            }
            assert_eq!(old.switch_to("b", WayBack::Open), SwitchTo::Switched);
            assert!(matches!(msgs(&old).last(), Some(Msg::Enter { .. })), "{speaks:?}: {:?}", msgs(&old));
        }
    }

    #[test]
    fn a_centered_entry_survives_the_hand_moving_back_toward_the_way_back() {
        let leftward = |f: &mut Forwarder<Rec, Mail>, seq: u32| {
            // The hand drifting back the way it came, 100 px over 300 ms, then at rest.
            for t in 1..=10 {
                f.on_msg("b", Msg::MotionRel { seq, dx: -10, dy: 0 }, t * 30);
            }
            f.tick(1000);
        };
        let mut f = fwd(Edge::Left);
        f.on_msg("b", Msg::EnterCentered { seq: 7, edge: Edge::Left, frac: FRAC_MID, mods: ModifierSet(0), pressed: vec![] }, 0);
        assert!(f.sink().0.contains(&Fx::Warp(Point { x: W / 2, y: H / 2 })), "{:?}", f.sink().0);
        leftward(&mut f, 7);
        assert!(matches!(f.mode(), Mode::RemoteIn { .. }), "{:?}", f.mode());
        assert!(f.out().0.is_empty(), "{:?}", f.out().0);

        // The same hand after an edge entry is back where it came from: what the menu switch used to do.
        let mut edge = fwd(Edge::Left);
        edge.on_msg("b", Msg::Enter { seq: 7, edge: Edge::Left, frac: FRAC_MID, mods: ModifierSet(0), pressed: vec![] }, 0);
        leftward(&mut edge, 7);
        assert!(matches!(edge.mode(), Mode::Local));

        // The edge is still the way back, for a push that means it.
        f.on_msg("b", Msg::MotionRel { seq: 7, dx: -W, dy: 0 }, 2000);
        f.tick(2200);
        assert!(matches!(f.mode(), Mode::Local));
        assert!(matches!(msgs(&f).last(), Some(Msg::Leave { seq: 7, edge: Edge::Right, .. })), "{:?}", msgs(&f));
    }

    #[test]
    fn a_menu_switch_in_flight_holds_the_edges_and_the_chord_but_not_itself() {
        let mut f = fwd(Edge::Right);
        f.hold_for_switch(true, 0);
        dwell(&mut f, right_edge(), 0);
        f.tick(1000);
        assert!(matches!(f.mode(), Mode::Local));
        assert_eq!(chord(&mut f), Disposition::Swallow);
        assert!(matches!(f.mode(), Mode::Local));
        assert!(f.out().0.is_empty() && !f.sink().0.contains(&Fx::Grab(true)), "{:?}", f.out().0);
        assert_eq!(f.switch_to("b", WayBack::Open), SwitchTo::Switched);
        assert!(matches!(f.mode(), Mode::RemoteOut { .. }));

        // Lifted, the edge works again from where the cursor rests.
        let mut g = fwd(Edge::Right);
        g.hold_for_switch(true, 0);
        dwell(&mut g, right_edge(), 0);
        g.hold_for_switch(false, 300);
        dwell(&mut g, right_edge(), 300);
        assert!(matches!(g.mode(), Mode::RemoteOut { .. }));

        // A chord pressed while held switches nothing and leaves the edge armed: it works once lifted.
        let mut k = fwd(Edge::Right);
        k.hold_for_switch(true, 0);
        assert_eq!(chord(&mut k), Disposition::Swallow);
        k.hold_for_switch(false, 100);
        dwell(&mut k, right_edge(), 100);
        assert!(matches!(k.mode(), Mode::RemoteOut { .. }), "{:?}", k.mode());

        // Never lifted, it lapses; holding again each turn does not restart its clock.
        let mut h = fwd(Edge::Right);
        h.hold_for_switch(true, 0);
        h.hold_for_switch(true, SWITCH_HOLD_LIMIT_MS - 1);
        dwell(&mut h, right_edge(), SWITCH_HOLD_LIMIT_MS - 200);
        assert!(matches!(h.mode(), Mode::Local), "still held");
        dwell(&mut h, right_edge(), SWITCH_HOLD_LIMIT_MS);
        assert!(matches!(h.mode(), Mode::RemoteOut { .. }));
    }

    #[test]
    fn a_settings_change_keeps_the_protocol_the_link_speaks() {
        let mut f = fwd(Edge::Right);
        f.peer_speaks("b", CENTERED_PROTO_VERSION);
        go_out(&mut f);
        let mut changed = cfg(Edge::Right);
        changed.peers[0].keys = KeyMap::Position;
        f.set_config(changed);
        assert!(matches!(f.mode(), Mode::Local));
        assert_eq!(f.switch_to("b", WayBack::Open), SwitchTo::Switched);
        assert!(matches!(msgs(&f).last(), Some(Msg::EnterCentered { .. })), "{:?}", msgs(&f));
    }

    #[test]
    fn a_centered_enter_that_cannot_be_taken_is_refused_like_an_enter() {
        let centered = |seq| Msg::EnterCentered { seq, edge: Edge::Left, frac: FRAC_MID, mods: ModifierSet(0), pressed: vec![] };
        let mut busy = fwd(Edge::Right);
        go_out(&mut busy);
        busy.on_msg("b", centered(77), 10);
        assert!(matches!(busy.mode(), Mode::RemoteOut { .. }));
        assert_eq!(msgs(&busy).last(), Some(&Msg::Release { seq: 77, reason: Reason::Busy }));
        // The peer this machine relays into reaches on_enter too, and hears it.
        let mut relay = relay_fwd(KeyMap::None);
        let seq = start_relay(&mut relay, vec![]);
        relay.on_msg("c", centered(50), 1300);
        assert_eq!(relay.out().0.last(), Some(&("c".into(), Msg::Release { seq: 50, reason: Reason::Busy })));
        assert!(matches!(relay.mode(), Mode::Relay { to_seq, .. } if *to_seq == seq));
    }

    #[test]
    fn a_switch_that_moved_every_display_closes_the_way_back_where_the_peer_reads_it() {
        let mut f = fwd(Edge::Right);
        f.peer_speaks("b", CLOSED_PROTO_VERSION);
        assert_eq!(f.switch_to("b", WayBack::Closed), SwitchTo::Switched);
        assert!(matches!(msgs(&f).last(), Some(Msg::EnterCenteredClosed { edge: Edge::Left, .. })), "{:?}", msgs(&f));
        assert!(matches!(f.mode(), Mode::RemoteOut { .. }));

        // Open, a peer that reads protocol 9 still gets the plain centered entry.
        let mut open = fwd(Edge::Right);
        open.peer_speaks("b", CLOSED_PROTO_VERSION);
        assert_eq!(open.switch_to("b", WayBack::Open), SwitchTo::Switched);
        assert!(matches!(msgs(&open).last(), Some(Msg::EnterCentered { .. })), "{:?}", msgs(&open));

        // Older peers get what they decode: their way back stays open.
        for speaks in [Some(CLOSED_PROTO_VERSION - 1), Some(CENTERED_PROTO_VERSION - 1), None] {
            let mut old = fwd(Edge::Right);
            if let Some(v) = speaks {
                old.peer_speaks("b", v);
            }
            assert_eq!(old.switch_to("b", WayBack::Closed), SwitchTo::Switched);
            let last = msgs(&old).pop();
            let centered = speaks.is_some_and(|v| v >= CENTERED_PROTO_VERSION);
            assert!(
                if centered { matches!(last, Some(Msg::EnterCentered { .. })) } else { matches!(last, Some(Msg::Enter { .. })) },
                "{speaks:?}: {last:?}"
            );
        }
    }

    /// Driven from the left, a push all the way through the edge, then the hand at rest. The first step
    /// stays outside the strip, so a dwell that must rearm first (after a relay) can.
    fn push_through_left(f: &mut Forwarder<Rec, Mail>, seq: u32, t: u64) {
        f.on_msg("b", Msg::MotionRel { seq, dx: -10, dy: 0 }, t);
        f.on_msg("b", Msg::MotionRel { seq, dx: -2 * W, dy: 0 }, t + 10);
        f.tick(t + 200);
        f.tick(t + 1000);
    }

    #[test]
    fn a_closed_way_back_keeps_a_push_through_the_edge_here() {
        let closed = |seq| Msg::EnterCenteredClosed { seq, edge: Edge::Left, frac: FRAC_MID, mods: ModifierSet(0), pressed: vec![] };
        let mut f = fwd(Edge::Left);
        f.on_msg("b", closed(7), 0);
        push_through_left(&mut f, 7, 100);
        assert!(matches!(f.mode(), Mode::RemoteIn { pos, .. } if pos.x == 0), "{:?}", f.mode());
        assert!(f.out().0.is_empty(), "{:?}", f.out().0);
        assert!(!f.needs_tick(), "a cursor resting at a closed edge has no dwell to finish");

        // The same push after an open centered entry goes back: the way back is what decides.
        let mut open = fwd(Edge::Left);
        open.on_msg("b", Msg::EnterCentered { seq: 7, edge: Edge::Left, frac: FRAC_MID, mods: ModifierSet(0), pressed: vec![] }, 0);
        push_through_left(&mut open, 7, 100);
        assert!(matches!(open.mode(), Mode::Local), "{:?}", open.mode());
        assert!(matches!(msgs(&open).last(), Some(Msg::Leave { seq: 7, edge: Edge::Right, .. })), "{:?}", msgs(&open));

        // The controller's chord still ends it, and the next session's way back is open again.
        f.on_msg("b", Msg::Release { seq: 7, reason: Reason::UserEscape }, 2000);
        assert!(matches!(f.mode(), Mode::Local));
        f.on_msg("b", Msg::Enter { seq: 8, edge: Edge::Left, frac: FRAC_MID, mods: ModifierSet(0), pressed: vec![] }, 2100);
        push_through_left(&mut f, 8, 2200);
        assert!(matches!(f.mode(), Mode::Local), "{:?}", f.mode());
        assert!(matches!(msgs(&f).last(), Some(Msg::Leave { seq: 8, .. })), "{:?}", msgs(&f));

        // This machine's menu still hands control back while closed.
        let mut menu = fwd(Edge::Left);
        menu.on_msg("b", closed(7), 0);
        assert!(menu.release_control());
        assert!(matches!(menu.mode(), Mode::Local));
        assert!(matches!(msgs(&menu).last(), Some(Msg::Leave { seq: 7, .. })), "{:?}", msgs(&menu));

        // Another peer's entry, refused as busy, leaves this session's way back as it was.
        let mut two = relay_fwd(KeyMap::None);
        two.on_msg("b", closed(7), 0);
        two.on_msg("c", Msg::EnterCentered { seq: 3, edge: Edge::Right, frac: FRAC_MID, mods: ModifierSet(0), pressed: vec![] }, 50);
        assert_eq!(two.out().0.last(), Some(&("c".into(), Msg::Release { seq: 3, reason: Reason::Busy })));
        push_through_left(&mut two, 7, 100);
        assert!(matches!(two.mode(), Mode::RemoteIn { peer, .. } if peer == "b"), "{:?}", two.mode());
    }

    #[test]
    fn a_closed_way_back_survives_a_relay_and_its_return() {
        for (way_back, stays) in [(WayBack::Closed, true), (WayBack::Open, false)] {
            let mut f = relay_fwd(KeyMap::None);
            let enter = match way_back {
                WayBack::Closed => Msg::EnterCenteredClosed { seq: 7, edge: Edge::Left, frac: FRAC_MID, mods: ModifierSet(0), pressed: vec![] },
                WayBack::Open => Msg::EnterCentered { seq: 7, edge: Edge::Left, frac: FRAC_MID, mods: ModifierSet(0), pressed: vec![] },
            };
            f.on_msg("b", enter, 0);
            f.on_msg("b", Msg::MotionRel { seq: 7, dx: 2 * W, dy: 0 }, 1000);
            f.tick(1200);
            let Mode::Relay { to_seq, .. } = f.mode().clone() else { panic!("{way_back:?}: {:?}", f.mode()) };
            f.on_msg("c", Msg::Leave { seq: to_seq, edge: Edge::Left, frac: FRAC_MID }, 1300);
            assert!(matches!(f.mode(), Mode::RemoteIn { .. }), "{way_back:?}: {:?}", f.mode());
            push_through_left(&mut f, 7, 1400);
            assert_eq!(matches!(f.mode(), Mode::RemoteIn { .. }), stays, "{way_back:?}: {:?}", f.mode());
        }
    }

    #[test]
    fn can_switch_to_answers_without_switching() {
        let mut f = fwd(Edge::Right);
        assert_eq!(f.can_switch_to("b"), SwitchTo::Switched);
        assert!(matches!(f.mode(), Mode::Local));
        assert!(f.out().0.is_empty());
        assert_eq!(f.can_switch_to("nope"), SwitchTo::UnknownPeer);
        go_out(&mut f);
        assert_eq!(f.can_switch_to("b"), SwitchTo::NotLocal);
        let down = Forwarder::new(cfg(Edge::Right), screen(), Rec::default(), Mail::default());
        assert_eq!(down.can_switch_to("b"), SwitchTo::PeerDown);
    }

    #[test]
    fn switch_to_disconnected_peer_does_nothing() {
        let mut f = Forwarder::new(cfg(Edge::Right), screen(), Rec::default(), Mail::default());
        assert_eq!(f.switch_to("b", WayBack::Open), SwitchTo::PeerDown);
        assert!(matches!(f.mode(), Mode::Local));
        assert!(f.out().0.is_empty());
    }

    #[test]
    fn switch_to_unknown_peer_does_nothing() {
        let mut f = fwd(Edge::Right);
        assert_eq!(f.switch_to("nope", WayBack::Open), SwitchTo::UnknownPeer);
        assert!(matches!(f.mode(), Mode::Local));
        assert!(f.out().0.is_empty());
    }

    #[test]
    fn switch_to_while_remote_out_does_nothing() {
        let mut f = fwd(Edge::Right);
        go_out(&mut f);
        let mode = f.mode().clone();
        let sent = f.out().0.len();
        assert_eq!(f.switch_to("b", WayBack::Open), SwitchTo::NotLocal);
        assert_eq!(f.mode(), &mode);
        assert_eq!(f.out().0.len(), sent);
    }

    #[test]
    fn release_control_from_remote_in_releases_injected_key() {
        let mut f = fwd(Edge::Left);
        go_in(&mut f, 3, vec![]);
        f.on_msg("b", Msg::Key { seq: 3, hid: 0x04, down: true, repeat: false }, 1);
        assert!(f.release_control());
        assert!(matches!(f.mode(), Mode::Local));
        assert!(matches!(f.out().0.last(), Some((peer, Msg::Leave { seq: 3, .. })) if peer == "b"));
        assert!(f.sink().0.contains(&Fx::Key(0x04, false)));
    }

    #[test]
    fn release_control_from_remote_out_sends_leave() {
        let mut f = fwd(Edge::Right);
        let seq = go_out(&mut f);
        assert!(f.release_control());
        assert!(matches!(f.mode(), Mode::Local));
        assert!(matches!(f.out().0.last(), Some((peer, Msg::Leave { seq: s, .. })) if peer == "b" && *s == seq));
    }

    #[test]
    fn release_control_in_local_does_nothing() {
        let mut f = fwd(Edge::Right);
        assert!(!f.release_control());
        assert!(matches!(f.mode(), Mode::Local));
        assert!(f.out().0.is_empty());
    }

    #[test]
    fn events_after_enter_are_forwarded_in_order_and_swallowed() {
        let mut f = fwd(Edge::Right);
        let seq = go_out(&mut f);
        let evs = [
            CapturedEvent::Motion { dx: 5, dy: -2 },
            CapturedEvent::Button { button: 1, down: true },
            CapturedEvent::Key { hid: 0x04, down: true },
            CapturedEvent::Wheel { dx: 0, dy: 3 },
            CapturedEvent::Button { button: 1, down: false },
        ];
        for e in evs {
            assert_eq!(f.on_captured(e, 300), Disposition::Swallow);
        }
        let m = msgs(&f);
        assert_eq!(
            m[1..],
            [
                Msg::MotionRel { seq, dx: 5, dy: -2 },
                Msg::Button { seq, button: 1, down: true },
                Msg::Key { seq, hid: 0x04, down: true, repeat: false },
                Msg::Wheel { seq, dx: 0, dy: 3 },
                Msg::Button { seq, button: 1, down: false },
            ]
        );
    }

    #[test]
    fn local_mode_passes_input_through_and_sends_nothing() {
        let mut f = fwd(Edge::Right);
        assert_eq!(f.on_captured(CapturedEvent::Motion { dx: 1, dy: 1 }, 0), Disposition::Pass);
        assert_eq!(key(&mut f, 0x04, true), Disposition::Pass);
        assert!(f.out().0.is_empty());
    }

    /// The controller maps its keys per peer before they leave. By position, Command arrives as Alt and
    /// Option as the Windows key: in Enter's held set, in its modifier set, and in every key after.
    #[test]
    fn keys_leave_mapped_for_the_peer() {
        let mut c = cfg(Edge::Right);
        c.peers[0].keys = KeyMap::Position;
        let mut f = Forwarder::new(c, screen(), Rec::default(), Mail::default());
        f.link_up("b");
        key(&mut f, GUI, true); // Command held while crossing
        dwell(&mut f, right_edge(), 0);
        key(&mut f, 0x06, true); // C
        key(&mut f, ALT, true); // Option
        let m = msgs(&f);
        let Msg::Enter { pressed, mods, .. } = &m[0] else { panic!("{m:?}") };
        assert_eq!(pressed, &vec![ALT]);
        assert!(mods.contains(ModifierSet::ALT) && !mods.contains(ModifierSet::GUI), "{mods:?}");
        let sent: Vec<u16> = m.iter().filter_map(|x| if let Msg::Key { hid, .. } = x { Some(*hid) } else { None }).collect();
        assert_eq!(sent, vec![0x06, GUI]);
    }

    /// Mac shortcuts on the peer: ⌘ held while crossing is not pressed there until a key gives it a
    /// meaning; ⌘Tab arrives as Alt+Tab, then ⌘-click as Ctrl+click after Alt is let go.
    #[test]
    fn mac_shortcuts_are_translated_whole_for_the_peer() {
        let mut c = cfg(Edge::Right);
        c.peers[0].keys = KeyMap::Mac;
        let mut f = Forwarder::new(c, screen(), Rec::default(), Mail::default());
        f.link_up("b");
        key(&mut f, GUI, true); // Command held while crossing
        dwell(&mut f, right_edge(), 0);
        key(&mut f, 0x2B, true); // Tab
        key(&mut f, 0x2B, false);
        key(&mut f, GUI, false);
        key(&mut f, GUI, true);
        f.on_captured(CapturedEvent::Button { button: 0, down: true }, 0);
        let m = msgs(&f);
        let Msg::Enter { pressed, .. } = &m[0] else { panic!("{m:?}") };
        assert!(pressed.is_empty(), "a held ⌘ means nothing yet: {pressed:?}");
        let sent: Vec<(u16, bool)> = m.iter().filter_map(|x| if let Msg::Key { hid, down, .. } = x { Some((*hid, *down)) } else { None }).collect();
        assert_eq!(sent, [(ALT, true), (0x2B, true), (0x2B, false), (ALT, false), (CTRL, true)]);
        assert!(matches!(m.last(), Some(Msg::Button { button: 0, down: true, .. })), "Ctrl goes down before the click");
    }

    #[test]
    fn chord_switches_out_and_swallows_the_chord_key() {
        let mut f = fwd(Edge::Right);
        for h in [CTRL, ALT, GUI] {
            assert_eq!(key(&mut f, h, true), Disposition::Pass);
        }
        assert_eq!(key(&mut f, 0x16, true), Disposition::Swallow);
        assert!(matches!(f.mode(), Mode::RemoteOut { .. }));
        let Msg::Enter { pressed, mods, .. } = &msgs(&f)[0] else { panic!() };
        assert_eq!(pressed, &vec![CTRL, ALT, GUI]);
        assert!(mods.contains(ModifierSet::CTRL | ModifierSet::ALT | ModifierSet::GUI));
    }

    #[test]
    fn peer_leave_returns_local_and_warps_to_the_matching_edge() {
        let mut f = fwd(Edge::Right);
        let seq = go_out(&mut f);
        f.on_msg("b", Msg::Leave { seq, edge: Edge::Right, frac: 0x8000 }, 500);
        assert!(matches!(f.mode(), Mode::Local));
        let want = entry_point(&screen(), Edge::Right, 0x8000).unwrap();
        assert_eq!(f.sink().0[3..], [Fx::Grab(false), Fx::Show, Fx::Warp(want)]);
        assert_eq!(f.on_captured(CapturedEvent::Motion { dx: 1, dy: 0 }, 600), Disposition::Pass);
    }

    #[test]
    fn leave_with_a_stale_seq_is_dropped() {
        let mut f = fwd(Edge::Right);
        let seq = go_out(&mut f);
        f.on_msg("b", Msg::Leave { seq: seq - 1, edge: Edge::Right, frac: 0 }, 500);
        f.on_msg("b", Msg::Leave { seq: seq + 1, edge: Edge::Right, frac: 0 }, 500);
        assert!(matches!(f.mode(), Mode::RemoteOut { .. }));
    }

    #[test]
    fn emergency_from_remote_out_sends_leave_then_restores_local() {
        let mut f = fwd(Edge::Right);
        let seq = go_out(&mut f);
        assert_eq!(emergency_chord(&mut f), Disposition::Swallow);
        assert!(matches!(f.mode(), Mode::Local));
        let leaves: Vec<_> = msgs(&f).into_iter().filter(|m| matches!(m, Msg::Leave { .. })).collect();
        assert_eq!(leaves.len(), 1);
        assert!(matches!(leaves[0], Msg::Leave { seq: s, .. } if s == seq));
        let fx = &f.sink().0;
        assert!(fx.contains(&Fx::Grab(false)) && fx.contains(&Fx::Show));
        // The Esc itself never reaches the peer.
        assert!(!msgs(&f).contains(&Msg::Key { seq, hid: ESC, down: true, repeat: false }));
    }

    #[test]
    fn emergency_from_remote_in_tells_the_controller_and_releases_keys() {
        let mut f = fwd(Edge::Left);
        go_in(&mut f, 3, vec![]);
        f.on_msg("b", Msg::Key { seq: 3, hid: 0x04, down: true, repeat: false }, 1);
        emergency_chord(&mut f);
        assert!(matches!(f.mode(), Mode::Local));
        assert!(matches!(msgs(&f).last(), Some(Msg::Leave { seq: 3, .. })));
        assert!(f.sink().0.contains(&Fx::Key(0x04, false)));
        assert_eq!(f.sink().0.last(), Some(&Fx::Assoc(true)));
    }

    #[test]
    fn emergency_in_local_does_nothing() {
        let mut f = fwd(Edge::Right);
        emergency_chord(&mut f);
        assert!(matches!(f.mode(), Mode::Local) && f.out().0.is_empty());
    }

    #[test]
    fn link_down_in_remote_out_frees_the_local_machine_at_once() {
        let mut f = fwd(Edge::Right);
        go_out(&mut f);
        f.link_down("b");
        assert!(matches!(f.mode(), Mode::Local));
        let fx = &f.sink().0;
        assert!(fx.contains(&Fx::Grab(false)) && fx.contains(&Fx::Show));
        assert_eq!(f.on_captured(CapturedEvent::Motion { dx: 1, dy: 1 }, 9), Disposition::Pass);
    }

    #[test]
    fn link_down_of_another_peer_changes_nothing() {
        let mut f = fwd(Edge::Right);
        go_out(&mut f);
        f.link_down("someone-else");
        assert!(matches!(f.mode(), Mode::RemoteOut { .. }));
    }

    #[test]
    fn link_down_in_remote_in_releases_every_key_and_button() {
        let mut f = fwd(Edge::Left);
        go_in(&mut f, 1, vec![]);
        f.on_msg("b", Msg::Key { seq: 1, hid: 0x04, down: true, repeat: false }, 1);
        f.on_msg("b", Msg::Key { seq: 1, hid: 0xE1, down: true, repeat: false }, 1);
        f.on_msg("b", Msg::Button { seq: 1, button: 2, down: true }, 1);
        let before = f.sink().0.len();
        f.link_down("b");
        assert!(matches!(f.mode(), Mode::Local));
        let tail = &f.sink().0[before..];
        for want in [Fx::Key(0x04, false), Fx::Key(0xE1, false), Fx::Button(2, false), Fx::Assoc(true)] {
            assert!(tail.contains(&want), "missing {want:?} in {tail:?}");
        }
    }

    #[test]
    fn removing_the_driven_peer_brings_control_home() {
        let mut f = fwd(Edge::Right);
        go_out(&mut f);
        f.set_config(FwdConfig { peers: vec![], ..cfg(Edge::Right) });
        assert!(matches!(f.mode(), Mode::Local));
        assert!(!f.up.contains("b"));
        assert!(f.sink().0.contains(&Fx::Grab(false)) && f.sink().0.contains(&Fx::Show));
        assert_eq!(f.on_captured(CapturedEvent::Motion { dx: 1, dy: 1 }, 300), Disposition::Pass);
    }

    #[test]
    fn changing_the_driven_peers_key_map_brings_control_home_but_keeps_its_link() {
        let mut f = fwd(Edge::Right);
        go_out(&mut f);
        let mut changed = cfg(Edge::Right);
        changed.peers[0].keys = KeyMap::Position;
        let seq = match f.mode() { Mode::RemoteOut { seq, .. } => *seq, other => panic!("{other:?}") };
        f.set_config(changed);
        assert!(matches!(f.mode(), Mode::Local));
        assert!(f.up.contains("b"));
        assert!(msgs(&f).contains(&Msg::Release { seq, reason: Reason::TopologyChange }), "the peer must hear the switch ended: {:?}", msgs(&f));
        assert!(f.sink().0.contains(&Fx::Grab(false)) && f.sink().0.contains(&Fx::Show));
    }

    #[test]
    fn changing_another_peers_edge_keeps_the_active_session() {
        let mut original = cfg(Edge::Right);
        original.peers.push(PeerSpec { name: "c".into(), edge: Edge::Left, keys: KeyMap::None, gestures: Default::default() });
        let mut f = Forwarder::new(original.clone(), screen(), Rec::default(), Mail::default());
        f.link_up("b");
        f.link_up("c");
        let seq = go_out(&mut f);
        original.peers[1].edge = Edge::Top;
        f.set_config(original);
        assert!(matches!(f.mode(), Mode::RemoteOut { peer, seq: s, .. } if peer == "b" && *s == seq));
        assert!(f.up.contains("b") && f.up.contains("c"));
        assert!(!f.sink().0.contains(&Fx::Grab(false)));
    }

    #[test]
    fn removing_the_driving_peer_releases_every_injected_key_and_button() {
        let mut f = fwd(Edge::Left);
        go_in(&mut f, 1, vec![]);
        for hid in [0x04, 0xE1] {
            f.on_msg("b", Msg::Key { seq: 1, hid, down: true, repeat: false }, 1);
        }
        for button in [1, 2] {
            f.on_msg("b", Msg::Button { seq: 1, button, down: true }, 1);
        }
        let before = f.sink().0.len();
        f.set_config(FwdConfig { peers: vec![], ..cfg(Edge::Left) });
        assert!(matches!(f.mode(), Mode::Local));
        let tail = &f.sink().0[before..];
        for want in [Fx::Key(0x04, false), Fx::Key(0xE1, false), Fx::Button(1, false), Fx::Button(2, false), Fx::Assoc(true)] {
            assert!(tail.contains(&want), "missing {want:?} in {tail:?}");
        }
        assert!(f.inj_keys.is_empty() && f.inj_btn.is_empty());
    }

    #[test]
    fn added_peer_can_be_switched_to_after_link_up() {
        let mut f = fwd(Edge::Right);
        let mut changed = cfg(Edge::Right);
        changed.peers.push(PeerSpec { name: "c".into(), edge: Edge::Left, keys: KeyMap::None, gestures: Default::default() });
        f.set_config(changed);
        f.link_up("c");
        dwell(&mut f, left_edge(), 0);
        assert!(matches!(f.mode(), Mode::RemoteOut { peer, .. } if peer == "c"));
        assert!(matches!(f.out().0.last(), Some((peer, Msg::Enter { .. })) if peer == "c"));
    }

    #[test]
    fn policy_change_restarts_an_active_dwell() {
        let mut f = fwd(Edge::Right);
        f.on_cursor(right_edge(), 0);
        let mut changed = cfg(Edge::Right);
        changed.policy.dwell_ms = 250;
        f.set_config(changed);
        f.tick(200);
        assert!(matches!(f.mode(), Mode::Local));
        f.tick(449);
        assert!(matches!(f.mode(), Mode::Local));
        f.tick(450);
        assert!(matches!(f.mode(), Mode::RemoteOut { .. }));
    }

    #[test]
    fn policy_change_restarts_remote_in_return_dwell() {
        let mut f = fwd(Edge::Left);
        go_in(&mut f, 1, vec![]);
        f.on_msg("b", Msg::MotionRel { seq: 1, dx: -100, dy: 0 }, 1000);
        let mut changed = cfg(Edge::Left);
        changed.policy.dwell_ms = 250;
        f.set_config(changed);
        f.tick(1200);
        assert!(matches!(f.mode(), Mode::RemoteIn { .. }));
        f.tick(1449);
        assert!(matches!(f.mode(), Mode::RemoteIn { .. }));
        f.tick(1450);
        assert!(matches!(f.mode(), Mode::Local));
    }

    #[test]
    fn unchanged_policy_keeps_an_active_dwell() {
        let mut f = fwd(Edge::Right);
        f.on_cursor(right_edge(), 0);
        f.set_config(cfg(Edge::Right));
        f.tick(200);
        assert!(matches!(f.mode(), Mode::RemoteOut { .. }));
    }

    #[test]
    fn enter_warps_detaches_mouse_then_motion_is_injected_in_order() {
        let mut f = fwd(Edge::Left);
        go_in(&mut f, 1, vec![]);
        let want = entry_point(&screen(), Edge::Left, 0x8000).unwrap();
        assert_eq!(f.sink().0, vec![Fx::Warp(want), Fx::Assoc(false)]);
        f.on_msg("b", Msg::MotionRel { seq: 1, dx: 4, dy: 1 }, 1);
        f.on_msg("b", Msg::Button { seq: 1, button: 1, down: true }, 1);
        f.on_msg("b", Msg::Wheel { seq: 1, dx: 0, dy: -1 }, 1);
        assert_eq!(
            f.sink().0[2..],
            [Fx::Motion(4, 1), Fx::Button(1, true), Fx::Wheel(0, -1)]
        );
    }

    #[test]
    fn stale_enter_and_old_session_messages_are_dropped() {
        let mut f = fwd(Edge::Left);
        go_in(&mut f, 5, vec![]);
        f.on_msg("b", Msg::Leave { seq: 5, edge: Edge::Right, frac: 0 }, 1);
        assert!(matches!(f.mode(), Mode::Local));
        // An Enter older than (or equal to) one already accepted is a replay.
        for old in [5, 4] {
            f.on_msg("b", Msg::Enter { seq: old, edge: Edge::Left, frac: 0, mods: ModifierSet(0), pressed: vec![] }, 2);
            assert!(matches!(f.mode(), Mode::Local), "enter seq {old} accepted");
        }
        go_in(&mut f, 6, vec![]);
        let n = f.sink().0.len();
        f.on_msg("b", Msg::MotionRel { seq: 5, dx: 9, dy: 9 }, 3);
        f.on_msg("b", Msg::Key { seq: 5, hid: 0x04, down: true, repeat: false }, 3);
        assert_eq!(f.sink().0.len(), n, "old-session traffic must not be injected");
        f.on_msg("b", Msg::MotionRel { seq: 6, dx: 9, dy: 9 }, 3);
        assert_eq!(f.sink().0.len(), n + 1);
    }

    #[test]
    fn peer_restart_resets_the_enter_seq_floor() {
        let mut f = fwd(Edge::Left);
        go_in(&mut f, 50, vec![]);
        f.on_msg("b", Msg::Leave { seq: 50, edge: Edge::Right, frac: 0 }, 1);
        f.link_down("b");
        f.link_up("b");
        go_in(&mut f, 1, vec![]);
    }

    #[test]
    fn enter_while_already_controlling_is_ignored() {
        let mut f = fwd(Edge::Right);
        let seq = go_out(&mut f);
        f.on_msg("b", Msg::Enter { seq: 1, edge: Edge::Right, frac: 0, mods: ModifierSet(0), pressed: vec![] }, 400);
        assert!(matches!(f.mode(), Mode::RemoteOut { seq: s, .. } if *s == seq));
    }

    #[test]
    fn keys_held_at_enter_are_pressed_here_and_released_on_leave() {
        let mut f = fwd(Edge::Left);
        go_in(&mut f, 1, vec![0xE3]);
        assert!(f.sink().0.contains(&Fx::Key(0xE3, true)));
        f.on_msg("b", Msg::Leave { seq: 1, edge: Edge::Right, frac: 0 }, 1);
        assert!(f.sink().0.contains(&Fx::Key(0xE3, false)));
    }

    #[test]
    fn stale_injected_key_not_in_pressed_gets_a_key_up_on_enter() {
        let mut f = fwd(Edge::Left);
        f.inj_keys.insert(0x04); // a key left behind by an earlier session
        go_in(&mut f, 1, vec![0xE1]);
        assert!(f.sink().0.contains(&Fx::Key(0x04, false)));
        assert!(f.sink().0.contains(&Fx::Key(0xE1, true)));
    }

    #[test]
    fn return_edge_dwell_sends_leave_and_goes_local_without_bouncing() {
        let mut f = fwd(Edge::Left);
        go_in(&mut f, 7, vec![]);
        // Push the cursor into the left wall and hold it there.
        f.on_msg("b", Msg::MotionRel { seq: 7, dx: -100, dy: 0 }, 1000);
        f.tick(1199);
        assert!(matches!(f.mode(), Mode::RemoteIn { .. }), "dwell not reached");
        f.tick(1200);
        assert!(matches!(f.mode(), Mode::Local));
        let Some(Msg::Leave { seq, edge, frac }) = msgs(&f).pop() else { panic!() };
        assert_eq!((seq, edge), (7, Edge::Right));
        assert!((0x7000..0x9000).contains(&frac));
        assert_eq!(f.sink().0.last(), Some(&Fx::Assoc(true)));
        // Cursor is still parked in the strip: dwell alone must not send it back out.
        f.on_cursor(left_edge(), 2000);
        f.on_cursor(left_edge(), 2500);
        assert!(matches!(f.mode(), Mode::Local));
    }

    #[test]
    fn release_in_remote_in_lets_go_of_every_injected_key_and_button() {
        let mut f = fwd(Edge::Left);
        go_in(&mut f, 4, vec![]);
        f.on_msg("b", Msg::Key { seq: 4, hid: 0x04, down: true, repeat: false }, 1);
        f.on_msg("b", Msg::Button { seq: 4, button: 1, down: true }, 1);
        let before = f.sink().0.len();
        f.on_msg("b", Msg::Release { seq: 4, reason: Reason::Timeout }, 2);
        assert!(matches!(f.mode(), Mode::Local), "{:?}", f.mode());
        let tail = &f.sink().0[before..];
        for want in [Fx::Key(0x04, false), Fx::Button(1, false), Fx::Assoc(true)] {
            assert!(tail.contains(&want), "missing {want:?} in {tail:?}");
        }
    }

    #[test]
    fn release_from_the_driven_peer_frees_the_controller() {
        let mut f = fwd(Edge::Right);
        let seq = go_out(&mut f);
        f.on_msg("b", Msg::Release { seq, reason: Reason::Shutdown }, 500);
        assert!(matches!(f.mode(), Mode::Local), "{:?}", f.mode());
        let fx = &f.sink().0;
        assert!(fx.contains(&Fx::Grab(false)) && fx.contains(&Fx::Show), "{fx:?}");
    }

    #[test]
    fn release_for_another_session_changes_nothing() {
        let mut f = fwd(Edge::Left);
        go_in(&mut f, 4, vec![]);
        f.on_msg("b", Msg::Release { seq: 3, reason: Reason::Timeout }, 2);
        f.on_msg("someone-else", Msg::Release { seq: 4, reason: Reason::Timeout }, 2);
        assert!(matches!(f.mode(), Mode::RemoteIn { .. }));
    }

    #[test]
    fn mod_sync_reconciles_injected_keys_with_the_controller() {
        let mut f = fwd(Edge::Left);
        go_in(&mut f, 1, vec![0xE1]);
        f.on_msg("b", Msg::ModSync { seq: 1, mods: ModifierSet(0), pressed: vec![0xE0] }, 1);
        let fx = &f.sink().0;
        assert!(fx.contains(&Fx::Key(0xE1, false)), "{fx:?}");
        assert!(fx.contains(&Fx::Key(0xE0, true)), "{fx:?}");
        // A stale-session ModSync is ignored.
        let n = f.sink().0.len();
        f.on_msg("b", Msg::ModSync { seq: 0, mods: ModifierSet(0), pressed: vec![] }, 1);
        assert_eq!(f.sink().0.len(), n);
    }

    #[test]
    fn topology_change_while_controlling_releases_the_peer_and_comes_home() {
        let mut f = fwd(Edge::Right);
        let seq = go_out(&mut f);
        // The display the cursor left from is unplugged; only a smaller one remains.
        let small = vec![Rect { x: 0, y: 0, w: 800, h: 600 }];
        f.set_displays(small.clone());
        assert!(matches!(f.mode(), Mode::Local), "{:?}", f.mode());
        assert!(msgs(&f).contains(&Msg::Release { seq, reason: Reason::TopologyChange }), "{:?}", msgs(&f));
        let Some(Fx::Warp(p)) = f.sink().0.iter().rev().find(|x| matches!(x, Fx::Warp(_))).cloned() else {
            panic!("no warp: {:?}", f.sink().0)
        };
        assert!(small.iter().any(|r| contains(r, p)), "restored off-screen at {p:?}");
    }

    #[test]
    fn identical_display_refresh_is_not_a_topology_change() {
        let mut f = fwd(Edge::Right);
        go_out(&mut f);
        f.set_displays(screen());
        assert!(matches!(f.mode(), Mode::RemoteOut { .. }));
    }

    #[test]
    fn topology_change_while_driven_keeps_the_virtual_cursor_on_a_display() {
        let mut f = fwd(Edge::Left);
        go_in(&mut f, 1, vec![]);
        let moved = vec![Rect { x: 2000, y: 0, w: 1024, h: 768 }];
        f.set_displays(moved.clone());
        let Mode::RemoteIn { pos, .. } = f.mode().clone() else { panic!("{:?}", f.mode()) };
        assert!(moved.iter().any(|r| contains(r, pos)), "virtual cursor stranded at {pos:?}");
        // And it still moves.
        f.on_msg("b", Msg::MotionRel { seq: 1, dx: 5, dy: 0 }, 2);
        let Mode::RemoteIn { pos: after, .. } = f.mode().clone() else { panic!() };
        assert_ne!(after, pos);
    }

    #[test]
    fn widest_allowed_strip_does_not_bounce_the_cursor_back_on_entry() {
        let mut c = cfg(Edge::Left);
        c.boundary_px = crate::config::BOUNDARY_PX_MAX;
        let mut f = Forwarder::new(c, screen(), Rec::default(), Mail::default());
        f.link_up("b");
        go_in(&mut f, 1, vec![]);
        for t in (0..=2000).step_by(100) {
            f.tick(t);
        }
        assert!(matches!(f.mode(), Mode::RemoteIn { .. }), "bounced back with no user motion");
    }

    #[test]
    fn config_maps_peers_and_rejects_a_bad_chord() {
        let mut c = Config::default();
        c.peers.push(crate::config::Peer {
            name: "b".into(),
            addr: "x:1".into(),
            edge: crate::config::Edge::Top,
            fingerprint: "0".repeat(64),
            keys: crate::config::KeyMap::Mac,
            gestures: crate::config::GestureMap::Windows,
            displays: Vec::new(),
        });
        let f = FwdConfig::from_config(&c).unwrap();
        assert_eq!(f.peers, vec![PeerSpec { name: "b".into(), edge: Edge::Top, keys: crate::config::KeyMap::Mac, gestures: GestureMap::Windows }]);
        c.switch.chord = "bogus".into();
        assert!(FwdConfig::from_config(&c).is_err());
    }

    fn relay_fwd(map: KeyMap) -> Forwarder<Rec, Mail> {
        let mut config = cfg(Edge::Left);
        config.peers.push(PeerSpec { name: "c".into(), edge: Edge::Right, keys: map, gestures: Default::default() });
        let mut f = Forwarder::new(config, screen(), Rec::default(), Mail::default());
        f.link_up("b");
        f.link_up("c");
        f
    }

    fn start_relay(f: &mut Forwarder<Rec, Mail>, pressed: Vec<u16>) -> u32 {
        go_in(f, 7, pressed);
        f.on_msg("b", Msg::MotionRel { seq: 7, dx: 2000, dy: 0 }, 1000);
        f.tick(1200);
        match f.mode() {
            Mode::Relay { from, from_seq, to, to_seq, .. } => {
                assert_eq!((from.as_str(), *from_seq, to.as_str()), ("b", 7, "c"));
                *to_seq
            }
            other => panic!("expected Relay: {other:?}"),
        }
    }

    #[test]
    fn remote_in_local_motion_takes_over_at_eight_pixels() {
        let mut f = fwd(Edge::Left);
        go_in(&mut f, 7, vec![0x04]);
        for (dx, at) in [(3, 10), (3, 20)] {
            assert_eq!(f.on_captured(CapturedEvent::Motion { dx, dy: 0 }, at), Disposition::Pass);
            assert!(matches!(f.mode(), Mode::RemoteIn { .. }));
        }
        assert_eq!(f.on_captured(CapturedEvent::Motion { dx: 3, dy: 0 }, 30), Disposition::Pass);
        assert!(matches!(f.mode(), Mode::Local));
        assert_eq!(f.out().0.last(), Some(&("b".into(), Msg::Release { seq: 7, reason: Reason::UserEscape })));
        assert!(f.sink().0.ends_with(&[Fx::Key(0x04, false), Fx::Assoc(true)]));
    }

    #[test]
    fn remote_in_motion_gap_restarts_takeover_run() {
        let mut f = fwd(Edge::Left);
        go_in(&mut f, 7, vec![]);
        for at in [10, 410, 810, 1210] {
            assert_eq!(f.on_captured(CapturedEvent::Motion { dx: 3, dy: 0 }, at), Disposition::Pass);
            assert!(matches!(f.mode(), Mode::RemoteIn { .. }));
        }
        assert!(f.out().0.is_empty());
    }

    #[test]
    fn remote_in_button_wheel_and_key_press_take_over_but_releases_do_not() {
        for event in [
            CapturedEvent::Button { button: 1, down: true },
            CapturedEvent::Wheel { dx: 0, dy: -1 },
            CapturedEvent::Key { hid: 0x04, down: true },
        ] {
            let mut f = fwd(Edge::Left);
            go_in(&mut f, 7, vec![]);
            assert_eq!(f.on_captured(CapturedEvent::Button { button: 1, down: false }, 1), Disposition::Pass);
            assert_eq!(f.on_captured(CapturedEvent::Key { hid: 0x04, down: false }, 2), Disposition::Pass);
            assert!(matches!(f.mode(), Mode::RemoteIn { .. }));
            assert_eq!(f.on_captured(event, 3), Disposition::Pass);
            assert!(matches!(f.mode(), Mode::Local));
            assert_eq!(f.out().0.last(), Some(&("b".into(), Msg::Release { seq: 7, reason: Reason::UserEscape })));
        }
    }

    #[test]
    fn relay_local_motion_releases_driver_and_continues_to_target() {
        let mut f = relay_fwd(KeyMap::Mac);
        assert_eq!(key(&mut f, GUI, true), Disposition::Pass);
        assert_eq!(key(&mut f, 0x06, true), Disposition::Pass);
        let seq = start_relay(&mut f, vec![0x04]);
        assert_eq!(f.on_captured(CapturedEvent::Motion { dx: 3, dy: 0 }, 1300), Disposition::Pass);
        assert!(matches!(f.mode(), Mode::Relay { .. }));
        assert_eq!(f.on_captured(CapturedEvent::Motion { dx: 5, dy: 0 }, 1350), Disposition::Swallow);
        assert!(matches!(f.mode(), Mode::RemoteOut { peer, seq: s, exit_pos } if peer == "c" && *s == seq && *exit_pos == right_edge()));
        assert!(f.out().0.ends_with(&[
            ("b".into(), Msg::Release { seq: 7, reason: Reason::UserEscape }),
            ("c".into(), Msg::ModSync { seq, mods: ModifierSet(ModifierSet::CTRL), pressed: vec![0x06, CTRL] }),
            ("c".into(), Msg::MotionRel { seq, dx: 5, dy: 0 }),
        ]), "{:?}", f.out().0);
        assert_eq!(f.sink().0.last(), Some(&Fx::Grab(true)));
        f.on_msg("c", Msg::Leave { seq, edge: Edge::Left, frac: FRAC_MID }, 1400);
        assert!(matches!(f.mode(), Mode::Local));
        let home = entry_point(&screen(), Edge::Right, FRAC_MID).unwrap();
        assert!(f.sink().0.ends_with(&[Fx::Grab(false), Fx::Show, Fx::Warp(home)]));
    }

    #[test]
    fn relay_local_key_press_takes_over_and_reaches_target_mapped() {
        let mut f = relay_fwd(KeyMap::Mac);
        assert_eq!(key(&mut f, GUI, true), Disposition::Pass);
        let seq = start_relay(&mut f, vec![]);
        let before = f.out().0.len();
        assert_eq!(f.on_captured(CapturedEvent::Key { hid: 0x14, down: true }, 1300), Disposition::Swallow);
        assert!(matches!(f.mode(), Mode::RemoteOut { peer, seq: s, .. } if peer == "c" && *s == seq));
        let sent = &f.out().0[before..];
        assert_eq!(sent[0], ("b".into(), Msg::Release { seq: 7, reason: Reason::UserEscape }));
        // Cmd+Q on this Mac is Alt+F4 there, and it must happen once: the triggering Q is not also in the sync.
        assert!(matches!(&sent[1], (p, Msg::ModSync { seq: s, pressed, .. }) if p == "c" && *s == seq && !pressed.contains(&0x3D)), "{sent:?}");
        let f4_presses = sent.iter().filter(|(p, m)| p == "c" && matches!(m, Msg::Key { hid: 0x3D, down: true, .. })).count();
        assert_eq!(f4_presses, 1, "{sent:?}");
        // Without a key map, a plain key that takes over is typed once.
        let mut g = relay_fwd(KeyMap::None);
        let seq = start_relay(&mut g, vec![]);
        let before = g.out().0.len();
        g.on_captured(CapturedEvent::Key { hid: 0x04, down: true }, 1300);
        assert_eq!(&g.out().0[before + 1..], &[
            ("c".into(), Msg::ModSync { seq, mods: ModifierSet(0), pressed: vec![] }),
            ("c".into(), Msg::Key { seq, hid: 0x04, down: true, repeat: false }),
        ]);
    }

    #[test]
    fn relay_start_maps_held_keys_and_blocks_every_held_button() {
        let mut f = relay_fwd(KeyMap::Mac);
        let seq = start_relay(&mut f, vec![GUI, 0x06]);
        assert_eq!(seq, 1);
        assert!(f.out().0.contains(&("c".into(), Msg::Enter {
            seq, edge: Edge::Left, frac: frac_at(&screen(), right_edge(), Edge::Right),
            mods: ModifierSet(ModifierSet::CTRL), pressed: vec![0x06, CTRL],
        })), "{:?}", f.out().0);
        assert!(f.sink().0.ends_with(&[Fx::Key(0x06, false), Fx::Key(GUI, false), Fx::Hide])
            || f.sink().0.ends_with(&[Fx::Key(GUI, false), Fx::Key(0x06, false), Fx::Hide]));
        assert!(!f.sink().0.contains(&Fx::Assoc(true)));
        for button in [1, 2] {
            let mut held = relay_fwd(KeyMap::None);
            go_in(&mut held, 7, vec![]);
            held.on_msg("b", Msg::Button { seq: 7, button, down: true }, 1);
            held.on_msg("b", Msg::MotionRel { seq: 7, dx: 2000, dy: 0 }, 1000);
            held.tick(1200);
            assert!(matches!(held.mode(), Mode::RemoteIn { .. }), "button {button}");
            assert!(held.out().0.is_empty());
            held.on_msg("b", Msg::Button { seq: 7, button, down: false }, 1201);
            held.tick(1201);
            held.tick(1401);
            assert!(matches!(held.mode(), Mode::Relay { .. }));
        }
    }

    #[test]
    fn relay_forwards_input_and_maps_mac_keys_without_touching_sink() {
        let mut f = relay_fwd(KeyMap::Mac);
        let seq = start_relay(&mut f, vec![]);
        let before = f.sink().0.len();
        f.on_msg("b", Msg::MotionRel { seq: 7, dx: 4, dy: -2 }, 1300);
        f.on_msg("b", Msg::Key { seq: 7, hid: GUI, down: true, repeat: false }, 1300);
        f.on_msg("b", Msg::Key { seq: 7, hid: 0x06, down: true, repeat: false }, 1300);
        f.on_msg("b", Msg::Button { seq: 7, button: 1, down: true }, 1300);
        f.on_msg("b", Msg::Wheel { seq: 7, dx: 0, dy: -1 }, 1300);
        f.on_msg("b", Msg::ModSync { seq: 7, mods: ModifierSet(0), pressed: vec![GUI] }, 1300);
        let sent: Vec<_> = f.out().0.iter().filter(|(p, _)| p == "c").map(|(_, m)| m.clone()).collect();
        assert_eq!(&sent[1..], &[
            Msg::MotionRel { seq, dx: 4, dy: -2 },
            Msg::Key { seq, hid: CTRL, down: true, repeat: false },
            Msg::Key { seq, hid: 0x06, down: true, repeat: false },
            Msg::Button { seq, button: 1, down: true },
            Msg::Wheel { seq, dx: 0, dy: -1 },
            Msg::ModSync { seq, mods: ModifierSet(0), pressed: vec![] },
        ]);
        assert_eq!(f.sink().0.len(), before);
        assert_eq!(f.on_captured(CapturedEvent::Motion { dx: 1, dy: 1 }, 1300), Disposition::Pass);
        f.tick(9999);
        assert!(matches!(f.mode(), Mode::Relay { keys, exit_pos, .. } if keys == &BTreeSet::from([GUI]) && *exit_pos == right_edge()));
        f.on_msg("b", Msg::Key { seq: 6, hid: 0x04, down: true, repeat: false }, 1300);
        f.on_msg("c", Msg::MotionRel { seq, dx: 9, dy: 9 }, 1300);
        assert_eq!(f.out().0.len(), sent.len(), "stale or reversed traffic is dropped");
        // The peer we relay into trying to drive us is refused, not left holding its grab.
        f.on_msg("c", Msg::Enter { seq: 40, edge: Edge::Right, frac: FRAC_MID, mods: ModifierSet(0), pressed: vec![] }, 1300);
        assert_eq!(f.out().0.last(), Some(&("c".into(), Msg::Release { seq: 40, reason: Reason::Busy })));
        assert!(matches!(f.mode(), Mode::Relay { .. }), "{:?}", f.mode());
    }

    #[test]
    fn relay_leave_from_target_restores_driving_keys_and_disarms_return() {
        let mut f = relay_fwd(KeyMap::None);
        let seq = start_relay(&mut f, vec![0x04]);
        let at = entry_point(&screen(), Edge::Right, 0x4000).unwrap();
        f.on_msg("c", Msg::Leave { seq, edge: Edge::Left, frac: 0x4000 }, 1300);
        assert!(matches!(f.mode(), Mode::RemoteIn { peer, seq: 7, pos, ret, .. } if peer == "b" && *pos == at && !ret.armed));
        assert!(f.sink().0.ends_with(&[Fx::Warp(at), Fx::Show, Fx::Key(0x04, true)]));
        f.tick(2000);
        assert!(matches!(f.mode(), Mode::RemoteIn { .. }));
    }

    #[test]
    fn relay_target_release_or_link_drop_restores_exit_position() {
        for drop_link in [false, true] {
            let mut f = relay_fwd(KeyMap::None);
            let seq = start_relay(&mut f, vec![0x04]);
            if drop_link { f.link_down("c"); }
            else { f.on_msg("c", Msg::Release { seq, reason: Reason::Busy }, 1300); }
            assert!(matches!(f.mode(), Mode::RemoteIn { pos, .. } if *pos == right_edge()));
            assert!(f.sink().0.ends_with(&[Fx::Warp(right_edge()), Fx::Show, Fx::Key(0x04, true)]));
            assert_eq!(f.out().0.len(), 1);
        }
    }

    #[test]
    fn relay_driver_leave_release_or_link_drop_releases_target() {
        for case in 0..3 {
            let mut f = relay_fwd(KeyMap::None);
            let seq = start_relay(&mut f, vec![]);
            match case {
                0 => f.on_msg("b", Msg::Leave { seq: 7, edge: Edge::Right, frac: FRAC_MID }, 1300),
                1 => f.on_msg("b", Msg::Release { seq: 7, reason: Reason::Emergency }, 1300),
                _ => f.link_down("b"),
            }
            let reason = if case == 2 { Reason::Shutdown } else { Reason::UserEscape };
            assert_eq!(f.out().0.last(), Some(&("c".into(), Msg::Release { seq, reason })));
            assert!(matches!(f.mode(), Mode::Local));
            assert!(f.sink().0.ends_with(&[Fx::Show, Fx::Assoc(true)]));
            assert_eq!(f.last_pos, right_edge());
        }
    }

    #[test]
    fn relay_newer_enter_releases_target_and_accepts_new_session() {
        let mut f = relay_fwd(KeyMap::None);
        let seq = start_relay(&mut f, vec![]);
        f.on_msg("b", Msg::Enter { seq: 8, edge: Edge::Left, frac: FRAC_MID, mods: ModifierSet(0), pressed: vec![0x05] }, 1300);
        assert_eq!(f.out().0.last(), Some(&("c".into(), Msg::Release { seq, reason: Reason::TopologyChange })));
        assert!(matches!(f.mode(), Mode::RemoteIn { peer, seq: 8, .. } if peer == "b"));
        assert!(f.sink().0.contains(&Fx::Key(0x05, true)));
    }

    #[test]
    fn relay_refuses_third_peer_enter_as_busy() {
        let mut f = relay_fwd(KeyMap::None);
        let seq = start_relay(&mut f, vec![]);
        f.on_msg("d", Msg::Enter { seq: 9, edge: Edge::Top, frac: FRAC_MID, mods: ModifierSet(0), pressed: vec![] }, 1300);
        assert_eq!(f.out().0.last(), Some(&("d".into(), Msg::Release { seq: 9, reason: Reason::Busy })));
        assert!(matches!(f.mode(), Mode::Relay { to_seq, .. } if *to_seq == seq));
    }

    #[test]
    fn relay_local_emergency_releases_target_and_leaves_driver() {
        let mut f = relay_fwd(KeyMap::None);
        let seq = start_relay(&mut f, vec![]);
        emergency_chord(&mut f);
        assert!(f.out().0.ends_with(&[
            ("c".into(), Msg::Release { seq, reason: Reason::UserEscape }),
            ("b".into(), Msg::Leave { seq: 7, edge: Edge::Right, frac: FRAC_MID }),
        ]));
        assert!(matches!(f.mode(), Mode::Local));
        assert!(f.sink().0.ends_with(&[Fx::Show, Fx::Assoc(true)]));
    }

    #[test]
    fn relay_display_change_releases_target_and_restores_clamped_cursor() {
        let mut f = relay_fwd(KeyMap::None);
        let seq = start_relay(&mut f, vec![0x04]);
        f.set_displays(screen());
        assert!(matches!(f.mode(), Mode::Relay { .. }));
        let smaller = vec![Rect { x: 0, y: 0, w: 800, h: 600 }];
        let at = clamp_inside(&smaller, right_edge()).unwrap();
        f.set_displays(smaller);
        assert_eq!(f.out().0.last(), Some(&("c".into(), Msg::Release { seq, reason: Reason::TopologyChange })));
        assert!(matches!(f.mode(), Mode::RemoteIn { pos, .. } if *pos == at));
        assert!(f.sink().0.ends_with(&[Fx::Warp(at), Fx::Show, Fx::Key(0x04, true)]));
    }

    #[test]
    fn relay_config_changes_release_target_and_preserve_or_end_driver() {
        for change_from in [false, true] {
            let mut f = relay_fwd(KeyMap::None);
            let seq = start_relay(&mut f, vec![0x04]);
            let mut new = f.cfg.clone();
            if change_from { new.peers[0].edge = Edge::Top; }
            else { new.peers[1].keys = KeyMap::Position; }
            f.set_config(new);
            assert!(f.out().0.contains(&("c".into(), Msg::Release { seq, reason: Reason::TopologyChange })));
            if change_from {
                assert!(f.out().0.contains(&("b".into(), Msg::Release { seq: 7, reason: Reason::TopologyChange })));
                assert!(matches!(f.mode(), Mode::Local));
                assert!(f.sink().0.contains(&Fx::Assoc(true)));
            } else {
                assert!(matches!(f.mode(), Mode::RemoteIn { peer, .. } if peer == "b"));
                assert!(f.sink().0.ends_with(&[Fx::Warp(right_edge()), Fx::Show, Fx::Key(0x04, true)]));
            }
        }
        let mut removed = relay_fwd(KeyMap::None);
        let seq = start_relay(&mut removed, vec![]);
        let mut new = removed.cfg.clone();
        new.peers.pop();
        removed.set_config(new);
        assert_eq!(removed.out().0.last(), Some(&("c".into(), Msg::Release { seq, reason: Reason::TopologyChange })));
        assert!(matches!(removed.mode(), Mode::RemoteIn { .. }));
    }

    #[test]
    fn abort_switch_does_not_interrupt_relay() {
        let mut f = relay_fwd(KeyMap::None);
        let seq = start_relay(&mut f, vec![]);
        f.abort_switch();
        assert!(matches!(f.mode(), Mode::Relay { to_seq, .. } if *to_seq == seq));
        assert_eq!(f.out().0.len(), 1);
    }

    const LEFT_ARROW: u16 = 0x50;
    const TAB: u16 = 0x2B;

    fn chord_msgs(seq: u32, keys: &[u16]) -> Vec<Msg> {
        let key = |hid, down| Msg::Key { seq, hid, down, repeat: false };
        keys.iter().map(|&k| key(k, true)).chain(keys.iter().rev().map(|&k| key(k, false))).collect()
    }

    #[test]
    fn a_gesture_goes_as_itself_only_to_a_mac_that_decodes_it() {
        let mut f = fwd(Edge::Right);
        f.peer_speaks("b", GESTURE_PROTO_VERSION);
        let seq = go_out(&mut f);
        let n = f.out().0.len();
        assert_eq!(f.on_captured(CapturedEvent::Gesture(Gesture::SpaceRight), 300), Disposition::Swallow);
        assert_eq!(&msgs(&f)[n..], &[Msg::Gesture { seq, gesture: Gesture::SpaceRight }]);

        // An older Mac would end the link on a Gesture it cannot decode, and its macOS shortcut would reach a
        // PC it relays to as other keys: it gets nothing.
        let mut old = fwd(Edge::Right);
        old.peer_speaks("b", GESTURE_PROTO_VERSION - 1);
        let mut relinked = fwd(Edge::Right);
        relinked.peer_speaks("b", GESTURE_PROTO_VERSION);
        relinked.link_down("b");
        relinked.link_up("b");
        for (what, mut f) in [("older protocol", old), ("never told", fwd(Edge::Right)), ("relinked, not told again", relinked)] {
            go_out(&mut f);
            let n = f.out().0.len();
            assert_eq!(f.on_captured(CapturedEvent::Gesture(Gesture::SpaceRight), 300), Disposition::Swallow, "{what}");
            assert_eq!(f.out().0.len(), n, "{what}");
        }
    }

    #[test]
    fn a_pc_gets_the_windows_shortcut_whatever_it_speaks() {
        const D: u16 = 0x07;
        // Swiping down is the one gesture the habits differ on; linked, the habit is the key mapping's.
        for (map, gestures, down) in [
            (KeyMap::Position, GestureMap::Linked, D),
            (KeyMap::Mac, GestureMap::Linked, TAB),
            (KeyMap::Position, GestureMap::Mac, TAB),
            (KeyMap::Mac, GestureMap::Windows, D),
        ] {
            let mut config = cfg(Edge::Right);
            config.peers[0].keys = map;
            config.peers[0].gestures = gestures;
            let mut f = Forwarder::new(config, screen(), Rec::default(), Mail::default());
            f.link_up("b");
            f.peer_speaks("b", GESTURE_PROTO_VERSION);
            let seq = go_out(&mut f);
            let n = f.out().0.len();
            f.on_captured(CapturedEvent::Gesture(Gesture::Zoom(2)), 300);
            f.on_captured(CapturedEvent::Gesture(Gesture::Overview), 300);
            f.on_captured(CapturedEvent::Gesture(Gesture::AppWindows), 300);
            let mut want = vec![
                Msg::Key { seq, hid: CTRL, down: true, repeat: false },
                Msg::Wheel { seq, dx: 0, dy: 2 },
                Msg::Key { seq, hid: CTRL, down: false, repeat: false },
            ];
            want.extend(chord_msgs(seq, &[GUI, TAB]));
            want.extend(chord_msgs(seq, &[ESC]));
            assert_eq!(&msgs(&f)[n..], want, "{map:?} {gestures:?}");

            // A click in Task View picked a window: the next swipe down opens something rather than closing.
            f.on_captured(CapturedEvent::Gesture(Gesture::Overview), 300);
            f.on_captured(CapturedEvent::Button { button: LEFT, down: true }, 300);
            let n = f.out().0.len();
            f.on_captured(CapturedEvent::Gesture(Gesture::AppWindows), 300);
            assert_eq!(&msgs(&f)[n..], chord_msgs(seq, &[GUI, down]), "{map:?} {gestures:?}");
        }
    }

    #[test]
    fn a_gesture_with_a_modifier_held_does_nothing_anywhere() {
        // Controlling: ⌘ held while pinching. The shortcut's own ⌘ up would release the user's hold there.
        let mut config = cfg(Edge::Right);
        config.peers[0].keys = KeyMap::Position;
        let mut out = Forwarder::new(config, screen(), Rec::default(), Mail::default());
        out.link_up("b");
        let seq = go_out(&mut out);
        key(&mut out, GUI, true);
        let n = out.out().0.len();
        assert_eq!(out.on_captured(CapturedEvent::Gesture(Gesture::Zoom(1)), 300), Disposition::Swallow);
        assert_eq!(out.out().0.len(), n, "nothing sent while ⌘ is held");
        key(&mut out, GUI, false);
        let n = out.out().0.len();
        out.on_captured(CapturedEvent::Gesture(Gesture::SmartZoom), 300);
        assert_eq!(&msgs(&out)[n..], chord_msgs(seq, &[CTRL, 0x27]), "and sent again once it is up");

        // Driven: the controller's ⌘ is held here.
        let mut driven = fwd(Edge::Left);
        go_in(&mut driven, 7, vec![GUI]);
        let n = driven.sink().0.len();
        driven.on_msg("b", Msg::Gesture { seq: 7, gesture: Gesture::Zoom(1) }, 10);
        assert_eq!(driven.sink().0.len(), n);
        assert!(driven.inj_keys.contains(&GUI), "the hold is intact");

        // Relaying, with the controller holding ⌃.
        let mut relay = relay_fwd(KeyMap::Position);
        relay.peer_speaks("c", GESTURE_PROTO_VERSION);
        start_relay(&mut relay, vec![CTRL]);
        let n = relay.out().0.len();
        relay.on_msg("b", Msg::Gesture { seq: 7, gesture: Gesture::SpaceLeft }, 1300);
        assert_eq!(relay.out().0.len(), n);
    }

    #[test]
    fn a_gesture_at_home_stays_home() {
        let mut f = fwd(Edge::Right);
        f.peer_speaks("b", GESTURE_PROTO_VERSION);
        assert_eq!(f.on_captured(CapturedEvent::Gesture(Gesture::SpaceLeft), 0), Disposition::Pass);
        assert!(f.out().0.is_empty());
        assert!(f.sink().0.is_empty());
    }

    #[test]
    fn a_driven_machine_plays_a_gesture_as_its_shortcut() {
        let mut f = fwd(Edge::Left);
        go_in(&mut f, 7, vec![]);
        let n = f.sink().0.len();
        f.on_msg("b", Msg::Gesture { seq: 7, gesture: Gesture::Overview }, 10);
        f.on_msg("b", Msg::Gesture { seq: 6, gesture: Gesture::Overview }, 10);
        f.on_msg("b", Msg::Gesture { seq: 7, gesture: Gesture::AppWindows }, 10);
        let mut expect = GestureKeys::new();
        let want: Vec<Fx> = [Gesture::Overview, Gesture::AppWindows]
            .into_iter()
            .flat_map(|g| expect.strokes(g, Platform::here()))
            .map(|s| match s {
                Stroke::Key(k, d) => Fx::Key(k, d),
                Stroke::Wheel(n) => Fx::Wheel(0, n),
            })
            .collect();
        assert_eq!(&f.sink().0[n..], want, "the stale session's gesture is dropped");
        assert!(want.ends_with(&[Fx::Key(ESC, true), Fx::Key(ESC, false)]), "the second one closed the first");
        assert!(f.inj_keys.is_empty(), "nothing left held");
        assert!(f.out().0.iter().all(|(_, m)| !matches!(m, Msg::Gesture { .. })));
    }

    #[test]
    fn a_relay_hands_a_gesture_on_as_the_next_machine_takes_it() {
        let mut pc = relay_fwd(KeyMap::Position);
        pc.peer_speaks("c", GESTURE_PROTO_VERSION);
        let seq = start_relay(&mut pc, vec![]);
        let (n, fx) = (pc.out().0.len(), pc.sink().0.len());
        pc.on_msg("b", Msg::Gesture { seq: 7, gesture: Gesture::SpaceLeft }, 1300);
        pc.on_msg("b", Msg::Gesture { seq: 6, gesture: Gesture::SpaceLeft }, 1300);
        assert_eq!(&pc.out().0[n..], chord_msgs(seq, &[CTRL, GUI, LEFT_ARROW]).into_iter().map(|m| ("c".to_string(), m)).collect::<Vec<_>>());
        assert_eq!(pc.sink().0.len(), fx, "nothing plays on the relay itself");
        // The relay's own setting for the PC decides the habit: keys by position, linked, take Windows'.
        let n = pc.out().0.len();
        pc.on_msg("b", Msg::Gesture { seq: 7, gesture: Gesture::AppWindows }, 1300);
        assert_eq!(&pc.out().0[n..], chord_msgs(seq, &[GUI, 0x07]).into_iter().map(|m| ("c".to_string(), m)).collect::<Vec<_>>());

        let mut mac = relay_fwd(KeyMap::None);
        mac.peer_speaks("c", GESTURE_PROTO_VERSION);
        let seq = start_relay(&mut mac, vec![]);
        let n = mac.out().0.len();
        mac.on_msg("b", Msg::Gesture { seq: 7, gesture: Gesture::SpaceLeft }, 1300);
        assert_eq!(&mac.out().0[n..], &[("c".to_string(), Msg::Gesture { seq, gesture: Gesture::SpaceLeft })]);
    }
}
