//! `MacBackend`: the macOS implementation of flowseam_core's backend traits (design §4–§7).
//!
//! Cursor grab = hide + disassociate + warp to center, with a lock file written
//! first so a crashed process leaves a trace for `recover_stale_lock`.
//! Grab requires an active capture tap; without one it refuses (fail loud).
//! All cursor state lives in [`GrabGuard`], which the tap thread also holds.

use crate::ffi::{self, CGDirectDisplayID};
use crate::grab::{io_err, signal_pass, CursorOs, GrabGuard, WeakGrab};
use crate::keymap::{flags_for, hid_to_keycode, is_modifier_hid, keycode_by_position};
use crate::state::{plan_recovery, RecoveryPlan};
use crate::switch_gate::TapConfig;
use crate::sys;
use crate::tap::{self, TapHandle};
use flowseam_core::backend::{
    BackendError, CaptureSink, Clipboard, EmitOutcome, InputBackend, InputCapture, InputEmitter, Preflight,
};
use flowseam_core::cursor::PostedCursor;
use flowseam_core::modifiers::{KeyError, KeyTracker};
use flowseam_core::proto::{ModifierSet, Point, Rect, HID_LIMIT};
use std::collections::BTreeSet;
use std::ffi::c_void;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::thread;
use std::time::{Duration, Instant};

type DisplaySink = Mutex<Box<dyn Fn() + Send>>;

/// Set by the signal handler, read by the watcher thread. Handler does nothing else.
static PENDING_SIGNAL: AtomicI32 = AtomicI32::new(0);
/// Every armed grab, so the watcher can restore them all on a signal.
static ARMED_GRABS: Mutex<Vec<WeakGrab>> = Mutex::new(Vec::new());
static WATCH: Once = Once::new();

/// Async-signal-safe: one atomic store.
extern "C" fn on_terminate(sig: i32) {
    PENDING_SIGNAL.store(sig, Ordering::SeqCst);
}

/// Register the grab for signal restore and, once per process, install SIGHUP/SIGINT/SIGTERM
/// handlers plus a watcher thread. Called from `start()` after preflight succeeds, so tests that
/// hit a missing permission never install real handlers.
fn arm_signal_restore(guard: &GrabGuard) {
    {
        let mut g = ARMED_GRABS.lock().unwrap_or_else(|p| p.into_inner());
        g.retain(|w| w.upgrade().is_some());
        g.push(guard.downgrade());
    }
    WATCH.call_once(|| {
        const SIGHUP: i32 = 1;
        const SIGINT: i32 = 2;
        const SIGTERM: i32 = 15;
        let handler: extern "C" fn(i32) = on_terminate;
        for sig in [SIGHUP, SIGINT, SIGTERM] {
            // SAFETY: `handler` is an `extern "C"` fn with the signal-handler ABI; it only stores to an atomic.
            unsafe { ffi::signal(sig, handler as usize) };
        }
        thread::spawn(|| loop {
            if let Some(sig) = signal_pass(&PENDING_SIGNAL, &ARMED_GRABS) {
                eprintln!("flowseam: signal {sig}: cursor restored, exiting");
                std::process::exit(128 + sig);
            }
            thread::sleep(Duration::from_millis(50));
        });
    });
}

/// The real OS calls behind [`CursorOs`].
pub struct SysCursorOs;

impl CursorOs for SysCursorOs {
    fn capture_preflight(&self) -> Preflight {
        sys::capture_preflight()
    }

    fn secure_input(&self) -> Result<Option<bool>, BackendError> {
        Ok(Some(sys::secure_input_enabled()))
    }

    fn hide_cursor(&mut self) -> Result<(), BackendError> {
        sys::hide_cursor()
    }

    fn show_cursor(&mut self) -> Result<(), BackendError> {
        sys::show_cursor()
    }

    fn set_association(&mut self, connected: bool) -> Result<(), BackendError> {
        sys::set_mouse_association(connected)
    }

    fn warp(&mut self, p: Point) -> Result<(), BackendError> {
        sys::warp_cursor(p)
    }

    fn main_center(&self) -> Point {
        sys::main_display_center()
    }

    fn position(&self) -> Option<Point> {
        sys::cursor_position().ok()
    }
}

