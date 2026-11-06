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
use crate::config::{Config, KeyMap};
use crate::emergency::is_emergency;
use crate::geometry::{clamp_inside, contains, entry_point, exit_frac, in_exit_strip};
use crate::mac_shortcuts::MacShortcuts;
use crate::modifiers::KeyTracker;
use crate::proto::{Edge, ModifierSet, Msg, Point, Reason, Rect, PRIMARY_BUTTON};
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
            peers: cfg.peers.iter().map(|p| PeerSpec { name: p.name.clone(), edge: p.edge.into(), keys: p.keys }).collect(),
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
}

pub fn opposite(e: Edge) -> Edge {
    match e {
        Edge::Left => Edge::Right,
        Edge::Right => Edge::Left,
        Edge::Top => Edge::Bottom,
        Edge::Bottom => Edge::Top,
    }
}

const FRAC_MID: u16 = 0x8000;

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
    /// The primary button is down from a press made here while input stayed local.
    held_here: bool,
    /// The primary button is physically down (tap view, any mode).
    primary_down: bool,
    /// Control left with a press made here still down: this machine saw that press, and its release goes
    /// to the peer, so this machine owes itself a release when control comes back.
    crossed_holding: bool,
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
            held_here: false,
            primary_down: false,
            crossed_holding: false,
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
    pub fn set_config(&mut self, cfg: FwdConfig) {
        let active = match &self.mode {
            Mode::RemoteOut { peer, seq, .. } | Mode::RemoteIn { peer, seq, .. } => Some((peer.clone(), *seq)),
            Mode::Local => None,
        };
        if let Some((peer, seq)) = active {
            let old = self.cfg.peers.iter().find(|p| p.name == peer);
            let new = cfg.peers.iter().find(|p| p.name == peer);
            if old != new {
                let was_up = self.up.contains(&peer);
                // The link stays up, so the other end must hear that the switch is over: otherwise it keeps
                // driving us, or keeps the keys we injected held down.
                if was_up {
                    self.out.send(&peer, Msg::Release { seq, reason: Reason::TopologyChange });
                }
                self.link_down(&peer);
                if was_up && new.is_some() {
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
                self.restore_local(at);
            }
            Mode::RemoteIn { pos, .. } => {
                if let Some(p) = clamp_inside(&self.displays, pos).filter(|p| *p != pos) {
                    if let Mode::RemoteIn { pos, .. } = &mut self.mode {
                        *pos = p;
                    }
                    self.sink.warp(p);
                }
            }
            Mode::Local => {}
        }
    }

    pub fn link_up(&mut self, peer: &str) {
        self.up.insert(peer.to_string());
    }

    /// A dropped link frees whatever it held: RemoteOut returns local at once, RemoteIn
    /// releases every injected key and button.
    pub fn link_down(&mut self, peer: &str) {
        self.up.remove(peer);
        self.last_enter.remove(peer);
        match self.mode.clone() {
            Mode::RemoteOut { peer: p, exit_pos, .. } if p == peer => self.restore_local(exit_pos),
            Mode::RemoteIn { peer: p, .. } if p == peer => self.leave_remote_in(),
            _ => {}
        }
    }

    /// The grab for a switch could not be taken: tell the peer and return to Local.
    pub fn abort_switch(&mut self) {
        if let Mode::RemoteOut { peer, seq, exit_pos } = self.mode.clone() {
            let edge = self.cfg.peers.iter().find(|x| x.name == peer).map(|x| x.edge).unwrap_or(Edge::Right);
            self.out.send(&peer, Msg::Leave { seq, edge, frac: FRAC_MID });
            self.restore_local(exit_pos);
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
        let fire = decide(
            &self.cfg.policy,
            &mut self.sw,
            SwitchEvent::Cursor { pos, in_boundary: target.is_some() && !held },
            now_ms,
        );
        if let (true, Some(i)) = (fire, target) {
            self.enter_out(i, pos, None);
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

    pub fn on_captured(&mut self, ev: CapturedEvent, now_ms: u64) -> Disposition {
        if let CapturedEvent::Button { button: PRIMARY_BUTTON, down } = ev {
            let local = matches!(self.mode, Mode::Local);
            self.primary_down = down;
            self.held_here = down && local;
            if local {
                self.sink.primary(down);
            }
        }
        if let CapturedEvent::Key { hid, down } = ev {
            if self.keys.set_key(hid, down).is_err() {
                return Disposition::Swallow;
            }
            if is_emergency(self.keys.mods(), hid, down) {
                self.emergency();
                return Disposition::Swallow;
            }
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
                        self.restore_local(exit_pos);
                        return Disposition::Swallow;
                    }
                }
                let keys = self.cfg.peers.iter().find(|p| p.name == peer).map_or(KeyMap::None, |p| p.keys);
                let mac = keys == KeyMap::Mac;
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

    fn local_key(&mut self, hid: u16, down: bool, now_ms: u64) -> Disposition {
        let target = self.peer_at(self.last_pos).or_else(|| self.first_up());
        let Some(i) = target else { return Disposition::Pass };
        let mods = self.keys.mods();
        if decide(&self.cfg.policy, &mut self.sw, SwitchEvent::Key { hid, down, mods }, now_ms) {
            let pos = self.last_pos;
            // The chord key is swallowed here; it must not arrive as a held key and type on the peer.
            self.enter_out(i, pos, Some(hid));
            return Disposition::Swallow;
        }
        Disposition::Pass
    }

    pub fn on_msg(&mut self, peer: &str, msg: Msg, now_ms: u64) {
        match msg {
            Msg::Enter { seq, edge, frac, pressed, .. } => self.on_enter(peer, seq, edge, frac, &pressed),
            Msg::Leave { seq, frac, .. } => self.on_leave(peer, seq, frac),
            Msg::Release { seq, .. } => self.on_release(peer, seq),
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
                self.inject_key(hid, down, repeat);
            }
            _ => {}
        }
    }

    fn on_enter(&mut self, peer: &str, seq: u32, edge: Edge, frac: u16, pressed: &[u16]) {
        if let Some(&last) = self.last_enter.get(peer) {
            if (seq.wrapping_sub(last) as i32) <= 0 {
                return; // a duplicate of a session already handled
            }
        }
        // The same peer starting a newer session means it considers the old one over.
        if matches!(&self.mode, Mode::RemoteIn { peer: p, .. } if p == peer) {
            self.leave_remote_in();
        }
        if !self.cfg.peers.iter().any(|p| p.name == peer) || !matches!(self.mode, Mode::Local) {
            // The sender has already hidden its cursor and grabbed its keyboard and mouse. Unanswered, it
            // would keep them until a restart; refused, it comes home at once.
            self.out.send(peer, Msg::Release { seq, reason: Reason::Busy });
            return;
        }
        self.last_enter.insert(peer.to_string(), seq);
        let Ok(pos) = entry_point(&self.displays, edge, frac) else {
            self.out.send(peer, Msg::Leave { seq, edge: opposite(edge), frac });
            return;
        };
        self.sink.warp(pos);
        self.sink.set_association(false);
        self.sync_injected(pressed);
        self.mode = Mode::RemoteIn { peer: peer.to_string(), seq, edge, pos, ret: SwitchState::new(pos) };
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
    fn on_release(&mut self, peer: &str, seq: u32) {
        match self.mode.clone() {
            Mode::RemoteOut { peer: p, seq: s, exit_pos } if p == peer && s == seq => self.restore_local(exit_pos),
            Mode::RemoteIn { peer: p, seq: s, .. } if p == peer && s == seq => self.leave_remote_in(),
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
                self.restore_local(at);
            }
            Mode::RemoteIn { peer: p, seq: s, .. } if p == peer && s == seq => self.leave_remote_in(),
            _ => {}
        }
    }

    fn live_in(&self, peer: &str, seq: u32) -> bool {
        matches!(&self.mode, Mode::RemoteIn { peer: p, seq: s, .. } if p == peer && *s == seq)
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
            // ponytail: clamps inside the display holding the cursor, so seams between displays
            // are not crossed in this model; track the OS cursor position if multi-display needs it.
            if let Some(r) = displays.iter().find(|r| contains(r, *pos)) {
                let nx = (pos.x as i64 + dx as i64).clamp(r.x as i64, r.x as i64 + r.w as i64 - 1);
                let ny = (pos.y as i64 + dy as i64).clamp(r.y as i64, r.y as i64 + r.h as i64 - 1);
                *pos = Point { x: nx as i32, y: ny as i32 };
            }
        }
    }

    fn check_return(&mut self, now_ms: u64) {
        // A press injected here and still held keeps the cursor here unless it started a file drag. A drag
        // that crossed in already held left no injected press, so it can always be carried back.
        let held = self.inj_btn.contains(&PRIMARY_BUTTON) && !self.sink.drag_started_here();
        let Mode::RemoteIn { peer, seq, edge, pos, ret } = &mut self.mode else { return };
        let in_boundary = !held && in_exit_strip(&self.displays, *pos, *edge, self.cfg.boundary_px);
        if !decide(&self.cfg.policy, ret, SwitchEvent::Cursor { pos: *pos, in_boundary }, now_ms) {
            return;
        }
        let (peer, seq, edge, pos) = (peer.clone(), *seq, *edge, *pos);
        // The controller's Enter carries no buttons, so a drag that arrived here with the button already
        // down left no trace in `inj_btn`: the hook runs on every crossing and the sink decides.
        self.sink.drag_away(&peer);
        let frac = frac_at(&self.displays, pos, edge);
        self.out.send(&peer, Msg::Leave { seq, edge: opposite(edge), frac });
        self.leave_remote_in();
    }

    fn enter_out(&mut self, idx: usize, pos: Point, skip_key: Option<u16>) {
        let spec = self.cfg.peers[idx].clone();
        self.crossed_holding = self.held_here;
        self.held_here = false;
        self.sink.drag_away(&spec.name);
        self.next_seq = self.next_seq.wrapping_add(1);
        let seq = self.next_seq;
        let mut held: Vec<u16> = self.keys.snapshot().into_iter().filter(|h| Some(*h) != skip_key).collect();
        self.mac_keys = MacShortcuts::new();
        let pressed: Vec<u16> = if spec.keys == KeyMap::Mac {
            // Modifiers first, so a key held while crossing is translated under them.
            held.sort_by_key(|h| !(0xE0..=0xE7).contains(h));
            self.mac_keys = MacShortcuts::with_held(&held);
            self.mac_keys.pressed()
        } else {
            held.into_iter().map(|h| spec.keys.apply(h)).collect()
        };
        self.out.send(
            &spec.name,
            Msg::Enter {
                seq,
                edge: opposite(spec.edge),
                frac: frac_at(&self.displays, pos, spec.edge),
                mods: mods_of(&pressed, self.keys.mods().contains(ModifierSet::CAPS)),
                pressed,
            },
        );
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
                self.restore_local(exit_pos);
            }
            Mode::RemoteIn { peer, seq, edge, .. } => {
                self.out.send(&peer, Msg::Leave { seq, edge: opposite(edge), frac: FRAC_MID });
                self.leave_remote_in();
            }
            Mode::Local => {}
        }
    }

    fn restore_local(&mut self, at: Point) {
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

    fn leave_remote_in(&mut self) {
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

    fn first_up(&self) -> Option<usize> {
        self.cfg.peers.iter().position(|p| self.up.contains(&p.name))
    }
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
            peers: vec![PeerSpec { name: "b".into(), edge, keys: KeyMap::None }],
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
        original.peers.push(PeerSpec { name: "c".into(), edge: Edge::Left, keys: KeyMap::None });
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
        changed.peers.push(PeerSpec { name: "c".into(), edge: Edge::Left, keys: KeyMap::None });
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
        });
        let f = FwdConfig::from_config(&c).unwrap();
        assert_eq!(f.peers, vec![PeerSpec { name: "b".into(), edge: Edge::Top, keys: crate::config::KeyMap::Mac }]);
        c.switch.chord = "bogus".into();
        assert!(FwdConfig::from_config(&c).is_err());
    }
}
