//! Peer input onto this machine, driven by flowseam_core's forwarder. The OS calls sit behind [`Injector`], so
//! the mapping (keys, buttons, wheel, motion) is tested on any host; `win_input` is the SendInput one.
//!
//! Motion is injected as absolute positions (current cursor + delta), not relative moves: Windows applies
//! its own pointer acceleration to relative injected motion, and the delta already carries the Mac's.

use std::io::Write;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use flowseam_core::cursor::PostedCursor;
use flowseam_core::forward::{InputSink, Outbox};
use flowseam_core::proto::{Msg, Point, Rect};
use flowseam_net::files::DragHub;
#[cfg(windows)]
use crate::edge_drop::EdgeDropSource;
#[cfg(windows)]
use flowseam_core::proto::Edge;

use crate::keymap::{hid_to_scan, Scan};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WinButton {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

/// The OS side of injection. Errors are text; the sink logs them, it never drops them silently.
pub trait Injector: Send {
    fn key(&mut self, scan: Scan, down: bool) -> Result<(), String>;
    /// Absolute move to a point in virtual-screen pixels.
    fn move_to(&mut self, p: Point) -> Result<(), String>;
    fn cursor(&mut self) -> Result<Point, String>;
    fn button(&mut self, b: WinButton, down: bool) -> Result<(), String>;
    /// In wheel notches: positive `v` scrolls up (away from the user), positive `h` scrolls right.
    fn wheel(&mut self, v: i32, h: i32) -> Result<(), String>;
    /// Whether NumLock is on here.
    fn num_lock(&mut self) -> Result<bool, String>;
}

impl Injector for Box<dyn Injector> {
    fn key(&mut self, scan: Scan, down: bool) -> Result<(), String> {
        (**self).key(scan, down)
    }
    fn move_to(&mut self, p: Point) -> Result<(), String> {
        (**self).move_to(p)
    }
    fn cursor(&mut self) -> Result<Point, String> {
        (**self).cursor()
    }
    fn button(&mut self, b: WinButton, down: bool) -> Result<(), String> {
        (**self).button(b, down)
    }
    fn wheel(&mut self, v: i32, h: i32) -> Result<(), String> {
        (**self).wheel(v, h)
    }
    fn num_lock(&mut self) -> Result<bool, String> {
        (**self).num_lock()
    }
}

/// Keypad digits and the decimal point: Windows turns them into navigation keys while NumLock is off.
fn needs_num_lock(hid: u16) -> bool {
    (0x59..=0x63).contains(&hid)
}

const HID_NUM_LOCK: u16 = 0x53;

/// CoreGraphics button numbers, which the Mac puts on the wire: 0 left, 1 right, 2 middle, 3 back, 4 forward.
/// HID usage of Escape: cancels an OLE drag in progress.
const HID_ESCAPE: u16 = 0x29;

pub fn wire_button(b: u8) -> Option<WinButton> {
    match b {
        0 => Some(WinButton::Left),
        1 => Some(WinButton::Right),
        2 => Some(WinButton::Middle),
        3 => Some(WinButton::X1),
        4 => Some(WinButton::X2),
        _ => None,
    }
}

pub use flowseam_core::geometry::clamp_to_displays;

/// What a controller does with its own cursor while it drives a peer. The hooks swallow the moves, so the
/// cursor stays where it is put.
pub trait Grab: Send {
    /// Put the cursor at `at` without a move the hooks would see.
    fn park(&mut self, at: Point) -> Result<(), String>;
    fn hide(&mut self) -> Result<(), String>;
    fn show(&mut self) -> Result<(), String>;
}

/// Where the cursor waits while this machine drives a peer: the middle of the display it is on (the first
/// one when it is on none). At an edge, a hook may report a push further out as no move at all, and the peer
/// would never get it; from the middle every move has room.
pub fn park_point(displays: &[Rect], at: Option<Point>) -> Option<Point> {
    let on = |r: &&Rect| at.is_some_and(|p| p.x >= r.x && p.x < r.x + r.w && p.y >= r.y && p.y < r.y + r.h);
    let r = displays.iter().find(on).or_else(|| displays.first())?;
    Some(Point { x: r.x + r.w / 2, y: r.y + r.h / 2 })
}

/// flowseam_core's sink: everything a peer sends is injected through `I`. A machine whose input is captured
/// (see `capture`) also drives peers and has a [`Grab`]; without one the controller-only calls (grab, hide,
/// show) are reported as bugs.
pub struct InjectSink<I: Injector> {
    inj: I,
    displays: Vec<Rect>,
    log: fn(&str),
    /// Last failure text, so a failing stream of events logs once per distinct error, not per event.
    last_err: Option<String>,
    /// When this sink last turned NumLock on. The OS's answer may lag the injected toggle, and asking
    /// again too soon would toggle it back off.
    num_lock_sent: Option<std::time::Instant>,
    drags: Option<Arc<Mutex<DragHub>>>,
    #[cfg(windows)]
    edge_drop: Option<(EdgeDropSource, Vec<Edge>)>,
    /// Where this sink last sent the cursor: the base for the next relative move (see flowseam_core::cursor).
    posted: PostedCursor,
    grab: Option<Box<dyn Grab>>,
    /// Where the cursor stood when this machine started driving a peer. The hooks swallow every move until
    /// it comes back, so a cursor that has left this point means Windows took the hooks out.
    held_at: Option<Point>,
}

impl<I: Injector> InjectSink<I> {
    pub fn new(inj: I, displays: Vec<Rect>, log: fn(&str)) -> Self {
        InjectSink { inj, displays, log, last_err: None, num_lock_sent: None, drags: None, posted: PostedCursor::default(), grab: None, held_at: None, #[cfg(windows)] edge_drop: None }
    }

    /// Whether the cursor left the point where this machine started driving a peer: Windows removes a
    /// low-level hook it finds too slow, without a word, and this machine's input then reaches it again.
    /// `Ok(false)` while not driving; `Err` when the cursor cannot be read, which says nothing either way.
    pub fn cursor_moved(&mut self) -> Result<bool, String> {
        let Some(at) = self.held_at else { return Ok(false) };
        let now = self.inj.cursor()?;
        Ok((now.x - at.x).abs() > 1 || (now.y - at.y).abs() > 1)
    }

    /// This machine's input is captured: it drives peers, and `grab` keeps its cursor while it does.
    pub fn with_grab(mut self, grab: Box<dyn Grab>) -> Self {
        self.grab = Some(grab);
        self
    }

    /// A point on this machine's desktop: the nearest one to `p`, which a hook may report past an edge.
    pub fn clamp(&self, p: Point) -> Point {
        clamp_to_displays(&self.displays, p)
    }

    /// File drags: presses and crossings go to `hub`.
    pub fn with_drags(mut self, hub: Arc<Mutex<DragHub>>) -> Self {
        self.drags = Some(hub);
        self
    }

    #[cfg(windows)]
    pub fn with_edge_drop(mut self, source: EdgeDropSource, edges: Vec<Edge>) -> Self {
        self.edge_drop = Some((source, edges));
        self
    }

    pub fn set_displays(&mut self, d: Vec<Rect>) {
        #[cfg(windows)]
        if let Some((source, edges)) = &self.edge_drop { source.set_edges(edges.clone(), d.clone()); }
        self.displays = d;
        self.posted.forget();
    }

    pub fn injector(&self) -> &I {
        &self.inj
    }

    fn report(&mut self, what: &str, r: Result<(), String>) {
        match r {
            Ok(()) => self.last_err = None,
            Err(e) => {
                let msg = format!("inject {what} failed: {e}");
                if self.last_err.as_deref() != Some(msg.as_str()) {
                    (self.log)(&msg);
                    self.last_err = Some(msg);
                }
            }
        }
    }
}

impl<I: Injector> InputSink for InjectSink<I> {
    fn motion_rel(&mut self, dx: i32, dy: i32) {
        let now = std::time::Instant::now();
        let r = self.posted.target(dx, dy, &self.displays, now, || self.inj.cursor()).and_then(|t| self.inj.move_to(t).map(|()| t));
        match &r {
            Ok(t) => self.posted.placed(*t, now),
            Err(_) => self.posted.forget(),
        }
        self.report("motion", r.map(|_| ()));
    }

    fn button(&mut self, button: u8, down: bool) {
        let r = match wire_button(button) {
            Some(b) => self.inj.button(b, down),
            None => Err(format!("mouse button {button} has no Windows equivalent")),
        };
        self.report("button", r);
    }

    fn key(&mut self, hid: u16, down: bool, _repeat: bool) {
        // The controller already mapped its modifiers for this machine (its per-device setting).
        let mapped = hid;
        let just_sent = self.num_lock_sent.is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(1));
        if down && needs_num_lock(mapped) && !just_sent {
            // A Mac keypad always types digits and has no NumLock to turn on, so turn on this one.
            let r = self.inj.num_lock().and_then(|on| match (on, hid_to_scan(HID_NUM_LOCK)) {
                (true, _) => Ok(()),
                (false, Some(sc)) => {
                    self.num_lock_sent = Some(std::time::Instant::now());
                    self.inj.key(sc, true).and_then(|()| self.inj.key(sc, false))
                }
                (false, None) => Err("NumLock has no scan code".into()),
            });
            self.report("NumLock", r);
        }
        let r = match hid_to_scan(mapped) {
            Some(sc) => self.inj.key(sc, down),
            None => Err(format!("HID usage {hid:#04x} has no Windows scan code")),
        };
        self.report("key", r);
    }

    fn wheel(&mut self, dx: i32, dy: i32) {
        // Same vertical sense; the Mac's horizontal axis is positive to the left, Windows' to the right.
        let r = self.inj.wheel(dy, dx.saturating_neg());
        self.report("wheel", r);
    }

    fn warp(&mut self, p: Point) {
        let to = clamp_to_displays(&self.displays, p);
        let r = self.inj.move_to(to);
        match r {
            Ok(()) => self.posted.placed(to, std::time::Instant::now()),
            Err(_) => self.posted.forget(),
        }
        self.report("warp", r);
    }

    /// The hooks swallow this machine's input while the forwarder drives a peer; this puts the cursor in the
    /// middle of its display ([`park_point`]) and notes where it is held, for [`InjectSink::cursor_moved`].
    fn set_grab(&mut self, on: bool) {
        if !on {
            self.held_at = None;
            return;
        }
        let Some(grab) = &mut self.grab else {
            (self.log)("bug: grab requested on a machine that only receives input; ignored");
            return;
        };
        let from = self.inj.cursor().ok();
        let r = match park_point(&self.displays, from) {
            Some(p) => grab.park(p),
            None => Err("no display to park the cursor on".into()),
        };
        // Parked or not, the hooks hold the cursor where it ends up, which may not be the point asked for
        // (another program can clip the cursor): read it back.
        self.held_at = self.inj.cursor().ok().or(from);
        self.posted.forget();
        self.report("park", r);
    }
    fn hide(&mut self) {
        let r = match &mut self.grab {
            Some(g) => g.hide(),
            None => Err("bug: hide requested on a machine that only receives input; ignored".into()),
        };
        self.report("hide", r);
    }
    fn show(&mut self) {
        if let Some(g) = &mut self.grab {
            let r = g.show();
            self.report("show", r);
        }
    }
    /// Windows cannot detach the physical mouse; it simply keeps working alongside the peer.
    fn set_association(&mut self, _connected: bool) {}
    fn primary(&mut self, down: bool) {
        #[cfg(windows)]
        if let Some((source, _)) = &self.edge_drop { source.arm(down); }
        if let Some(hub) = &self.drags {
            hub.lock().unwrap_or_else(|e| e.into_inner()).primary(down);
        }
    }
    fn drag_started_here(&mut self) -> bool {
        self.drags.as_ref().is_none_or(|hub| hub.lock().unwrap_or_else(|e| e.into_inner()).drag_started())
    }
    fn drag_away(&mut self, peer: &str) {
        #[cfg(windows)]
        if let Some((source, _)) = &self.edge_drop { source.arm(false); }
        let sent = self.drags.as_ref().is_some_and(|hub| hub.lock().unwrap_or_else(|e| e.into_inner()).drag_away(peer));
        if sent {
            // The files went to the peer; the drag here must end without dropping them at the edge.
            self.key(HID_ESCAPE, true, false);
            self.key(HID_ESCAPE, false, false);
        }
    }
}

/// The forwarder's messages to the one peer this session talks to.
pub struct ChanOut(pub Sender<Msg>);

impl Outbox for ChanOut {
    fn send(&mut self, _peer: &str, msg: Msg) {
        let _ = self.0.send(msg);
    }
}

/// Stand-in for `--stdio` runs and non-Windows hosts: prints each injection as `INJECT …` on stdout and
/// tracks a virtual cursor, so a cross-machine test can see what would be injected without moving anything.
pub struct PrintInjector {
    pos: Point,
}

impl PrintInjector {
    pub fn new(start: Point) -> Self {
        PrintInjector { pos: start }
    }
    fn out(line: String) {
        let mut o = std::io::stdout().lock();
        let _ = writeln!(o, "INJECT {line}");
        let _ = o.flush();
    }
}

impl Injector for PrintInjector {
    fn key(&mut self, scan: Scan, down: bool) -> Result<(), String> {
        Self::out(format!("key {:#04x}{} {}", scan.code, if scan.extended { "e" } else { "" }, if down { "down" } else { "up" }));
        Ok(())
    }
    fn move_to(&mut self, p: Point) -> Result<(), String> {
        self.pos = p;
        Self::out(format!("move {} {}", p.x, p.y));
        Ok(())
    }
    fn cursor(&mut self) -> Result<Point, String> {
        Ok(self.pos)
    }
    fn button(&mut self, b: WinButton, down: bool) -> Result<(), String> {
        Self::out(format!("button {b:?} {}", if down { "down" } else { "up" }));
        Ok(())
    }
    fn wheel(&mut self, v: i32, h: i32) -> Result<(), String> {
        Self::out(format!("wheel {v} {h}"));
        Ok(())
    }
    /// No real keyboard state here; reported on so the printout shows only what the peer sent.
    fn num_lock(&mut self) -> Result<bool, String> {
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Parks the way the real one does: the cursor is where it is put.
    struct Parks(Arc<Mutex<Point>>);
    impl Grab for Parks {
        fn park(&mut self, at: Point) -> Result<(), String> {
            *self.0.lock().unwrap() = at;
            Ok(())
        }
        fn hide(&mut self) -> Result<(), String> {
            Ok(())
        }
        fn show(&mut self) -> Result<(), String> {
            Ok(())
        }
    }

    const BENQ: Rect = Rect { x: -2560, y: 0, w: 2560, h: 1440 };
    const ASUS: Rect = Rect { x: 0, y: 0, w: 1920, h: 1080 };

    /// The cursor leaves the left edge for the middle of the display it was on, so a push further left still
    /// moves the peer's cursor; off every display, the first one.
    #[test]
    fn driving_parks_the_cursor_in_the_middle_of_its_display() {
        let two = [BENQ, ASUS];
        assert_eq!(park_point(&two, Some(Point { x: -2560, y: 700 })), Some(Point { x: -1280, y: 720 }));
        assert_eq!(park_point(&two, Some(Point { x: 1919, y: 0 })), Some(Point { x: 960, y: 540 }));
        assert_eq!(park_point(&two, Some(Point { x: 5000, y: 0 })), Some(Point { x: -1280, y: 720 }));
        assert_eq!(park_point(&two, None), Some(Point { x: -1280, y: 720 }));
        assert_eq!(park_point(&[], None), None);
        let rec = Rec::default();
        *rec.pos.lock().unwrap() = Point { x: -2560, y: 700 };
        let mut s = InjectSink::new(rec.clone(), two.to_vec(), |_| {}).with_grab(Box::new(Parks(rec.pos.clone())));
        s.set_grab(true);
        assert_eq!(*rec.pos.lock().unwrap(), Point { x: -1280, y: 720 });
        assert_eq!(s.cursor_moved(), Ok(false), "held where it was parked");
    }

    /// Parks somewhere else than asked, as a cursor clipped by another program does.
    struct ParksClipped(Arc<Mutex<Point>>);
    impl Grab for ParksClipped {
        fn park(&mut self, _at: Point) -> Result<(), String> {
            *self.0.lock().unwrap() = Point { x: -2000, y: 300 };
            Ok(())
        }
        fn hide(&mut self) -> Result<(), String> {
            Ok(())
        }
        fn show(&mut self) -> Result<(), String> {
            Ok(())
        }
    }

    /// The cursor is held where it ended up, not where it was sent: a clipped park is no lost hook.
    #[test]
    fn a_clipped_park_holds_where_the_cursor_is() {
        let rec = Rec::default();
        *rec.pos.lock().unwrap() = Point { x: -2560, y: 700 };
        let mut s = InjectSink::new(rec.clone(), vec![BENQ], |_| {}).with_grab(Box::new(ParksClipped(rec.pos.clone())));
        s.set_grab(true);
        assert_eq!(s.cursor_moved(), Ok(false));
    }

    /// The hooks hold the cursor while this machine drives a peer: a cursor that moves anyway means they are
    /// gone. Before the grab, after it, and on a machine that only receives, nothing counts.
    #[test]
    fn a_cursor_that_leaves_its_held_point_means_the_hooks_are_gone() {
        let rec = Rec::default();
        *rec.pos.lock().unwrap() = Point { x: -2560, y: 700 };
        let mut s = InjectSink::new(rec.clone(), vec![BENQ], |_| {}).with_grab(Box::new(Parks(rec.pos.clone())));
        assert_eq!(s.cursor_moved(), Ok(false), "not driving");
        s.set_grab(true);
        assert_eq!(s.cursor_moved(), Ok(false));
        *rec.pos.lock().unwrap() = Point { x: -1279, y: 721 };
        assert_eq!(s.cursor_moved(), Ok(false), "a pixel is rounding");
        *rec.pos.lock().unwrap() = Point { x: -1260, y: 720 };
        assert_eq!(s.cursor_moved(), Ok(true));
        *rec.pos.lock().unwrap() = Point { x: -1280, y: 710 };
        assert_eq!(s.cursor_moved(), Ok(true), "vertically too");
        s.set_grab(false);
        assert_eq!(s.cursor_moved(), Ok(false), "home again");
        let mut receiver = InjectSink::new(rec.clone(), vec![], |_| {});
        receiver.set_grab(true);
        assert_eq!(receiver.cursor_moved(), Ok(false), "a machine that only receives holds nothing");
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Op {
        Key(Scan, bool),
        Move(Point),
        Button(WinButton, bool),
        Wheel(i32, i32),
    }

    #[derive(Clone)]
    struct Rec {
        ops: Arc<Mutex<Vec<Op>>>,
        pos: Arc<Mutex<Point>>,
        fail: Arc<Mutex<Option<String>>>,
        num_lock: Arc<Mutex<bool>>,
    }

    impl Default for Rec {
        fn default() -> Self {
            Rec { ops: Arc::default(), pos: Arc::new(Mutex::new(Point { x: 0, y: 0 })), fail: Arc::default(), num_lock: Arc::new(Mutex::new(true)) }
        }
    }

    impl Injector for Rec {
        fn key(&mut self, scan: Scan, down: bool) -> Result<(), String> {
            self.ops.lock().unwrap().push(Op::Key(scan, down));
            if down && scan == (Scan { code: 0x45, extended: false }) {
                let mut n = self.num_lock.lock().unwrap();
                *n = !*n;
            }
            self.fail.lock().unwrap().clone().map_or(Ok(()), Err)
        }
        fn move_to(&mut self, p: Point) -> Result<(), String> {
            *self.pos.lock().unwrap() = p;
            self.ops.lock().unwrap().push(Op::Move(p));
            Ok(())
        }
        fn cursor(&mut self) -> Result<Point, String> {
            Ok(*self.pos.lock().unwrap())
        }
        fn button(&mut self, b: WinButton, down: bool) -> Result<(), String> {
            self.ops.lock().unwrap().push(Op::Button(b, down));
            Ok(())
        }
        fn wheel(&mut self, v: i32, h: i32) -> Result<(), String> {
            self.ops.lock().unwrap().push(Op::Wheel(v, h));
            Ok(())
        }
        fn num_lock(&mut self) -> Result<bool, String> {
            Ok(*self.num_lock.lock().unwrap())
        }
    }

    fn screen() -> Vec<Rect> {
        vec![Rect { x: 0, y: 0, w: 1920, h: 1080 }]
    }
    fn sink(rec: &Rec) -> InjectSink<Rec> {
        InjectSink::new(rec.clone(), screen(), |_| {})
    }
    fn ops(rec: &Rec) -> Vec<Op> {
        rec.ops.lock().unwrap().clone()
    }

    #[test]
    fn motion_moves_the_real_cursor_by_the_delta_and_stops_at_the_edge() {
        let rec = Rec::default();
        let mut s = sink(&rec);
        s.warp(Point { x: 100, y: 500 });
        s.motion_rel(30, -20);
        assert_eq!(*rec.pos.lock().unwrap(), Point { x: 130, y: 480 });
        // Far past the left edge: lands on the edge pixel, not inside an inset.
        s.motion_rel(-10_000, 0);
        assert_eq!(*rec.pos.lock().unwrap(), Point { x: 0, y: 480 });
    }

    /// An OS that reports a moved cursor one move late, as macOS was measured to.
    struct Lagging { rec: Rec, shown: Point }
    impl Injector for Lagging {
        fn key(&mut self, scan: Scan, down: bool) -> Result<(), String> { self.rec.key(scan, down) }
        fn move_to(&mut self, p: Point) -> Result<(), String> {
            self.shown = *self.rec.pos.lock().unwrap();
            self.rec.move_to(p)
        }
        fn cursor(&mut self) -> Result<Point, String> { Ok(self.shown) }
        fn button(&mut self, b: WinButton, down: bool) -> Result<(), String> { self.rec.button(b, down) }
        fn wheel(&mut self, v: i32, h: i32) -> Result<(), String> { self.rec.wheel(v, h) }
        fn num_lock(&mut self) -> Result<bool, String> { self.rec.num_lock() }
    }

    /// A burst of peer motion, as it arrives off the network, all lands even when the OS reports the cursor late.
    #[test]
    fn motion_faster_than_the_os_reports_the_cursor_adds_up() {
        let rec = Rec::default();
        *rec.pos.lock().unwrap() = Point { x: 100, y: 500 };
        let mut s = InjectSink::new(Lagging { rec: rec.clone(), shown: Point { x: 100, y: 500 } }, screen(), |_| {});
        for _ in 0..100 {
            s.motion_rel(5, -1);
        }
        assert_eq!(*rec.pos.lock().unwrap(), Point { x: 600, y: 400 });
    }

    #[test]
    fn keys_go_out_as_scan_codes_with_the_extended_flag() {
        let rec = Rec::default();
        let mut s = sink(&rec);
        s.key(0x04, true, false); // A
        s.key(0x52, true, false); // Up (extended)
        s.key(0x04, false, false);
        assert_eq!(
            ops(&rec),
            vec![
                Op::Key(Scan { code: 0x1E, extended: false }, true),
                Op::Key(Scan { code: 0x48, extended: true }, true),
                Op::Key(Scan { code: 0x1E, extended: false }, false),
            ]
        );
    }

    /// Measured: Mac keypad 0 and . arrived on Windows as Insert and Delete, because NumLock was off there.
    /// A keypad digit turns NumLock on first, once; keypad operators do not need it.
    #[test]
    fn keypad_digits_turn_num_lock_on_first() {
        let rec = Rec::default();
        *rec.num_lock.lock().unwrap() = false;
        let mut s = sink(&rec);
        s.key(0x57, true, false); // keypad +
        s.key(0x62, true, false); // keypad 0
        s.key(0x62, false, false);
        s.key(0x63, true, false); // keypad .
        let nl = Scan { code: 0x45, extended: false };
        assert_eq!(
            ops(&rec),
            vec![
                Op::Key(Scan { code: 0x4E, extended: false }, true),
                Op::Key(nl, true),
                Op::Key(nl, false),
                Op::Key(Scan { code: 0x52, extended: false }, true),
                Op::Key(Scan { code: 0x52, extended: false }, false),
                Op::Key(Scan { code: 0x53, extended: false }, true),
            ]
        );
    }

    /// GetKeyState can still answer "off" right after the injected toggle; asking again then would
    /// turn NumLock back off and the next digit would become a navigation key.
    #[test]
    fn a_stale_num_lock_reading_does_not_toggle_it_back_off() {
        let rec = Rec::default();
        *rec.num_lock.lock().unwrap() = false;
        let mut s = sink(&rec);
        s.key(0x62, true, false);
        *rec.num_lock.lock().unwrap() = false; // the OS has not caught up yet
        s.key(0x63, true, false);
        let toggles = ops(&rec).iter().filter(|o| **o == Op::Key(Scan { code: 0x45, extended: false }, true)).count();
        assert_eq!(toggles, 1, "{:?}", ops(&rec));
    }

    /// Modifiers arrive already mapped by the controller and are injected as sent: GUI is the Windows
    /// key, Alt is Alt.
    #[test]
    fn modifiers_are_injected_as_the_controller_sent_them() {
        let rec = Rec::default();
        let mut s = sink(&rec);
        s.key(0xE3, true, false);
        s.key(0xE2, true, false);
        assert_eq!(
            ops(&rec),
            vec![Op::Key(Scan { code: 0x5B, extended: true }, true), Op::Key(Scan { code: 0x38, extended: false }, true)]
        );
    }

    #[test]
    fn buttons_and_wheel_follow_the_wire_conventions() {
        let rec = Rec::default();
        let mut s = sink(&rec);
        s.button(0, true);
        s.button(1, true);
        s.button(2, true);
        s.button(3, true);
        s.button(4, false);
        s.wheel(2, 3); // Mac: 2 left, 3 up
        assert_eq!(
            ops(&rec),
            vec![
                Op::Button(WinButton::Left, true),
                Op::Button(WinButton::Right, true),
                Op::Button(WinButton::Middle, true),
                Op::Button(WinButton::X1, true),
                Op::Button(WinButton::X2, false),
                Op::Wheel(3, -2),
            ]
        );
    }

    #[test]
    fn unmappable_input_is_reported_not_injected_as_something_else() {
        let rec = Rec::default();
        let mut s = InjectSink::new(rec.clone(), screen(), |_| {});
        s.key(0x48, true, false); // Pause: no single scan code
        assert!(s.last_err.as_deref().is_some_and(|e| e.contains("0x48")), "{:?}", s.last_err);
        s.button(9, true);
        assert!(s.last_err.as_deref().is_some_and(|e| e.contains("button 9")), "{:?}", s.last_err);
        assert!(ops(&rec).is_empty(), "nothing injected: {:?}", ops(&rec));
    }

    #[test]
    fn a_failing_injection_is_logged_once_per_distinct_error() {
        static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let rec = Rec::default();
        *rec.fail.lock().unwrap() = Some("SendInput inserted 0 of 1".into());
        let mut s = InjectSink::new(rec.clone(), screen(), |m| LOG.lock().unwrap().push(m.into()));
        for _ in 0..50 {
            s.key(0x04, true, false);
        }
        assert_eq!(LOG.lock().unwrap().len(), 1, "{:?}", LOG.lock().unwrap());
    }
}
