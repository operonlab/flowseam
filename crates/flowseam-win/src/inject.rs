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

/// flowseam_core's sink for a machine being driven: everything a peer sends is injected through `I`.
/// This machine never captures, so the controller-only calls (grab, hide, show) are reported as bugs.
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
    edge_drop: Option<(EdgeDropSource, Edge)>,
    /// Where this sink last sent the cursor: the base for the next relative move (see flowseam_core::cursor).
    posted: PostedCursor,
}

impl<I: Injector> InjectSink<I> {
    pub fn new(inj: I, displays: Vec<Rect>, log: fn(&str)) -> Self {
        InjectSink { inj, displays, log, last_err: None, num_lock_sent: None, drags: None, posted: PostedCursor::default(), #[cfg(windows)] edge_drop: None }
    }

    /// File drags: presses and crossings go to `hub`.
    pub fn with_drags(mut self, hub: Arc<Mutex<DragHub>>) -> Self {
        self.drags = Some(hub);
        self
    }

    #[cfg(windows)]
    pub fn with_edge_drop(mut self, source: EdgeDropSource, edge: Edge) -> Self {
        self.edge_drop = Some((source, edge));
        self
    }

    pub fn set_displays(&mut self, d: Vec<Rect>) {
        #[cfg(windows)]
        if let Some((source, edge)) = &self.edge_drop { source.set_edge(*edge, d.clone()); }
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

    fn set_grab(&mut self, on: bool) {
        if on {
            (self.log)("bug: grab requested on a machine that only receives input; ignored");
        }
    }
    fn hide(&mut self) {
        (self.log)("bug: hide requested on a machine that only receives input; ignored");
    }
    fn show(&mut self) {}
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
