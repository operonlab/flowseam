//! Hiding the cursor from the foreground as well. The background hide (`SetsCursorInBackground` plus
//! `CGDisplayHideCursor`) works until the crossing passes over the Dock: then, even with the warp announced
//! to the Dock, it hid the cursor in 1 run of 4 (measured 2026-11-05 with `screencapture -C`, judged by
//! the pixels at the cursor). With this added it hid it in 4 of 4 crossings and 5 of 5 grabs started on
//! the Dock. So while a peer is driven the worker also makes itself the active app (accessory policy: no
//! Dock icon, not in Cmd-Tab, one invisible 1x1 window), hides the cursor there, and hands activation back
//! to the app that had it.
//!
//! The grab is decided on the tap thread and AppKit runs only on the main thread: [`request`] records the
//! wanted state from any thread, and [`pump`], the worker main loop's sleep, applies it.
//!
//! ponytail: a SIGINT/SIGHUP exit (`backend.rs` signal watcher) skips the give-back; the system then picks
//! the next app itself and the hide ends with the process. Route those signals through the main loop if
//! the app that gets the front after such an exit ever matters.

use std::sync::atomic::{AtomicBool, Ordering};

/// Applies (10 ms each) without the front, while it is wanted, before asking for it again: activation
/// normally completes well inside this, so a longer wait means another app took the front.
const RETAKE_AFTER: u32 = 20;
/// Applies after a give-back during which an activation that completes late is undone.
const SETTLE: u32 = 50;

static WANT_HIDDEN: AtomicBool = AtomicBool::new(false);

/// Ask for the cursor hidden (`true`) or shown. Any thread; applied by the next [`pump`].
pub fn request(hidden: bool) {
    WANT_HIDDEN.store(hidden, Ordering::SeqCst);
}

/// The AppKit side, behind a trait so the transitions are tested without a window server.
pub trait FrontOs {
    /// Process id of the frontmost app.
    fn frontmost(&mut self) -> Option<i32>;
    /// Order the invisible window front and ask to become the active app. Activation completes later.
    fn take_front(&mut self);
    /// Whether this process is the active app yet.
    fn is_active(&mut self) -> bool;
    fn hide_cursor(&mut self);
    fn unhide_cursor(&mut self);
    /// Order the window out and activate `prev`; with no `prev` (or one that quit), step aside so the
    /// system picks the next app. The keyboard must never be left on the invisible window.
    fn give_front(&mut self, prev: Option<i32>);
}

/// Applied state, advanced once per [`Front::apply`].
#[derive(Debug, Default)]
pub struct Front {
    /// The front has been asked for and not given back.
    taken: bool,
    /// `NSCursor hide` calls since the take; the give-back unhides as many, so the count stays balanced.
    hides: u32,
    /// Hidden since this process last became active. Cleared when the front is lost and asked for again.
    hidden_here: bool,
    /// Consecutive applies without the front while it is wanted.
    away: u32,
    /// Whom to give the front back to: the frontmost app when it was first taken.
    prev: Option<i32>,
    /// Applies left after a give-back in which a late activation is undone, and whom to give it to then.
    settle: u32,
    gave_to: Option<i32>,
}

impl Front {
    /// Move toward `want`. The hide waits until the system reports this process frontmost, so it is not
    /// sent while the process may still count as background; each activation gets one hide. A front lost
    /// to another app while wanted is asked for again after [`RETAKE_AFTER`] applies, and an activation
    /// that completes after the give-back is undone during the next [`SETTLE`] applies, so the keyboard is
    /// never left on the invisible window.
    pub fn apply(&mut self, want: bool, me: i32, os: &mut dyn FrontOs) {
        if want {
            if !self.taken {
                self.prev = os.frontmost().filter(|&pid| pid != me);
                os.take_front();
                (self.taken, self.hidden_here, self.away, self.settle) = (true, false, 0, 0);
                eprintln!("flowseam: taking the front (was pid {:?})", self.prev);
            }
            if os.is_active() {
                self.away = 0;
                if !self.hidden_here {
                    os.hide_cursor();
                    self.hides += 1;
                    self.hidden_here = true;
                }
            } else {
                self.away += 1;
                if self.away >= RETAKE_AFTER {
                    os.take_front();
                    (self.hidden_here, self.away) = (false, 0);
                    eprintln!("flowseam: the front went to pid {:?}; taking it again", os.frontmost());
                }
            }
        } else if self.taken {
            for _ in 0..self.hides {
                os.unhide_cursor();
            }
            let prev = self.prev.take();
            os.give_front(prev);
            (self.taken, self.hides, self.hidden_here, self.settle, self.gave_to) = (false, 0, false, SETTLE, prev);
            eprintln!("flowseam: gave the front back (to pid {prev:?}), cursor shown");
        } else if self.settle > 0 {
            self.settle -= 1;
            if os.is_active() {
                os.give_front(self.gave_to);
                eprintln!("flowseam: became active after the give-back; gave it back again");
            }
        }
    }
}

