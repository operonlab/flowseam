//! Cursor-grab guard shared by `MacBackend` and the tap thread (design §5).
//!
//! Every path that shows the cursor again (`set_grabbed(false)`, `Drop`, the
//! tap fault path, the emergency chord, the signal watcher) goes through
//! [`GrabGuard::restore`], so they cannot disagree about what was restored.
//! OS calls sit behind [`CursorOs`] so tests can inject a recorder instead of
//! touching the real cursor.

use crate::state::{render_lock, CursorAction, CursorState, GrabLock};
use flowseam_core::backend::{BackendError, Preflight};
use flowseam_core::proto::Point;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

/// The OS calls a cursor grab needs. The real implementation is `SysCursorOs`
/// in `backend.rs`; tests inject a recording mock.
pub trait CursorOs: Send {
    fn capture_preflight(&self) -> Preflight;
    /// `Ok(None)` means the query cannot tell.
    fn secure_input(&self) -> Result<Option<bool>, BackendError>;
    fn hide_cursor(&mut self) -> Result<(), BackendError>;
    fn show_cursor(&mut self) -> Result<(), BackendError>;
    fn set_association(&mut self, connected: bool) -> Result<(), BackendError>;
    fn warp(&mut self, p: Point) -> Result<(), BackendError>;
    fn main_center(&self) -> Point;
    /// Where the cursor is now; `None` when it cannot be read.
    fn position(&self) -> Option<Point> {
        None
    }
}

struct Inner {
    os: Box<dyn CursorOs>,
    lock_path: PathBuf,
    cursor: CursorState,
    grabbed: bool,
    /// Set by the emergency chord on the tap thread; read by every backend entry that would hide the cursor.
    halted: bool,
    /// The hide's warp jump, not yet seen in a motion delta. The OS folds a warp's jump into the next
    /// event's delta (measured with a real mouse: -1278,-86 after the edge-to-center warp, every later
    /// delta the hand's own motion).
    pending: Option<Pending>,
}

#[derive(Clone, Copy, Debug)]
struct Pending {
    /// Center minus where the cursor was before the warp; `None` when the position could not be read.
    jump: Option<(i32, i32)>,
    /// Events left to look for it. A motion queued before the warp can arrive first.
    left: u8,
}

/// How far a delta may stray from the expected jump and still be it: the hand's own motion in that one event.
const JUMP_SLACK: i32 = 64;

/// Cheap to clone; every clone sees the same state.
#[derive(Clone)]
pub struct GrabGuard(Arc<Mutex<Inner>>);

impl GrabGuard {
    pub fn new(os: Box<dyn CursorOs>, lock_path: PathBuf) -> Self {
        Self(Arc::new(Mutex::new(Inner {
            os,
            lock_path,
            cursor: CursorState::default(),
            grabbed: false,
            halted: false,
            pending: None,
        })))
    }

    /// The motion to forward for a mouse event carrying `delta`, or `None` when no grab is held (the
    /// delta is then used as is). The delta that carries the grab warp's jump has the jump taken out; it is
    /// recognised by size within the first few events, so a motion queued before the warp passes as is.
    /// When the jump is unknown, the first event is dropped instead.
    ///
    /// The hidden cursor still wanders under a background process (detaching it has no effect, and a warp
    /// from inside the tap callback does not hold either, both measured); [`Self::recenter_if_far`] brings
    /// it back from outside the callback, and its jump is taken out here the same way.
    pub fn grabbed_motion(&self, delta: (i32, i32)) -> Option<(i32, i32)> {
        let mut g = self.lock();
        if !g.grabbed || g.halted {
            return None;
        }
        let Some(p) = g.pending else { return Some(delta) };
        let Some(jump) = p.jump else {
            g.pending = None;
            return Some((0, 0));
        };
        if (delta.0 - jump.0).abs() <= JUMP_SLACK && (delta.1 - jump.1).abs() <= JUMP_SLACK {
            g.pending = None;
            return Some((delta.0 - jump.0, delta.1 - jump.1));
        }
        g.pending = (p.left > 1).then_some(Pending { left: p.left - 1, ..p });
        Some(delta)
    }

