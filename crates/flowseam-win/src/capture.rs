//! This PC's own keyboard and mouse, as the forwarder's captured events: what lets it drive a peer.
//!
//! Pure. The low-level hooks (`hook`, Windows only) hand over the fields Windows reports; [`Translator`]
//! decides what each one is and whether Windows still gets it. The live session attaches its [`Target`]
//! (its forwarder); between sessions nothing is attached and every event passes.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use flowseam_core::backend::{CapturedEvent, Disposition};
use flowseam_core::proto::Point;

use crate::keymap::scan_to_hid;

/// Tags every event this process injects ("kmsh"). The hooks pass those untouched: they are a peer's input
/// arriving here, and capturing them would send the peer its own input back.
pub const INJECT_MARK: usize = 0x6B6D_7368;

pub const LLKHF_EXTENDED: u32 = 0x01;
pub const LLKHF_UP: u32 = 0x80;

pub const WM_MOUSEMOVE: u32 = 0x0200;
pub const WM_LBUTTONDOWN: u32 = 0x0201;
pub const WM_LBUTTONUP: u32 = 0x0202;
pub const WM_RBUTTONDOWN: u32 = 0x0204;
pub const WM_RBUTTONUP: u32 = 0x0205;
pub const WM_MBUTTONDOWN: u32 = 0x0207;
pub const WM_MBUTTONUP: u32 = 0x0208;
pub const WM_MOUSEWHEEL: u32 = 0x020A;
pub const WM_XBUTTONDOWN: u32 = 0x020B;
pub const WM_XBUTTONUP: u32 = 0x020C;
pub const WM_MOUSEHWHEEL: u32 = 0x020E;

/// One wheel notch. High-resolution wheels report fractions of it; they add up to a line on the wire.
pub const WHEEL_DELTA: i32 = 120;

/// The live session's side of capture: its forwarder, behind whatever lock the session keeps.
pub trait Target: Send + Sync {
    /// One captured event; the verdict is whether Windows still gets it.
    fn event(&self, ev: CapturedEvent) -> Disposition;
    /// Where a move puts the cursor (it may be past the edge of the desktop), before the verdict on it.
    fn cursor(&self, at: Point);
    /// The clock: `true` while another tick is needed (an edge dwell is running with the mouse at rest).
    fn tick(&self) -> bool;
    /// Whether the clock has to run at all now.
    fn needs_tick(&self) -> bool;
    /// Whether this machine is driving a peer now, so local input must not reach Windows.
    fn grabbed(&self) -> bool;
}

static TARGET: Mutex<Option<Arc<dyn Target>>> = Mutex::new(None);

/// The session that starts takes over capture.
pub fn attach(t: Arc<dyn Target>) {
    *TARGET.lock().unwrap_or_else(|e| e.into_inner()) = Some(t);
}