#[cfg(target_os = "macos")]
pub use sys_front::pump;

#[cfg(target_os = "macos")]
mod sys_front {
    use super::{Front, FrontOs, WANT_HIDDEN};
    use crate::ffi;
    use std::cell::RefCell;
    use std::ffi::{c_void, CStr};
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    type Id = *mut c_void;
    type Untyped = unsafe extern "C" fn();

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct NsRect {
        x: f64,
        y: f64,
        w: f64,
        h: f64,
    }

    // SAFETY (all sends): each cast below matches the Objective-C signature of the selector it is used
    // with; BOOL is passed and returned as i8, which is its size on both arm64 and x86_64.
    unsafe fn send0(obj: Id, sel: &CStr) -> Id {
        let f = std::mem::transmute::<Untyped, unsafe extern "C" fn(Id, Id) -> Id>(ffi::objc_msgSend as Untyped);
        f(obj, ffi::sel_registerName(sel.as_ptr()))
    }
    unsafe fn send_id(obj: Id, sel: &CStr, a: Id) -> Id {
        let f = std::mem::transmute::<Untyped, unsafe extern "C" fn(Id, Id, Id) -> Id>(ffi::objc_msgSend as Untyped);
        f(obj, ffi::sel_registerName(sel.as_ptr()), a)
    }
    /// For setters taking an NSInteger; a BOOL result, if any, is ignored.
    unsafe fn send_int(obj: Id, sel: &CStr, a: isize) {
        let f = std::mem::transmute::<Untyped, unsafe extern "C" fn(Id, Id, isize)>(ffi::objc_msgSend as Untyped);
        f(obj, ffi::sel_registerName(sel.as_ptr()), a)
    }
    unsafe fn send_bool(obj: Id, sel: &CStr, a: i8) {
        let f = std::mem::transmute::<Untyped, unsafe extern "C" fn(Id, Id, i8)>(ffi::objc_msgSend as Untyped);
        f(obj, ffi::sel_registerName(sel.as_ptr()), a)
    }
    unsafe fn send_ret_i32(obj: Id, sel: &CStr) -> i32 {
        let f = std::mem::transmute::<Untyped, unsafe extern "C" fn(Id, Id) -> i32>(ffi::objc_msgSend as Untyped);
        f(obj, ffi::sel_registerName(sel.as_ptr()))
    }
    unsafe fn class(name: &CStr) -> Id {
        ffi::objc_getClass(name.as_ptr())
    }

    struct SysFront {
        app: Id,
        window: Id,
    }

    /// An Objective-C exception cannot unwind through Rust frames: the process aborts with no reason given.
    /// The preprocessor runs at the throw, before unwinding, so the reason and the throw site are logged.
    extern "C" fn log_exception(exc: Id) -> Id {
        // SAFETY: `exc` is the NSException being thrown; description and callStackSymbols return objects
        // whose UTF8String is valid while the exception lives.
        unsafe {
            let text = |o: Id| -> String {
                let s = if o.is_null() { o } else { send0(o, c"description") };
                let p = if s.is_null() { std::ptr::null() } else { send0(s, c"UTF8String") as *const std::ffi::c_char };
                if p.is_null() { "?".into() } else { CStr::from_ptr(p).to_string_lossy().into_owned() }
            };
            eprintln!("flowseam: Objective-C exception: {}\n{}", text(exc), text(send0(exc, c"callStackSymbols")));
        }
        exc
    }

    extern "C" {
        fn objc_setExceptionPreprocessor(f: extern "C" fn(Id) -> Id) -> usize;
    }