/// The keycode that presses the key at `hid`'s position on this Mac's keyboard; an ISO keyboard swaps two.
fn keycode_for(hid: u16) -> Option<u16> {
    let kc = hid_to_keycode(hid)?;
    Some(keycode_by_position(hid, kc, matches!(hid, 0x35 | 0x64) && sys::keyboard_is_iso()))
}

pub struct MacBackend {
    guard: GrabGuard,
    tap: Option<TapHandle>,
    /// Raw `Arc<DisplaySink>` pointer registered with CoreGraphics; owned here.
    display_cb: Option<usize>,
    keys: KeyTracker,
    buttons: BTreeSet<u8>,
    tap_cfg: TapConfig,
    /// Where this backend last sent the cursor: the base for the next relative move.
    posted: PostedCursor,
}

impl MacBackend {
    pub fn new(lock_path: PathBuf) -> Self {
        Self::with_os(lock_path, Box::new(SysCursorOs))
    }

    /// Construct with an injected OS layer. Tests use this to record calls instead of touching the cursor.
    pub fn with_os(lock_path: PathBuf, os: Box<dyn CursorOs>) -> Self {
        Self {
            guard: GrabGuard::new(os, lock_path),
            tap: None,
            display_cb: None,
            keys: KeyTracker::new(),
            buttons: BTreeSet::new(),
            tap_cfg: TapConfig::default(),
            posted: PostedCursor::default(),
        }
    }

    /// Switch policy and dwell zone for the next `start`. The default has no peer edge, so only the chord switches.
    pub fn guard(&self) -> GrabGuard {
        self.guard.clone()
    }

    pub fn set_tap_config(&mut self, cfg: TapConfig) {
        self.tap_cfg = cfg;
    }

    /// `$HOME/Library/Application Support/flowseam/grab.lock`, or `None` without `HOME`.
    pub fn default_lock_path() -> Option<PathBuf> {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support/flowseam/grab.lock"))
    }

    fn post_button(&mut self, button: u8, down: bool) -> Result<(), BackendError> {
        if button == 0 && crate::native::left_held() {
            flowseam_core::log::line(&format!("flowseam: peer's button {button} {} during a native drag", if down { "down" } else { "up" }));
        }
        if button == 0 && !down {
            crate::native::released(); // this release is a native drag's drop
        }
        let (ty, btn) = match (button, down) {
            (0, true) => (ffi::EV_LEFT_DOWN, 0),
            (0, false) => (ffi::EV_LEFT_UP, 0),
            (1, true) => (ffi::EV_RIGHT_DOWN, 1),
            (1, false) => (ffi::EV_RIGHT_UP, 1),
            (b, true) => (ffi::EV_OTHER_DOWN, b as u32),
            (b, false) => (ffi::EV_OTHER_UP, b as u32),
        };
        sys::post_mouse(ty, sys::cursor_position()?, btn)
    }

    fn drag_type(&self) -> (u32, u32) {
        // A native drop's own press counts: its system drag follows only drag events.
        if self.buttons.contains(&0) || crate::native::left_held() {
            (ffi::EV_LEFT_DRAGGED, 0)
        } else if self.buttons.contains(&1) {
            (ffi::EV_RIGHT_DRAGGED, 1)
        } else if let Some(&b) = self.buttons.iter().next() {
            (ffi::EV_OTHER_DRAGGED, b as u32)
        } else {
            (ffi::EV_MOUSE_MOVED, 0)
        }
    }

    fn post_key_for(&self, hid: u16, down: bool, repeat: bool, kc: u16) -> Result<(), BackendError> {
        sys::post_key(kc, down, repeat, flags_for(self.keys.mods()), is_modifier_hid(hid))
    }

    fn set_grabbed(&mut self, grabbed: bool) -> Result<(), BackendError> {
        if grabbed {
            if self.guard.is_grabbed() {
                return Ok(());
            }
            if self.tap.is_none() {
                return Err(BackendError::Unsupported); // nothing to swallow local input with
            }
            // The Secure Input and halt gate lives in `GrabGuard::grab` (shared with `set_cursor_visible`).
            self.guard.grab()
        } else {
            self.guard.restore()
        }
    }
}