    /// While grabbed, warp the hidden cursor back to the main display's center once it is more than `max`
    /// from it on either axis, and expect that warp's jump in a coming delta as after the hide. Left to
    /// wander, it reaches the screen edge, where the Dock takes it and draws it again (measured: the hand
    /// moving left on the peer carried the Mac cursor 600 px in under half a second). Must be called from
    /// outside the tap callback. Returns whether it warped.
    pub fn recenter_if_far(&self, max: i32) -> Result<bool, BackendError> {
        let mut g = self.lock();
        if !g.grabbed || g.halted {
            return Ok(false);
        }
        let os = g.os.as_mut();
        let Some(at) = os.position() else { return Ok(false) };
        let c = os.main_center();
        if (at.x - c.x).abs() <= max && (at.y - c.y).abs() <= max {
            return Ok(false);
        }
        os.warp(c)?;
        // A jump not yet seen is still coming: with no event between the warps the OS folds both into
        // the next delta, so expect their sum rather than dropping the first into the peer's motion.
        let add = (c.x - at.x, c.y - at.y);
        let jump = match g.pending {
            Some(Pending { jump: Some(j), .. }) => Some((j.0 + add.0, j.1 + add.1)),
            Some(Pending { jump: None, .. }) => None,
            None => Some(add),
        };
        g.pending = Some(Pending { jump, left: 4 });
        Ok(true)
    }

    /// Fail closed until the tap is started again: hides and grabs are refused while halted.
    pub fn halt(&self) {
        self.lock().halted = true;
    }

    pub fn clear_halt(&self) {
        self.lock().halted = false;
    }

    pub fn is_halted(&self) -> bool {
        self.lock().halted
    }

    /// A poisoned mutex still holds valid state. A restore must never be skipped
    /// because another thread panicked, so recover the guard instead.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn is_grabbed(&self) -> bool {
        self.lock().grabbed
    }

    pub fn cursor(&self) -> CursorState {
        self.lock().cursor
    }

    pub fn capture_preflight(&self) -> Preflight {
        self.lock().os.capture_preflight()
    }

    pub fn secure_input(&self) -> Result<Option<bool>, BackendError> {
        self.lock().os.secure_input()
    }

    /// Run `f` with the OS and the lock path. Used by startup recovery.
    pub fn with_os<R>(&self, f: impl FnOnce(&mut dyn CursorOs, &Path) -> R) -> R {
        let mut g = self.lock();
        let inner = &mut *g;
        f(inner.os.as_mut(), &inner.lock_path)
    }

    /// Hide, freeze, and center the cursor. Idempotent.
    pub fn hide(&self) -> Result<(), BackendError> {
        hide_locked(&mut self.lock())
    }

    /// Hide and mark the grab held, in one critical section so a concurrent
    /// restore cannot interleave between the two.
    pub fn grab(&self) -> Result<(), BackendError> {
        let mut g = self.lock();
        hide_locked(&mut g)?;
        g.grabbed = true;
        Ok(())
    }

    /// Show the cursor without touching the grab flag. Refuses while grabbed.
    pub fn show(&self) -> Result<(), BackendError> {
        let mut g = self.lock();
        if g.grabbed {
            return Err(BackendError::Unsupported);
        }
        show_locked(&mut g)
    }

    /// Show, reassociate, and clear the grab. The one restore path.
    /// On failure the cursor is recorded as `Unknown`, the grab is cleared, and
    /// the lock file is kept so startup recovery can retry.
    pub fn restore(&self) -> Result<(), BackendError> {
        let mut g = self.lock();
        let r = show_locked(&mut g);
        g.grabbed = false;
        r
    }
}

/// The single entry gate for every path that hides the cursor (grab, direct hide). Fails closed:
/// refused while halted by the emergency chord, and refused under Secure Input or when it cannot be told.
/// Under Secure Input the OS drops the tap's events, so a hidden cursor would strand the local user.
fn hide_gate(g: &mut Inner) -> Result<(), BackendError> {
    if g.halted {
        return Err(BackendError::Unsupported);
    }
    match g.os.secure_input() {
        Ok(Some(false)) => Ok(()),
        Ok(Some(true)) | Ok(None) | Err(_) => Err(BackendError::SecureInput),
    }
}

fn hide_locked(g: &mut Inner) -> Result<(), BackendError> {
    hide_gate(g)?;
    if g.cursor.next(false).is_none() {
        return Ok(());
    }
    write_lock(&g.lock_path)?;
    let os = g.os.as_mut();
    let mut jump = None;
    let r = os
        .hide_cursor()
        .and_then(|()| os.set_association(false))
        .and_then(|()| {
            let c = os.main_center();
            jump = os.position().map(|b| (c.x - b.x, c.y - b.y));
            os.warp(c)
        });
    match r {
        Ok(()) => {
            g.cursor.apply(CursorAction::Hide);
            g.pending = Some(Pending { jump, left: 4 });
            Ok(())
        }
        Err(e) => {
            // Roll back the partial hide. If the rollback fails, the cursor state is unknown
            // and the lock file stays so the next start recovers it.
            let shown = g.os.show_cursor();
            let assoc = g.os.set_association(true);
            match shown.and(assoc) {
                Ok(()) => {
                    let _ = remove_lock(&g.lock_path);
                }
                Err(re) => {
                    eprintln!("flowseam: rollback after failed hide also failed: {re:?}");
                    g.cursor.mark_unknown();
                }
            }
            Err(e)
        }
    }
}

