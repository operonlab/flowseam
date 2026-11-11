//! Worker entry (design §5): runs the capture tap until told to stop, then writes `CLEAN` on stdout.
//!
//! Stop protocol: stdin EOF, SIGTERM or a quit Apple event ends the run normally. All go through the same path:
//! stop the tap, restore the cursor, then write `CLEAN\n` (`QUIT\n` for the Apple event). A tap fault other than the emergency
//! chord (which has already restored the cursor) returns an error and writes no marker, so the
//! supervisor treats it as a death and restores again.
//!
//! Before EOF, each stdin line is a command from the window (see `menu_command`).

use crate::backend::{MacBackend, SysCursorOs};
use crate::ffi;
use crate::grab::{CursorOs, GrabGuard};
use crate::supervisor::{CLEAN, QUIT};
use crate::links::{DragCfg, LinkParams, Links, RouteOutbox};
use crate::sink::EmitSink;
use crate::switch_gate::TapConfig;
use crate::sys;
use crate::wire::{Clock, ForwardSink, Shared};
use crate::kvm::{self, Cmd, Done, Kvm, Phase, Step};
use flowseam_core::forward::{Forwarder, FwdConfig, Mode, SwitchTo, WayBack};
use flowseam_core::status::{CheckState, InspectStatus, KvmPhase, KvmStatus, MonitorCheck, MonitorState, OutMonitor, TrialMonitor, TrialStatus};
use std::collections::{BTreeMap, HashMap};
use flowseam_net::identity::{from_hex, Identity};
use flowseam_net::pins::PinSet;
use flowseam_net::session::{PeerInfo, Timing};
use std::time::Instant;
use flowseam_core::backend::{BackendError, CaptureFault, CaptureSink, CapturedEvent, Disposition, InputCapture};
use flowseam_core::config::{Config, DisplaySwitch};
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// The watchdog checks every 500 ms; this turn stays well under its 3 s stall limit.
const IDLE_TURN: Duration = Duration::from_millis(500);

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
/// How long a stop waits for the monitor to land home. Inside the window's 6 s before SIGKILL; if it runs
/// out, kvm.toml still says the monitor is out and the window sends it home itself.
const KVM_STOP_LIMIT: Duration = Duration::from_secs(4);
/// How often a configured monitor is checked again while nothing is switching: one input read each.
const PROBE_EVERY: Duration = Duration::from_secs(30);
/// How far the hidden cursor may wander from the main display's center before it is warped back: well
/// inside even a small display, so it never reaches an edge, a corner or the Dock.
const RECENTER_PX: i32 = 256;

static TERM: AtomicBool = AtomicBool::new(false);
static TERM_PIPE_WRITE: AtomicI32 = AtomicI32::new(-1);
static QUIT_ASKED: AtomicBool = AtomicBool::new(false);

/// Async-signal-safe: record SIGTERM and wake the main loop through its self-pipe reader.
extern "C" fn on_term(_sig: i32) {
    note_term(&TERM, TERM_PIPE_WRITE.load(Ordering::Relaxed));
}

/// The handler's work, on explicit state so a test exercises it without touching the process-wide flag.
fn note_term(term: &AtomicBool, wake_fd: i32) {
    term.store(true, Ordering::SeqCst);
    // SAFETY: `write(2)` is async-signal-safe and the descriptor is nonblocking.
    unsafe { crate::signal_pipe::notify_fd(wake_fd) };
}

struct TermWaker {
    pipe: std::sync::Arc<crate::signal_pipe::SignalPipe>,
    running: std::sync::Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl TermWaker {
    fn start() -> std::io::Result<Self> {
        let pipe = std::sync::Arc::new(crate::signal_pipe::SignalPipe::new()?);
        let running = std::sync::Arc::new(AtomicBool::new(true));
        TERM_PIPE_WRITE.store(pipe.write_fd(), Ordering::SeqCst);
        let reader = pipe.clone();
        let active = running.clone();
        let thread = thread::spawn(move || {
            while reader.wait().is_ok() {
                if !active.load(Ordering::SeqCst) {
                    return;
                }
                crate::front::wake_if_idle();
            }
        });
        Ok(Self { pipe, running, thread: Some(thread) })
    }
}

impl Drop for TermWaker {
    fn drop(&mut self) {
        TERM_PIPE_WRITE.store(-1, Ordering::SeqCst);
        self.running.store(false, Ordering::SeqCst);
        // SAFETY: this normal-path byte wakes the same reader the handler uses.
        unsafe { crate::signal_pipe::notify_fd(self.pipe.write_fd()) };
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The quit Apple event: a script or a quit-all tool asking the app to quit, or logout. The worker shares the app's
/// bundle ID, so the event can reach it rather than the window. AppKit's own handler exits on the spot, skipping the
/// restore, and the window then starts a new worker; this ends the run the way stdin EOF does and says why.
extern "C" fn on_quit_event(_event: *const std::ffi::c_void, _reply: *mut std::ffi::c_void, _refcon: *mut std::ffi::c_void) -> i16 {
    QUIT_ASKED.store(true, Ordering::SeqCst);
    // Main thread, inside the idle wait: a logout waits for this process.
    crate::front::wake_if_idle();
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
        crate::front::wake_if_idle();
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
        crate::front::wake_if_idle();
    })
}

/// Identity, pins and addresses for the links. Config was already validated, so a bad value here is a bug.
fn link_params(cfg: &Config, config_path: &std::path::Path) -> Result<LinkParams, BackendError> {
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
        monitors: match crate::ddc::mac::online() {
            Ok(displays) => displays.into_iter().map(|m| flowseam_core::proto::MonitorId { vendor: m.edid.vendor as u16, product: m.edid.product as u16, serial: m.edid.serial }).collect(),
            Err(e) => { flowseam_core::log::line(&format!("flowseam worker: monitor EDID: {e:?}")); vec![] }
        },
        peer_monitors: crate::links::PeerMonitors::default(),
        monitors_home: crate::links::MonitorsHome::default(),
        pins: PinSet::new(fps),
        peers,
        listen: cfg.listen.parse().map_err(|_| bad("listen addr"))?,
        timing: Timing::default(),
        // Beside the config this worker runs, where the window that started it reads it.
        status: Some(flowseam_core::status::status_path(config_path)),
    })
}