impl Drop for MacBackend {
    fn drop(&mut self) {
        if let Err(e) = self.set_grabbed(false) {
            eprintln!("flowseam: drop could not restore cursor: {e:?}");
        }
        if let Some(mut t) = self.tap.take() {
            t.stop();
        }
        if let Some(raw) = self.display_cb.take() {
            let cb: extern "C" fn(CGDirectDisplayID, u32, *mut c_void) = display_reconfigured;
            // SAFETY: (cb, raw) is the pair registered in `set_display_change_sink`.
            let _ = unsafe { sys::remove_display_callback(cb, raw as *mut c_void) };
            // SAFETY: `raw` came from `Arc::into_raw` in `set_display_change_sink`
            // and is removed from CoreGraphics above, so this is the last owner.
            drop(unsafe { Arc::from_raw(raw as *const DisplaySink) });
        }
    }
}

/// Fail loud on every lock state that is not "clean" or "stale and restored".
fn recover_on_start(guard: &GrabGuard) -> Result<(), BackendError> {
    match guard.with_os(|os, path| recover_stale_lock(path, os)) {
        Ok(_) => Ok(()),
        Err(RecoveryError::LiveOwner(pid)) => Err(BackendError::Os { code: pid as i32, api: "grab.lock: live owner" }),
        Err(RecoveryError::Corrupt(_)) => Err(BackendError::Os { code: -3, api: "grab.lock: corrupt" }),
        Err(RecoveryError::Io(_)) => Err(BackendError::Os { code: -4, api: "grab.lock: io" }),
        Err(RecoveryError::Os(e)) => Err(e),
    }
}

/// Show the cursor and reassociate the mouse. Attempts both, returns the first error.
fn restore_best_effort(os: &mut dyn CursorOs) -> Result<(), BackendError> {
    let shown = os.show_cursor();
    let assoc = os.set_association(true);
    shown.and(assoc)
}

extern "C" fn display_reconfigured(_id: CGDirectDisplayID, flags: u32, user: *mut c_void) {
    if flags & ffi::CG_DISPLAY_BEGIN_CONFIG != 0 {
        return; // report once, after the change has settled
    }
    // SAFETY: `user` is the Arc registered by `set_display_change_sink`, alive until removed.
    let sink = unsafe { &*(user as *const DisplaySink) };
    match sink.lock() {
        Ok(f) => f(),
        Err(_) => eprintln!("flowseam: display sink mutex poisoned; reconfiguration not reported"),
    }
}

impl MacBackend {
    /// `start` with the two side effects injected: the signal-restore arm and the tap open. Tests use
    /// this to observe whether arming happens after preflight and never on a refused start.
    fn start_with(
        &mut self,
        sink: Box<dyn CaptureSink>,
        arm: &mut dyn FnMut(&GrabGuard),
        open: impl FnOnce(Box<dyn CaptureSink>, GrabGuard) -> Result<TapHandle, BackendError>,
    ) -> Result<(), BackendError> {
        // A crashed previous run may have left the cursor hidden. Undo that before anything else.
        recover_on_start(&self.guard)?;
        match self.guard.capture_preflight() {
            Preflight::Ok => {}
            Preflight::Missing(p) => return Err(BackendError::PermissionMissing(p)),
            Preflight::Unknown(_) => return Err(BackendError::Unsupported),
        }
        if self.guard.is_grabbed() {
            return Err(BackendError::Unsupported); // cannot swap sinks under an active grab
        }
        if let Some(mut old) = self.tap.take() {
            old.stop();
        }
        arm(&self.guard);
        self.tap = Some(open(sink, self.guard.clone())?);
        Ok(())
    }
}

impl InputCapture for MacBackend {
    fn preflight(&self) -> Preflight {
        self.guard.capture_preflight()
    }

    fn displays(&self) -> Result<Vec<Rect>, BackendError> {
        sys::active_displays()
    }

    fn set_display_change_sink(&mut self, cb: Box<dyn Fn() + Send>) -> Result<(), BackendError> {
        let cb_fn: extern "C" fn(CGDirectDisplayID, u32, *mut c_void) = display_reconfigured;
        if let Some(old) = self.display_cb.take() {
            // SAFETY: (cb_fn, old) is the pair registered by the previous call.
            unsafe { sys::remove_display_callback(cb_fn, old as *mut c_void) }?;
            // SAFETY: as in Drop; this pointer is no longer registered.
            drop(unsafe { Arc::from_raw(old as *const DisplaySink) });
        }
        let raw = Arc::into_raw(Arc::new(Mutex::new(cb))) as usize;
        // SAFETY: `raw` is an Arc<DisplaySink> leaked until `display_cb` is cleared in Drop or here.
        if let Err(e) = unsafe { sys::add_display_callback(cb_fn, raw as *mut c_void) } {
            drop(unsafe { Arc::from_raw(raw as *const DisplaySink) });
            return Err(e);
        }
        self.display_cb = Some(raw);
        Ok(())
    }