fn show_locked(g: &mut Inner) -> Result<(), BackendError> {
    if g.cursor.next(true).is_none() {
        return Ok(());
    }
    let shown = g.os.show_cursor();
    let assoc = g.os.set_association(true);
    match shown.and(assoc) {
        Ok(()) => {
            g.cursor.apply(CursorAction::Show);
            remove_lock(&g.lock_path)
        }
        Err(e) => {
            g.cursor.mark_unknown();
            Err(e)
        }
    }
}

/// Write the lock file before the cursor is hidden. Contents: pid, timestamp.
pub fn write_lock(path: &Path) -> Result<(), BackendError> {
    let since = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let text = render_lock(&GrabLock { pid: std::process::id(), since_unix: since });
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| io_err(e, "grab.lock dir"))?;
    }
    fs::write(path, text).map_err(|e| io_err(e, "grab.lock write"))
}

pub fn remove_lock(path: &Path) -> Result<(), BackendError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_err(e, "grab.lock remove")),
    }
}

pub fn io_err(e: io::Error, api: &'static str) -> BackendError {
    BackendError::Os { code: e.raw_os_error().unwrap_or(-1), api }
}

/// Recording stand-in for the OS. Records each call as a short string so tests can assert on what happened.
#[cfg(test)]
pub mod mock {
    use super::*;

    #[derive(Clone, Default)]
    pub struct Log(pub Arc<Mutex<Vec<String>>>);

    impl Log {
        pub fn calls(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
        pub fn count(&self, what: &str) -> usize {
            self.calls().iter().filter(|c| c.as_str() == what).count()
        }
    }

    pub struct MockOs {
        pub log: Log,
        pub preflight: Preflight,
        pub secure: Result<Option<bool>, BackendError>,
        pub fail_show: bool,
    }

    impl MockOs {
        /// Preflight missing, secure input off, no failures. Safe default: nothing reaches the tap.
        #[allow(clippy::new_ret_no_self)] // test fixture: returns the mock boxed plus its call log
        pub fn new() -> (Box<dyn CursorOs>, Log) {
            let log = Log::default();
            let os = MockOs {
                log: log.clone(),
                preflight: Preflight::Missing(flowseam_core::backend::Permission::Accessibility),
                secure: Ok(Some(false)),
                fail_show: false,
            };
            (Box::new(os), log)
        }

        fn rec(&self, s: &str) {
            self.log.0.lock().unwrap().push(s.to_string());
        }
    }

    impl CursorOs for MockOs {
        fn capture_preflight(&self) -> Preflight {
            self.preflight.clone()
        }
        fn secure_input(&self) -> Result<Option<bool>, BackendError> {
            self.secure.clone()
        }
        fn hide_cursor(&mut self) -> Result<(), BackendError> {
            self.rec("hide");
            Ok(())
        }
        fn show_cursor(&mut self) -> Result<(), BackendError> {
            self.rec("show");
            if self.fail_show {
                Err(BackendError::Os { code: -9, api: "mock show" })
            } else {
                Ok(())
            }
        }
        fn set_association(&mut self, connected: bool) -> Result<(), BackendError> {
            self.rec(if connected { "assoc:true" } else { "assoc:false" });
            Ok(())
        }
        fn warp(&mut self, _p: Point) -> Result<(), BackendError> {
            self.rec("warp");
            Ok(())
        }
        fn main_center(&self) -> Point {
            Point { x: 0, y: 0 }
        }
    }
}

/// Non-owning handle, so the signal watcher never keeps a dropped backend's guard alive.
#[derive(Clone)]
pub struct WeakGrab(Weak<Mutex<Inner>>);

impl WeakGrab {
    pub fn upgrade(&self) -> Option<GrabGuard> {
        self.0.upgrade().map(GrabGuard)
    }
}

impl GrabGuard {
    pub fn downgrade(&self) -> WeakGrab {
        WeakGrab(Arc::downgrade(&self.0))
    }
}

/// One watcher pass (design §5, signal path). If a termination signal was recorded, restore every
/// live grab and return the signal. The caller then exits. Only the flag is set from the handler,
/// so restores run on the watcher thread, never in signal context.
pub fn signal_pass(pending: &AtomicI32, registry: &Mutex<Vec<WeakGrab>>) -> Option<i32> {
    let sig = pending.swap(0, Ordering::SeqCst);
    if sig == 0 {
        return None;
    }
    let live: Vec<GrabGuard> = {
        let mut r = registry.lock().unwrap_or_else(|p| p.into_inner());
        r.retain(|w| w.0.strong_count() > 0);
        r.iter().filter_map(WeakGrab::upgrade).collect()
    };
    for g in live {
        if let Err(e) = g.restore() {
            eprintln!("flowseam: signal restore failed: {e:?}; cursor state unknown");
        }
    }
    Some(sig)
}

#[cfg(test)]
mod motion_tests {
    use super::mock::MockOs;
    use super::*;