    impl SysFront {
        /// `NSApplication` with the accessory policy and the invisible window. Main thread only.
        fn new() -> Option<SysFront> {
            // SAFETY: main thread (the only caller is `pump`); classes are checked for null before use.
            unsafe {
                objc_setExceptionPreprocessor(log_exception);
                let (app_cls, win_cls, color_cls) = (class(c"NSApplication"), class(c"NSWindow"), class(c"NSColor"));
                if app_cls.is_null() || win_cls.is_null() || color_cls.is_null() {
                    return None;
                }
                let app = send0(app_cls, c"sharedApplication");
                if app.is_null() {
                    return None;
                }
                send_int(app, c"setActivationPolicy:", 1); // NSApplicationActivationPolicyAccessory
                send0(app, c"finishLaunching");
                let init = std::mem::transmute::<Untyped, unsafe extern "C" fn(Id, Id, NsRect, usize, usize, i8) -> Id>(
                    ffi::objc_msgSend as Untyped,
                );
                let rect = NsRect { x: 0.0, y: 0.0, w: 1.0, h: 1.0 };
                // styleMask 0 = borderless, backing 2 = buffered; the window is kept for the process lifetime.
                let window = init(send0(win_cls, c"alloc"), ffi::sel_registerName(c"initWithContentRect:styleMask:backing:defer:".as_ptr()), rect, 0, 2, 0);
                if window.is_null() {
                    return None;
                }
                send_bool(window, c"setOpaque:", 0);
                send_id(window, c"setBackgroundColor:", send0(color_cls, c"clearColor"));
                send_bool(window, c"setIgnoresMouseEvents:", 1);
                send_int(window, c"setLevel:", 1000); // NSScreenSaverWindowLevel
                Some(SysFront { app, window })
            }
        }

        /// Hand every queued event to AppKit, waiting up to `wait` for the first. This is the main loop's sleep.
        fn drain(&self, wait: Duration) {
            // SAFETY: main thread; NSDate and the events are autoreleased into the caller's pool.
            unsafe {
                let next = std::mem::transmute::<Untyped, unsafe extern "C" fn(Id, Id, u64, Id, Id, i8) -> Id>(
                    ffi::objc_msgSend as Untyped,
                );
                let since = std::mem::transmute::<Untyped, unsafe extern "C" fn(Id, Id, f64) -> Id>(ffi::objc_msgSend as Untyped);
                let sel_next = ffi::sel_registerName(c"nextEventMatchingMask:untilDate:inMode:dequeue:".as_ptr());
                let mode = ffi::kCFRunLoopDefaultMode as Id;
                let mut until = since(class(c"NSDate"), ffi::sel_registerName(c"dateWithTimeIntervalSinceNow:".as_ptr()), wait.as_secs_f64());
                loop {
                    let ev = next(self.app, sel_next, u64::MAX, until, mode, 1);
                    if ev.is_null() {
                        break;
                    }
                    send_id(self.app, c"sendEvent:", ev);
                    until = send0(class(c"NSDate"), c"distantPast");
                }
            }
        }
    }

    impl FrontOs for SysFront {
        fn frontmost(&mut self) -> Option<i32> {
            // SAFETY: main thread; nil receivers return 0, which is checked.
            unsafe {
                let ws = send0(class(c"NSWorkspace"), c"sharedWorkspace");
                let front = if ws.is_null() { std::ptr::null_mut() } else { send0(ws, c"frontmostApplication") };
                (!front.is_null()).then(|| send_ret_i32(front, c"processIdentifier"))
            }
        }

        fn take_front(&mut self) {
            // SAFETY: main thread.
            unsafe {
                send0(self.window, c"orderFrontRegardless");
                send_bool(self.app, c"activateIgnoringOtherApps:", 1);
            }
        }

        /// Whether the system reports this process frontmost. Not `NSApp.isActive`, which read true the
        /// moment activation was requested (observed), before the switch is known to have happened.
        fn is_active(&mut self) -> bool {
            self.frontmost() == Some(std::process::id() as i32)
        }

        fn hide_cursor(&mut self) {
            // SAFETY: main thread.
            unsafe { send0(class(c"NSCursor"), c"hide") };
        }