    fn start(&mut self, sink: Box<dyn CaptureSink>) -> Result<(), BackendError> {
        let cfg = self.tap_cfg.clone();
        self.start_with(sink, &mut arm_signal_restore, move |s, g| tap::start(s, g, cfg))
    }

    fn stop(&mut self) -> Result<(), BackendError> {
        self.set_grabbed(false)?;
        if let Some(mut t) = self.tap.take() {
            t.stop();
        }
        Ok(())
    }

    fn set_grabbed(&mut self, grabbed: bool) -> Result<(), BackendError> {
        MacBackend::set_grabbed(self, grabbed)
    }

    fn cursor_position(&self) -> Result<Point, BackendError> {
        sys::cursor_position()
    }

    fn warp_cursor(&mut self, p: Point) -> Result<(), BackendError> {
        sys::warp_cursor(p)
    }

    fn secure_input_active(&self) -> Result<Option<bool>, BackendError> {
        self.guard.secure_input()
    }
}

impl InputEmitter for MacBackend {
    fn preflight(&self) -> Preflight {
        sys::emit_preflight()
    }

    fn move_rel(&mut self, dx: i32, dy: i32) -> Result<EmitOutcome, BackendError> {
        let displays = sys::active_displays()?;
        let p = self.posted.target(dx, dy, &displays, Instant::now(), sys::cursor_position)?;
        self.move_abs(p)
    }

    fn move_abs(&mut self, p: Point) -> Result<EmitOutcome, BackendError> {
        let (ty, btn) = self.drag_type();
        sys::post_mouse(ty, p, btn)?;
        self.posted.placed(p, Instant::now());
        Ok(EmitOutcome::Delivered)
    }

    fn cursor_placed(&mut self, at: Option<Point>) {
        match at {
            Some(p) => self.posted.placed(p, Instant::now()),
            None => self.posted.forget(),
        }
    }

    fn button(&mut self, button: u8, down: bool) -> Result<EmitOutcome, BackendError> {
        self.post_button(button, down)?;
        if down {
            self.buttons.insert(button);
        } else {
            self.buttons.remove(&button);
        }
        Ok(EmitOutcome::Delivered)
    }

    fn wheel(&mut self, dx: i32, dy: i32) -> Result<EmitOutcome, BackendError> {
        sys::post_scroll(dx, dy)?;
        Ok(EmitOutcome::Delivered)
    }

    fn key(&mut self, hid: u16, down: bool, repeat: bool) -> Result<EmitOutcome, BackendError> {
        if hid >= HID_LIMIT {
            return Err(BackendError::Unsupported);
        }
        // Volume, mute and transport keys act only as system-defined events here, not as key presses.
        if let Some(nx) = crate::keymap::nx_key_for(hid) {
            sys::post_media_key(nx, down)?;
            return Ok(EmitOutcome::Delivered);
        }
        // Map before mutating state: an unmappable key must not enter the tracker.
        let kc = keycode_for(hid).ok_or(BackendError::Unsupported)?;
        self.keys.set_key(hid, down).map_err(|_: KeyError| BackendError::Unsupported)?;
        self.post_key_for(hid, down, repeat, kc)?;
        // Secure Input makes macOS drop posted keys silently; say so instead.
        if sys::secure_input_enabled() {
            return Ok(EmitOutcome::Unknown("Secure Input active: macOS may drop posted keys".into()));
        }
        Ok(EmitOutcome::Delivered)
    }

    /// ponytail: modifier bits follow the held key set (design §6), so `mods`
    /// can only set CapsLock state here. Modifier bits without a held key are
    /// ignored rather than forced.
    fn set_modifiers(&mut self, mods: ModifierSet) -> Result<EmitOutcome, BackendError> {
        let held = self.keys.snapshot();
        self.keys.sync(mods, &held).map_err(|_: KeyError| BackendError::Unsupported)?;
        Ok(EmitOutcome::Delivered)
    }