    /// The mock with a readable cursor position (the mock's center is 0,0).
    struct At(Box<dyn CursorOs>, Point);
    impl CursorOs for At {
        fn capture_preflight(&self) -> Preflight { self.0.capture_preflight() }
        fn secure_input(&self) -> Result<Option<bool>, BackendError> { self.0.secure_input() }
        fn hide_cursor(&mut self) -> Result<(), BackendError> { self.0.hide_cursor() }
        fn show_cursor(&mut self) -> Result<(), BackendError> { self.0.show_cursor() }
        fn set_association(&mut self, c: bool) -> Result<(), BackendError> { self.0.set_association(c) }
        fn warp(&mut self, p: Point) -> Result<(), BackendError> { self.0.warp(p) }
        fn main_center(&self) -> Point { self.0.main_center() }
        fn position(&self) -> Option<Point> { Some(self.1) }
    }

    fn guard(name: &str, at: Option<Point>) -> GrabGuard {
        let dir = std::env::temp_dir().join(format!("flowseam-mac-motion-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let os = MockOs::new().0;
        let os: Box<dyn CursorOs> = match at { Some(p) => Box::new(At(os, p)), None => os };
        GrabGuard::new(os, dir.join("grab.lock"))
    }

    /// The cursor sat at the right edge (1279,86 from a center at 0,0), so the warp's jump is -1279,-86,
    /// as measured with a real mouse. It is taken out of the delta that carries it and the hand's own
    /// motion in that event is kept.
    #[test]
    fn the_warp_jump_is_taken_out_of_the_delta_that_carries_it() {
        let g = guard("jump", Some(Point { x: 1279, y: 86 }));
        assert_eq!(g.grabbed_motion((5, 0)), None, "no grab: the delta is used as is");
        g.grab().unwrap();
        assert_eq!(g.grabbed_motion((-1276, -86)), Some((3, 0)));
        assert_eq!(g.grabbed_motion((4, -1)), Some((4, -1)));
        g.restore().unwrap();
        assert_eq!(g.grabbed_motion((9, 9)), None, "released: back to the delta");
    }

    /// The hand keeps moving the hidden cursor while the peer is driven. Once it is far from the center it
    /// is warped back, and that warp's jump is taken out of the delta that carries it like the hide's.
    #[test]
    fn a_wandering_hidden_cursor_is_brought_back_and_its_jump_taken_out() {
        let g = guard("wander", Some(Point { x: 900, y: 40 }));
        g.grab().unwrap();
        assert_eq!(g.grabbed_motion((-900, -40)), Some((0, 0)), "the hide's own jump");
        assert_eq!(g.recenter_if_far(256), Ok(true));
        assert_eq!(g.grabbed_motion((-895, -40)), Some((5, 0)));
        assert_eq!(g.grabbed_motion((-895, -40)), Some((-895, -40)), "only once per warp");
    }

    /// Two warps with no event between them arrive as one delta carrying both jumps.
    #[test]
    fn a_recenter_before_the_last_jump_was_seen_expects_both_jumps() {
        let g = guard("twice", Some(Point { x: 900, y: 40 }));
        g.grab().unwrap();
        assert_eq!(g.recenter_if_far(256), Ok(true));
        assert_eq!(g.grabbed_motion((-1797, -80)), Some((3, 0)));
    }

    #[test]
    fn a_hidden_cursor_near_the_center_or_without_a_grab_is_left_alone() {
        let near = guard("near", Some(Point { x: 256, y: -256 }));
        near.grab().unwrap();
        assert_eq!(near.recenter_if_far(256), Ok(false));
        let free = guard("free", Some(Point { x: 900, y: 0 }));
        assert_eq!(free.recenter_if_far(256), Ok(false), "no grab");
        free.grab().unwrap();
        free.halt();
        assert_eq!(free.recenter_if_far(256), Ok(false), "halted");
    }

    /// A motion queued before the warp can reach the tap first. It is the hand's own and must pass; the
    /// jump still has to be caught in the event after it.
    #[test]
    fn a_motion_queued_before_the_warp_does_not_let_the_jump_through() {
        let g = guard("queued", Some(Point { x: 1279, y: 86 }));
        g.grab().unwrap();
        assert_eq!(g.grabbed_motion((2, 1)), Some((2, 1)));
        assert_eq!(g.grabbed_motion((-1279, -86)), Some((0, 0)));
    }

    /// A warp the OS did not apply leaves no jump to find; after a few events the guard stops looking.
    #[test]
    fn a_jump_that_never_shows_stops_being_looked_for() {
        let g = guard("never", Some(Point { x: 1279, y: 86 }));
        g.grab().unwrap();
        for _ in 0..4 {
            assert_eq!(g.grabbed_motion((1, 0)), Some((1, 0)));
        }
        assert_eq!(g.grabbed_motion((-1279, -86)), Some((-1279, -86)), "no longer looked for");
    }

    /// Without a readable position the jump is unknown: the first event is dropped instead.
    #[test]
    fn unknown_jump_drops_the_first_event() {
        let g = guard("unknown", None);
        g.grab().unwrap();
        assert_eq!(g.grabbed_motion((-1278, -86)), Some((0, 0)));
        assert_eq!(g.grabbed_motion((4, -1)), Some((4, -1)));
    }

    /// Every grab warps, so every grab looks for its own jump.
    #[test]
    fn each_grab_looks_for_its_own_jump() {
        let g = guard("each", Some(Point { x: 1279, y: 86 }));
        g.grab().unwrap();
        g.grabbed_motion((-1279, -86));
        g.restore().unwrap();
        g.grab().unwrap();
        assert_eq!(g.grabbed_motion((-1279, -86)), Some((0, 0)));
        assert_eq!(g.grabbed_motion((6, 0)), Some((6, 0)));
    }

    #[test]
    fn halted_grab_reads_no_motion() {
        let g = guard("halted", None);
        g.grab().unwrap();
        g.halt();
        assert_eq!(g.grabbed_motion((3, 3)), None);
    }
}

#[cfg(test)]
mod signal_tests {
    use super::*;
    use super::mock::{Log, MockOs};
    use flowseam_core::backend::{Permission, Preflight};
    use std::sync::atomic::Ordering;

    const SIGTERM: i32 = 15;

    fn grabbed(name: &str) -> (GrabGuard, Log) {
        let dir = std::env::temp_dir().join(format!("flowseam-mac-sig-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let log = Log::default();
        let os = MockOs { log: log.clone(), preflight: Preflight::Missing(Permission::Accessibility), secure: Ok(Some(false)), fail_show: false };
        let g = GrabGuard::new(Box::new(os), dir.join("grab.lock"));
        g.grab().unwrap();
        (g, log)
    }

    /// A4 signal path: a recorded SIGTERM restores every live grab. Driven by setting the flag directly;
    /// no real signal is sent.
    #[test]
    fn recorded_signal_restores_live_grabs() {
        let (g, log) = grabbed("term");
        let registry = Mutex::new(vec![g.downgrade()]);
        let pending = AtomicI32::new(SIGTERM);
        assert_eq!(signal_pass(&pending, &registry), Some(SIGTERM));
        assert_eq!(log.count("show"), 1, "watcher must restore: {:?}", log.calls());
        assert!(!g.is_grabbed());
        assert_eq!(pending.load(Ordering::SeqCst), 0, "signal must be consumed once");
    }

    #[test]
    fn no_signal_is_a_no_op() {
        let (g, log) = grabbed("none");
        let registry = Mutex::new(vec![g.downgrade()]);
        assert_eq!(signal_pass(&AtomicI32::new(0), &registry), None);
        assert_eq!(log.count("show"), 0);
        assert!(g.is_grabbed());
    }

    /// A dropped backend's guard must not be kept alive by the registry.
    #[test]
    fn dropped_guard_is_skipped_not_resurrected() {
        let (g, log) = grabbed("dropped");
        let weak = g.downgrade();
        drop(g);
        let registry = Mutex::new(vec![weak]);
        assert_eq!(signal_pass(&AtomicI32::new(SIGTERM), &registry), Some(SIGTERM));
        assert_eq!(log.count("show"), 0, "dead guard must not be touched");
    }
}
