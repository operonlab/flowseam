//! Worker entry (design §5): runs the capture tap until told to stop, then writes `CLEAN` on stdout.
//!
//! Stop protocol: stdin EOF, SIGTERM or a quit Apple event ends the run normally. All go through the same path:
//! stop the tap, restore the cursor, then write `CLEAN\n` (`QUIT\n` for the Apple event). A tap fault other than the emergency
//! chord (which has already restored the cursor) returns an error and writes no marker, so the
//! supervisor treats it as a death and restores again.
//!

use crate::backend::{MacBackend, SysCursorOs};
use crate::ffi;
use crate::grab::{CursorOs, GrabGuard};
use crate::supervisor::{CLEAN, QUIT};
use crate::links::{DragCfg, LinkParams, Links, RouteOutbox};
use crate::sink::EmitSink;
use crate::switch_gate::TapConfig;
use crate::sys;
use crate::wire::{Clock, ForwardSink, Shared};
use flowseam_core::forward::{Forwarder, FwdConfig};
use flowseam_net::identity::{from_hex, Identity};
use flowseam_net::pins::PinSet;
use flowseam_net::session::{PeerInfo, Timing};
use std::time::Instant;
use flowseam_core::backend::{BackendError, CaptureFault, CaptureSink, CapturedEvent, Disposition, InputCapture};
use flowseam_core::config::Config;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const SIGTERM: i32 = 15;
const ERR_CONFIG: i32 = -5;
const ERR_NO_HOME: i32 = -6;
const ERR_TAP_FAULT: i32 = -7;
const ERR_CLEAN_WRITE: i32 = -8;
const ERR_IDENTITY: i32 = -9;
const ERR_LINKS: i32 = -10;
const ERR_PANIC: i32 = -11;
const ERR_STUCK: i32 = -12;
const GRANTS_EVERY: Duration = Duration::from_secs(1);

/// The first privacy grant this process is missing now, if any. `Unknown` is not a revocation.
fn missing_grant() -> Option<String> {
    use flowseam_core::backend::Preflight;
    [sys::capture_preflight(), sys::emit_preflight()].into_iter().find_map(|p| match p {
        Preflight::Missing(what) => Some(format!("{what:?}")),
        _ => None,
    })
}
/// How long the main loop may go without a turn before the worker gives up. A turn takes 50 ms; one that
/// cannot take the forwarder's lock for this long means the event tap is blocked too.
const STALL_LIMIT: Duration = Duration::from_secs(3);
/// How long the stop after a quit Apple event may take before the worker exits without it.
const QUIT_LIMIT: Duration = Duration::from_secs(5);
/// How far the hidden cursor may wander from the main display's center before it is warped back: well
/// inside even a small display, so it never reaches an edge, a corner or the Dock.
const RECENTER_PX: i32 = 256;

static TERM: AtomicBool = AtomicBool::new(false);
static QUIT_ASKED: AtomicBool = AtomicBool::new(false);

/// Async-signal-safe: one atomic store.
extern "C" fn on_term(_sig: i32) {
    TERM.store(true, Ordering::SeqCst);
}

/// The quit Apple event: a script or a quit-all tool asking the app to quit, or logout. The worker shares the app's
/// bundle ID, so the event can reach it rather than the window. AppKit's own handler exits on the spot, skipping the
/// restore, and the window then starts a new worker; this ends the run the way stdin EOF does and says why.
extern "C" fn on_quit_event(_event: *const std::ffi::c_void, _reply: *mut std::ffi::c_void, _refcon: *mut std::ffi::c_void) -> i16 {
    QUIT_ASKED.store(true, Ordering::SeqCst);
    0
}

/// What the worker writes on a clean end: QUIT when the system asked the app to quit, so the window quits too.
fn end_marker(quit_asked: bool) -> &'static [u8] {
    if quit_asked { QUIT } else { CLEAN }
}