    fn release_all(&mut self) -> Result<(), BackendError> {
        let mut first: Option<BackendError> = None;
        for hid in self.keys.release_all() {
            let Some(kc) = keycode_for(hid) else { continue };
            if let Err(e) = self.post_key_for(hid, false, false, kc) {
                first.get_or_insert(e);
            }
        }
        let held: Vec<u8> = self.buttons.iter().copied().collect();
        self.buttons.clear();
        for b in held {
            if let Err(e) = self.post_button(b, false) {
                first.get_or_insert(e);
            }
        }
        first.map_or(Ok(()), Err)
    }

    /// Idempotent. Refuses to show the cursor while a grab holds it (fail loud).
    fn set_cursor_visible(&mut self, visible: bool) -> Result<(), BackendError> {
        if visible {
            self.guard.show()
        } else {
            self.guard.hide()
        }
    }
}

impl Clipboard for MacBackend {
    fn change_count(&self) -> Result<u64, BackendError> {
        sys::pasteboard_change_count().map(|n| n as u64)
    }

    fn get_text(&self) -> Result<Option<String>, BackendError> {
        let out = Command::new("/usr/bin/pbpaste")
            .output()
            .map_err(|e| io_err(e, "pbpaste spawn"))?;
        if !out.status.success() {
            return Err(BackendError::Os { code: out.status.code().unwrap_or(-1), api: "pbpaste" });
        }
        let text = String::from_utf8(out.stdout).map_err(|_| BackendError::Os { code: -2, api: "pbpaste utf8" })?;
        Ok(if text.is_empty() { None } else { Some(text) })
    }

    fn set_text(&mut self, s: &str) -> Result<(), BackendError> {
        let mut child = Command::new("/usr/bin/pbcopy")
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|e| io_err(e, "pbcopy spawn"))?;
        child
            .stdin
            .take()
            .ok_or(BackendError::Os { code: -1, api: "pbcopy stdin" })?
            .write_all(s.as_bytes())
            .map_err(|e| io_err(e, "pbcopy write"))?;
        let status = child.wait().map_err(|e| io_err(e, "pbcopy wait"))?;
        if status.success() {
            Ok(())
        } else {
            Err(BackendError::Os { code: status.code().unwrap_or(-1), api: "pbcopy" })
        }
    }
}

impl InputBackend for MacBackend {}

/// Startup check for a lock file left by a previous process.
#[derive(Debug)]
pub enum RecoveryError {
    LiveOwner(u32),
    Corrupt(String),
    Io(String),
    Os(BackendError),
}

pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false; // kill(0, 0) would probe our own process group
    }
    // SAFETY: signal 0 only probes existence and permission.
    if unsafe { ffi::kill(pid as i32, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() == Some(1) // EPERM: exists, not ours
}

/// Read the lock at `path`, restore the cursor if its owner is gone, and clear it.
pub fn recover_stale_lock(path: &Path, os: &mut dyn CursorOs) -> Result<RecoveryPlan, RecoveryError> {
    let text = match fs::read_to_string(path) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(RecoveryError::Io(e.to_string())),
    };
    let plan = plan_recovery(text.as_deref(), pid_alive);
    match &plan {
        RecoveryPlan::Clean => Ok(plan),
        RecoveryPlan::RestoreAndClear { .. } => {
            restore_best_effort(os).map_err(RecoveryError::Os)?;
            fs::remove_file(path).map_err(|e| RecoveryError::Io(e.to_string()))?;
            Ok(plan)
        }
        RecoveryPlan::LiveOwner { pid } => Err(RecoveryError::LiveOwner(*pid)),
        RecoveryPlan::Corrupt(e) => {
            // Restoring is harmless; the unreadable file is kept for a human to look at.
            restore_best_effort(os).map_err(RecoveryError::Os)?;
            Err(RecoveryError::Corrupt(e.clone()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_lock(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("flowseam-mac-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir.join("grab.lock")
    }

    /// Grab without a tap must refuse before touching the cursor or the lock file.
    #[test]
    fn grab_without_capture_tap_is_refused_and_leaves_no_lock() {
        let lock = temp_lock("no-tap");
        let mut b = MacBackend::new(lock.clone());
        assert_eq!(b.set_grabbed(true), Err(BackendError::Unsupported));
        assert!(!lock.exists(), "refusal must not write a lock");
        assert!(!b.guard.cursor().is_hidden());
    }

    #[test]
    fn unmappable_key_is_refused_before_state_changes() {
        let mut b = MacBackend::new(temp_lock("keymap"));
        assert_eq!(b.key(0x46, true, false), Err(BackendError::Unsupported), "PrintScreen has no keycode here");
        assert!(b.keys.snapshot().is_empty(), "tracker untouched");
        assert_eq!(b.key(HID_LIMIT, true, false), Err(BackendError::Unsupported));
    }

    #[test]
    fn stale_lock_with_dead_owner_is_planned_for_restore_not_live() {
        let lock = temp_lock("recover");
        fs::create_dir_all(lock.parent().unwrap()).unwrap();
        // pid 0 is never alive by our rule, so this takes the restore branch
        // without touching any real process.
        fs::write(&lock, crate::state::render_lock(&crate::state::GrabLock { pid: 0, since_unix: 1 })).unwrap();
        // Restore touches the real cursor; the plan itself is what this test pins.
        assert_eq!(
            plan_recovery(Some(&fs::read_to_string(&lock).unwrap()), pid_alive),
            RecoveryPlan::RestoreAndClear { pid: 0 }
        );
    }

    #[test]
    fn live_owner_refuses_recovery_without_touching_anything() {
        let lock = temp_lock("live");
        fs::create_dir_all(lock.parent().unwrap()).unwrap();
        fs::write(
            &lock,
            crate::state::render_lock(&crate::state::GrabLock { pid: std::process::id(), since_unix: 1 }),
        )
        .unwrap();
        let mut os = SysCursorOs;
        match recover_stale_lock(&lock, &mut os) {
            Err(RecoveryError::LiveOwner(p)) => assert_eq!(p, std::process::id()),
            other => panic!("expected LiveOwner, got {other:?}"),
        }
        assert!(lock.exists(), "live owner's lock must not be deleted");
    }

    #[test]
    fn pid_alive_sees_this_process() {
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(0));
    }

    #[test]
    fn change_count_read_is_real() {
        let b = MacBackend::new(temp_lock("cc"));
        assert!(b.change_count().is_ok());
    }

    /// Through the real backend: a peer's motion arriving back to back, as a burst off the network does, all
    /// lands. Reading the cursor back before each move kept 2% of it (cursor.rs).
    #[test]
    #[ignore = "moves this Mac's cursor for a moment"]
    fn back_to_back_peer_motion_travels_its_full_distance() {
        let home = sys::cursor_position().unwrap();
        let mut b = MacBackend::new(temp_lock("motion"));
        let main = sys::active_displays().unwrap()[0];
        let start = Point { x: main.x + main.w / 2 - 250, y: main.y + main.h / 2 };
        b.move_abs(start).unwrap();
        thread::sleep(Duration::from_millis(100));
        b.cursor_placed(None);
        let from = sys::cursor_position().unwrap();
        for _ in 0..100 {
            b.move_rel(5, 0).unwrap();
        }
        thread::sleep(Duration::from_millis(100));
        let to = sys::cursor_position().unwrap();
        let _ = sys::warp_cursor(home);
        assert_eq!((to.x - from.x, to.y - from.y), (500, 0));
    }
}

#[cfg(test)]
mod grab_tests {
    use super::*;
    use crate::grab::mock::MockOs;
    use crate::tap::TapHandle;

    fn temp_lock(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("flowseam-mac-grab-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir.join("grab.lock")
    }

    fn backend(name: &str, secure: Result<Option<bool>, BackendError>) -> (MacBackend, crate::grab::mock::Log, PathBuf) {
        let lock = temp_lock(name);
        let log = crate::grab::mock::Log::default();
        let m = MockOs { log: log.clone(), preflight: Preflight::Ok, secure, fail_show: false };
        let mut b = MacBackend::with_os(lock.clone(), Box::new(m));
        b.tap = Some(TapHandle::dummy());
        (b, log, lock)
    }

    /// A3: Secure Input on, or unable to tell, must refuse the grab before any cursor call.
    #[test]
    fn secure_input_on_refuses_grab_without_hiding_cursor() {
        let (mut b, log, lock) = backend("a3-on", Ok(Some(true)));
        assert_eq!(b.set_grabbed(true), Err(BackendError::SecureInput));
        assert_eq!(log.count("hide"), 0, "cursor must not be hidden under Secure Input");
        assert!(!lock.exists(), "refusal must not write a lock");
        assert!(!b.guard.cursor().is_hidden());
    }

    #[test]
    fn secure_input_unknown_fails_closed() {
        let (mut b, log, _lock) = backend("a3-none", Ok(None));
        assert_eq!(b.set_grabbed(true), Err(BackendError::SecureInput), "None means cannot tell: refuse");
        assert_eq!(log.count("hide"), 0);
    }

    #[test]
    fn secure_input_query_error_fails_closed() {
        let (mut b, log, _lock) = backend("a3-err", Err(BackendError::Os { code: -1, api: "mock" }));
        assert_eq!(b.set_grabbed(true), Err(BackendError::SecureInput), "a failed query must not be read as off");
        assert_eq!(log.count("hide"), 0);
    }

    #[test]
    fn secure_input_off_allows_grab() {
        let (mut b, log, _lock) = backend("a3-off", Ok(Some(false)));
        assert_eq!(b.set_grabbed(true), Ok(()));
        assert_eq!(log.count("hide"), 1);
        assert!(b.guard.is_grabbed());
    }

    /// M2: the direct cursor-hide entry must pass the same Secure Input gate as the grab path.
    #[test]
    fn secure_input_on_refuses_cursor_hide_path() {
        let (mut b, log, _lock) = backend("m2-on", Ok(Some(true)));
        assert!(b.set_cursor_visible(false).is_err(), "hide must be refused under Secure Input");
        assert_eq!(log.count("hide"), 0, "cursor must not be hidden under Secure Input");
        assert!(!b.guard.cursor().is_hidden());
    }

    /// M2 control: with Secure Input off the same entry is allowed.
    #[test]
    fn secure_input_off_allows_cursor_hide_path() {
        let (mut b, log, _lock) = backend("m2-off", Ok(Some(false)));
        assert_eq!(b.set_cursor_visible(false), Ok(()));
        assert_eq!(log.count("hide"), 1);
    }

    /// M1: after the emergency chord the backend itself must refuse to re-grab or hide the cursor.
    #[test]
    fn emergency_halt_blocks_regrab_and_hide() {
        use flowseam_core::backend::{CaptureFault, CapturedEvent, Disposition};
        use flowseam_core::emergency::HID_ESCAPE;
        use flowseam_core::modifiers::KeyTracker;

        struct Quiet;
        impl CaptureSink for Quiet {
            fn on_event(&mut self, _ev: CapturedEvent) -> Disposition {
                Disposition::Pass
            }
            fn on_tap_fault(&mut self, _f: CaptureFault) {}
        }

        let (mut b, log, _lock) = backend("m1-halt", Ok(Some(false)));
        b.set_grabbed(true).unwrap();
        let mut state = tap::TapState {
            sink: Box::new(Quiet),
            guard: b.guard.clone(),
            keys: KeyTracker::new(),
            env: Box::new(tap::ScriptEnv::idle()),
        };
        let key = |hid: u16| CapturedEvent::Key { hid, down: true };
        let d = tap::handle(&mut state, vec![key(0xE0), key(0xE2), key(0xE3), key(HID_ESCAPE)]);
        assert_eq!(d, Disposition::Swallow, "chord must fire");
        assert!(b.set_grabbed(true).is_err(), "re-grab after emergency must fail closed");
        assert!(b.set_cursor_visible(false).is_err(), "hide after emergency must fail closed");
        assert_eq!(log.count("hide"), 1, "only the initial grab may have hidden the cursor: {:?}", log.calls());
    }
}

#[cfg(test)]
mod start_recovery_tests {
    use super::*;
    use crate::grab::mock::{Log, MockOs};
    use crate::state::{render_lock, GrabLock};
    use flowseam_core::backend::Permission;

    struct NullSink;
    impl CaptureSink for NullSink {
        fn on_event(&mut self, _ev: flowseam_core::backend::CapturedEvent) -> flowseam_core::backend::Disposition {
            flowseam_core::backend::Disposition::Pass
        }
        fn on_tap_fault(&mut self, _f: flowseam_core::backend::CaptureFault) {}
    }

    /// Mock preflight is Missing, so `start()` can never reach the tap: no real capture is created.
    fn backend(name: &str, lock_text: &str) -> (MacBackend, Log, PathBuf) {
        let dir = std::env::temp_dir().join(format!("flowseam-mac-start-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let lock = dir.join("grab.lock");
        fs::write(&lock, lock_text).unwrap();
        let log = Log::default();
        let os = MockOs { log: log.clone(), preflight: Preflight::Missing(Permission::Accessibility), secure: Ok(Some(false)), fail_show: false };
        (MacBackend::with_os(lock.clone(), Box::new(os)), log, lock)
    }

    /// A4: a stale lock (owner dead) is restored and cleared at start, before preflight.
    #[test]
    fn start_restores_and_clears_stale_lock() {
        let (mut b, log, lock) = backend("stale", &render_lock(&GrabLock { pid: 0, since_unix: 1 }));
        let r = b.start(Box::new(NullSink));
        assert_eq!(log.count("show"), 1, "stale lock must restore the cursor: {:?}", log.calls());
        assert!(!lock.exists(), "stale lock must be cleared");
        assert_eq!(r, Err(BackendError::PermissionMissing(Permission::Accessibility)));
    }

    /// A4: a live owner means another instance holds the grab. Refuse, touch nothing.
    #[test]
    fn start_refuses_when_lock_owner_is_alive() {
        let (mut b, log, lock) =
            backend("live", &render_lock(&GrabLock { pid: std::process::id(), since_unix: 1 }));
        let r = b.start(Box::new(NullSink));
        assert!(matches!(r, Err(BackendError::Os { api: "grab.lock: live owner", .. })), "got {r:?}");
        assert_eq!(log.count("show"), 0, "live owner's cursor must not be touched");
        assert!(lock.exists());
    }

    /// A4: an unreadable lock is restored (harmless) but kept, and start fails loud.
    #[test]
    fn start_refuses_corrupt_lock_and_keeps_it() {
        let (mut b, _log, lock) = backend("corrupt", "not a lock");
        let r = b.start(Box::new(NullSink));
        assert!(matches!(r, Err(BackendError::Os { api: "grab.lock: corrupt", .. })), "got {r:?}");
        assert!(lock.exists(), "corrupt lock is kept for a human");
    }
}

#[cfg(test)]
mod arm_tests {
    use super::*;
    use crate::grab::mock::{Log, MockOs};
    use flowseam_core::backend::{CaptureFault, CapturedEvent, Disposition, Permission};

    struct Quiet;
    impl CaptureSink for Quiet {
        fn on_event(&mut self, _ev: CapturedEvent) -> Disposition {
            Disposition::Pass
        }
        fn on_tap_fault(&mut self, _f: CaptureFault) {}
    }

    fn backend(name: &str, preflight: Preflight) -> (MacBackend, Log) {
        let dir = std::env::temp_dir().join(format!("flowseam-mac-arm-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let log = Log::default();
        let os = MockOs { log: log.clone(), preflight, secure: Ok(Some(false)), fail_show: false };
        (MacBackend::with_os(dir.join("grab.lock"), Box::new(os)), log)
    }

    /// M3: a preflight that passes arms the signal restore once, before the tap is opened.
    #[test]
    fn preflight_ok_arms_signal_restore_before_opening_tap() {
        let (mut b, _log) = backend("ok", Preflight::Ok);
        let mut armed = 0;
        let r = b.start_with(
            Box::new(Quiet),
            &mut |_g: &GrabGuard| armed += 1,
            |_s, _g| Ok(TapHandle::dummy()),
        );
        assert_eq!(r, Ok(()));
        assert_eq!(armed, 1, "preflight passed: signal restore must be armed");
    }

    /// M3: a refused start must not arm the signal restore.
    #[test]
    fn preflight_missing_does_not_arm_signal_restore() {
        let (mut b, _log) = backend("missing", Preflight::Missing(Permission::Accessibility));
        let mut armed = 0;
        let r = b.start_with(
            Box::new(Quiet),
            &mut |_g: &GrabGuard| armed += 1,
            |_s, _g| Ok(TapHandle::dummy()),
        );
        assert_eq!(r, Err(BackendError::PermissionMissing(Permission::Accessibility)));
        assert_eq!(armed, 0, "refused start must not install signal handlers");
    }
}