/// The session that ends lets go of it.
pub fn detach() {
    *TARGET.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

pub fn target() -> Option<Arc<dyn Target>> {
    TARGET.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Held by each test that attaches: there is one target per process, and tests run in parallel.
#[cfg(test)]
pub(crate) static TARGET_TESTS: Mutex<()> = Mutex::new(());

/// The `short` a 32-bit message field carries in its high word: a wheel delta or an X button number.
fn high_word(data: u32) -> i32 {
    i32::from((data >> 16) as u16 as i16)
}

/// Marks the probes the router injects while this PC drives a peer ("kmsp"): each hook swallows and counts
/// them, so a count that stops growing means Windows took that hook out.
pub const PROBE_MARK: usize = 0x6B6D_7370;

static KEY_PROBES: AtomicU64 = AtomicU64::new(0);
static MOUSE_PROBES: AtomicU64 = AtomicU64::new(0);

/// The keyboard hook saw a probe.
pub fn key_probe_seen() {
    KEY_PROBES.fetch_add(1, Ordering::SeqCst);
}

/// The mouse hook saw a probe.
pub fn mouse_probe_seen() {
    MOUSE_PROBES.fetch_add(1, Ordering::SeqCst);
}

/// How many probes each hook has seen: (keyboard, mouse).
pub fn probes_seen() -> (u64, u64) {
    (KEY_PROBES.load(Ordering::SeqCst), MOUSE_PROBES.load(Ordering::SeqCst))
}

/// Which hook a probe round found gone: `before` are the counts when its probes went out, `now` the counts
/// after they had time to arrive. `None` when both saw theirs.
pub fn hooks_gone(before: (u64, u64), now: (u64, u64)) -> Option<&'static str> {
    match (now.0 > before.0, now.1 > before.1) {
        (true, true) => None,
        (false, true) => Some("keyboard hook"),
        (true, false) => Some("mouse hook"),
        (false, false) => Some("keyboard and mouse hooks"),
    }
}

/// Consecutive probe rounds a hook must miss before it counts as gone: one miss can be a round that went out
/// just before a slow moment, or another program's hook that ate one probe.
pub const MISSES_TO_GONE: u8 = 2;

/// Probe rounds, judged together: a hook is gone after [`MISSES_TO_GONE`] rounds in a row that it missed.
#[derive(Debug, Default)]
pub struct ProbeJudge {
    misses: u8,
}

impl ProbeJudge {
    /// One finished round (`hooks_gone` of its counts). `Some` names what is gone, once it has been missing
    /// long enough; the count then starts over.
    pub fn round(&mut self, gone: Option<&'static str>) -> Option<&'static str> {
        let Some(gone) = gone else {
            self.misses = 0;
            return None;
        };
        self.misses += 1;
        if self.misses < MISSES_TO_GONE {
            return None;
        }
        self.misses = 0;
        Some(gone)
    }
}

/// A physical key as a hook report names it: its scan code (taken whole, so AltGr's fake Ctrl is its own
/// key) and extended flag, which stay the same from press to release. The virtual key does not: a keypad
/// key pressed with Shift and released without it reports Home, then Numpad7. Only a key with no scan code
/// (some media keys) goes by its virtual key.
type RawKey = (u32, u32, bool);

fn raw_key(vk: u32, scan: u32, extended: bool) -> RawKey {
    if scan == 0 { (vk, 0, extended) } else { (0, scan, extended) }
}

/// Hook reports to events and verdicts. One per hook thread: it keeps what the wheel has not turned into a
/// whole line yet, and which keys and buttons Windows saw go down.
///
/// Windows gets the release of everything it saw pressed, whatever the forwarder says, a session attached or
/// not, a usage for the key or not: a switch to a peer with Ctrl still held swallows the rest of that key,
/// and Windows would keep Ctrl down until it is pressed again. The forwarder still hears the release, so the
/// peer lets go of it too. The other way round, a key whose press went to a peer does not start typing here
/// when the cursor comes home with it still held: its auto-repeat stays off Windows until it is released.
#[derive(Debug, Default)]
pub struct Translator {
    wheel_v: i32,
    wheel_h: i32,
    /// Whether the wheel's leftover was gathered while driving: it is dropped when that changes.
    wheel_driving: bool,
    passed: std::collections::BTreeSet<RawKey>,
    withheld: std::collections::BTreeSet<RawKey>,
    buttons: u8,
}

impl Translator {
    /// A `WH_KEYBOARD_LL` report, with the live session's target if one is attached. A key with no usage is
    /// no event: passed while input is local, swallowed while it goes to a peer (Windows' fake Ctrl under
    /// AltGr must not type here then either).
    pub fn key(&mut self, t: Option<&dyn Target>, vk: u32, scan: u32, flags: u32, extra: usize) -> Disposition {
        if extra == INJECT_MARK {
            return Disposition::Pass;
        }
        let extended = flags & LLKHF_EXTENDED != 0;
        let id = raw_key(vk, scan, extended);
        let down = flags & LLKHF_UP == 0;
        let d = match (t, scan_to_hid(scan, extended, vk)) {
            (Some(t), Some(hid)) => t.event(CapturedEvent::Key { hid, down }),
            (Some(t), None) => idle(t),
            (None, _) => Disposition::Pass,
        };
        if !down {
            self.withheld.remove(&id);
            return if self.passed.remove(&id) { Disposition::Pass } else { d };
        }
        match d {
            Disposition::Pass if self.withheld.contains(&id) => Disposition::Swallow,
            Disposition::Pass => {
                self.passed.insert(id);
                d
            }
            Disposition::Swallow => {
                if !self.passed.contains(&id) {
                    self.withheld.insert(id);
                }
                d
            }
        }
    }

    /// The button rule of [`Translator`]'s doc, on a button event's verdict.
    fn button(&mut self, button: u8, down: bool, d: Disposition) -> Disposition {
        let bit = 1u8 << button;
        match (down, d) {
            (true, Disposition::Pass) => {
                self.buttons |= bit;
                d
            }
            (false, _) if self.buttons & bit != 0 => {
                self.buttons &= !bit;
                Disposition::Pass
            }
            _ => d,
        }
    }

    /// A `WH_MOUSE_LL` report, with the live session's target if one is attached. `pt` is where the move puts
    /// the cursor; `cursor` is where it is now, so their difference is the move, Windows' acceleration
    /// included. While a peer is driven the moves are swallowed and the cursor stays put, so the difference
    /// stays the move.
    pub fn mouse(&mut self, t: Option<&dyn Target>, msg: u32, pt: Point, data: u32, extra: usize, cursor: Point) -> Disposition {
        if extra == INJECT_MARK {
            return Disposition::Pass;
        }
        let Some(t) = t else {
            // No session: Windows gets it all, and keeps count of the buttons it saw go down.
            if let Some((button, down)) = button_of(msg, data) {
                self.button(button, down, Disposition::Pass);
            }
            return Disposition::Pass;
        };
        if matches!(msg, WM_MOUSEWHEEL | WM_MOUSEHWHEEL) {
            let driving = t.grabbed();
            if driving != self.wheel_driving {
                (self.wheel_v, self.wheel_h, self.wheel_driving) = (0, 0, driving);
            }
        }
        let ev = match msg {
            WM_MOUSEMOVE => {
                t.cursor(pt);
                let (dx, dy) = (pt.x - cursor.x, pt.y - cursor.y);
                (dx != 0 || dy != 0).then_some(CapturedEvent::Motion { dx, dy })
            }
            WM_LBUTTONDOWN..=WM_MBUTTONUP | WM_XBUTTONDOWN | WM_XBUTTONUP => {
                button_of(msg, data).map(|(button, down)| CapturedEvent::Button { button, down })
            }
            // Same vertical sense on the wire; the wire's horizontal axis is positive to the left (the Mac's),
            // Windows' to the right.
            WM_MOUSEWHEEL => lines(&mut self.wheel_v, high_word(data)).map(|dy| CapturedEvent::Wheel { dx: 0, dy }),
            WM_MOUSEHWHEEL => lines(&mut self.wheel_h, high_word(data)).map(|h| CapturedEvent::Wheel { dx: -h, dy: 0 }),
            _ => None,
        };
        match ev {
            Some(ev @ CapturedEvent::Button { button, down }) => {
                let d = t.event(ev);
                self.button(button, down, d)
            }
            Some(ev) => t.event(ev),
            None => idle(t),
        }
    }
}

/// The button a mouse message is about, CoreGraphics numbering as on the wire (0 left, 1 right, 2 middle,
/// 3 back, 4 forward), and whether it went down. `None` for other messages and X buttons past the second.
fn button_of(msg: u32, data: u32) -> Option<(u8, bool)> {
    match msg {
        WM_LBUTTONDOWN => Some((0, true)),
        WM_LBUTTONUP => Some((0, false)),
        WM_RBUTTONDOWN => Some((1, true)),
        WM_RBUTTONUP => Some((1, false)),
        WM_MBUTTONDOWN => Some((2, true)),
        WM_MBUTTONUP => Some((2, false)),
        WM_XBUTTONDOWN | WM_XBUTTONUP => match high_word(data) {
            1 => Some((3, msg == WM_XBUTTONDOWN)),
            2 => Some((4, msg == WM_XBUTTONDOWN)),
            _ => None,
        },
        _ => None,
    }
}

/// Whole lines out of the wheel's running total; the rest stays for the next notch.
fn lines(acc: &mut i32, delta: i32) -> Option<i32> {
    *acc = acc.saturating_add(delta);
    let n = *acc / WHEEL_DELTA;
    *acc -= n * WHEEL_DELTA;
    (n != 0).then_some(n)
}

/// The verdict on input that is no event: it goes nowhere while a peer is driven.
fn idle(t: &dyn Target) -> Disposition {
    if t.grabbed() { Disposition::Swallow } else { Disposition::Pass }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Rec {
        events: Mutex<Vec<CapturedEvent>>,
        cursors: Mutex<Vec<Point>>,
        grabbed: bool,
        verdict: Option<Disposition>,
    }

    impl Target for Rec {
        fn event(&self, ev: CapturedEvent) -> Disposition {
            self.events.lock().unwrap().push(ev);
            self.verdict.unwrap_or(Disposition::Pass)
        }
        fn cursor(&self, at: Point) {
            self.cursors.lock().unwrap().push(at);
        }
        fn tick(&self) -> bool {
            false
        }
        fn needs_tick(&self) -> bool {
            false
        }
        fn grabbed(&self) -> bool {
            self.grabbed
        }
    }

    /// A target whose verdict can change between events, as the forwarder's does on a switch.
    #[derive(Default)]
    struct Switching(Mutex<Vec<CapturedEvent>>, std::sync::atomic::AtomicBool);

    impl Target for Switching {
        fn event(&self, ev: CapturedEvent) -> Disposition {
            self.0.lock().unwrap().push(ev);
            if self.grabbed() { Disposition::Swallow } else { Disposition::Pass }
        }
        fn cursor(&self, _at: Point) {}
        fn tick(&self) -> bool {
            false
        }
        fn needs_tick(&self) -> bool {
            false
        }
        fn grabbed(&self) -> bool {
            self.1.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    /// Ctrl and the left button go down here, then the cursor leaves for a peer: their releases reach Windows
    /// (or they stay down here) and the forwarder (so the peer lets go too). A key pressed while driving does
    /// not reach Windows either way.
    #[test]
    fn windows_gets_the_release_of_whatever_it_saw_pressed() {
        let t = Switching::default();
        let mut tr = Translator::default();
        assert_eq!(tr.key(on(&t), 0xA2, 0x1D, 0, 0), Disposition::Pass);
        assert_eq!(tr.mouse(on(&t), WM_LBUTTONDOWN, P, 0, 0, P), Disposition::Pass);
        t.1.store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(tr.key(on(&t), 0x41, 0x1E, 0, 0), Disposition::Swallow, "a key pressed while driving");
        assert_eq!(tr.key(on(&t), 0xA2, 0x1D, LLKHF_UP, 0), Disposition::Pass, "Ctrl up");
        assert_eq!(tr.key(on(&t), 0xA2, 0x1D, LLKHF_UP, 0), Disposition::Swallow, "a second up is the peer's alone");
        assert_eq!(tr.mouse(on(&t), WM_LBUTTONUP, P, 0, 0, P), Disposition::Pass, "left up");
        assert_eq!(tr.key(on(&t), 0x41, 0x1E, LLKHF_UP, 0), Disposition::Swallow, "A was never Windows' key");
        assert_eq!(tr.mouse(on(&t), WM_RBUTTONUP, P, 0, 0, P), Disposition::Swallow, "nor the right button");
        assert_eq!(
            *t.0.lock().unwrap(),
            [
                CapturedEvent::Key { hid: 0xE0, down: true },
                CapturedEvent::Button { button: 0, down: true },
                CapturedEvent::Key { hid: 0x04, down: true },
                CapturedEvent::Key { hid: 0xE0, down: false },
                CapturedEvent::Key { hid: 0xE0, down: false },
                CapturedEvent::Button { button: 0, down: false },
                CapturedEvent::Key { hid: 0x04, down: false },
                CapturedEvent::Button { button: 1, down: false },
            ],
            "the forwarder hears every one of them"
        );
    }

    impl Rec {
        fn events(&self) -> Vec<CapturedEvent> {
            self.events.lock().unwrap().clone()
        }
    }

    const P: Point = Point { x: 100, y: 200 };

    fn on(t: &dyn Target) -> Option<&dyn Target> {
        Some(t)
    }

    fn wheel(delta: i16) -> u32 {
        u32::from(delta as u16) << 16
    }

    /// A peer's input arrives here injected with our mark: handing it to the forwarder would send it back.
    #[test]
    fn our_own_injection_passes_without_reaching_the_forwarder() {
        let t = Rec { grabbed: true, verdict: Some(Disposition::Swallow), ..Rec::default() };
        let mut tr = Translator::default();
        assert_eq!(tr.key(on(&t), 0x41, 0x1E, 0, INJECT_MARK), Disposition::Pass);
        assert_eq!(tr.mouse(on(&t), WM_MOUSEMOVE, Point { x: 110, y: 200 }, 0, INJECT_MARK, P), Disposition::Pass);
        assert_eq!(tr.mouse(on(&t), WM_LBUTTONDOWN, P, 0, INJECT_MARK, P), Disposition::Pass);
        assert!(t.events().is_empty());
        assert!(t.cursors.lock().unwrap().is_empty());
        // Another program's injection (no mark, or a different one) is input at this machine.
        assert_eq!(tr.key(on(&t), 0x41, 0x1E, 0, 0), Disposition::Swallow);
        assert_eq!(tr.key(on(&t), 0x41, 0x1E, 0, 0x5350_494B), Disposition::Swallow);
        assert_eq!(t.events().len(), 2);
    }

    #[test]
    fn keys_go_by_scan_code_with_their_direction() {
        let t = Rec::default();
        let mut tr = Translator::default();
        tr.key(on(&t), 0x41, 0x1E, 0, 0);
        tr.key(on(&t), 0x41, 0x1E, LLKHF_UP, 0);
        tr.key(on(&t), 0x26, 0x48, LLKHF_EXTENDED, 0);
        tr.key(on(&t), 0x68, 0x48, 0, 0);
        tr.key(on(&t), 0xAF, 0x30, LLKHF_EXTENDED, 0);
        assert_eq!(
            t.events(),
            [
                CapturedEvent::Key { hid: 0x04, down: true },
                CapturedEvent::Key { hid: 0x04, down: false },
                CapturedEvent::Key { hid: 0x52, down: true },
                CapturedEvent::Key { hid: 0x60, down: true },
                CapturedEvent::Key { hid: 0x80, down: true },
            ],
            "A down and up, Up (extended 0x48), keypad 8 (bare 0x48), Volume Up"
        );
    }

    /// A key with no usage reaches Windows only while input is local.
    #[test]
    fn a_key_without_a_usage_follows_the_grab() {
        let mut tr = Translator::default();
        let local = Rec::default();
        assert_eq!(tr.key(on(&local), 0xA2, 0x21D, 0, 0), Disposition::Pass);
        let driving = Rec { grabbed: true, ..Rec::default() };
        assert_eq!(tr.key(on(&driving), 0xA2, 0x21D, 0, 0), Disposition::Swallow);
        assert!(local.events().is_empty() && driving.events().is_empty());
    }

    #[test]
    fn the_verdict_is_the_forwarders() {
        let mut tr = Translator::default();
        let t = Rec { verdict: Some(Disposition::Swallow), ..Rec::default() };
        assert_eq!(tr.key(on(&t), 0x41, 0x1E, 0, 0), Disposition::Swallow);
        assert_eq!(tr.mouse(on(&t), WM_RBUTTONDOWN, P, 0, 0, P), Disposition::Swallow);
        let t = Rec { grabbed: true, verdict: Some(Disposition::Pass), ..Rec::default() };
        assert_eq!(tr.mouse(on(&t), WM_MOUSEMOVE, Point { x: 101, y: 200 }, 0, 0, P), Disposition::Pass);
    }

    /// The move is where it puts the cursor minus where the cursor is; the forwarder also hears where that is,
    /// first, past the edge or not, so a dwell at the edge can start on it.
    #[test]
    fn a_move_is_the_cursor_sample_then_the_difference() {
        let t = Rec::default();
        let mut tr = Translator::default();
        tr.mouse(on(&t), WM_MOUSEMOVE, Point { x: 95, y: 207 }, 0, 0, P);
        tr.mouse(on(&t), WM_MOUSEMOVE, Point { x: -2565, y: 300 }, 0, 0, Point { x: -2560, y: 300 });
        assert_eq!(t.events(), [CapturedEvent::Motion { dx: -5, dy: 7 }, CapturedEvent::Motion { dx: -5, dy: 0 }]);
        assert_eq!(*t.cursors.lock().unwrap(), [Point { x: 95, y: 207 }, Point { x: -2565, y: 300 }]);
    }

    #[test]
    fn a_move_that_moves_nothing_is_no_event() {
        let t = Rec { grabbed: true, ..Rec::default() };
        let mut tr = Translator::default();
        assert_eq!(tr.mouse(on(&t), WM_MOUSEMOVE, P, 0, 0, P), Disposition::Swallow);
        assert!(t.events().is_empty());
        assert_eq!(t.cursors.lock().unwrap().len(), 1, "the cursor sample still counts for a dwell");
    }

    /// CoreGraphics numbering on the wire: 0 left, 1 right, 2 middle, 3 back, 4 forward.
    #[test]
    fn buttons_carry_the_wire_numbers() {
        let t = Rec::default();
        let mut tr = Translator::default();
        for (msg, data) in [
            (WM_LBUTTONDOWN, 0),
            (WM_LBUTTONUP, 0),
            (WM_RBUTTONDOWN, 0),
            (WM_MBUTTONUP, 0),
            (WM_XBUTTONDOWN, 1 << 16),
            (WM_XBUTTONUP, 2 << 16),
            (WM_XBUTTONDOWN, 3 << 16),
        ] {
            tr.mouse(on(&t), msg, P, data, 0, P);
        }
        assert_eq!(
            t.events(),
            [
                CapturedEvent::Button { button: 0, down: true },
                CapturedEvent::Button { button: 0, down: false },
                CapturedEvent::Button { button: 1, down: true },
                CapturedEvent::Button { button: 2, down: false },
                CapturedEvent::Button { button: 3, down: true },
                CapturedEvent::Button { button: 4, down: false },
            ],
            "an X button that is neither 1 nor 2 is no button"
        );
    }

    /// A notch is a line; a high-resolution wheel's fractions add up to one, in either direction.
    #[test]
    fn the_wheel_turns_notches_into_lines() {
        let t = Rec::default();
        let mut tr = Translator::default();
        tr.mouse(on(&t), WM_MOUSEWHEEL, P, wheel(120), 0, P);
        tr.mouse(on(&t), WM_MOUSEWHEEL, P, wheel(-240), 0, P);
        for _ in 0..3 {
            tr.mouse(on(&t), WM_MOUSEWHEEL, P, wheel(40), 0, P);
        }
        tr.mouse(on(&t), WM_MOUSEWHEEL, P, wheel(-60), 0, P);
        tr.mouse(on(&t), WM_MOUSEWHEEL, P, wheel(-60), 0, P);
        assert_eq!(
            t.events(),
            [
                CapturedEvent::Wheel { dx: 0, dy: 1 },
                CapturedEvent::Wheel { dx: 0, dy: -2 },
                CapturedEvent::Wheel { dx: 0, dy: 1 },
                CapturedEvent::Wheel { dx: 0, dy: -1 },
            ]
        );
    }

    /// Tilting right is a positive Windows delta and a negative wire one; a fraction of a notch is swallowed
    /// while driving and passed while local.
    #[test]
    fn the_horizontal_wheel_turns_around_and_fractions_follow_the_grab() {
        let t = Rec::default();
        let mut tr = Translator::default();
        tr.mouse(on(&t), WM_MOUSEHWHEEL, P, wheel(120), 0, P);
        assert_eq!(t.events(), [CapturedEvent::Wheel { dx: -1, dy: 0 }]);
        assert_eq!(tr.mouse(on(&t), WM_MOUSEWHEEL, P, wheel(30), 0, P), Disposition::Pass);
        let driving = Rec { grabbed: true, ..Rec::default() };
        assert_eq!(tr.mouse(on(&driving), WM_MOUSEWHEEL, P, wheel(30), 0, P), Disposition::Swallow);
        assert!(driving.events().is_empty());
    }

    fn switching(driving: bool) -> Switching {
        let t = Switching::default();
        t.1.store(driving, std::sync::atomic::Ordering::SeqCst);
        t
    }

    /// Ctrl went down while no session was attached (a reconnect), and AltGr's fake Ctrl, which has no usage,
    /// went down while input was local: Windows saw both presses, so it gets both releases while driving.
    #[test]
    fn windows_gets_releases_of_presses_no_session_or_usage_saw() {
        let mut tr = Translator::default();
        assert_eq!(tr.key(None, 0xA2, 0x1D, 0, 0), Disposition::Pass, "Ctrl down, no session");
        assert_eq!(tr.mouse(None, WM_LBUTTONDOWN, P, 0, 0, P), Disposition::Pass, "left down, no session");
        let local = switching(false);
        assert_eq!(tr.key(on(&local), 0xA2, 0x21D, 0, 0), Disposition::Pass, "AltGr's fake Ctrl, local");
        let driving = switching(true);
        assert_eq!(tr.key(on(&driving), 0xA2, 0x1D, LLKHF_UP, 0), Disposition::Pass, "Ctrl up");
        assert_eq!(tr.key(on(&driving), 0xA2, 0x21D, LLKHF_UP, 0), Disposition::Pass, "fake Ctrl up");
        assert_eq!(tr.mouse(on(&driving), WM_LBUTTONUP, P, 0, 0, P), Disposition::Pass, "left up");
        assert_eq!(tr.mouse(None, WM_RBUTTONUP, P, 0, 0, P), Disposition::Pass, "no session: everything passes");
    }

    /// A key pressed while driving is still held when the cursor comes home: its auto-repeat does not type
    /// here, its release and the next press of it are Windows' again.
    #[test]
    fn a_key_held_coming_home_does_not_type_here() {
        let mut tr = Translator::default();
        let t = switching(true);
        assert_eq!(tr.key(on(&t), 0x41, 0x1E, 0, 0), Disposition::Swallow);
        t.1.store(false, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(tr.key(on(&t), 0x41, 0x1E, 0, 0), Disposition::Swallow, "auto-repeat, now local");
        assert_eq!(tr.key(on(&t), 0x41, 0x1E, LLKHF_UP, 0), Disposition::Pass, "the release");
        assert_eq!(tr.key(on(&t), 0x41, 0x1E, 0, 0), Disposition::Pass, "a new press");
        assert_eq!(t.0.lock().unwrap().len(), 4, "the forwarder hears all of them");
    }

    /// Half a notch gathered while local does not make the first half notch while driving a whole line.
    #[test]
    fn the_wheel_leftover_does_not_cross_a_switch() {
        let mut tr = Translator::default();
        let t = switching(false);
        tr.mouse(on(&t), WM_MOUSEWHEEL, P, wheel(60), 0, P);
        t.1.store(true, std::sync::atomic::Ordering::SeqCst);
        tr.mouse(on(&t), WM_MOUSEWHEEL, P, wheel(60), 0, P);
        assert!(t.0.lock().unwrap().is_empty(), "{:?}", t.0.lock().unwrap());
        tr.mouse(on(&t), WM_MOUSEWHEEL, P, wheel(60), 0, P);
        assert_eq!(*t.0.lock().unwrap(), [CapturedEvent::Wheel { dx: 0, dy: 1 }]);
    }

    /// A keypad key pressed with Shift (Home) and released without it (Numpad7) is one key: Windows saw it
    /// pressed, so it gets the release.
    #[test]
    fn a_key_whose_virtual_key_changes_while_held_is_still_one_key() {
        let mut tr = Translator::default();
        let local = switching(false);
        assert_eq!(tr.key(on(&local), 0x24, 0x47, 0, 0), Disposition::Pass, "Home, Shift held");
        let driving = switching(true);
        assert_eq!(tr.key(on(&driving), 0x67, 0x47, LLKHF_UP, 0), Disposition::Pass, "Numpad7 up");
        assert_eq!(tr.key(on(&driving), 0x67, 0x47, 0, 0), Disposition::Swallow, "and the next press goes to the peer");
    }

    /// One missed round is not enough; two in a row are, and a round both hooks saw starts the count over.
    #[test]
    fn a_hook_is_gone_after_two_missed_rounds_in_a_row() {
        let mut j = ProbeJudge::default();
        assert_eq!(j.round(Some("mouse hook")), None);
        assert_eq!(j.round(None), None);
        assert_eq!(j.round(Some("mouse hook")), None);
        assert_eq!(j.round(Some("keyboard hook")), Some("keyboard hook"));
        assert_eq!(j.round(Some("keyboard hook")), None, "the count starts over after a verdict");
    }

    #[test]
    fn a_probe_round_names_the_hook_that_saw_nothing() {
        assert_eq!(hooks_gone((3, 7), (4, 8)), None);
        assert_eq!(hooks_gone((3, 7), (3, 8)), Some("keyboard hook"));
        assert_eq!(hooks_gone((3, 7), (5, 7)), Some("mouse hook"));
        assert_eq!(hooks_gone((3, 7), (3, 7)), Some("keyboard and mouse hooks"));
    }

    #[test]
    fn only_the_attached_session_gets_events() {
        let _alone = TARGET_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let t: Arc<dyn Target> = Arc::new(Rec::default());
        attach(t.clone());
        assert!(target().is_some_and(|x| Arc::ptr_eq(&x, &t)));
        detach();
        assert!(target().is_none());
    }
}