/// Watch the main loop's heartbeat (`beat`, in `clock` milliseconds) and call `on_stall` once if it stops
/// for `limit`. A stuck worker can hold the grab (cursor hidden, mouse detached, keys swallowed) with nothing
/// left running to undo it: the input lock that otherwise only a restart ends. Exiting ends it, because the
/// event tap and the cursor-hiding window go with the process and the supervisor restores the cursor.
pub fn spawn_watchdog(beat: Arc<AtomicU64>, clock: Clock, limit: Duration, on_stall: impl FnOnce() + Send + 'static) {
    thread::Builder::new()
        .name("flowseam-watchdog".into())
        .spawn(move || loop {
            thread::sleep(limit / 6);
            let since = clock().saturating_sub(beat.load(Ordering::SeqCst));
            if since >= limit.as_millis() as u64 {
                on_stall();
                return;
            }
        })
        .expect("spawn watchdog");
}

/// The real cursor layer, for the parent's supervisor to restore the cursor after a worker death.
pub fn real_cursor_os() -> Box<dyn CursorOs> {
    Box::new(SysCursorOs)
}

/// Passes everything (the tap swallows only what its sink says to) and turns a tap fault into a stop request.
struct WorkerSink {
    stop: Arc<AtomicBool>,
    fault: Arc<Mutex<Option<CaptureFault>>>,
}