/// Run until stdin closes or SIGTERM arrives. Blocks. Writes `CLEAN\n` to stdout on a normal end.
/// `config_path` is where `cfg` was read from; the status files go beside it.
pub fn run(cfg: &Config, config_path: &std::path::Path) -> Result<(), BackendError> {
    run_with(cfg, config_path, std::io::stdin(), std::io::stdout())
}

fn run_with<R: Read + Send + 'static, W: Write>(cfg: &Config, config_path: &std::path::Path, stdin: R, mut out: W) -> Result<(), BackendError> {
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
    let link_params = if cfg.peers.is_empty() { None } else { Some(link_params(cfg, config_path)?) };
    let peer_monitors = link_params.as_ref().map(|p| p.peer_monitors.clone());
    let home_asks = link_params.as_ref().map(|p| p.monitors_home.clone());

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
    let _term_waker = match TermWaker::start() {
        Ok(waker) => waker,
        Err(_) => {
            let _ = backend.stop();
            let _ = guard.restore();
            return Err(BackendError::Os { code: ERR_LINKS, api: "SIGTERM self-pipe" });
        }
    };
    // SAFETY: `on_term` stores an atomic and writes a byte to its nonblocking self-pipe.
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
    let (line_tx, lines) = std::sync::mpsc::channel::<String>();
    // Set with each line, so a click that lands between a turn's last look and its idle wait is not left
    // for the next idle turn: the loop reads it before choosing how long to wait.
    let line_waiting = Arc::new(AtomicBool::new(false));
    let line_flag = line_waiting.clone();
    thread::spawn(move || {
        let mut r = BufReader::new(stdin);
        let mut buf = Vec::new();
        while matches!(r.read_until(b'\n', &mut buf), Ok(n) if n > 0) {
            let _ = line_tx.send(String::from_utf8_lossy(&buf).into_owned());
            line_flag.store(true, Ordering::SeqCst);
            crate::front::wake_if_idle();
            buf.clear();
        }
        eof_stop.store(true, Ordering::SeqCst);
        crate::front::wake_if_idle();
    });

    let kvm_file = Some(flowseam_core::status::kvm_path(config_path));
    let mut kvm = kvm_start(kvm_file.as_deref());
    let (ddc_tx, ddc_rx) = match kvm::mac::spawn() {
        Ok((tx, rx)) => (Some(tx), Some(rx)),
        Err(e) => {
            flowseam_core::log::line(&format!("flowseam worker: no DDC thread ({e}); menu switches move input only"));
            (None, None)
        }
    };
    let mut kvm_written: Option<KvmStatus> = None;
    let mut side = KvmSide::new(cfg, peer_monitors);
    let mut probed: Option<Instant> = None;
    let switch_fwd = fwd.clone();
    let mut switch = move |peer: &str, way_back: WayBack| switch_fwd.lock().unwrap_or_else(|e| e.into_inner()).switch_to(peer, way_back);
    // Monitors the previous run left out: its session is over, so they go home now.
    let steps = kvm.return_stranded();
    run_kvm(&mut kvm, steps, ddc_tx.as_ref(), &mut switch);
    // Peers whose monitors were asked home, from this Mac's window or the peer's "back" button.
    let mut home_wanted: Vec<String> = Vec::new();

    let beat = Arc::new(AtomicU64::new(clock()));
    spawn_watchdog(beat.clone(), clock.clone(), STALL_LIMIT, || {
        flowseam_core::log::line(&format!("flowseam worker: the main loop has not turned for {STALL_LIMIT:?}; exiting so input is released"));
        std::process::exit(ERR_STUCK);
    });
    let timer_fwd = fwd.clone();
    let timer_beat = beat.clone();
    let timer_clock = clock.clone();
    let timer_guard = guard.clone();
    // AppKit's drag tracking loop holds the main thread; this timer keeps the beat and forwarder tick going in tracking mode.
    // SAFETY: this worker runs on the main thread and owns its current run loop.
    let heartbeat = crate::heartbeat::keep_beating(unsafe { ffi::CFRunLoopGetCurrent() }, Duration::from_millis(50), Box::new(move || {
        // try_lock: this runs on the main thread, which may already hold the forwarder if a call under it spins the
        // run loop; waiting here would deadlock that thread. The next beat ticks instead.
        // The cadence is marked while the forwarder is still held: a change made after the lock is released
        // then finds the heartbeat marked slow and pokes it.
        let mark = |f: &mut Forwarder<EmitSink<MacBackend>, RouteOutbox>| {
            f.tick(timer_clock());
            let fast = f.needs_tick() || timer_guard.is_grabbed();
            crate::front::beat_slow(!fast);
            fast
        };
        let fast = match timer_fwd.try_lock() {
            Ok(mut f) => mark(&mut f),
            Err(std::sync::TryLockError::Poisoned(e)) => mark(&mut e.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => { crate::front::beat_slow(false); true }
        };
        timer_beat.store(timer_clock(), Ordering::SeqCst);
        if fast { Duration::from_millis(50) } else { IDLE_TURN }
    }));
    crate::front::set_poker(heartbeat.poker());
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
        line_waiting.store(false, Ordering::SeqCst);
        for line in lines.try_iter() {
            let Some(cmd) = menu_command(&line) else {
                flowseam_core::log::line(&format!("flowseam worker: unknown command from the window: {:?}", line.trim()));
                continue;
            };
            let (peer, target) = match cmd {
                MenuCmd::Test { .. } | MenuCmd::Inspect(_) => {
                    let local = matches!(fwd.lock().unwrap_or_else(|e| e.into_inner()).mode(), Mode::Local);
                    settings_command(cmd, local, &mut kvm, &mut side, ddc_tx.as_ref(), &mut switch);
                    continue;
                }
                MenuCmd::Return(peer) => {
                    want_home(&mut home_wanted, peer);
                    continue;
                }
                MenuCmd::Input(peer) => (peer, None),
                MenuCmd::Switch { uuids, peer } => match kvm_ready(&side, peer, &uuids) {
                    Ok(t) => (peer, Some(t)),
                    Err(why) => {
                        flowseam_core::log::line(&format!("flowseam worker: {why}"));
                        kvm.last_error = Some(why);
                        continue;
                    }
                },
            };
            let ready = fwd.lock().unwrap_or_else(|e| e.into_inner()).can_switch_to(peer);
            if ready != SwitchTo::Switched {
                flowseam_core::log::line(&format!("flowseam worker: menu switch to {peer} refused: {ready:?}"));
                kvm.last_error = Some(format!("cannot switch to {peer}: {ready:?}"));
                continue;
            }
            // Read before anything moves: a monitor switched away may drop off this Mac's display list.
            let way_back = target.as_ref().map_or(WayBack::Open, way_back_for);
            let steps = kvm.request(peer, target, way_back);
            run_kvm(&mut kvm, steps, ddc_tx.as_ref(), &mut switch);
        }
        for done in ddc_rx.iter().flat_map(|rx| rx.try_iter()) {
            let done = match done {
                Done::Probe { uuid, check } => {
                    let key = (check.peer.clone(), uuid);
                    if side.checks.get(&key) != Some(&check) {
                        flowseam_core::log::line(&format!("flowseam worker: monitor check {} {check:?}", key.1));
                    }
                    side.checks.insert(key, check);
                    continue;
                }
                Done::Inspect(s) => {
                    flowseam_core::log::line(&format!("flowseam worker: monitor inspected {s:?}"));
                    side.inspect = Some(s);
                    continue;
                }
                other => other,
            };
            flowseam_core::log::line(&format!("flowseam worker: monitor {done:?}"));
            let steps = kvm.done(done);
            run_kvm(&mut kvm, steps, ddc_tx.as_ref(), &mut switch);
        }
        for peer in home_asks.iter().flat_map(|a| std::mem::take(&mut *a.lock().unwrap_or_else(|e| e.into_inner()))) {
            want_home(&mut home_wanted, &peer);
        }
        let driving = match fwd.lock().unwrap_or_else(|e| e.into_inner()).mode() {
            Mode::RemoteOut { peer, .. } => Some(peer.clone()),
            _ => None,
        };
        let steps = kvm.tick(driving.as_deref(), clock());
        run_kvm(&mut kvm, steps, ddc_tx.as_ref(), &mut switch);
        // A "back" waits for a switch or return under way to land, then brings the rest of that peer's monitors.
        while let Some(peer) = home_wanted.first().cloned() {
            let Some(steps) = kvm.bring_home(&peer, side.monitors_of(&peer)) else { break };
            home_wanted.remove(0);
            flowseam_core::log::line(&format!("flowseam worker: bringing {peer}'s monitors home"));
            run_kvm(&mut kvm, steps, ddc_tx.as_ref(), &mut switch);
        }
        // Once per turn, after every step that can move the phase: a request, an answer, the tick, a "back".
        // The edges and the chord are read only in the pump below (the tap and the heartbeat run on this
        // thread's run loop), so none is crossed before this.
        // ponytail: a session that ends on a link thread is seen at the next turn's tick (46 ms measured);
        // dwell_ms plus the rearm distance outlasts that, so hold from restore_local if that ever shrinks.
        hold_for_kvm(&fwd, &kvm, clock());
        // Only between switches: a probe's reads are bus traffic, and a switch must not queue behind one.
        if matches!(kvm.phase(), Phase::Idle) && probed.is_none_or(|t| t.elapsed() >= PROBE_EVERY) {
            probed = Some(Instant::now());
            side.refresh_edids();
            probe_monitors(&side.monitors, ddc_tx.as_ref(), &mut side.checks);
        }
        write_kvm_status(&kvm, &side, kvm_file.as_deref(), &mut kvm_written);
        let grabbed = guard.is_grabbed();
        if grabbed != was_grabbed {
            flowseam_core::log::line(if grabbed { "flowseam worker: cursor grabbed" } else { "flowseam worker: cursor restored" });
            was_grabbed = grabbed;
        }
        if cfg.peers.is_empty() && status_checked.elapsed() >= Duration::from_secs(1) {
            status_checked = Instant::now();
            let path = flowseam_core::status::status_path(config_path);
            if let Err(e) = flowseam_core::status::write(&path, &flowseam_core::status::LinkStatus { written_ms: flowseam_core::status::now_ms(), peers: Vec::new(), driven_by: None }) {
                flowseam_core::log::line(&format!("flowseam worker: status write: {e}"));
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
        crate::front::arm_idle();
        // A stop that landed after the loop condition and before `arm_idle` found nothing to wake: look again.
        let ending = stop.load(Ordering::SeqCst) || TERM.load(Ordering::SeqCst) || QUIT_ASKED.load(Ordering::SeqCst);
        // A monitor switch, return or test turns fast: its steps and timers run on this loop.
        // ponytail: a probe's answer, and a link drop that should return a stranded monitor, wait out the
        // idle turn (500 ms at most); wake from the DDC thread and link_down if that ever shows.
        let kvm_busy = !matches!(kvm.phase(), Phase::Idle) || !home_wanted.is_empty() || line_waiting.load(Ordering::SeqCst)
            || home_asks.as_ref().is_some_and(|a| !a.lock().unwrap_or_else(|e| e.into_inner()).is_empty());
        let fast = ending || kvm_busy || guard.is_grabbed() || fwd.lock().unwrap_or_else(|e| e.into_inner()).needs_tick() || crate::front::busy();
        crate::front::pump(if fast { Duration::from_millis(50) } else { IDLE_TURN }, !fast);
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
    // The monitor starts home now and is waited for at the end, so the teardown below overlaps it.
    let steps = kvm.stop();
    run_kvm(&mut kvm, steps, ddc_tx.as_ref(), &mut switch);
    write_kvm_status(&kvm, &side, kvm_file.as_deref(), &mut kvm_written);

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
    crate::front::pump(Duration::ZERO, false);
    finish_kvm(&mut kvm, ddc_rx.as_ref(), KVM_STOP_LIMIT);
    write_kvm_status(&kvm, &side, kvm_file.as_deref(), &mut kvm_written);
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

/// A line from the window: `switch <uuid>[,<uuid>…] <peer>` moves those monitors of the peer and then the
/// input, `input <peer>` moves only the keyboard and mouse, `return <peer>` brings every monitor configured
/// for the peer home (one may have changed input by itself). From the settings: `inspect <uuid>` reads a
/// monitor's inputs, and `test <token> <uuid> <input> <home> <peer>` shows a candidate input for a while and
/// comes back; its result carries `token`, so the window asks only about the test it started. The peer
/// comes last because a name may hold spaces.
#[derive(Debug, PartialEq, Eq)]
enum MenuCmd<'a> {
    Switch { uuids: Vec<&'a str>, peer: &'a str },
    Input(&'a str),
    Return(&'a str),
    Inspect(&'a str),
    /// Monitors as (uuid, candidate input, this Mac's input).
    Test { token: u64, monitors: Vec<(&'a str, u16, u16)>, peer: &'a str },
}

/// How long a test shows the candidate input: long enough to recognize a desktop, short enough that the
/// person waits for it rather than doing something else.
const TRIAL_HOLD_MS: u64 = 8000;

fn menu_command(line: &str) -> Option<MenuCmd<'_>> {
    fn rest(s: &str) -> Option<&str> {
        Some(s.trim()).filter(|p| !p.is_empty())
    }
    let line = line.trim();
    let (verb, args) = line.split_once(' ')?;
    match verb {
        "switch" => {
            let (uuids, peer) = args.trim().split_once(' ')?;
            let uuids: Vec<&str> = uuids.split(',').filter(|u| !u.is_empty()).collect();
            if uuids.is_empty() {
                return None;
            }
            Some(MenuCmd::Switch { uuids, peer: rest(peer)? })
        }
        "input" => rest(args).map(MenuCmd::Input),
        "return" => rest(args).map(MenuCmd::Return),
        "inspect" => rest(args).map(MenuCmd::Inspect),
        // `test <token> <uuid>:<input>:<home>[,…] <peer>`: the peer last, since a name may hold spaces.
        "test" => {
            let mut it = args.trim().splitn(3, ' ');
            let token = it.next()?.parse().ok()?;
            let monitors = it
                .next()?
                .split(',')
                .map(|m| {
                    let mut f = m.split(':');
                    let monitor = (f.next().filter(|u| !u.is_empty())?, f.next()?.parse().ok()?, f.next()?.parse().ok()?);
                    f.next().is_none().then_some(monitor)
                })
                .collect::<Option<Vec<_>>>()?;
            let peer = rest(it.next()?)?;
            Some(MenuCmd::Test { token, monitors, peer })
        }
        _ => None,
    }
}

/// The settings' commands. Both read or move only the monitor, and only between switches; a test also
/// needs input here, since the person must be looking at this Mac to judge it.
fn settings_command(cmd: MenuCmd<'_>, local: bool, kvm: &mut Kvm, side: &mut KvmSide, ddc: Option<&Sender<Cmd>>, switch: &mut SwitchInput<'_>) {
    if !matches!(kvm.phase(), Phase::Idle) {
        kvm.last_error = Some(format!("busy: {:?}", kvm.phase()));
        return;
    }
    match cmd {
        MenuCmd::Inspect(uuid) => {
            let sent = ddc.is_some_and(|tx| tx.send(Cmd::Inspect { uuid: uuid.to_string() }).is_ok());
            if !sent {
                side.inspect = Some(InspectStatus { uuid: uuid.to_string(), inputs: None, current: None, reason: Some("no DDC thread".into()) });
            }
        }
        MenuCmd::Test { token, monitors, peer } => {
            if !local {
                kvm.last_error = Some("a test needs the keyboard and mouse on this Mac".into());
                return;
            }
            side.trial_id = token;
            let target = kvm::Target { monitors: monitors.iter().map(|&(uuid, input, home)| kvm::Monitor { uuid: uuid.to_string(), input, home }).collect() };
            let steps = kvm.trial(peer, target, TRIAL_HOLD_MS);
            run_kvm(kvm, steps, ddc, switch);
        }
        MenuCmd::Switch { .. } | MenuCmd::Input(_) | MenuCmd::Return(_) => {}
    }
}

/// The chosen monitors of `peer`, if each is in the config the worker runs with and its last probe found
/// it usable; an untested one switches too. One that is not refuses the whole switch: switching only the
/// rest would leave the person in front of a screen they did not choose.
fn kvm_ready(side: &KvmSide, peer: &str, uuids: &[&str]) -> Result<kvm::Target, String> {
    let mut monitors: Vec<kvm::Monitor> = Vec::new();
    for &uuid in uuids {
        let (_, d) = side.monitors.iter().find(|(p, d)| p == peer && d.uuid == uuid).ok_or_else(|| format!("monitor {uuid} is not one of {peer}'s"))?;
        if let Some(b) = side.state(peer, d).blocked() {
            return Err(format!("monitor switch to {peer} is not available: monitor {uuid}: {b:?}"));
        }
        if !monitors.iter().any(|m| m.uuid == uuid) {
            monitors.push(kvm::Monitor { uuid: d.uuid.clone(), input: d.input, home: d.home });
        }
    }
    Ok(kvm::Target { monitors })
}

/// Queues a "back" for `peer` once: asked twice before it runs, it still runs once.
fn want_home(wanted: &mut Vec<String>, peer: &str) {
    if !wanted.iter().any(|p| p == peer) {
        wanted.push(peer.to_string());
    }
}

/// Queues a probe of every configured monitor on the DDC thread; one that cannot be queued is recorded
/// as unknown rather than left looking fine.
fn probe_monitors(monitors: &[(String, DisplaySwitch)], ddc: Option<&Sender<Cmd>>, checks: &mut BTreeMap<(String, String), MonitorCheck>) {
    for (peer, d) in monitors {
        let monitor = kvm::Monitor { uuid: d.uuid.clone(), input: d.input, home: d.home };
        let sent = ddc.is_some_and(|tx| tx.send(Cmd::Probe { peer: peer.clone(), monitor }).is_ok());
        if !sent {
            checks.insert((peer.clone(), d.uuid.clone()), MonitorCheck { peer: peer.clone(), state: CheckState::Unknown, reason: Some("no DDC thread".into()) });
        }
    }
}

/// The edges and the chord wait while monitors move (`Forwarder::hold_for_switch`), either way: on the way
/// out, a cursor that wandered to another peer found this Mac busy and undid the switch; on the way home,
/// one that went straight back to the peer was driving it while its monitors left (11:19:27, 2026-11-10).
fn hold_for_kvm<S: flowseam_core::forward::InputSink, O: flowseam_core::forward::Outbox>(fwd: &Mutex<Forwarder<S, O>>, kvm: &Kvm, now_ms: u64) {
    fwd.lock().unwrap_or_else(|e| e.into_inner()).hold_for_switch(holds(kvm.phase()), now_ms);
}

fn holds(phase: &Phase) -> bool {
    matches!(phase, Phase::Switching { .. } | Phase::Returning { .. })
}

/// Hands input to a peer (`Forwarder::switch_to`).
type SwitchInput<'a> = dyn FnMut(&str, WayBack) -> SwitchTo + 'a;

/// `Closed` when the switch will move every display of this Mac: the peer's edge back here would only lead
/// to a screen nobody sees. A display not named in the target, or one that cannot be identified, leaves it open.
fn way_back_for(target: &kvm::Target) -> WayBack {
    match crate::ddc::mac::online_uuids() {
        Ok(here) if every_display_moved(&here, target) => WayBack::Closed,
        Ok(here) => {
            flowseam_core::log::line(&format!("flowseam worker: displays here {here:?}, not all moved: the peer's edge back stays open"));
            WayBack::Open
        }
        Err(e) => {
            flowseam_core::log::line(&format!("flowseam worker: displays here unreadable ({e:?}): the peer's edge back stays open"));
            WayBack::Open
        }
    }
}

fn every_display_moved(here: &[Option<String>], target: &kvm::Target) -> bool {
    !here.is_empty() && here.iter().all(|u| u.as_ref().is_some_and(|u| target.monitors.iter().any(|m| m.uuid == *u)))
}

/// Runs the KVM policy's steps in order. A command the DDC thread cannot take is answered as failed,
/// and whatever the policy does about that runs too.
fn run_kvm(kvm: &mut Kvm, steps: Vec<Step>, ddc: Option<&Sender<Cmd>>, switch: &mut SwitchInput<'_>) {
    let mut queue = std::collections::VecDeque::from(steps);
    while let Some(step) = queue.pop_front() {
        match step {
            Step::SwitchInput(peer, way_back) => {
                let r = switch(&peer, way_back);
                flowseam_core::log::line(&format!("flowseam worker: menu switch to {peer}: {r:?}"));
            }
            Step::Ddc(cmd) => {
                flowseam_core::log::line(&format!("flowseam worker: monitor command {cmd:?}"));
                let unsent = match ddc {
                    Some(tx) => tx.send(cmd).err().map(|e| e.0),
                    None => Some(cmd),
                };
                if let Some(cmd) = unsent {
                    let why = "no DDC thread".to_string();
                    queue.extend(kvm.done(match cmd {
                        Cmd::Away { peer, .. } => Done::Away { peer, result: Err(why), out: Vec::new() },
                        // Nothing was written, so whatever was out is still out.
                        Cmd::Home { peer, target } => Done::Home { peer, result: Err(why), out: target.monitors },
                        Cmd::Probe { peer, monitor } => Done::Probe { uuid: monitor.uuid, check: MonitorCheck { peer, state: CheckState::Unknown, reason: Some(why) } },
                        Cmd::Inspect { uuid } => Done::Inspect(InspectStatus { uuid, inputs: None, current: None, reason: Some(why) }),
                    }));
                }
            }
        }
    }
}

/// Waits, at most `limit`, for an unfinished switch or return to land.
fn finish_kvm(kvm: &mut Kvm, rx: Option<&Receiver<Done>>, limit: Duration) {
    let deadline = Instant::now() + limit;
    while !matches!(kvm.phase(), Phase::Idle) {
        let Some(rx) = rx else { break };
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(done) => {
                flowseam_core::log::line(&format!("flowseam worker: monitor {done:?}"));
                kvm.done(done);
            }
            Err(_) => {
                flowseam_core::log::line(&format!("flowseam worker: the monitors' return is not confirmed {limit:?} into the stop; the window takes over"));
                break;
            }
        }
    }
}

/// What the worker keeps beside the policy for kvm.toml: the configured monitors, probe results, the
/// settings' last inspect, and the number of the current test, so the window asks its question once per test.
#[derive(Default)]
struct KvmSide {
    /// Every monitor in the config the worker runs with, and its peer, in config order.
    monitors: Vec<(String, DisplaySwitch)>,
    /// What the last probe found, by peer and monitor UUID.
    checks: BTreeMap<(String, String), MonitorCheck>,
    inspect: Option<InspectStatus>,
    trial_id: u64,
    /// What each linked peer says it is attached to; filled by the links.
    peer_monitors: Option<crate::links::PeerMonitors>,
    /// The EDID identity of each monitor here, by UUID; refreshed with every probe round.
    edids: HashMap<String, flowseam_core::proto::MonitorId>,
}

impl KvmSide {
    fn new(cfg: &Config, peer_monitors: Option<crate::links::PeerMonitors>) -> Self {
        let monitors = cfg.peers.iter().flat_map(|p| p.displays.iter().map(|d| (p.name.clone(), d.clone()))).collect();
        Self { monitors, peer_monitors, ..Self::default() }
    }

    /// One configured monitor as it stands. Whether the peer sees it is reported, never a reason to refuse:
    /// a monitor may power down the inputs it is not showing, so the peer only sees it once it switches.
    fn state(&self, peer: &str, d: &DisplaySwitch) -> MonitorState {
        let reported: Option<Vec<flowseam_core::proto::MonitorId>> = self.peer_monitors.as_ref().and_then(|m| m.lock().unwrap_or_else(|e| e.into_inner()).get(peer).cloned()).flatten();
        MonitorState {
            peer: peer.to_string(),
            uuid: d.uuid.clone(),
            verified: d.verified,
            check: self.checks.get(&(peer.to_string(), d.uuid.clone())).cloned(),
            peer_sees: flowseam_core::status::peer_on_monitor(self.edids.get(&d.uuid).copied(), reported.as_deref()),
        }
    }

    /// Every monitor the config has for `peer`.
    fn monitors_of(&self, peer: &str) -> Vec<kvm::Monitor> {
        self.monitors.iter().filter(|(p, _)| p == peer).map(|(_, d)| kvm::Monitor { uuid: d.uuid.clone(), input: d.input, home: d.home }).collect()
    }

    fn states(&self) -> Vec<MonitorState> {
        self.monitors.iter().map(|(p, d)| self.state(p, d)).collect()
    }

    fn refresh_edids(&mut self) {
        match crate::ddc::mac::online() {
            Ok(list) => {
                self.edids = list.into_iter().map(|m| (m.uuid, flowseam_core::proto::MonitorId { vendor: m.edid.vendor as u16, product: m.edid.product as u16, serial: m.edid.serial })).collect();
            }
            Err(e) => flowseam_core::log::line(&format!("flowseam worker: monitor EDID: {e:?}")),
        }
    }
}

fn kvm_view(kvm: &Kvm, side: &KvmSide) -> KvmStatus {
    let phase = match kvm.phase() {
        Phase::Idle => KvmPhase::Idle,
        Phase::Switching { .. } => KvmPhase::Switching,
        Phase::Away { .. } => KvmPhase::Away,
        Phase::Returning { trial: None, .. } => KvmPhase::Returning,
        Phase::Returning { trial: Some(_), .. } | Phase::Trying { .. } | Phase::Shown { .. } => KvmPhase::Testing,
    };
    let out = kvm.out();
    let trial = kvm.last_trial.as_ref().map(|t| TrialStatus {
        peer: t.peer.clone(),
        monitors: t.monitors.iter().map(|m| TrialMonitor { uuid: m.uuid.clone(), input: m.input, home: m.home }).collect(),
        ok: t.result.is_ok(),
        reason: t.result.clone().err(),
        id: side.trial_id,
    });
    KvmStatus {
        written_ms: 0,
        phase,
        peer: out.as_ref().map(|(p, _)| p.clone()),
        error: kvm.last_error.clone(),
        out: out.map(|(_, ms)| ms.into_iter().map(|m| OutMonitor { uuid: m.uuid, home: m.home }).collect()).unwrap_or_default(),
        monitors: side.states(),
        trial,
        trial_hold_ms: TRIAL_HOLD_MS,
        inspect: side.inspect.clone(),
    }
}

/// The policy this worker starts with: monitors the previous run's kvm.toml still lists as out (its return,
/// and the window's after it, did not land) are adopted before this worker replaces the file; otherwise
/// nothing would ever try them again.
fn kvm_start(path: Option<&std::path::Path>) -> Kvm {
    let mut kvm = Kvm::new();
    let Some(prev) = path.and_then(|p| flowseam_core::status::read_kvm(p).ok()) else { return kvm };
    if !prev.out.is_empty() {
        flowseam_core::log::line(&format!("flowseam worker: {} monitor(s) still out from the previous run: {:?}", prev.out.len(), prev.out));
        kvm.adopt(prev.peer.unwrap_or_default(), prev.out.into_iter().map(|o| kvm::Monitor { uuid: o.uuid, input: o.home, home: o.home }).collect());
    }
    kvm
}

/// Writes kvm.toml when what it says changes; the first call always writes, replacing a previous run's file.
fn write_kvm_status(kvm: &Kvm, side: &KvmSide, path: Option<&std::path::Path>, written: &mut Option<KvmStatus>) {
    let view = kvm_view(kvm, side);
    if written.as_ref() == Some(&view) {
        return;
    }
    let Some(path) = path else { return };
    let stamped = KvmStatus { written_ms: flowseam_core::status::now_ms(), ..view.clone() };
    match flowseam_core::status::write_kvm(path, &stamped) {
        Ok(()) => *written = Some(view),
        Err(e) => flowseam_core::log::line(&format!("flowseam worker: kvm status write: {e}")),
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

    fn mon(uuid: &str) -> kvm::Monitor {
        kvm::Monitor { uuid: uuid.into(), input: 15, home: 18 }
    }
    fn target() -> kvm::Target {
        kvm::Target { monitors: vec![mon("U"), mon("V")] }
    }

    #[test]
    fn window_lines_name_the_peer_and_what_to_move() {
        assert_eq!(menu_command("switch U,V my pc\n"), Some(MenuCmd::Switch { uuids: vec!["U", "V"], peer: "my pc" }));
        assert_eq!(menu_command("switch U pc\n"), Some(MenuCmd::Switch { uuids: vec!["U"], peer: "pc" }));
        assert_eq!(menu_command("switch pc\n"), None, "no monitor named: the window sends `input` for that");
        assert_eq!(menu_command("switch , pc\n"), None);
        assert_eq!(menu_command("switch \n"), None);
        assert_eq!(menu_command("  input  my pc \r\n"), Some(MenuCmd::Input("my pc")));
        assert_eq!(menu_command("return my pc\n"), Some(MenuCmd::Return("my pc")));
        assert_eq!(menu_command("return \n"), None);
        assert_eq!(menu_command("home\n"), None);
        assert_eq!(menu_command("inspect U-1\n"), Some(MenuCmd::Inspect("U-1")));
        assert_eq!(menu_command("test 77 U-1:15:18 my pc\n"), Some(MenuCmd::Test { token: 77, monitors: vec![("U-1", 15, 18)], peer: "my pc" }));
        assert_eq!(menu_command("test 77 U-1:15:18,V-2:17:19 pc\n"), Some(MenuCmd::Test { token: 77, monitors: vec![("U-1", 15, 18), ("V-2", 17, 19)], peer: "pc" }));
        for bad in ["test 77 U-1:15:x pc", "test 77 U-1:15:18", "test 77 U-1:15 pc", "test 77 U-1:15:18:9 pc", "test 77 U-1:15:18, pc", "test 77 :15:18 pc", "test x U-1:15:18 pc"] {
            assert_eq!(menu_command(&format!("{bad}\n")), None, "{bad}");
        }
    }

    #[test]
    fn a_test_needs_input_here_and_reports_under_its_own_number() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut k = Kvm::new();
        let mut side = KvmSide::default();
        let test = MenuCmd::Test { token: 42, monitors: vec![("U", 15, 18)], peer: "pc" };
        settings_command(test, false, &mut k, &mut side, Some(&tx), &mut |_, _| panic!("no input moves in a test"));
        assert!(rx.try_recv().is_err() && k.last_error.is_some(), "not while input is away");
        let test = MenuCmd::Test { token: 42, monitors: vec![("U", 15, 18), ("V", 15, 18)], peer: "pc" };
        settings_command(test, true, &mut k, &mut side, Some(&tx), &mut |_, _| panic!("no input moves in a test"));
        assert_eq!(rx.try_recv(), Ok(Cmd::Away { peer: "pc".into(), target: kvm::Target { monitors: vec![mon("U"), mon("V")] } }), "both in one switch");
        assert_eq!((side.trial_id, kvm_view(&k, &side).phase), (42, KvmPhase::Testing));
        settings_command(MenuCmd::Inspect("U"), true, &mut k, &mut side, Some(&tx), &mut |_, _| SwitchTo::Switched);
        assert!(rx.try_recv().is_err(), "nothing else while a test runs");
    }

    fn cfg_with(verified: [bool; 2]) -> Config {
        let mut cfg = Config::default();
        cfg.peers.push(flowseam_core::config::Peer {
            name: "pc".into(),
            addr: "192.168.1.2:24800".into(),
            edge: flowseam_core::config::Edge::Right,
            fingerprint: "ab".repeat(32),
            keys: Default::default(),
            gestures: Default::default(),
            displays: vec![
                DisplaySwitch { uuid: "U".into(), input: 15, home: 18, verified: verified[0] },
                DisplaySwitch { uuid: "V".into(), input: 15, home: 18, verified: verified[1] },
            ],
        });
        cfg
    }

    fn checked(cfg: &Config, state: CheckState) -> KvmSide {
        let mut side = KvmSide::new(cfg, None);
        for u in ["U", "V"] {
            side.checks.insert(("pc".into(), u.into()), MonitorCheck { peer: "pc".into(), state, reason: Some("why".into()) });
        }
        side
    }

    #[test]
    fn a_monitor_switch_waits_for_a_usable_probe_of_every_chosen_monitor() {
        let both = cfg_with([true, true]);
        assert_eq!(kvm_ready(&checked(&both, CheckState::Ok), "pc", &["U", "V"]), Ok(target()));
        assert_eq!(kvm_ready(&checked(&both, CheckState::Ok), "pc", &["V"]), Ok(kvm::Target::one(mon("V"))), "one of them alone");
        assert_eq!(kvm_ready(&checked(&both, CheckState::Ok), "pc", &["U", "U"]), Ok(kvm::Target::one(mon("U"))), "named twice, switched once");
        let one_unverified = checked(&cfg_with([true, false]), CheckState::Ok);
        assert_eq!(kvm_ready(&one_unverified, "pc", &["U", "V"]), Ok(target()), "an untested monitor switches too");
        assert_eq!(kvm_ready(&one_unverified, "pc", &["U"]), Ok(kvm::Target::one(mon("U"))));
        let one_down = { let mut s = checked(&cfg_with([true, false]), CheckState::Ok); s.checks.insert(("pc".into(), "V".into()), MonitorCheck { peer: "pc".into(), state: CheckState::Problem, reason: Some("gone".into()) }); s };
        let e = kvm_ready(&one_down, "pc", &["U", "V"]).unwrap_err();
        assert!(e.contains("monitor V") && e.contains("gone"), "one unusable monitor refuses the whole switch and is named: {e}");
        assert!(kvm_ready(&checked(&both, CheckState::Problem), "pc", &["U"]).unwrap_err().contains("why"));
        assert!(kvm_ready(&KvmSide::new(&both, None), "pc", &["U"]).unwrap_err().contains("NotChecked"));
        assert!(kvm_ready(&checked(&both, CheckState::Ok), "other", &["U"]).is_err(), "another peer's name");
        assert!(kvm_ready(&checked(&both, CheckState::Ok), "pc", &["W"]).unwrap_err().contains("not one of"));
    }

    /// Some monitors power down the inputs they are not showing, so the peer cannot see one until it
    /// switches: what the peer reports is shown, never a reason to refuse.
    #[test]
    fn what_the_peer_reports_is_shown_but_does_not_block_the_switch() {
        use flowseam_core::proto::MonitorId;
        let mine = MonitorId { vendor: 0x09D1, product: 0x7955, serial: 0x1234 };
        let reports = crate::links::PeerMonitors::default();
        let mut side = checked(&cfg_with([true, true]), CheckState::Ok);
        side.peer_monitors = Some(reports.clone());
        side.edids.insert("U".into(), mine);
        assert_eq!(side.states()[0].peer_sees, None, "no report yet");
        reports.lock().unwrap().insert("pc".into(), Some(vec![MonitorId { vendor: 0x0693, product: 0xA714, serial: 1 }]));
        assert_eq!(side.states()[0].peer_sees, Some(false));
        assert_eq!(kvm_ready(&side, "pc", &["U", "V"]), Ok(target()));
        reports.lock().unwrap().insert("pc".into(), Some(vec![mine]));
        assert_eq!(kvm_view(&Kvm::new(), &side).monitors[0].peer_sees, Some(true), "kvm.toml says the same");
    }

    #[test]
    fn every_configured_monitor_is_probed_and_an_unsent_probe_is_unknown() {
        let (tx, rx) = std::sync::mpsc::channel();
        let side = KvmSide::new(&cfg_with([true, true]), None);
        let mut checks = BTreeMap::new();
        probe_monitors(&side.monitors, Some(&tx), &mut checks);
        assert_eq!(rx.try_recv(), Ok(Cmd::Probe { peer: "pc".into(), monitor: mon("U") }));
        assert_eq!(rx.try_recv(), Ok(Cmd::Probe { peer: "pc".into(), monitor: mon("V") }));
        assert!(checks.is_empty());
        probe_monitors(&side.monitors, None, &mut checks);
        assert_eq!(checks[&("pc".to_string(), "V".to_string())].state, CheckState::Unknown);
        assert_eq!(checks.len(), 2);
    }

    #[test]
    fn a_menu_switch_reaches_the_ddc_thread_and_input_follows_its_answer() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut switched = Vec::new();
        let mut k = Kvm::new();
        let steps = k.request("pc", Some(target()), WayBack::Closed);
        run_kvm(&mut k, steps, Some(&tx), &mut |p, w| { switched.push((p.to_string(), w)); SwitchTo::Switched });
        assert_eq!(rx.try_recv(), Ok(Cmd::Away { peer: "pc".into(), target: target() }));
        assert!(switched.is_empty(), "input waits for the monitors");
        let steps = k.done(Done::Away { peer: "pc".into(), result: Ok(()), out: vec![] });
        run_kvm(&mut k, steps, Some(&tx), &mut |p, w| { switched.push((p.to_string(), w)); SwitchTo::Switched });
        assert_eq!(switched, [("pc".to_string(), WayBack::Closed)], "input follows with the way back its request planned");
    }

    #[test]
    fn the_way_back_closes_only_when_every_display_here_moved() {
        let both = kvm::Target { monitors: vec![
            kvm::Monitor { uuid: "U".into(), input: 15, home: 18 },
            kvm::Monitor { uuid: "V".into(), input: 17, home: 19 },
        ] };
        let here = |us: &[Option<&str>]| us.iter().map(|u| u.map(String::from)).collect::<Vec<_>>();
        assert!(every_display_moved(&here(&[Some("U"), Some("V")]), &both));
        assert!(every_display_moved(&here(&[Some("V")]), &both), "a target may name a monitor not attached now");
        assert!(!every_display_moved(&here(&[Some("U"), Some("W")]), &both), "one display still shows this Mac");
        assert!(!every_display_moved(&here(&[Some("U"), None]), &both), "an unidentified display may still show this Mac");
        assert!(!every_display_moved(&here(&[]), &both), "no displays read is no answer");
        let one = kvm::Target { monitors: vec![both.monitors[0].clone()] };
        assert!(!every_display_moved(&here(&[Some("U"), Some("V")]), &one), "a partial switch keeps the edge back");
    }

    #[test]
    fn the_edges_hold_while_monitors_move_either_way() {
        let t = target();
        assert!(holds(&Phase::Switching { peer: "pc".into(), target: t.clone() }));
        assert!(holds(&Phase::Returning { peer: "pc".into(), target: t.clone(), trial: None }));
        assert!(!holds(&Phase::Away { peer: "pc".into(), target: t.clone() }), "the session is the peer's then");
        assert!(!holds(&Phase::Idle));
        assert!(!holds(&Phase::Trying { peer: "pc".into(), target: t, hold_ms: 1 }), "a test keeps input here");
    }

    #[test]
    fn without_a_ddc_thread_a_switch_fails_and_says_why() {
        let mut k = Kvm::new();
        let steps = k.request("pc", Some(target()), WayBack::Open);
        run_kvm(&mut k, steps, None, &mut |_, _| panic!("input must not move"));
        assert_eq!(k.phase(), &Phase::Idle);
        assert_eq!(k.last_error.as_deref(), Some("no DDC thread"));
    }

    #[test]
    fn kvm_status_is_written_once_per_change() {
        let path = std::env::temp_dir().join(format!("flowseam-kvm-worker-{}-{}.toml", std::process::id(), flowseam_core::status::now_ms()));
        let mut written = None;
        let mut k = Kvm::new();
        let side = KvmSide::new(&cfg_with([true, false]), None);
        write_kvm_status(&k, &side, Some(&path), &mut written);
        let first = flowseam_core::status::read_kvm(&path).unwrap();
        assert_eq!(first.phase, KvmPhase::Idle);
        assert_eq!(first.monitors.iter().map(|m| (m.uuid.as_str(), m.verified)).collect::<Vec<_>>(), [("U", true), ("V", false)], "the window gates on what the worker runs");
        assert_eq!(first.trial_hold_ms, TRIAL_HOLD_MS);
        std::fs::remove_file(&path).unwrap();
        write_kvm_status(&k, &side, Some(&path), &mut written);
        assert!(!path.exists(), "unchanged: not rewritten");
        k.request("pc", Some(target()), WayBack::Open);
        write_kvm_status(&k, &side, Some(&path), &mut written);
        let s = flowseam_core::status::read_kvm(&path).unwrap();
        assert_eq!((s.phase, s.peer.as_deref()), (KvmPhase::Switching, Some("pc")));
        assert_eq!(s.out, [OutMonitor { uuid: "U".into(), home: 18 }, OutMonitor { uuid: "V".into(), home: 18 }], "names every monitor for the window's fallback");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn monitors_the_previous_run_left_out_are_adopted_and_stay_listed() {
        let path = std::env::temp_dir().join(format!("flowseam-kvm-adopt-{}-{}.toml", std::process::id(), flowseam_core::status::now_ms()));
        assert_eq!(kvm_start(Some(&path)).out(), None, "no previous file");
        let prev = KvmStatus { written_ms: 1, peer: Some("pc".into()), out: vec![OutMonitor { uuid: "V".into(), home: 18 }], ..Default::default() };
        flowseam_core::status::write_kvm(&path, &prev).unwrap();
        let mut k = kvm_start(Some(&path));
        let mut written = None;
        write_kvm_status(&k, &KvmSide::default(), Some(&path), &mut written);
        assert_eq!(flowseam_core::status::read_kvm(&path).unwrap().out, prev.out, "this run's kvm.toml keeps naming it");
        assert_eq!(k.stop(), vec![Step::Ddc(Cmd::Home { peer: "pc".into(), target: kvm::Target { monitors: vec![kvm::Monitor { uuid: "V".into(), input: 18, home: 18 }] } })]);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_back_brings_every_monitor_of_that_peer_and_is_asked_once() {
        let side = KvmSide::new(&cfg_with([true, false]), None);
        assert_eq!(side.monitors_of("pc"), target().monitors, "unconfirmed ones too: going home needs no confirmation");
        assert_eq!(side.monitors_of("air"), vec![]);
        let mut wanted = Vec::new();
        want_home(&mut wanted, "pc");
        want_home(&mut wanted, "pc");
        want_home(&mut wanted, "air");
        assert_eq!(wanted, ["pc", "air"]);
    }

    #[test]
    fn sigterm_handler_writes_wake_byte() {
        let pipe = crate::signal_pipe::SignalPipe::new().unwrap();
        let term = AtomicBool::new(false);
        note_term(&term, pipe.write_fd());
        assert!(pipe.ready_within(100), "SIGTERM did not wake the pipe reader");
        pipe.wait().unwrap();
        assert!(term.load(Ordering::SeqCst));
    }

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
        let r = run_with(&cfg, &std::env::temp_dir().join("flowseam-unused-config.toml"), std::io::empty(), &mut out);
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
            gestures: Default::default(),
            displays: Vec::new(),
        });
        let mut out = Vec::new();
        let r = run_with(&cfg, &std::env::temp_dir().join("flowseam-unused-config.toml"), std::io::empty(), &mut out);
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