        fn unhide_cursor(&mut self) {
            // SAFETY: main thread.
            unsafe { send0(class(c"NSCursor"), c"unhide") };
        }

        fn give_front(&mut self, prev: Option<i32>) {
            // SAFETY: main thread; a pid with no running app yields nil, checked before use.
            unsafe {
                send_id(self.window, c"orderOut:", std::ptr::null_mut());
                let target = prev.map_or(std::ptr::null_mut(), |pid| {
                    let f = std::mem::transmute::<Untyped, unsafe extern "C" fn(Id, Id, i32) -> Id>(ffi::objc_msgSend as Untyped);
                    f(class(c"NSRunningApplication"), ffi::sel_registerName(c"runningApplicationWithProcessIdentifier:".as_ptr()), pid)
                });
                // `activateWithOptions:`, not `activate`: the latter is only Swift's name for it and an
                // unrecognized selector here throws, which aborts the process.
                let activate = std::mem::transmute::<Untyped, unsafe extern "C" fn(Id, Id, usize) -> i8>(ffi::objc_msgSend as Untyped);
                if target.is_null() || activate(target, ffi::sel_registerName(c"activateWithOptions:".as_ptr()), 0) == 0 {
                    send_id(self.app, c"hide:", std::ptr::null_mut());
                }
            }
        }
    }

    thread_local! {
        static STATE: RefCell<Option<(Front, Option<SysFront>)>> = const { RefCell::new(None) };
    }

    const SLICE: Duration = Duration::from_millis(10);