impl CaptureSink for WorkerSink {
    fn on_event(&mut self, _ev: CapturedEvent) -> Disposition {
        Disposition::Pass
    }
    fn on_tap_fault(&mut self, f: CaptureFault) {
        *self.fault.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// A panicking link thread frees the cursor first, then asks the worker to end without CLEAN.
fn panic_hook(guard: GrabGuard, stop: Arc<AtomicBool>, panicked: Arc<AtomicBool>) -> Arc<dyn Fn() + Send + Sync> {
    Arc::new(move || {
        if let Err(e) = guard.restore() {
            flowseam_core::log::line(&format!("flowseam worker: restore after panic failed: {e:?}"));
        }
        panicked.store(true, Ordering::SeqCst);
        stop.store(true, Ordering::SeqCst);
    })
}

/// Identity, pins and addresses for the links. Config was already validated, so a bad value here is a bug.
fn link_params(cfg: &Config) -> Result<LinkParams, BackendError> {
    let bad = |api: &'static str| BackendError::Os { code: ERR_CONFIG, api };
    let dir = std::path::PathBuf::from(std::env::var_os("HOME").ok_or(BackendError::Os { code: ERR_NO_HOME, api: "HOME unset: no identity dir" })?)
        .join(".config/flowseam/identity");
    let identity = Identity::load_or_create(&dir).map_err(|e| {
        flowseam_core::log::line(&format!("flowseam worker: identity: {e:?}"));
        BackendError::Os { code: ERR_IDENTITY, api: "identity load_or_create" }
    })?;
    let mut peers = Vec::new();
    let mut fps = Vec::new();
    for p in &cfg.peers {
        let fp = from_hex(&p.fingerprint).ok_or(bad("peer fingerprint"))?;
        let addr = p.addr.parse().map_err(|_| bad("peer addr"))?;
        peers.push((p.name.clone(), addr, <[u8; 16]>::try_from(&fp[..16]).expect("32-byte fingerprint")));
        fps.push(fp);
    }
    let me = PeerInfo {
        node: <[u8; 16]>::try_from(&identity.fingerprint()[..16]).expect("32-byte fingerprint"),
        name: "flowseam".into(),
        displays: sys::active_displays().unwrap_or_default(),
    };
    Ok(LinkParams {
        identity,
        me,
        pins: PinSet::new(fps),
        peers,
        listen: cfg.listen.parse().map_err(|_| bad("listen addr"))?,
        timing: Timing::default(),
        // Beside the identity above and the config the GUI reads it next to (crate flowseam's config_dir).
        status: std::env::var_os("HOME").map(|h| flowseam_core::status::status_path(&std::path::PathBuf::from(h).join(".config/flowseam/config.toml"))),
    })
}

/// Run until stdin closes or SIGTERM arrives. Blocks. Writes `CLEAN\n` to stdout on a normal end.
pub fn run(cfg: &Config) -> Result<(), BackendError> {
    run_with(cfg, std::io::stdin(), std::io::stdout())
}

fn run_with<R: Read + Send + 'static, W: Write>(cfg: &Config, stdin: R, mut out: W) -> Result<(), BackendError> {
    // Refuse before touching the OS: every peer must be paired.
    cfg.validate_for_run().map_err(|e| {
        flowseam_core::log::line(&format!("flowseam worker: config not runnable: {e:?}"));
        BackendError::Os { code: ERR_CONFIG, api: "config: validate_for_run" }
    })?;
    let tap_cfg = TapConfig::from_config(cfg).map_err(|e| {
        flowseam_core::log::line(&format!("flowseam worker: bad switch config: {e}"));
        BackendError::Os { code: ERR_CONFIG, api: "config: switch chord" }
    })?;
    let fwd_cfg = FwdConfig::from_config(cfg).map_err(|e| {
        flowseam_core::log::line(&format!("flowseam worker: bad switch config: {e}"));
        BackendError::Os { code: ERR_CONFIG, api: "config: switch chord" }
    })?;
    let lock = MacBackend::default_lock_path()
        .ok_or(BackendError::Os { code: ERR_NO_HOME, api: "HOME unset: no grab.lock path" })?;
    let link_params = if cfg.peers.is_empty() { None } else { Some(link_params(cfg)?) };

    TERM.store(false, Ordering::SeqCst);
    QUIT_ASKED.store(false, Ordering::SeqCst);
    let stop = Arc::new(AtomicBool::new(false));
    let panicked = Arc::new(AtomicBool::new(false));
    let fault = Arc::new(Mutex::new(None));
    let mut backend = MacBackend::new(lock.clone());
    backend.set_tap_config(tap_cfg);
    let guard = backend.guard();

    let routes = RouteOutbox::default();
    let displays = sys::active_displays().unwrap_or_default();
    let home = std::path::PathBuf::from(std::env::var_os("HOME").ok_or(BackendError::Os { code: ERR_NO_HOME, api: "HOME unset: no Downloads" })?);
    let drags = DragCfg {
        hub: Arc::new(Mutex::new(flowseam_net::files::DragHub::new(Some(Box::new(sys::MacDragSource))))),
        // Same volume as Downloads on a normal Mac, so a drop is a rename.
        staging: home.join("Library/Caches/Flowseam/drags"),
        dest: home.join("Downloads"),
        native: Some(Arc::new(crate::native::MacNativeDrop)),
    };
    let cancel_drag = Box::new(|at| match sys::cancel_drag(at) {
        Ok(Some((pid, via))) => flowseam_core::log::line(&format!("flowseam files: drag ended here (Escape to pid {pid}, {via}, pressed at {at:?})")),
        Ok(None) => flowseam_core::log::line("flowseam files: no active app found; Escape sent through the HID stream"),
        Err(e) => flowseam_core::log::line(&format!("flowseam files: could not end the drag here: {e}")),
    });
    let sink = EmitSink::new(MacBackend::new(lock), guard.clone()).with_drags(drags.hub.clone(), cancel_drag);
    let fwd: Shared<MacBackend, RouteOutbox> = Arc::new(Mutex::new(Forwarder::new(fwd_cfg, displays, sink, routes.clone())));
    let started = Instant::now();
    let clock: Clock = Arc::new(move || started.elapsed().as_millis() as u64);
    let tap_sink = ForwardSink::new(
        fwd.clone(),
        clock.clone(),
        Box::new(|| sys::active_displays().ok()),
        Box::new(WorkerSink { stop: stop.clone(), fault: fault.clone() }),
    );
    backend.start(Box::new(tap_sink))?;
    // After start(): that call installs the restore-and-exit handlers. SIGTERM is taken over here so it
    // ends through the CLEAN path; SIGINT and SIGHUP keep restore-and-exit.
    // SAFETY: `on_term` is an `extern "C"` fn that only stores to an atomic.
    let handler: extern "C" fn(i32) = on_term;
    unsafe { ffi::signal(SIGTERM, handler as usize) };

    let links = match link_params {
        None => None,
        Some(p) => {
            let on_panic = panic_hook(guard.clone(), stop.clone(), panicked.clone());
            match Links::start(p, fwd.clone(), routes, clock.clone(), on_panic, Some(Arc::new(sys::MacClipboard)), Some(drags)) {
                Ok(l) => Some(l),
                Err(e) => {
                    flowseam_core::log::line(&format!("flowseam worker: links failed to start: {e:?}"));
                    let _ = backend.stop();
                    let _ = guard.restore();
                    return Err(BackendError::Os { code: ERR_LINKS, api: "links start" });
                }
            }
        }
    };

    let eof_stop = stop.clone();
    thread::spawn(move || {
        let mut r = stdin;
        let _ = std::io::copy(&mut r, &mut std::io::sink());
        eof_stop.store(true, Ordering::SeqCst);
    });

    let beat = Arc::new(AtomicU64::new(clock()));
    spawn_watchdog(beat.clone(), clock.clone(), STALL_LIMIT, || {
        flowseam_core::log::line(&format!("flowseam worker: the main loop has not turned for {STALL_LIMIT:?}; exiting so input is released"));
        std::process::exit(ERR_STUCK);
    });
    let timer_fwd = fwd.clone();
    let timer_beat = beat.clone();
    let timer_clock = clock.clone();
    // AppKit's drag tracking loop holds the main thread; this timer keeps the beat and forwarder tick going in tracking mode.
    // SAFETY: this worker runs on the main thread and owns its current run loop.
    let heartbeat = crate::heartbeat::keep_beating(unsafe { ffi::CFRunLoopGetCurrent() }, Duration::from_millis(50), Box::new(move || {
        // try_lock: this runs on the main thread, which may already hold the forwarder if a call under it spins the
        // run loop; waiting here would deadlock that thread. The next beat ticks instead.
        match timer_fwd.try_lock() {
            Ok(mut f) => f.tick(timer_clock()),
            Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner().tick(timer_clock()),
            Err(std::sync::TryLockError::WouldBlock) => {}
        }
        timer_beat.store(timer_clock(), Ordering::SeqCst);
    }));
    let mut grants_checked = Instant::now();
    let mut status_checked = Instant::now() - Duration::from_secs(1);
    let mut was_grabbed = guard.is_grabbed();
    // AppKit installs its own quit handler as it starts; replace it before any event is dispatched.
    crate::front::start();
    // SAFETY: a plain function pointer that only stores to an atomic; the handler table is process-wide.
    let err = unsafe { ffi::AEInstallEventHandler(ffi::kCoreEventClass, ffi::kAEQuitApplication, on_quit_event, std::ptr::null_mut(), 0) };
    if err != 0 {
        flowseam_core::log::line(&format!("flowseam worker: could not take over the quit Apple event (OSErr {err}); one sent to the app will end this worker without its restore"));
    }
    while !stop.load(Ordering::SeqCst) && !TERM.load(Ordering::SeqCst) && !QUIT_ASKED.load(Ordering::SeqCst) {
        fwd.lock().unwrap_or_else(|e| e.into_inner()).tick(clock());
        beat.store(clock(), Ordering::SeqCst);
        let grabbed = guard.is_grabbed();
        if grabbed != was_grabbed {
            flowseam_core::log::line(if grabbed { "flowseam worker: cursor grabbed" } else { "flowseam worker: cursor restored" });
            was_grabbed = grabbed;
        }
        if cfg.peers.is_empty() && status_checked.elapsed() >= Duration::from_secs(1) {
            status_checked = Instant::now();
            if let Some(home) = std::env::var_os("HOME") {
                let path = flowseam_core::status::status_path(&std::path::PathBuf::from(home).join(".config/flowseam/config.toml"));
                if let Err(e) = flowseam_core::status::write(&path, &flowseam_core::status::LinkStatus { written_ms: flowseam_core::status::now_ms(), peers: Vec::new() }) {
                    flowseam_core::log::line(&format!("flowseam worker: status write: {e}"));
                }
            }
        }
        // A grant taken back while running (switched off in System Settings, or not carried over by an
        // update) leaves the tap deaf with the cursor still hidden and the mouse detached: input locked.
        // End cleanly instead; the normal exit path restores everything.
        if grants_checked.elapsed() >= GRANTS_EVERY {
            grants_checked = Instant::now();
            if let Some(missing) = missing_grant() {
                flowseam_core::log::line(&format!("flowseam worker: {missing} was taken away while running; stopping and restoring input"));
                break;
            }
        }
        if let Err(e) = guard.recenter_if_far(RECENTER_PX) {
            flowseam_core::log::line(&format!("flowseam worker: re-centering the hidden cursor failed: {e:?}"));
        }
        // The sleep; also where a hide or show asked for on another thread is applied (AppKit is main-thread).
        crate::front::pump(Duration::from_millis(50));
    }
    drop(heartbeat);
    if QUIT_ASKED.load(Ordering::SeqCst) {
        flowseam_core::log::line("flowseam worker: the system asked the app to quit");
        // Logout waits for this process, so a stop that hangs must not hold it. Exiting still releases the tap and
        // the cursor-hiding window; the window restores the cursor after an end without a marker.
        thread::spawn(|| {
            thread::sleep(QUIT_LIMIT);
            flowseam_core::log::line(&format!("flowseam worker: still stopping {QUIT_LIMIT:?} after a quit; exiting"));
            std::process::exit(ERR_STUCK);
        });
    }
    // Shutting down can take longer than a turn (links join, the grab is restored): not a stall.
    flowseam_core::log::line("flowseam worker: stopping");
    beat.store(u64::MAX, Ordering::SeqCst);

    // Links first: each ended session runs link_down, which frees a held grab or injected keys.
    if let Some(l) = links {
        l.stop();
    }
    let stopped = backend.stop();
    if let Err(e) = guard.restore() {
        flowseam_core::log::line(&format!("flowseam worker: final cursor restore failed: {e:?}"));
    } else if was_grabbed {
        flowseam_core::log::line("flowseam worker: cursor restored on stop");
    }
    // Hand the front back now; after exit the system would pick an app, not necessarily the one it was.
    crate::front::pump(Duration::ZERO);
    stopped?;
    if panicked.load(Ordering::SeqCst) {
        flowseam_core::log::line("flowseam worker: a link thread panicked; cursor restored, no CLEAN marker");
        return Err(BackendError::Os { code: ERR_PANIC, api: "link thread panic" });
    }
    let seen = *fault.lock().unwrap_or_else(|e| e.into_inner());
    match seen {
        None | Some(CaptureFault::Emergency) => write_marker(&mut out, end_marker(QUIT_ASKED.load(Ordering::SeqCst))),
        Some(f) => {
            flowseam_core::log::line(&format!("flowseam worker: tap fault {f:?}; no CLEAN marker"));
            Err(BackendError::Os { code: ERR_TAP_FAULT, api: "tap fault" })
        }
    }
}

/// The normal-exit marker. A failed write is an error: without the marker the supervisor
/// would restore a cursor that is already fine, and the caller must know that.
fn write_marker(out: &mut impl Write, marker: &[u8]) -> Result<(), BackendError> {
    let io = |_| BackendError::Os { code: ERR_CLEAN_WRITE, api: "stdout: CLEAN marker" };
    out.write_all(marker).map_err(io)?;
    out.write_all(b"\n").map_err(io)?;
    out.flush().map_err(io)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_marker_is_clean_plus_newline() {
        let mut buf = Vec::new();
        write_marker(&mut buf, CLEAN).unwrap();
        assert_eq!(buf, b"CLEAN\n");
    }

    /// The handler AppKit would otherwise run exits on the spot; this one only asks the loop to end, and the end
    /// it leads to is the QUIT marker, which the window takes as "quit" rather than "start a new worker".
    #[test]
    fn a_quit_event_asks_to_stop_and_ends_with_the_quit_marker() {
        assert_eq!(end_marker(false), CLEAN);
        QUIT_ASKED.store(false, Ordering::SeqCst);
        assert_eq!(on_quit_event(std::ptr::null(), std::ptr::null_mut(), std::ptr::null_mut()), 0, "noErr: the sender sees success");
        assert!(QUIT_ASKED.load(Ordering::SeqCst));
        assert_eq!(end_marker(QUIT_ASKED.load(Ordering::SeqCst)), QUIT);
        QUIT_ASKED.store(false, Ordering::SeqCst);
    }

    #[test]
    fn failed_marker_write_is_an_error() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("closed"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        assert!(write_marker(&mut Broken, CLEAN).is_err());
    }

    #[test]
    fn bad_chord_fails_before_touching_the_os_and_writes_nothing() {
        let mut cfg = Config::default();
        cfg.switch.chord = "bogus".into();
        let mut out = Vec::new();
        let r = run_with(&cfg, std::io::empty(), &mut out);
        assert!(matches!(r, Err(BackendError::Os { code: ERR_CONFIG, .. })), "{r:?}");
        assert!(out.is_empty(), "no CLEAN on a failed start");
    }

    #[test]
    fn sink_passes_and_fault_requests_stop() {
        let stop = Arc::new(AtomicBool::new(false));
        let fault = Arc::new(Mutex::new(None));
        let mut s = WorkerSink { stop: stop.clone(), fault: fault.clone() };
        assert_eq!(s.on_event(CapturedEvent::Motion { dx: 1, dy: 1 }), Disposition::Pass);
        assert!(!stop.load(Ordering::SeqCst));
        s.on_tap_fault(CaptureFault::TapDisabledByTimeout);
        assert!(stop.load(Ordering::SeqCst));
        assert_eq!(*fault.lock().unwrap(), Some(CaptureFault::TapDisabledByTimeout));
    }

    #[test]
    fn unpaired_peer_refuses_to_start_before_the_os_and_writes_nothing() {
        let mut cfg = Config::default();
        cfg.peers.push(flowseam_core::config::Peer {
            name: "mini".into(),
            addr: "192.168.1.2:24800".into(),
            edge: flowseam_core::config::Edge::Right,
            fingerprint: String::new(),
            keys: Default::default(),
        });
        let mut out = Vec::new();
        let r = run_with(&cfg, std::io::empty(), &mut out);
        assert!(matches!(r, Err(BackendError::Os { code: ERR_CONFIG, api: "config: validate_for_run" })), "{r:?}");
        assert!(out.is_empty());
    }

    #[test]
    fn link_thread_panic_frees_the_cursor_and_requests_stop() {
        use crate::grab::mock::{Log, MockOs};
        use flowseam_core::backend::Preflight;
        let log = Log::default();
        let os = MockOs { log: log.clone(), preflight: Preflight::Ok, secure: Ok(Some(false)), fail_show: false };
        let guard = GrabGuard::new(Box::new(os), std::env::temp_dir().join(format!("flowseam-mac-panic-{}.lock", std::process::id())));
        guard.grab().unwrap();
        let (stop, panicked) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let hook = panic_hook(guard.clone(), stop.clone(), panicked.clone());
        assert!(!crate::links::run_guarded(|| panic!("boom"), &*hook));
        assert!(!guard.is_grabbed(), "cursor must be restored");
        assert!(log.count("show") >= 1);
        assert!(stop.load(Ordering::SeqCst) && panicked.load(Ordering::SeqCst));
    }
}

#[cfg(test)]
mod watchdog_tests {
    use super::*;
    use std::sync::mpsc::channel;

