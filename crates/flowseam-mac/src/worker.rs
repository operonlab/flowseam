//! Worker entry (design §5): runs the capture tap until told to stop, then writes `CLEAN` on stdout.
//!
//! Stop protocol: stdin EOF or SIGTERM ends the run normally. Both go through the same path:
//! stop the tap, restore the cursor, then write `CLEAN\n`. A tap fault other than the emergency
//! chord (which has already restored the cursor) returns an error and writes no marker, so the
//! supervisor treats it as a death and restores again.
//!

use crate::backend::{MacBackend, SysCursorOs};
use crate::ffi;
use crate::grab::{CursorOs, GrabGuard};
use crate::supervisor::CLEAN;
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
use std::sync::atomic::{AtomicBool, Ordering};
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
/// How far the hidden cursor may wander from the main display's center before it is warped back: well
/// inside even a small display, so it never reaches an edge, a corner or the Dock.
const RECENTER_PX: i32 = 256;

static TERM: AtomicBool = AtomicBool::new(false);

/// Async-signal-safe: one atomic store.
extern "C" fn on_term(_sig: i32) {
    TERM.store(true, Ordering::SeqCst);
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
            eprintln!("flowseam worker: restore after panic failed: {e:?}");
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
        eprintln!("flowseam worker: identity: {e:?}");
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
    })
}

/// Run until stdin closes or SIGTERM arrives. Blocks. Writes `CLEAN\n` to stdout on a normal end.
pub fn run(cfg: &Config) -> Result<(), BackendError> {
    run_with(cfg, std::io::stdin(), std::io::stdout())
}

fn run_with<R: Read + Send + 'static, W: Write>(cfg: &Config, stdin: R, mut out: W) -> Result<(), BackendError> {
    // Refuse before touching the OS: every peer must be paired.
    cfg.validate_for_run().map_err(|e| {
        eprintln!("flowseam worker: config not runnable: {e:?}");
        BackendError::Os { code: ERR_CONFIG, api: "config: validate_for_run" }
    })?;
    let tap_cfg = TapConfig::from_config(cfg).map_err(|e| {
        eprintln!("flowseam worker: bad switch config: {e}");
        BackendError::Os { code: ERR_CONFIG, api: "config: switch chord" }
    })?;
    let fwd_cfg = FwdConfig::from_config(cfg).map_err(|e| {
        eprintln!("flowseam worker: bad switch config: {e}");
        BackendError::Os { code: ERR_CONFIG, api: "config: switch chord" }
    })?;
    let lock = MacBackend::default_lock_path()
        .ok_or(BackendError::Os { code: ERR_NO_HOME, api: "HOME unset: no grab.lock path" })?;
    let link_params = if cfg.peers.is_empty() { None } else { Some(link_params(cfg)?) };

    TERM.store(false, Ordering::SeqCst);
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
    };
    let cancel_drag = Box::new(|| match sys::cancel_drag_in_front() {
        Ok(Some(pid)) => eprintln!("flowseam files: drag ended here (Escape to pid {pid})"),
        Ok(None) => eprintln!("flowseam files: no active app found; Escape sent through the HID stream"),
        Err(e) => eprintln!("flowseam files: could not end the drag here: {e}"),
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
                    eprintln!("flowseam worker: links failed to start: {e:?}");
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

    while !stop.load(Ordering::SeqCst) && !TERM.load(Ordering::SeqCst) {
        fwd.lock().unwrap_or_else(|e| e.into_inner()).tick(clock());
        if let Err(e) = guard.recenter_if_far(RECENTER_PX) {
            eprintln!("flowseam worker: re-centering the hidden cursor failed: {e:?}");
        }
        // The sleep; also where a hide or show asked for on another thread is applied (AppKit is main-thread).
        crate::front::pump(Duration::from_millis(50));
    }

    // Links first: each ended session runs link_down, which frees a held grab or injected keys.
    if let Some(l) = links {
        l.stop();
    }
    let stopped = backend.stop();
    if let Err(e) = guard.restore() {
        eprintln!("flowseam worker: final cursor restore failed: {e:?}");
    }
    // Hand the front back now; after exit the system would pick an app, not necessarily the one it was.
    crate::front::pump(Duration::ZERO);
    stopped?;
    if panicked.load(Ordering::SeqCst) {
        eprintln!("flowseam worker: a link thread panicked; cursor restored, no CLEAN marker");
        return Err(BackendError::Os { code: ERR_PANIC, api: "link thread panic" });
    }
    let seen = *fault.lock().unwrap_or_else(|e| e.into_inner());
    match seen {
        None | Some(CaptureFault::Emergency) => write_clean(&mut out),
        Some(f) => {
            eprintln!("flowseam worker: tap fault {f:?}; no CLEAN marker");
            Err(BackendError::Os { code: ERR_TAP_FAULT, api: "tap fault" })
        }
    }
}

/// The normal-exit marker. A failed write is an error: without the marker the supervisor
/// would restore a cursor that is already fine, and the caller must know that.
fn write_clean(out: &mut impl Write) -> Result<(), BackendError> {
    let io = |_| BackendError::Os { code: ERR_CLEAN_WRITE, api: "stdout: CLEAN marker" };
    out.write_all(CLEAN).map_err(io)?;
    out.write_all(b"\n").map_err(io)?;
    out.flush().map_err(io)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_marker_is_clean_plus_newline() {
        let mut buf = Vec::new();
        write_clean(&mut buf).unwrap();
        assert_eq!(buf, b"CLEAN\n");
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
        assert!(write_clean(&mut Broken).is_err());
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