    /// Apply the requested cursor state and hand AppKit its events for `total` (at least once), in
    /// slices short enough that a request waits at most ~10 ms. Main thread only. If AppKit cannot be
    /// set up this logs once and just sleeps, leaving the background hide as the only attempt.
    pub fn pump(total: Duration) {
        let end = Instant::now() + total;
        let me = std::process::id() as i32;
        STATE.with(|s| {
            let mut s = s.borrow_mut();
            let (front, os) = s.get_or_insert_with(|| {
                let os = SysFront::new();
                if os.is_none() {
                    eprintln!("flowseam: AppKit unavailable; the cursor can only be hidden from the background");
                }
                (Front::default(), os)
            });
            loop {
                // SAFETY: balanced push/pop on this thread.
                let pool = unsafe { ffi::objc_autoreleasePoolPush() };
                let left = end.saturating_duration_since(Instant::now());
                match os {
                    Some(os) => {
                        front.apply(WANT_HIDDEN.load(Ordering::SeqCst), me, os);
                        os.drain(left.min(SLICE));
                    }
                    None => std::thread::sleep(left.min(SLICE)),
                }
                unsafe { ffi::objc_autoreleasePoolPop(pool) };
                if Instant::now() >= end {
                    break;
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: i32 = 100;

    /// Records calls. Activation is asynchronous: a `take` completes after `lag` polls of `is_active`, and a
    /// give-back does not cancel one still in flight (as on the real system).
    #[derive(Default)]
    struct Rec {
        front: Option<i32>,
        lag: u32,
        activating: bool,
        active: bool,
        calls: Vec<String>,
    }

    impl FrontOs for Rec {
        fn frontmost(&mut self) -> Option<i32> {
            self.calls.push("frontmost".into());
            self.front
        }
        fn take_front(&mut self) {
            self.calls.push("take".into());
            self.activating = true;
        }
        fn is_active(&mut self) -> bool {
            if self.activating {
                if self.lag == 0 {
                    (self.active, self.activating, self.front) = (true, false, Some(ME));
                } else {
                    self.lag -= 1;
                }
            }
            self.active
        }
        fn hide_cursor(&mut self) {
            self.calls.push("hide".into());
        }
        fn unhide_cursor(&mut self) {
            self.calls.push("unhide".into());
        }
        fn give_front(&mut self, prev: Option<i32>) {
            self.calls.push(format!("give {prev:?}"));
            self.front = prev;
            self.active = false;
        }
    }

    #[test]
    fn a_grab_takes_the_front_and_its_end_gives_it_back_to_the_app_that_had_it() {
        let (mut f, mut os) = (Front::default(), Rec { front: Some(7), ..Rec::default() });
        f.apply(true, ME, &mut os);
        f.apply(false, ME, &mut os);
        assert_eq!(os.calls, ["frontmost", "take", "hide", "unhide", "give Some(7)"]);
        assert_eq!(os.front, Some(7));
    }

    #[test]
    fn the_cursor_is_hidden_only_once_activation_has_completed() {
        let (mut f, mut os) = (Front::default(), Rec { front: Some(7), lag: 2, ..Rec::default() });
        f.apply(true, ME, &mut os);
        f.apply(true, ME, &mut os);
        assert_eq!(os.calls, ["frontmost", "take"], "hidden while still in the background");
        f.apply(true, ME, &mut os);
        assert_eq!(os.calls, ["frontmost", "take", "hide"]);
    }

    #[test]
    fn a_grab_that_ends_before_activation_gives_back_without_an_unmatched_unhide() {
        let (mut f, mut os) = (Front::default(), Rec { front: Some(7), lag: 5, ..Rec::default() });
        f.apply(true, ME, &mut os);
        f.apply(false, ME, &mut os);
        assert_eq!(os.calls, ["frontmost", "take", "give Some(7)"]);
    }

    #[test]
    fn repeated_requests_reach_the_os_once_and_keep_the_first_owner() {
        let (mut f, mut os) = (Front::default(), Rec { front: Some(7), ..Rec::default() });
        f.apply(true, ME, &mut os);
        f.apply(true, ME, &mut os);
        f.apply(false, ME, &mut os);
        f.apply(false, ME, &mut os);
        assert_eq!(os.calls, ["frontmost", "take", "hide", "unhide", "give Some(7)"]);
    }

    /// Released before the activation landed: it lands afterwards and would leave the keyboard on the
    /// invisible window, so the front is given back again.
    #[test]
    fn an_activation_that_lands_after_the_release_is_given_back_again() {
        let (mut f, mut os) = (Front::default(), Rec { front: Some(7), lag: 3, ..Rec::default() });
        f.apply(true, ME, &mut os);
        f.apply(false, ME, &mut os);
        for _ in 0..4 {
            f.apply(false, ME, &mut os);
        }
        assert_eq!(os.calls, ["frontmost", "take", "give Some(7)", "give Some(7)"]);
        assert_eq!((os.front, os.active), (Some(7), false));
    }

    /// Another app takes the front mid-grab: it is asked for again, the cursor is hidden again once it is
    /// back, and the release unhides once per hide and still returns the front to the first owner.
    #[test]
    fn a_front_taken_mid_grab_is_taken_back_and_every_hide_is_undone() {
        let (mut f, mut os) = (Front::default(), Rec { front: Some(7), ..Rec::default() });
        f.apply(true, ME, &mut os);
        (os.active, os.front) = (false, Some(9));
        for _ in 0..RETAKE_AFTER - 1 {
            f.apply(true, ME, &mut os);
        }
        assert_eq!(os.calls, ["frontmost", "take", "hide"], "not yet: activation may just be slow");
        f.apply(true, ME, &mut os);
        f.apply(true, ME, &mut os);
        f.apply(false, ME, &mut os);
        assert_eq!(os.calls, ["frontmost", "take", "hide", "take", "frontmost", "hide", "unhide", "unhide", "give Some(7)"]);
    }

    #[test]
    fn a_show_with_nothing_hidden_touches_nothing() {
        let (mut f, mut os) = (Front::default(), Rec::default());
        f.apply(false, ME, &mut os);
        assert!(os.calls.is_empty());
    }

    #[test]
    fn this_process_or_no_app_in_front_is_never_the_one_given_back_to() {
        for front in [Some(ME), None] {
            let (mut f, mut os) = (Front::default(), Rec { front, ..Rec::default() });
            f.apply(true, ME, &mut os);
            f.apply(false, ME, &mut os);
            assert_eq!(os.calls.last().map(String::as_str), Some("give None"), "front was {front:?}");
        }
    }

    /// Every message `sys_front` sends: (receiver class, selector, sent to the class rather than an instance).
    #[cfg(target_os = "macos")]
    const SENDS: &[(&str, &str, bool)] = &[
        ("NSApplication", "sharedApplication", true),
        ("NSApplication", "setActivationPolicy:", false),
        ("NSApplication", "finishLaunching", false),
        ("NSApplication", "activateIgnoringOtherApps:", false),
        ("NSApplication", "hide:", false),
        ("NSApplication", "nextEventMatchingMask:untilDate:inMode:dequeue:", false),
        ("NSApplication", "sendEvent:", false),
        ("NSWindow", "alloc", true),
        ("NSWindow", "initWithContentRect:styleMask:backing:defer:", false),
        ("NSWindow", "setOpaque:", false),
        ("NSWindow", "setBackgroundColor:", false),
        ("NSWindow", "setIgnoresMouseEvents:", false),
        ("NSWindow", "setLevel:", false),
        ("NSWindow", "orderFrontRegardless", false),
        ("NSWindow", "orderOut:", false),
        ("NSColor", "clearColor", true),
        ("NSDate", "dateWithTimeIntervalSinceNow:", true),
        ("NSDate", "distantPast", true),
        ("NSWorkspace", "sharedWorkspace", true),
        ("NSWorkspace", "frontmostApplication", false),
        ("NSRunningApplication", "runningApplicationWithProcessIdentifier:", true),
        ("NSRunningApplication", "processIdentifier", false),
        ("NSRunningApplication", "activateWithOptions:", false),
        ("NSCursor", "hide", true),
        ("NSCursor", "unhide", true),
        ("NSException", "description", false),
        ("NSException", "callStackSymbols", false),
        ("NSString", "UTF8String", false),
    ];

    /// A selector the receiver does not answer throws, and a throw through Rust aborts the worker mid-grab
    /// (this happened: `-[NSRunningApplication activate]`; `activate` is only Swift's name, and
    /// NSApplication does answer it, so "some class answers" is not enough). Each entry of `SENDS` must be
    /// answered by its own receiver, and every selector literal in this file must have an entry.
    #[cfg(target_os = "macos")]
    #[test]
    fn every_selector_sent_is_one_its_receiver_answers() {
        use std::ffi::{c_void, CString};
        extern "C" {
            fn objc_getClass(name: *const std::ffi::c_char) -> *mut c_void;
            fn object_getClass(obj: *mut c_void) -> *mut c_void;
            fn sel_registerName(name: *const std::ffi::c_char) -> *mut c_void;
            fn class_respondsToSelector(cls: *mut c_void, sel: *mut c_void) -> bool;
        }
        let src = include_str!("front.rs");
        let quote = ['c', '"'].iter().collect::<String>();
        // A literal's opening `c"` follows a non-identifier character; `"alloc"`'s closing `c"` does not.
        let lits: Vec<&str> = src
            .match_indices(quote.as_str())
            .filter(|&(i, _)| !src[..i].ends_with(|ch: char| ch.is_alphanumeric() || ch == '_'))
            .filter_map(|(i, _)| src[i + 2..].split('"').next())
            .filter(|s| !s.is_empty() && s.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == ':'))
            .collect();
        let sels: Vec<&str> = lits.iter().copied().filter(|s| !s.starts_with("NS")).collect();
        assert!(sels.len() >= 20, "the literal scan found too few selectors: {sels:?}");
        for s in &sels {
            assert!(SENDS.iter().any(|e| e.1 == *s), "selector {s} is sent but has no SENDS entry naming its receiver");
        }
        // SAFETY: runtime lookups by NUL-terminated name; a null class is asserted against.
        unsafe {
            for &(class, sel, on_class) in SENDS {
                let k = objc_getClass(CString::new(class).unwrap().as_ptr());
                assert!(!k.is_null(), "class {class} not found");
                let target = if on_class { object_getClass(k) } else { k };
                let s = sel_registerName(CString::new(sel).unwrap().as_ptr());
                assert!(class_respondsToSelector(target, s), "{class} does not answer {sel} (class method: {on_class})");
            }
        }
    }

    #[test]
    fn a_second_grab_gives_back_to_whoever_was_in_front_at_its_start() {
        let (mut f, mut os) = (Front::default(), Rec { front: Some(7), ..Rec::default() });
        f.apply(true, ME, &mut os);
        f.apply(false, ME, &mut os);
        os.front = Some(9);
        f.apply(true, ME, &mut os);
        f.apply(false, ME, &mut os);
        assert_eq!(os.calls.last().map(String::as_str), Some("give Some(9)"));
    }
}