    fn clock() -> Clock {
        let t0 = Instant::now();
        Arc::new(move || t0.elapsed().as_millis() as u64)
    }

    #[test]
    fn a_stopped_heartbeat_trips_the_watchdog_once() {
        let c = clock();
        let beat = Arc::new(AtomicU64::new(c()));
        let (tx, rx) = channel();
        spawn_watchdog(beat, c, Duration::from_millis(120), move || tx.send(()).unwrap());
        assert!(rx.recv_timeout(Duration::from_secs(2)).is_ok(), "never tripped");
        assert!(rx.recv_timeout(Duration::from_millis(400)).is_err(), "tripped twice");
    }

    #[test]
    fn a_turning_main_loop_never_trips_it() {
        let c = clock();
        let beat = Arc::new(AtomicU64::new(c()));
        let (tx, rx) = channel();
        // A wide margin over the 20 ms beat: a loaded test machine oversleeps, and that is not a stall.
        spawn_watchdog(beat.clone(), c.clone(), Duration::from_millis(500), move || tx.send(()).unwrap());
        let end = Instant::now() + Duration::from_millis(1500);
        while Instant::now() < end {
            beat.store(c(), Ordering::SeqCst);
            thread::sleep(Duration::from_millis(20));
        }
        assert!(rx.try_recv().is_err(), "tripped while the loop was turning");
    }

    #[test]
    fn a_disarmed_watchdog_never_trips() {
        let c = clock();
        let beat = Arc::new(AtomicU64::new(u64::MAX));
        let (tx, rx) = channel();
        spawn_watchdog(beat, c, Duration::from_millis(60), move || tx.send(()).unwrap());
        assert!(rx.recv_timeout(Duration::from_millis(400)).is_err());
    }
}
