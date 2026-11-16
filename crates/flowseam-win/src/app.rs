//! Process-level wiring: config, identity, pairing and the worker.
//!
//! The worker runs the shared links (`flowseam_net::links`), as a Mac does: it listens and dials every
//! paired peer, and one flowseam_core `Forwarder` takes every peer's messages, this PC's own input through
//! the hooks, and the ticks of a clock thread. It injects a peer's input here, and sends this PC's input to
//! the peer it drives.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use flowseam_core::clipboard::ClipboardOs;
use flowseam_core::config::{Config, KeyMap};
use flowseam_core::forward::{Forwarder, FwdConfig, Mode, Outbox, SwitchTo, WayBack};
use flowseam_core::proto::{self, Rect};
use flowseam_core::kvm_side::{self, kvm_ready, kvm_start, probe_monitors, want_home, write_kvm_status, KvmSide, PeerReports};
use flowseam_core::menu::{menu_command, MenuCmd};
use flowseam_core::status::{self, KvmStatus, LinkStatus};
use flowseam_net::files::{DragHub, DragSource};
use flowseam_net::identity::Identity;
use flowseam_net::links::{Clock, DragCfg, LinkHost, LinkParams, Links, MonitorsHome, PeerMonitors, RouteOutbox, StatusBoard};
use flowseam_net::node;
#[cfg(test)]
use {
    flowseam_core::{
        config::{Edge, Peer},
        forward::PeerSpec,
        proto::Msg,
        switch::SwitchPolicy,
    },
    flowseam_net::{identity::to_hex, node::home_request_to},
};
use flowseam_net::session::{PeerInfo, Timing};
use crate::kvm::{Cmd, Done, Kvm, Phase, Step};
#[cfg(test)]
use flowseam_core::kvm as kvm_core;
#[cfg(windows)]
use crate::kvm;

#[cfg(windows)]
use crate::edge_drop::EdgeDropSource;
use crate::inject::{InjectSink, Injector};
#[cfg(windows)]
use crate::Clipboard;

const DISPLAY_REFRESH: Duration = Duration::from_secs(2);
/// How often the configured monitors are probed while nothing is being switched.
const PROBE_EVERY: Duration = Duration::from_secs(30);
static ROUTER_HEARTBEAT: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
/// Set by the window's "back" button; the live session's router takes it on its next turn.
static HOME_ASKED: AtomicBool = AtomicBool::new(false);
/// The peer driving this machine now, for status.toml and so for the window's "back" button.
static DRIVEN_BY: Mutex<Option<String>> = Mutex::new(None);

/// The window's "back" button: hands control back to the peer driving this machine, as the emergency
/// chord does, and asks that peer to bring home the monitors it switches for this machine, driving it or
/// not (one may have changed input by itself while that peer was down).
pub fn ask_home() {
    HOME_ASKED.store(true, Ordering::SeqCst);
}


fn route_heartbeat(value: Option<Instant>) {
    *ROUTER_HEARTBEAT.get_or_init(|| Mutex::new(None)).lock().unwrap_or_else(|e| e.into_inner()) = value;
}


pub fn log(msg: &str) {
    flowseam_core::log::line(&format!("[flowseam-win] {msg}"));
}

const NAMES: node::Names = node::Names { log, me: "this PC", connect_to: "to this PC" };

pub use node::{identity_dir, wake_clocks};

fn node_info(id: &Identity, displays: Vec<Rect>) -> PeerInfo {
    let name = std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_else(|_| "flowseam-win".into());
    node::node_info(id, displays, &name)
}

/// Print the code, wait for one successful pairing, store the client's fingerprint.
pub fn pair_serve(config: &Path, peer: &str, peer_addr: &str) -> Result<(), String> {
    pair_serve_with(config, peer, peer_addr, |code| println!("PAIR CODE: {code}"), |fp| println!("PAIRED {peer} {fp}"))
}

/// Serve one pairing: `on_code` gets the code to show before a client is accepted, `on_paired` the
/// stored fingerprint (hex). The CLI prints both; the GUI shows them.
pub fn pair_serve_with(config: &Path, peer: &str, peer_addr: &str, on_code: impl FnOnce(&str), on_paired: impl FnOnce(&str)) -> Result<(), String> {
    // How this PC's keys reach the peer when it drives it (see `fwd_config_all`).
    node::pair_serve_with(NAMES, config, peer, peer_addr, KeyMap::Position, on_code, on_paired)
}

/// `pair_serve_with` that ends with `Err(PAIR_CANCELLED)` once `cancel` is raised and woken by `pair_serve_cancel`.
pub fn pair_serve_cancellable(config: &Path, peer: &str, peer_addr: &str, cancel: &AtomicBool, on_code: impl FnOnce(&str), on_paired: impl FnOnce(&str)) -> Result<(), String> {
    node::pair_serve_cancellable(NAMES, config, peer, peer_addr, KeyMap::Position, cancel, on_code, on_paired)
}

/// End a waiting `pair_serve_cancellable`.
pub fn pair_serve_cancel(config: &Path, cancel: &AtomicBool) {
    node::pair_serve_cancel(config, cancel);
}

/// Dial a pairing server and store its fingerprint under `peer`.
pub fn pair_dial(config: &Path, peer: &str, addr: &str, code: &str) -> Result<(), String> {
    node::pair_dial(config, peer, addr, code, KeyMap::Position)
}

/// Runs one monitor command on this thread, for a caller with no DDC thread (the window after a worker died).
#[cfg(windows)]
pub fn run_ddc_now(cmd: Cmd) -> Done {
    kvm::DdcRunner::new(kvm::WindowsBackend, thread::sleep as fn(Duration)).run(cmd)
}

/// Whether this process captures its own input (the hooks are in): the worker then attaches to capture.
#[cfg(windows)]
static CAPTURING: AtomicBool = AtomicBool::new(false);

/// Install the hooks once per process. Without them this PC is driven but never drives.
#[cfg(windows)]
fn start_capture() {
    match crate::hook::start() {
        Ok(()) => {
            CAPTURING.store(true, Ordering::SeqCst);
            log("this PC's keyboard and mouse are captured: it drives a peer from the edge facing it");
        }
        Err(e) => log(&format!("capture is off ({e}): this PC can be driven but cannot drive")),
    }
}

/// This PC's forwarder, sending through `O`, shared by the links, the hook thread and the clock thread.
type WinFwd<O> = Arc<Mutex<Forwarder<InjectSink<Box<dyn Injector>>, O>>>;

/// The hook thread's way into a forwarder, on the clock the forwarder's other callers use.
struct SessionTarget<O: Outbox> {
    fwd: WinFwd<O>,
    clock: Clock,
}

impl<O: Outbox> SessionTarget<O> {
    fn now(&self) -> u64 {
        (self.clock)()
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, Forwarder<InjectSink<Box<dyn Injector>>, O>> {
        self.fwd.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl<O: Outbox + Send + 'static> crate::capture::Target for SessionTarget<O> {
    fn event(&self, ev: flowseam_core::backend::CapturedEvent) -> flowseam_core::backend::Disposition {
        let now = self.now();
        self.lock().on_captured(ev, now)
    }
    fn cursor(&self, at: proto::Point) {
        let now = self.now();
        let mut f = self.lock();
        let p = f.sink().clamp(at);
        f.on_cursor(p, now);
    }
    fn tick(&self) -> bool {
        let now = self.now();
        let mut f = self.lock();
        f.tick(now);
        f.needs_tick()
    }
    fn needs_tick(&self) -> bool {
        self.lock().needs_tick()
    }
    fn grabbed(&self) -> bool {
        matches!(self.lock().mode(), Mode::RemoteOut { .. })
    }
}

/// Lets go of capture when the session ends, however it ends, and uncovers the cursor: a session that ended
/// without bringing the cursor home (its router panicked) would leave the veil over it.
struct Attached;

impl Drop for Attached {
    fn drop(&mut self) {
        crate::capture::detach();
        #[cfg(windows)]
        if let Err(e) = crate::hook::grab().show() {
            log(&format!("the cursor could not be uncovered as the session ended: {e}"));
        }
    }
}

/// Unfinished files wait in %LOCALAPPDATA%\Flowseam\drags; dropped ones land in Downloads. `None` when
/// the environment names no user profile.
pub fn drag_dirs() -> Option<(PathBuf, PathBuf)> {
    // ponytail: %USERPROFILE%\Downloads misses a relocated Downloads folder; ask SHGetKnownFolderPath(FOLDERID_Downloads) if one shows up.
    let home = PathBuf::from(std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?);
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| home.join("AppData").join("Local"));
    Some((local.join("Flowseam").join("drags"), home.join("Downloads")))
}


/// "2560x1440 at (-2560,0), 1920x1080 at (0,0)": the primary display is the one at (0,0).
fn describe_layout(d: &[Rect]) -> String {
    d.iter().map(|r| format!("{}x{} at ({},{})", r.w, r.h, r.x, r.y)).collect::<Vec<_>>().join(", ")
}


/// A probe has this long to reach the hooks before its round counts: the router may wake again at once.
#[cfg(windows)]
const PROBE_WAIT: Duration = Duration::from_millis(300);

/// The probe round in flight: when its probes went out, and the hooks' counts then.
#[cfg(windows)]
type ProbeRound = Option<(Instant, (u64, u64))>;

/// While this PC drives the peer, each hook has to see its probes: Windows takes out a hook it finds too slow,
/// each on its own, and with only the keyboard hook gone the cursor stays held while every key types here.
/// A hook that missed its probes in consecutive rounds (`judge`) brings control home and the hooks are put
/// back. `probe` is the round in flight: when it went out and the counts then.
#[cfg(windows)]
fn probe_round<O: Outbox>(f: &mut Forwarder<InjectSink<Box<dyn Injector>>, O>, probe: &mut ProbeRound, judge: &mut crate::capture::ProbeJudge, unknown: &mut bool) {
    if !matches!(f.mode(), Mode::RemoteOut { .. }) {
        *probe = None;
        *judge = crate::capture::ProbeJudge::default();
        return;
    }
    if let Some((sent, before)) = *probe {
        if sent.elapsed() < PROBE_WAIT {
            return;
        }
        *probe = None;
        if let Some(gone) = judge.round(crate::capture::hooks_gone(before, crate::capture::probes_seen())) {
            log(&format!("the {gone} stopped while this PC drives the peer: Windows took it out. Coming home and putting the hooks back"));
            f.release_control();
            crate::hook::reinstall();
            return;
        }
    }
    let before = crate::capture::probes_seen();
    match crate::hook::send_probe() {
        Ok(()) => *probe = Some((Instant::now(), before)),
        Err(e) if !*unknown => {
            log(&format!("cannot tell whether the input hooks still run: {e}"));
            *unknown = true;
        }
        Err(_) => {}
    }
}


// One worker for every peer on the shared links (`flowseam_net::links`), as a Mac runs: this PC listens and
// dials, any number of peers are linked at once, and the cursor carries on through this PC to the next one.

/// What a worker needs from this machine besides the config.
pub struct Machine {
    pub injector: Box<dyn Injector>,
    pub displays: Arc<dyn Fn() -> Result<Vec<Rect>, String> + Send + Sync>,
    pub clipboard: Option<Arc<dyn ClipboardOs + Send + Sync>>,
    pub link: LinkHost,
    /// Where unfinished files wait and where a drop lands (`drag_dirs`); `None` turns file drags off.
    pub drags: Option<(PathBuf, PathBuf)>,
    /// This PC's own input reaches the worker through the hooks, so it drives its peers.
    pub capturing: bool,
}

/// This PC's machine for the worker: SendInput, its displays, its clipboard, its adapters, its own input.
#[cfg(windows)]
pub fn this_pc() -> Result<Machine, String> {
    if let Err(e) = crate::win_input::dpi_aware() {
        log(&format!("warning: {e}"));
    }
    let clipboard = match crate::clip_os::ClipThread::start(|| Ok(Box::new(crate::win32::WinClipboard::new()?) as Box<dyn Clipboard>)) {
        Ok(c) => Some(Arc::new(c) as Arc<dyn ClipboardOs + Send + Sync>),
        Err(e) => {
            log(&format!("clipboard sharing is off: {e}"));
            None
        }
    };
    start_capture();
    let capturing = CAPTURING.load(Ordering::SeqCst) && crate::hook::alive();
    Ok(Machine {
        injector: Box::new(crate::win_input::SendInputInjector),
        displays: Arc::new(crate::win_input::displays),
        clipboard,
        link: link_host(),
        drags: drag_dirs(),
        capturing,
    })
}

/// This PC's adapters and free space for the links. Elsewhere (tests, `--stdio` on another host) the
/// addresses are unknown, so no path is judged lost, and free space is unknown.
pub fn link_host() -> LinkHost {
    #[cfg(windows)]
    return LinkHost { interfaces: crate::netifs::local_interfaces, addrs: crate::netifs::local_addrs, free_bytes: crate::win32::free_bytes, wake: wake_clocks };
    #[cfg(not(windows))]
    LinkHost { interfaces: Vec::new, addrs: || None, free_bytes: |_| Err("free space unknown on this host".into()), wake: wake_clocks }
}

type LinkedShared = WinFwd<RouteOutbox>;

/// Every peer's edge, keys and the switch policy. "mac" keys translate a Mac keyboard's shortcuts for a PC,
/// the wrong way round on this PC: such a peer gets this PC's keys by position.
fn fwd_config_all(cfg: &Config) -> Result<FwdConfig, String> {
    node::fwd_config_all(cfg, |p| match p.keys {
        KeyMap::Mac => {
            log(&format!("{}: keys = \"mac\" is for a Mac keyboard driving a PC; this PC sends its keys by position", p.name));
            KeyMap::Position
        }
        k => k,
    })
}

/// A running worker. Production never stops one; tests do.
pub struct Worker {
    pub links: Links,
    pub fwd: LinkedShared,
    /// The forwarder's clock: every caller of the forwarder reads time from it.
    pub clock: Clock,
    stop: Arc<AtomicBool>,
    threads: Vec<thread::JoinHandle<()>>,
    attached: Option<Attached>,
    commands: Sender<String>,
}

impl Worker {
    fn clock_finished(&self) -> bool {
        self.threads.first().is_none_or(|t| t.is_finished())
    }

    /// Blocks while the clock thread runs, which in production is for good.
    fn wait(mut self) -> Result<(), String> {
        let clock = self.threads.remove(0);
        Err(match clock.join() {
            Ok(()) => "the clock thread stopped".to_string(),
            Err(_) => "the clock thread panicked".to_string(),
        })
    }

    pub fn stop(self) {
        let Worker { links, fwd, stop, threads, attached, .. } = self;
        stop.store(true, Ordering::SeqCst);
        drop(attached);
        // A session in progress ends the way the emergency chord ends it: the peer is told, and keys held
        // down on either side are let go before the links close.
        fwd.lock().unwrap_or_else(|e| e.into_inner()).release_control();
        links.stop();
        wake_clocks();
        for t in threads {
            let _ = t.join();
        }
    }
}

/// Start the worker for `config` on `m`.
pub fn start(config: &Path, m: Machine) -> Result<Worker, String> {
    let cfg = node::load(config)?;
    cfg.validate_for_run().map_err(|e| e.to_string())?;
    let fwd_cfg = fwd_config_all(&cfg)?;
    let id = Identity::load_or_create(&identity_dir(config)).map_err(|e| e.to_string())?;
    let displays = (m.displays)()?;
    let routes = RouteOutbox::default();
    let mut sink = InjectSink::new(m.injector, displays.clone(), log);
    let mut drags = None;
    if let Some((staging, dest)) = m.drags {
        #[cfg(windows)]
        let edges: Vec<proto::Edge> = fwd_cfg.peers.iter().map(|p| p.edge).collect();
        #[cfg(windows)]
        let edge_drop = EdgeDropSource::new(edges.clone(), displays.clone());
        #[cfg(windows)]
        let source: Option<Box<dyn DragSource>> = Some(Box::new(edge_drop.clone()));
        #[cfg(not(windows))]
        let source: Option<Box<dyn DragSource>> = None;
        let hub = Arc::new(Mutex::new(DragHub::new(source)));
        sink = sink.with_drags(hub.clone());
        #[cfg(windows)]
        {
            sink = sink.with_edge_drop(edge_drop, edges);
        }
        #[cfg(windows)]
        let native: Option<Arc<dyn flowseam_net::files::NativeDrop>> = Some(Arc::new(crate::native_drop::WinNativeDrop::new()));
        #[cfg(not(windows))]
        let native: Option<Arc<dyn flowseam_net::files::NativeDrop>> = None;
        drags = Some(DragCfg { hub, staging, dest, native });
    }
    #[cfg(windows)]
    if m.capturing {
        sink = sink.with_grab(crate::hook::grab());
    }
    let fwd: LinkedShared = Arc::new(Mutex::new(Forwarder::new(fwd_cfg.clone(), displays.clone(), sink, routes.clone())));
    let t0 = Instant::now();
    let clock: Clock = Arc::new(move || t0.elapsed().as_millis() as u64);
    let home_asks = MonitorsHome::default();
    let peer_monitors = PeerMonitors::default();
    let params = LinkParams {
        identity: id.clone(),
        me: node_info(&id, displays),
        monitors: crate::edid::present_monitors(),
        peer_monitors: peer_monitors.clone(),
        monitors_home: home_asks.clone(),
        pins: node::pins_of(&cfg),
        peers: node::link_peers(&cfg, NAMES),
        listen: node::resolve(&cfg.listen)?,
        timing: Timing::default(),
        status: None,
        host: m.link,
    };
    let links = Links::start(params, fwd.clone(), routes.clone(), clock.clone(), Arc::new(|| log("a link thread panicked")), m.clipboard, drags)
        .map_err(|e| format!("listen on {}: {e:?}", cfg.listen))?;
    log(&format!("listening on {}", links.listening()));
    let attached = m.capturing.then(|| {
        crate::capture::attach(Arc::new(SessionTarget { fwd: fwd.clone(), clock: clock.clone() }));
        Attached
    });
    let stop = Arc::new(AtomicBool::new(false));
    let (commands, command_rx) = mpsc::channel();
    #[cfg(windows)]
    let ddc = match kvm::spawn(kvm::WindowsBackend) {
        Ok(pair) => Some(pair),
        Err(e) => { log(&format!("no DDC thread: {e}")); None }
    };
    #[cfg(not(windows))]
    let ddc: Option<(Sender<Cmd>, Receiver<Done>)> = None;
    let names: Vec<String> = fwd_cfg.peers.iter().map(|p| p.name.clone()).collect();
    let clock_thread = {
        let (fwd, clock, routes, displays, stop, capturing) = (fwd.clone(), clock.clone(), routes.clone(), m.displays.clone(), stop.clone(), m.capturing);
        let side = win_side(&cfg, Some(peer_monitors));
        let kvm_file = status::kvm_path(config);
        thread::Builder::new()
            .name("flowseam-clock".into())
            .spawn(move || clock_loop(&fwd, &clock, routes, &*displays, &stop, &names, &home_asks, capturing, side, kvm_file, command_rx, ddc))
            .map_err(|e| format!("cannot start the clock thread: {e}"))?
    };
    let status_thread = {
        let (path, board, stop) = (status::status_path(config), links.status_board(), stop.clone());
        thread::Builder::new()
            .name("flowseam-status".into())
            .spawn(move || status_loop(&path, &board, &stop))
            .map_err(|e| format!("cannot start the status thread: {e}"))?
    };
    Ok(Worker { links, fwd, clock, stop, threads: vec![clock_thread, status_thread], attached, commands })
}

/// Run the worker for `config` on this machine until it is asked to stop: the window closing its end of
/// stdin (or `max`, for a test) stops the worker the way `Worker::stop` does, so held keys are released and
/// monitors sent home before this returns `Ok`. Returns an error only if the clock thread stops on its own
/// (it panicked): a worker without its clock would stay linked but never hand control back, so the process
/// ends and its supervisor starts a new one.
pub fn run(config: &Path, m: Machine) -> Result<(), String> {
    run_until(config, m, None)
}

pub fn run_until(config: &Path, m: Machine, max: Option<Duration>) -> Result<(), String> {
    let read_commands = m.capturing;
    let worker = start(config, m)?;
    let (quit, asked) = mpsc::channel::<&'static str>();
    if read_commands {
        let (tx, quit) = (worker.commands.clone(), quit.clone());
        thread::spawn(move || {
            let why = pump_commands(std::io::stdin().lock(), &tx, wake_clocks);
            let _ = quit.send(why);
        });
    }
    if let Some(max) = max {
        let quit = quit.clone();
        thread::spawn(move || {
            thread::sleep(max);
            let _ = quit.send("max-secs reached");
        });
    }
    loop {
        match asked.recv_timeout(Duration::from_millis(500)) {
            Ok(why) => {
                log(&format!("{why}: stopping"));
                worker.stop();
                return Ok(());
            }
            Err(_) if worker.clock_finished() => return worker.wait(),
            Err(_) => {}
        }
    }
}

/// Feeds the window's stdin lines to the clock thread, waking it for each, until the stream ends. Bytes,
/// not `lines()`: a line that is not UTF-8 must not end the stream, since the end is the stop request.
fn pump_commands(mut input: impl std::io::BufRead, tx: &Sender<String>, wake: impl Fn()) -> &'static str {
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match input.read_until(b'\n', &mut buf) {
            Ok(0) => return "command stdin closed",
            Ok(_) => {
                if tx.send(String::from_utf8_lossy(&buf).into_owned()).is_err() {
                    return "the clock thread is gone";
                }
                wake();
            }
            Err(e) => {
                log(&format!("command stdin: {e}"));
                return "command stdin failed";
            }
        }
    }
}

/// status.toml for the window, every second while the clock thread is alive: the links' view of each peer
/// and who drives this PC (for the window's "back").
fn status_loop(path: &Path, board: &StatusBoard, stop: &AtomicBool) {
    let mut failing = flowseam_net::clipboard::Repeats::default();
    while !stop.load(Ordering::SeqCst) {
        let active = *ROUTER_HEARTBEAT.get_or_init(|| Mutex::new(None)).lock().unwrap_or_else(|e| e.into_inner());
        if active.is_none_or(|at| at.elapsed() <= Duration::from_secs(5)) {
            let driven_by = DRIVEN_BY.lock().unwrap_or_else(|e| e.into_inner()).clone();
            match status::write(path, &LinkStatus { written_ms: status::now_ms(), peers: board.peers(), driven_by }) {
                Ok(()) => failing.settle(&log),
                Err(e) => failing.failed("status write", e.to_string(), &log),
            }
        }
        thread::sleep(Duration::from_secs(1));
    }
}

/// What the Windows clock adds to the shared turn (`flowseam_net::node::clock_loop`): the window's "back",
/// the displays, the hooks' probes, and the cursor check for a PC that drives a peer.
struct WinClock<'a> {
    displays: &'a (dyn Fn() -> Result<Vec<Rect>, String> + Send + Sync),
    refreshed: Instant,
    layout: Option<Vec<Rect>>,
    fresh: Option<Result<Vec<Rect>, String>>,
    driving: bool,
    hold_unknown: bool,
    kvm: Kvm,
    commands: Receiver<String>,
    ddc: Option<(Sender<Cmd>, Receiver<Done>)>,
    /// The configured monitors, probe and inspect results, and the current test: what kvm.toml says for the window.
    side: KvmSide,
    kvm_file: PathBuf,
    kvm_written: Option<KvmStatus>,
    probed: Option<Instant>,
    /// The stop's return has run: `finish` and `Drop` both ask, the second finds nothing to do.
    restored: bool,
    clock: &'a Clock,
    home_asks: MonitorsHome,
    home_wanted: Vec<String>,
    #[cfg(windows)]
    capturing: bool,
    #[cfg(windows)]
    probe: ProbeRound,
    #[cfg(windows)]
    probe_judge: crate::capture::ProbeJudge,
    #[cfg(windows)]
    probe_unknown: bool,
}

impl WinClock<'_> {
    /// One line from the window, in the Mac worker's grammar (`flowseam_core::menu`).
    fn command<O: Outbox>(&mut self, line: &str, f: &mut Forwarder<InjectSink<Box<dyn Injector>>, O>) {
        let Some(cmd) = menu_command(line) else {
            log(&format!("unknown command from the window: {:?}", line.trim()));
            return;
        };
        let (peer, target) = match cmd {
            MenuCmd::Test { .. } | MenuCmd::Inspect(_) => {
                let local = matches!(f.mode(), Mode::Local);
                kvm_side::settings_command(cmd, local, &mut self.kvm, &mut self.side, self.ddc.as_ref().map(|d| &d.0), &mut |peer, way_back| f.switch_to(peer, way_back));
                return;
            }
            MenuCmd::Home => {
                ask_home();
                return;
            }
            MenuCmd::Return(peer) => {
                want_home(&mut self.home_wanted, peer);
                return;
            }
            MenuCmd::Input(peer) => (peer, None),
            MenuCmd::Switch { uuids, peer } => match kvm_ready(&self.side, peer, &uuids) {
                Ok(target) => (peer, Some(target)),
                Err(why) => {
                    log(&why);
                    self.kvm.last_error = Some(why);
                    return;
                }
            },
        };
        let ready = f.can_switch_to(peer);
        if ready != SwitchTo::Switched {
            log(&format!("menu switch to {peer} refused: {ready:?}"));
            self.kvm.last_error = Some(format!("cannot switch to {peer}: {ready:?}"));
            return;
        }
        let steps = self.kvm.request(peer, target, WayBack::Open);
        run_kvm(&mut self.kvm, steps, self.ddc.as_ref().map(|d| &d.0), f);
    }

    /// Sends every monitor home and waits for the return to land, once, for at most `KVM_STOP_LIMIT` in all.
    /// What is not confirmed by then stays in kvm.toml as out, and the window sends it home.
    fn restore_monitors(&mut self) {
        self.restore_monitors_within(kvm_side::KVM_STOP_LIMIT);
    }

    fn restore_monitors_within(&mut self, limit: Duration) {
        if std::mem::replace(&mut self.restored, true) {
            return;
        }
        for step in self.kvm.stop() {
            if let (Step::Ddc(cmd), Some((tx, _))) = (step, &self.ddc) {
                let _ = tx.send(cmd);
            }
        }
        kvm_side::finish_kvm(&mut self.kvm, self.ddc.as_ref().map(|d| &d.1), limit);
        write_kvm_status(&self.kvm, &self.side, Some(&self.kvm_file), &mut self.kvm_written);
    }
}

impl Drop for WinClock<'_> {
    fn drop(&mut self) { self.restore_monitors(); }
}

impl<O: Outbox> node::ClockHooks<InjectSink<Box<dyn Injector>>, O> for WinClock<'_> {
    fn start(&mut self) {
        route_heartbeat(Some(Instant::now()));
        // Monitors the previous run left out: its session is over, so they go home now. The answers arrive
        // on the DDC channel and are read in the next turn.
        for step in self.kvm.return_stranded() {
            if let (Step::Ddc(cmd), Some((tx, _))) = (step, &self.ddc) {
                let _ = tx.send(cmd);
            }
        }
        self.refreshed = Instant::now();
        self.layout = (self.displays)().ok();
        if let Some(d) = &self.layout {
            log(&format!("displays: {}", describe_layout(d)));
        }
    }

    fn woke(&mut self) {
        self.fresh = (self.refreshed.elapsed() >= DISPLAY_REFRESH).then(|| {
            self.refreshed = Instant::now();
            (self.displays)()
        });
    }

    fn after_tick(&mut self, f: &mut Forwarder<InjectSink<Box<dyn Injector>>, O>) {
        if matches!(f.mode(), Mode::RemoteOut { .. }) {
            match f.sink_mut().cursor_moved() {
                Ok(true) => {
                    log("the cursor moved here while this PC drives a peer: the hooks are not holding it (Windows took them out, or an elevated window in front gets the input). Coming home");
                    f.release_control();
                    #[cfg(windows)]
                    crate::hook::reinstall();
                }
                Ok(false) => {}
                Err(e) if !self.hold_unknown => {
                    log(&format!("cannot tell whether the input hooks still hold the cursor: {e}"));
                    self.hold_unknown = true;
                }
                Err(_) => {}
            }
        }
        #[cfg(windows)]
        if self.capturing {
            probe_round(f, &mut self.probe, &mut self.probe_judge, &mut self.probe_unknown);
        }
    }

    fn take_back(&mut self) -> bool {
        HOME_ASKED.swap(false, Ordering::SeqCst)
    }

    fn driven_changed(&mut self, now: &Option<String>) {
        *DRIVEN_BY.lock().unwrap_or_else(|e| e.into_inner()) = now.clone();
    }

    fn end_turn(&mut self, f: &mut Forwarder<InjectSink<Box<dyn Injector>>, O>) {
        route_heartbeat(Some(Instant::now()));
        let now_driving = matches!(f.mode(), Mode::RemoteOut { .. });
        if self.driving != now_driving {
            log(if now_driving { "this PC's keyboard and mouse now drive a peer" } else { "this PC's keyboard and mouse are back here" });
            self.driving = now_driving;
        }
        for line in self.commands.try_iter().collect::<Vec<_>>() {
            self.command(&line, f);
        }
        let answers: Vec<Done> = self.ddc.as_ref().map_or_else(Vec::new, |(_, rx)| rx.try_iter().collect());
        for done in answers {
            let done = match done {
                Done::Probe { uuid, check } => {
                    let key = (check.peer.clone(), uuid);
                    if self.side.checks.get(&key) != Some(&check) {
                        log(&format!("monitor check {} {check:?}", key.1));
                    }
                    self.side.checks.insert(key, check);
                    continue;
                }
                Done::Inspect(s) => {
                    log(&format!("monitor inspected {s:?}"));
                    self.side.inspect = Some(s);
                    continue;
                }
                other => other,
            };
            log(&format!("monitor {done:?}"));
            let steps = self.kvm.done(done);
            run_kvm(&mut self.kvm, steps, self.ddc.as_ref().map(|d| &d.0), f);
        }
        let driving = match f.mode() { Mode::RemoteOut { peer, .. } => Some(peer.clone()), _ => None };
        let steps = self.kvm.tick(driving.as_deref(), (self.clock)());
        run_kvm(&mut self.kvm, steps, self.ddc.as_ref().map(|d| &d.0), f);
        for peer in std::mem::take(&mut *self.home_asks.lock().unwrap_or_else(|e| e.into_inner())) {
            if !self.home_wanted.contains(&peer) { self.home_wanted.push(peer); }
        }
        while let Some(peer) = self.home_wanted.first().cloned() {
            let Some(steps) = self.kvm.bring_home(&peer, self.side.monitors_of(&peer)) else { break };
            self.home_wanted.remove(0);
            run_kvm(&mut self.kvm, steps, self.ddc.as_ref().map(|d| &d.0), f);
        }
        f.hold_for_switch(!matches!(self.kvm.phase(), Phase::Idle | Phase::Away { .. }), (self.clock)());
        // Only between switches: a probe's reads are bus traffic, and a switch must not queue behind one.
        if matches!(self.kvm.phase(), Phase::Idle) && self.probed.is_none_or(|t| t.elapsed() >= PROBE_EVERY) {
            self.probed = Some(Instant::now());
            refresh_edids(&mut self.side);
            probe_monitors(&self.side.monitors, self.ddc.as_ref().map(|d| &d.0), &mut self.side.checks);
        }
        write_kvm_status(&self.kvm, &self.side, Some(&self.kvm_file), &mut self.kvm_written);
        match self.fresh.take() {
            Some(Ok(d)) => {
                if self.layout.as_ref() != Some(&d) {
                    log(&format!("displays changed: {}", describe_layout(&d)));
                    self.layout = Some(d.clone());
                    let steps = self.kvm.stop();
                    run_kvm(&mut self.kvm, steps, self.ddc.as_ref().map(|d| &d.0), f);
                }
                f.sink_mut().set_displays(d.clone());
                f.set_displays(d);
            }
            Some(Err(e)) => log(&format!("display refresh failed: {e}")),
            None => {}
        }
    }

    fn finish(&mut self) {
        self.restore_monitors();
        *DRIVEN_BY.lock().unwrap_or_else(|e| e.into_inner()) = None;
        route_heartbeat(None);
    }
}

/// The forwarder's clock: ticks it (15 ms while a peer drives this PC or a dwell runs, else once a second,
/// sooner when the links wake it), and while this PC drives a peer checks that the hooks still hold its input.
/// Also the window's "back", the displays, and the log lines and status for who drives whom.
#[allow(clippy::too_many_arguments)]
fn clock_loop<O: Outbox>(fwd: &WinFwd<O>, clock: &Clock, routes: O, displays: &(dyn Fn() -> Result<Vec<Rect>, String> + Send + Sync), stop: &AtomicBool, peers: &[String], home_asks: &MonitorsHome, capturing: bool, side: KvmSide, kvm_file: PathBuf, commands: Receiver<String>, ddc: Option<(Sender<Cmd>, Receiver<Done>)>) {
    #[cfg(not(windows))]
    let _ = capturing;
    let mut hook = WinClock {
        displays,
        refreshed: Instant::now(),
        layout: None,
        fresh: None,
        driving: false,
        hold_unknown: false,
        kvm: kvm_start(Some(&kvm_file)),
        commands,
        ddc,
        side,
        kvm_file,
        kvm_written: None,
        probed: None,
        restored: false,
        clock,
        home_asks: home_asks.clone(),
        home_wanted: Vec::new(),
        #[cfg(windows)]
        capturing,
        #[cfg(windows)]
        probe: None,
        #[cfg(windows)]
        probe_judge: Default::default(),
        #[cfg(windows)]
        probe_unknown: false,
    };
    // The platform hook owns requests for this PC's DDC monitors; the shared loop owns none.
    let shared_home_asks = MonitorsHome::default();
    node::clock_loop(fwd, routes, &node::ClockEnv { clock, stop, peers, home_asks: &shared_home_asks, names: NAMES }, &mut hook);
}

/// The worker's monitor state: ids that are only a hash of the adapter change with the cabling, so a switch
/// for one is refused here, whatever the window offered.
fn win_side(cfg: &Config, peer_monitors: Option<PeerReports>) -> KvmSide {
    KvmSide { openable: Some(crate::ddc::openable_id), ..KvmSide::new(cfg, peer_monitors) }
}

fn run_kvm<O: Outbox>(kvm: &mut Kvm, steps: Vec<Step>, ddc: Option<&Sender<Cmd>>, f: &mut Forwarder<InjectSink<Box<dyn Injector>>, O>) {
    kvm_side::run_kvm(kvm, steps, ddc, &mut |peer, way_back| f.switch_to(peer, way_back));
}

/// The EDID identity of each monitor here, by id, for what a peer reports it sees.
#[cfg(windows)]
fn refresh_edids(side: &mut KvmSide) {
    match crate::ddc::win::online() {
        Ok(list) => side.edids = list.into_iter().filter_map(|m| m.edid.map(|e| (m.id, flowseam_core::proto::MonitorId { vendor: e.vendor as u16, product: e.product as u16, serial: e.serial }))).collect(),
        Err(e) => log(&format!("monitor EDID: {e}")),
    }
}

#[cfg(not(windows))]
fn refresh_edids(_: &mut KvmSide) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stdin_lines_reach_the_clock_and_a_non_utf8_line_does_not_end_the_stream() {
        let (tx, rx) = mpsc::channel();
        let woke = std::cell::Cell::new(0);
        let input: &[u8] = b"home\nswitch \xff,\xfe pc\ninput pc";
        let why = pump_commands(input, &tx, || woke.set(woke.get() + 1));
        assert_eq!(why, "command stdin closed", "EOF is the stop request");
        let got: Vec<String> = rx.try_iter().collect();
        assert_eq!(got.len(), 3, "{got:?}");
        assert_eq!(got[0], "home\n");
        assert_eq!(got[2], "input pc", "the last line without a newline still arrives");
        assert_eq!(woke.get(), 3);
        drop(rx);
        assert_eq!(pump_commands(&b"x\n"[..], &tx, || {}), "the clock thread is gone");
    }

    /// The Mac's window lines drive the Windows worker: input only, monitors, inspect, a test, and home.
    #[test]
    fn window_lines_drive_the_kvm_policy_and_the_ddc_thread() {
        use flowseam_core::config::DisplaySwitch;
        use flowseam_core::status::{CheckState, MonitorCheck};
        let (out_tx, _from_fwd) = mpsc::channel();
        let cfg_fwd = FwdConfig {
            peers: vec![PeerSpec { name: "pc".into(), edge: proto::Edge::Left, keys: KeyMap::Position, gestures: Default::default() }],
            policy: SwitchPolicy::from_config(&Config::default().switch).unwrap(),
            boundary_px: Config::default().switch.boundary_px,
        };
        let screen = vec![Rect { x: 0, y: 0, w: 1920, h: 1080 }];
        let sink = InjectSink::new(Box::new(crate::inject::PrintInjector::new(proto::Point { x: 10, y: 10 })) as Box<dyn Injector>, screen.clone(), |_| {});
        let mut f = Forwarder::new(cfg_fwd, screen, sink, crate::inject::ChanOut(out_tx));
        f.link_up("pc");
        f.peer_speaks("pc", proto::PROTO_VERSION);
        let mut config = Config::default();
        config.peers.push(Peer {
            name: "pc".into(), addr: "192.0.2.1:24800".into(), edge: Edge::Left, fingerprint: "ab".repeat(32), keys: Default::default(), gestures: Default::default(),
            displays: vec![DisplaySwitch { uuid: "M1".into(), input: 15, home: 18, verified: true }],
        });
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (ddc_tx, ddc_rx) = mpsc::channel();
        let (_done_tx, done_rx) = mpsc::channel();
        let clock: Clock = Arc::new(|| 0);
        let path = std::env::temp_dir().join(format!("flowseam-win-cmd-{}-{}.toml", std::process::id(), status::now_ms()));
        let screens = || Ok(vec![Rect { x: 0, y: 0, w: 1920, h: 1080 }]);
        let mut hook = WinClock {
            displays: &screens, refreshed: Instant::now(), layout: None, fresh: None, driving: false, hold_unknown: false,
            kvm: Kvm::new(), commands: cmd_rx, ddc: Some((ddc_tx, done_rx)), side: win_side(&config, None), kvm_file: path.clone(),
            kvm_written: None, probed: None, restored: false, clock: &clock, home_asks: MonitorsHome::default(), home_wanted: Vec::new(),
            #[cfg(windows)]
            capturing: false,
            #[cfg(windows)]
            probe: None,
            #[cfg(windows)]
            probe_judge: Default::default(),
            #[cfg(windows)]
            probe_unknown: false,
        };
        drop(cmd_tx);
        hook.command("inspect M1\n", &mut f);
        assert_eq!(ddc_rx.try_recv(), Ok(Cmd::Inspect { uuid: "M1".into() }));
        hook.command("switch M1 pc\n", &mut f);
        assert!(ddc_rx.try_recv().is_err(), "an unprobed monitor does not switch");
        assert!(hook.kvm.last_error.as_deref().is_some_and(|e| e.contains("NotChecked")), "{:?}", hook.kvm.last_error);
        hook.side.checks.insert(("pc".into(), "M1".into()), MonitorCheck { peer: "pc".into(), state: CheckState::Ok, reason: None });
        hook.command("switch M1 pc\n", &mut f);
        assert!(matches!(ddc_rx.try_recv(), Ok(Cmd::Away { ref peer, .. }) if peer == "pc"));
        hook.command("return pc\n", &mut f);
        assert_eq!(hook.home_wanted, ["pc"]);
        hook.command("bogus\n", &mut f);
        hook.command("home\n", &mut f);
        assert!(HOME_ASKED.swap(false, Ordering::SeqCst), "home reaches the existing hand-back");
        let _ = std::fs::remove_file(&path);
    }

    /// The stop's monitor return runs once and gives up at its limit, however many times `finish` and `Drop` ask.
    #[test]
    fn the_stop_sends_monitors_home_once_and_waits_only_up_to_its_limit() {
        use flowseam_core::config::DisplaySwitch;
        let mut config = Config::default();
        config.peers.push(Peer {
            name: "pc".into(), addr: "192.0.2.1:24800".into(), edge: Edge::Left, fingerprint: "ab".repeat(32), keys: Default::default(), gestures: Default::default(),
            displays: vec![DisplaySwitch { uuid: "M1".into(), input: 15, home: 18, verified: true }, DisplaySwitch { uuid: "unstable:1".into(), input: 15, home: 18, verified: true }],
        });
        let (_cmd_tx, cmd_rx) = mpsc::channel();
        let (ddc_tx, ddc_rx) = mpsc::channel();
        let (_done_tx, done_rx) = mpsc::channel();
        let clock: Clock = Arc::new(|| 0);
        let path = std::env::temp_dir().join(format!("flowseam-win-stop-{}-{}.toml", std::process::id(), status::now_ms()));
        let screens = || Ok(vec![Rect { x: 0, y: 0, w: 1920, h: 1080 }]);
        let mut hook = WinClock {
            displays: &screens, refreshed: Instant::now(), layout: None, fresh: None, driving: false, hold_unknown: false,
            kvm: Kvm::new(), commands: cmd_rx, ddc: Some((ddc_tx, done_rx)), side: win_side(&config, None), kvm_file: path.clone(),
            kvm_written: None, probed: None, restored: false, clock: &clock, home_asks: MonitorsHome::default(), home_wanted: Vec::new(),
            #[cfg(windows)]
            capturing: false,
            #[cfg(windows)]
            probe: None,
            #[cfg(windows)]
            probe_judge: Default::default(),
            #[cfg(windows)]
            probe_unknown: false,
        };
        let steps = hook.kvm.request("pc", Some(kvm_ready_target("M1")), WayBack::Open);
        assert!(matches!(steps.as_slice(), [Step::Ddc(Cmd::Away { .. })]));
        hook.kvm.done(Done::Away { peer: "pc".into(), result: Ok(()), out: vec![] });
        let t0 = Instant::now();
        hook.restore_monitors_within(Duration::from_millis(200));
        assert!(t0.elapsed() < Duration::from_secs(3), "gives up at the limit, not at 30 s");
        assert!(matches!(ddc_rx.try_recv(), Ok(Cmd::Home { .. })), "the monitor is sent home");
        let t1 = Instant::now();
        hook.restore_monitors_within(Duration::from_secs(30));
        drop(hook);
        assert!(t1.elapsed() < Duration::from_secs(1), "a second ask (Drop, finish) does not wait again");
        assert!(ddc_rx.try_recv().is_err(), "and does not send the return twice");
        assert!(status::read_kvm(&path).is_ok_and(|k| !k.out.is_empty()), "kvm.toml still names the monitor as out, for the window to send home");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_unstable_monitor_id_is_refused_by_the_worker() {
        use flowseam_core::config::DisplaySwitch;
        let mut config = Config::default();
        config.peers.push(Peer {
            name: "pc".into(), addr: "192.0.2.1:24800".into(), edge: Edge::Left, fingerprint: "ab".repeat(32), keys: Default::default(), gestures: Default::default(),
            displays: vec![DisplaySwitch { uuid: "unstable:00AB:0".into(), input: 15, home: 18, verified: true }],
        });
        let mut side = win_side(&config, None);
        side.checks.insert(("pc".into(), "unstable:00AB:0".into()), flowseam_core::status::MonitorCheck { peer: "pc".into(), state: flowseam_core::status::CheckState::Ok, reason: None });
        assert!(kvm_ready(&side, "pc", &["unstable:00AB:0"]).unwrap_err().contains("reliably"));
    }

    fn kvm_ready_target(uuid: &str) -> kvm_core::Target {
        kvm_core::Target::one(kvm_core::Monitor { uuid: uuid.into(), input: 15, home: 18 })
    }

    #[test]
    fn the_layout_reads_size_then_position_in_windows_order() {
        let d = [Rect { x: -2560, y: 0, w: 2560, h: 1440 }, Rect { x: 0, y: 0, w: 1920, h: 1080 }];
        assert_eq!(describe_layout(&d), "2560x1440 at (-2560,0), 1920x1080 at (0,0)");
        assert_eq!(describe_layout(&[]), "");
    }

    /// Every peer gets its own edge and keys; "mac" keys are for a Mac keyboard, so this PC sends by position.
    #[test]
    fn each_peer_keeps_its_edge_and_keys_and_mac_keys_become_position() {
        let peer = |name: &str, edge, keys| Peer { name: name.into(), addr: "192.0.2.1:24800".into(), edge, fingerprint: "ab".repeat(32), keys, gestures: Default::default(), displays: Vec::new() };
        let cfg = Config { peers: vec![peer("mini", Edge::Left, KeyMap::Mac), peer("pc", Edge::Right, KeyMap::None), peer("mbp", Edge::Top, KeyMap::Position)], ..Config::default() };
        let f = fwd_config_all(&cfg).unwrap();
        assert_eq!(
            f.peers.iter().map(|p| (p.name.as_str(), p.edge, p.keys)).collect::<Vec<_>>(),
            [("mini", proto::Edge::Left, KeyMap::Position), ("pc", proto::Edge::Right, KeyMap::None), ("mbp", proto::Edge::Top, KeyMap::Position)]
        );
    }

    /// "back" asks each linked peer for its monitors, if it speaks protocol 6: an older one would end the link
    /// on a message it cannot read, and a peer with no link is not asked at all.
    #[test]
    fn the_monitor_request_goes_only_to_a_linked_peer_that_can_read_it() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut out = crate::inject::ChanOut(tx);
        assert_eq!(home_request_to("mbp", None, &mut out), None, "no link, nothing to log");
        assert_eq!(home_request_to("mini", Some(5), &mut out).as_deref(), Some("mini speaks protocol 5: it cannot be asked for its monitors (update it)"));
        assert!(rx.try_recv().is_err(), "nothing sent to a peer that cannot read it");
        assert_eq!(home_request_to("mini", Some(6), &mut out).as_deref(), Some("asked mini to bring its monitors home"));
        assert_eq!(rx.try_recv(), Ok(Msg::MonitorsHome));
    }

    mod routing {
        use super::super::*;
        use crate::inject::WinButton;
        use crate::keymap::Scan;
        use flowseam_core::proto::{ModifierSet, Point};
        use std::sync::mpsc::Receiver;
        use std::sync::{Arc, Mutex};

        #[derive(Debug, Clone, PartialEq, Eq)]
        enum Op {
            Key(u16, bool),
            Move(Point),
            Button(WinButton, bool),
        }

        #[derive(Clone)]
        struct Rec(Arc<Mutex<(Vec<Op>, Point)>>);
        impl Injector for Rec {
            fn key(&mut self, scan: Scan, down: bool) -> Result<(), String> {
                self.0.lock().unwrap().0.push(Op::Key(scan.code, down));
                Ok(())
            }
            fn move_to(&mut self, p: Point) -> Result<(), String> {
                let mut g = self.0.lock().unwrap();
                g.1 = p;
                g.0.push(Op::Move(p));
                Ok(())
            }
            fn cursor(&mut self) -> Result<Point, String> {
                Ok(self.0.lock().unwrap().1)
            }
            fn button(&mut self, b: WinButton, down: bool) -> Result<(), String> {
                self.0.lock().unwrap().0.push(Op::Button(b, down));
                Ok(())
            }
            fn wheel(&mut self, _v: i32, _h: i32) -> Result<(), String> {
                Ok(())
            }
            fn num_lock(&mut self) -> Result<bool, String> {
                Ok(true)
            }
        }

        fn screen() -> Result<Vec<Rect>, String> {
            Ok(vec![Rect { x: 0, y: 0, w: 1920, h: 1080 }])
        }

        /// One peer, "mac", on the left, fed the way the links feed the worker: its messages straight into the
        /// forwarder (drag messages into the hub's queue), and ticked by the worker's own clock loop. What the
        /// forwarder sends lands on `from_fwd`.
        struct Rig {
            rec: Rec,
            fwd: WinFwd<crate::inject::ChanOut>,
            clock: Clock,
            from_fwd: Receiver<Msg>,
            inbox: Option<flowseam_net::files::DragInbox>,
            stop: Arc<AtomicBool>,
            clock_thread: thread::JoinHandle<()>,
        }

        impl Rig {
            fn send(&self, m: Msg) {
                match m {
                    Msg::DragSoon { .. } | Msg::DragBegin { .. } | Msg::FileChunk { .. } | Msg::DragAck { .. } | Msg::DragCancel { .. } => {
                        self.inbox.as_ref().expect("file drags are on").deliver(m)
                    }
                    m => self.fwd.lock().unwrap().on_msg("mac", m, (self.clock)()),
                }
                wake_clocks();
            }

            /// The link ends, as the links report it, and the clock loop stops.
            fn end(self) -> Rec {
                self.fwd.lock().unwrap().link_down("mac");
                self.stop.store(true, Ordering::SeqCst);
                wake_clocks();
                self.clock_thread.join().unwrap();
                self.rec
            }
        }

        fn rig() -> Rig {
            rig_full(None, None, Point { x: 960, y: 540 }, false)
        }

        /// `dirs`: (staging, destination) for file drags, as `drag_dirs` gives them.
        fn rig_with(dirs: Option<(PathBuf, PathBuf)>) -> Rig {
            rig_full(dirs, None, Point { x: 960, y: 540 }, false)
        }

        fn rig_with_source(dirs: Option<(PathBuf, PathBuf)>, source: Option<Box<dyn DragSource>>) -> Rig {
            rig_full(dirs, source, Point { x: 960, y: 540 }, false)
        }

        fn rig_full(dirs: Option<(PathBuf, PathBuf)>, source: Option<Box<dyn DragSource>>, at: Point, parks: bool) -> Rig {
            let rec = Rec(Arc::new(Mutex::new((vec![], at))));
            let (out_tx, from_fwd) = std::sync::mpsc::channel();
            let cfg = FwdConfig {
                peers: vec![PeerSpec { name: "mac".into(), edge: proto::Edge::Left, keys: KeyMap::Position, gestures: Default::default() }],
                policy: SwitchPolicy::from_config(&Config::default().switch).unwrap(),
                boundary_px: Config::default().switch.boundary_px,
            };
            let mut sink = InjectSink::new(Box::new(rec.clone()) as Box<dyn Injector>, screen().unwrap(), |_| {});
            if parks {
                sink = sink.with_grab(Box::new(Parks(rec.clone())));
            }
            let mut inbox = None;
            if let Some((staging, dest)) = dirs {
                let hub = Arc::new(Mutex::new(DragHub::new(source)));
                let tx = out_tx.clone();
                let send: flowseam_net::files::SendFn = Arc::new(move |m| tx.send(m).is_ok());
                hub.lock().unwrap().attach("mac", Arc::new(Mutex::new(flowseam_net::files::DragPump::new(staging, dest))), send);
                inbox = hub.lock().unwrap().inbox("mac");
                sink = sink.with_drags(hub);
            }
            let fwd = Arc::new(Mutex::new(Forwarder::new(cfg, screen().unwrap(), sink, crate::inject::ChanOut(out_tx.clone()))));
            {
                let mut f = fwd.lock().unwrap();
                f.peer_speaks("mac", proto::PROTO_VERSION);
                f.link_up("mac");
            }
            let t0 = Instant::now();
            let clock: Clock = Arc::new(move || t0.elapsed().as_millis() as u64);
            let stop = Arc::new(AtomicBool::new(false));
            let clock_thread = {
                let (fwd, clock, stop) = (fwd.clone(), clock.clone(), stop.clone());
                thread::spawn(move || clock_loop(&fwd, &clock, crate::inject::ChanOut(out_tx), &screen, &stop, &["mac".to_string()], &MonitorsHome::default(), false, KvmSide::default(), std::env::temp_dir().join(format!("flowseam-win-test-{}.kvm.toml", std::process::id())), mpsc::channel().1, None))
            };
            Rig { rec, fwd, clock, from_fwd, inbox, stop, clock_thread }
        }

        /// Parks the way the real one does: the recorded cursor is where it is put.
        struct Parks(Rec);
        impl crate::inject::Grab for Parks {
            fn park(&mut self, at: Point) -> Result<(), String> {
                self.0 .0.lock().unwrap().1 = at;
                Ok(())
            }
            fn hide(&mut self) -> Result<(), String> {
                Ok(())
            }
            fn show(&mut self) -> Result<(), String> {
                Ok(())
            }
        }

        fn next_input(rx: &Receiver<Msg>) -> Msg {
            loop {
                match rx.recv_timeout(Duration::from_secs(2)).expect("the forwarder sent nothing") {
                    Msg::ModSync { .. } => continue,
                    m => return m,
                }
            }
        }

        fn wait(secs: u64, f: &dyn Fn() -> bool) -> bool {
            let end = Instant::now() + Duration::from_secs(secs);
            while Instant::now() < end {
                if f() {
                    return true;
                }
                thread::sleep(Duration::from_millis(20));
            }
            false
        }

        /// A loopback-only machine: the injector records, file drags and the clipboard are off.
        fn pc(rec: &Rec) -> Machine {
            Machine {
                injector: Box::new(rec.clone()),
                displays: Arc::new(screen),
                clipboard: None,
                link: LinkHost {
                    interfaces: Vec::new,
                    addrs: || Some(vec![std::net::IpAddr::from([127, 0, 0, 1])]),
                    free_bytes: |_| Ok(u64::MAX),
                    wake: wake_clocks,
                },
                drags: None,
                capturing: false,
            }
        }

        /// A config in `dir` that pairs with `peer`; the identity is made there first.
        fn paired(dir: &Path, peer: &str, fp: &[u8; 32], addr: &str, edge: Edge) -> PathBuf {
            let config = dir.join("config.toml");
            let cfg = Config {
                listen: "127.0.0.1:0".into(),
                peers: vec![Peer { name: peer.into(), addr: addr.into(), edge, fingerprint: to_hex(fp), keys: KeyMap::None, gestures: Default::default(), displays: Vec::new() }],
                ..Config::default()
            };
            cfg.save(&config).unwrap();
            config
        }

        /// Two PCs, each running this worker, link over pinned TLS on loopback. One drives the other from its
        /// edge: Enter, then a key that lands on the other's injector as itself. Stopping the other's links
        /// brings the driver home. Everything a PC driving a PC needs short of a second PC's real input.
        #[test]
        fn a_pc_drives_another_pc_running_this_worker() {
            use flowseam_core::backend::CapturedEvent;
            use flowseam_core::proto::Point;
            let root = std::env::temp_dir().join(format!("flowseam-win-pc2pc-{}-{}", std::process::id(), status::now_ms()));
            let (a_dir, b_dir) = (root.join("a"), root.join("b"));
            let a_fp = Identity::load_or_create(&a_dir.join("identity")).unwrap().fingerprint();
            let b_fp = Identity::load_or_create(&b_dir.join("identity")).unwrap().fingerprint();
            let (a_rec, b_rec) = (Rec(Arc::new(Mutex::new((vec![], Point { x: 960, y: 540 })))), Rec(Arc::new(Mutex::new((vec![], Point { x: 960, y: 540 })))));
            let b = start(&paired(&b_dir, "a", &a_fp, "127.0.0.1:1", Edge::Left), pc(&b_rec)).unwrap();
            let a_config = paired(&a_dir, "b", &b_fp, &b.links.listening().to_string(), Edge::Right);
            let a = start(&a_config, pc(&a_rec)).unwrap();
            assert!(wait(8, &|| a.links.live() == 1 && b.links.live() == 1), "the PCs never linked");
            assert!(
                wait(5, &|| status::read(&status::status_path(&a_config)).is_ok_and(|s| s.peers.iter().any(|p| p.name == "b" && p.up))),
                "a's status.toml never showed b up, so a's window would show it down"
            );
            let edge = Point { x: 1919, y: 540 };
            assert!(
                wait(5, &|| {
                    let mut f = a.fwd.lock().unwrap();
                    f.on_cursor(edge, (a.clock)());
                    matches!(f.mode(), Mode::RemoteOut { .. })
                }),
                "a never switched to b"
            );
            assert!(wait(5, &|| matches!(b.fwd.lock().unwrap().mode(), Mode::RemoteIn { peer, .. } if peer == "a")), "b never took a's Enter");
            let disposition = a.fwd.lock().unwrap().on_captured(CapturedEvent::Key { hid: 0x04, down: true }, (a.clock)());
            assert_eq!(disposition, flowseam_core::backend::Disposition::Swallow, "a's key goes to b, not to a");
            assert!(wait(5, &|| b_rec.0.lock().unwrap().0.contains(&Op::Key(0x1E, true))), "the A key never reached b's injector: {:?}", b_rec.0.lock().unwrap().0);
            assert!(!a_rec.0.lock().unwrap().0.iter().any(|op| matches!(op, Op::Key(..))), "nothing typed on a");
            b.stop();
            assert!(wait(8, &|| matches!(a.fwd.lock().unwrap().mode(), Mode::Local)), "a stayed on b after b went away");
            assert!(
                wait(5, &|| status::read(&status::status_path(&a_config)).is_ok_and(|s| s.peers.iter().any(|p| p.name == "b" && !p.up))),
                "a's status.toml still shows b up after b went away"
            );
            a.stop();
            let _ = std::fs::remove_dir_all(&root);
        }

        /// The other way round: a PC driven by another lets go of whatever the driver held down when the
        /// driver goes away, and is its own again.
        #[test]
        fn a_pc_driven_by_another_lets_go_when_the_driver_goes() {
            use flowseam_core::backend::CapturedEvent;
            let root = std::env::temp_dir().join(format!("flowseam-win-pcgone-{}-{}", std::process::id(), status::now_ms()));
            let (a_dir, b_dir) = (root.join("a"), root.join("b"));
            let a_fp = Identity::load_or_create(&a_dir.join("identity")).unwrap().fingerprint();
            let b_fp = Identity::load_or_create(&b_dir.join("identity")).unwrap().fingerprint();
            let (a_rec, b_rec) = (Rec(Arc::new(Mutex::new((vec![], Point { x: 960, y: 540 })))), Rec(Arc::new(Mutex::new((vec![], Point { x: 960, y: 540 })))));
            let b = start(&paired(&b_dir, "a", &a_fp, "127.0.0.1:1", Edge::Left), pc(&b_rec)).unwrap();
            let a = start(&paired(&a_dir, "b", &b_fp, &b.links.listening().to_string(), Edge::Right), pc(&a_rec)).unwrap();
            assert!(wait(8, &|| a.links.live() == 1 && b.links.live() == 1), "the PCs never linked");
            assert!(wait(5, &|| {
                let mut f = a.fwd.lock().unwrap();
                f.on_cursor(Point { x: 1919, y: 540 }, (a.clock)());
                matches!(f.mode(), Mode::RemoteOut { .. })
            }), "a never switched to b");
            a.fwd.lock().unwrap().on_captured(CapturedEvent::Key { hid: 0xE1, down: true }, (a.clock)());
            assert!(wait(5, &|| b_rec.0.lock().unwrap().0.contains(&Op::Key(0x2A, true))), "Shift never went down on b");
            a.stop();
            assert!(wait(8, &|| b_rec.0.lock().unwrap().0.contains(&Op::Key(0x2A, false))), "b kept Shift held after a went away: {:?}", b_rec.0.lock().unwrap().0);
            assert!(wait(5, &|| matches!(b.fwd.lock().unwrap().mode(), Mode::Local)), "b still thinks a drives it");
            b.stop();
            let _ = std::fs::remove_dir_all(&root);
        }

        /// A worker stopped while it drives a peer ends that session: this PC's forwarder is local again and
        /// the peer is no longer driven, so no key stays held on either side.
        #[test]
        fn stopping_a_driving_worker_ends_its_session_on_both_sides() {
            let root = std::env::temp_dir().join(format!("flowseam-win-stopdrive-{}-{}", std::process::id(), status::now_ms()));
            let (a_dir, b_dir) = (root.join("a"), root.join("b"));
            let a_fp = Identity::load_or_create(&a_dir.join("identity")).unwrap().fingerprint();
            let b_fp = Identity::load_or_create(&b_dir.join("identity")).unwrap().fingerprint();
            let rec = || Rec(Arc::new(Mutex::new((vec![], Point { x: 960, y: 540 }))));
            let a = start(&paired(&a_dir, "b", &b_fp, "127.0.0.1:1", Edge::Right), pc(&rec())).unwrap();
            let b = start(&paired(&b_dir, "a", &a_fp, &a.links.listening().to_string(), Edge::Left), pc(&rec())).unwrap();
            assert!(wait(8, &|| a.links.live() == 1 && b.links.live() == 1), "the link did not come up");
            assert_eq!(a.fwd.lock().unwrap().switch_to("b", WayBack::Open), SwitchTo::Switched);
            assert!(wait(8, &|| matches!(b.fwd.lock().unwrap().mode(), Mode::RemoteIn { .. })), "b was never driven");
            let (a_fwd, b_fwd) = (a.fwd.clone(), b.fwd.clone());
            a.stop();
            assert!(matches!(a_fwd.lock().unwrap().mode(), Mode::Local), "a still drives after its stop");
            assert!(wait(8, &|| matches!(b_fwd.lock().unwrap().mode(), Mode::Local)), "b is still driven after a stopped");
            b.stop();
            let _ = std::fs::remove_dir_all(&root);
        }

        /// The worker's process waits on its clock thread: when that thread ends, `run` returns an error (and
        /// the process exits for its supervisor to restart it) instead of staying linked with no clock.
        #[test]
        fn a_worker_whose_clock_stops_does_not_carry_on_without_it() {
            let root = std::env::temp_dir().join(format!("flowseam-win-noclock-{}-{}", std::process::id(), status::now_ms()));
            let fp = Identity::load_or_create(&root.join("peer").join("identity")).unwrap().fingerprint();
            let w = start(&paired(&root, "peer", &fp, "127.0.0.1:1", Edge::Left), pc(&Rec(Arc::new(Mutex::new((vec![], Point { x: 960, y: 540 })))))).unwrap();
            let stop = w.stop.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            thread::spawn(move || {
                let _ = tx.send(w.wait());
            });
            assert!(rx.recv_timeout(Duration::from_millis(1500)).is_err(), "wait returned while the clock runs");
            stop.store(true, Ordering::SeqCst);
            wake_clocks();
            assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Ok(Err("the clock thread stopped".to_string())));
            let _ = std::fs::remove_dir_all(&root);
        }

        /// The bug this replaces the test of: a link that ended while the hooks held the forwarder left this PC
        /// taking no new connection until Flowseam restarted. Here the driver captures its input (the hooks'
        /// target is attached) and its peer goes away and comes back: the peer is taken back each time.
        #[test]
        fn a_pc_with_the_hooks_in_takes_a_peer_back_after_its_link_ends() {
            let _alone = crate::capture::TARGET_TESTS.lock().unwrap_or_else(|e| e.into_inner());
            let root = std::env::temp_dir().join(format!("flowseam-win-relink-{}-{}", std::process::id(), status::now_ms()));
            let (a_dir, b_dir) = (root.join("a"), root.join("b"));
            let a_fp = Identity::load_or_create(&a_dir.join("identity")).unwrap().fingerprint();
            let b_fp = Identity::load_or_create(&b_dir.join("identity")).unwrap().fingerprint();
            let rec = || Rec(Arc::new(Mutex::new((vec![], Point { x: 960, y: 540 }))));
            let a = start(&paired(&a_dir, "b", &b_fp, "127.0.0.1:1", Edge::Right), Machine { capturing: true, ..pc(&rec()) }).unwrap();
            assert!(crate::capture::target().is_some(), "the hooks feed a's forwarder");
            let b_config = paired(&b_dir, "a", &a_fp, &a.links.listening().to_string(), Edge::Left);
            for round in 0..3 {
                let b = start(&b_config, pc(&rec())).unwrap();
                assert!(wait(8, &|| a.links.live() == 1), "round {round}: a did not take b's link");
                b.stop();
                assert!(wait(8, &|| a.links.live() == 0), "round {round}: a never saw b's link end");
            }
            assert!(crate::capture::target().is_some(), "the hooks still feed a's forwarder");
            a.stop();
            assert!(crate::capture::target().is_none(), "a stopped worker lets go of the hooks");
            let _ = std::fs::remove_dir_all(&root);
        }

        /// This PC drives the Mac, wired as in production: the hook thread's target and the router share the
        /// forwarder, on one clock. Once the handshake names the protocol, resting at the left edge for the
        /// dwell sends Enter; a move and a key go over (Alt lands on Command, by position) and stay off this PC;
        /// the Mac's Leave brings the cursor home, where the next move is this PC's again.
        #[test]
        fn this_pc_drives_the_peer_from_its_edge_and_comes_home() {
            use crate::capture::Target;
            use flowseam_core::backend::{CapturedEvent, Disposition};
            let r = rig_full(None, None, Point { x: 0, y: 540 }, true);
            let (rec, from_fwd) = (r.rec.clone(), &r.from_fwd);
            let t = SessionTarget { fwd: r.fwd.clone(), clock: r.clock.clone() };

            // The hook reports a move past the left edge: the dwell starts on the clamped point.
            t.cursor(Point { x: -3, y: 540 });
            assert!(t.needs_tick(), "the dwell needs the clock while the mouse rests");
            assert!(!t.grabbed());
            thread::sleep(Duration::from_millis(250));
            t.tick();
            assert!(t.grabbed(), "the dwell handed input to the Mac");
            let seq = match next_input(from_fwd) {
                Msg::Enter { seq, edge, .. } => {
                    assert_eq!(edge, proto::Edge::Right, "the cursor appears at the Mac's right edge");
                    seq
                }
                m => panic!("expected Enter, got {m:?}"),
            };

            assert_eq!(t.event(CapturedEvent::Motion { dx: -7, dy: 2 }), Disposition::Swallow);
            assert_eq!(next_input(from_fwd), Msg::MotionRel { seq, dx: -7, dy: 2 });
            assert_eq!(t.event(CapturedEvent::Key { hid: 0xE2, down: true }), Disposition::Swallow, "Alt");
            assert_eq!(next_input(from_fwd), Msg::Key { seq, hid: 0xE3, down: true, repeat: false }, "Command on the Mac");

            r.send(Msg::Leave { seq, edge: proto::Edge::Left, frac: 0x8000 });
            let home = (0..100).any(|_| {
                thread::sleep(Duration::from_millis(10));
                !t.grabbed()
            });
            assert!(home, "the Mac's Leave did not bring the cursor home");
            assert!(rec.0.lock().unwrap().0.iter().any(|op| matches!(op, Op::Move(p) if p.x < 100)), "the cursor lands at the left edge");
            assert_eq!(t.event(CapturedEvent::Motion { dx: 5, dy: 0 }), Disposition::Pass, "a move here is this PC's again");
            r.end();
        }

        /// A Mac drags a file across and lets go here: the file is in Downloads once the button comes up, and
        /// not before. Each chunk is acknowledged to the sender.
        #[test]
        fn a_file_dragged_here_lands_in_downloads_on_release() {
            let root = std::env::temp_dir().join(format!("flowseam-win-drag-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let (staging, dest) = (root.join("staging"), root.join("Downloads"));
            std::fs::create_dir_all(&dest).unwrap();
            let r = rig_with(Some((staging, dest.clone())));
            r.send(Msg::DragSoon { drag: 1 });
            r.send(Msg::DragBegin { drag: 1, files: vec![proto::FileMeta { name: "報告.pdf".into(), size: 5 }] });
            r.send(Msg::FileChunk { drag: 1, file: 0, data: b"%PDF-".to_vec() });
            r.send(enter(1));
            let ack = r.from_fwd.recv_timeout(Duration::from_secs(2)).expect("ack");
            assert_eq!(ack, Msg::DragAck { drag: 1, bytes: 5 });
            assert!(std::fs::read_dir(&dest).unwrap().next().is_none(), "nothing before the drop");
            r.send(Msg::Button { seq: 1, button: 0, down: false });
            let deadline = Instant::now() + Duration::from_secs(2);
            // The name is claimed as an empty file before the staged one is renamed onto it: wait for the content.
            while Instant::now() < deadline && std::fs::read(dest.join("報告.pdf")).ok().as_deref() != Some(&b"%PDF-"[..]) {
                thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(std::fs::read(dest.join("報告.pdf")).unwrap(), b"%PDF-");
            r.end();
            let _ = std::fs::remove_dir_all(&root);
        }

        fn ops(r: &Rig) -> Vec<Op> {
            r.rec.0.lock().unwrap().0.clone()
        }

        fn enter(seq: u32) -> Msg {
            Msg::Enter { seq, edge: proto::Edge::Left, frac: 0x8000, mods: ModifierSet(0), pressed: vec![] }
        }

        #[derive(Clone)]
        struct FakeSource {
            generation: Arc<std::sync::atomic::AtomicU64>,
            file: PathBuf,
        }

        impl DragSource for FakeSource {
            fn generation(&self) -> Result<u64, String> {
                Ok(self.generation.load(std::sync::atomic::Ordering::SeqCst))
            }
            fn files(&self) -> Result<Vec<PathBuf>, String> { Ok(vec![self.file.clone()]) }
        }

        #[test]
        fn file_drag_returns_to_mac_but_held_non_drag_stays() {
            let root = std::env::temp_dir().join(format!("flowseam-win-source-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            let file = root.join("out.txt");
            std::fs::write(&file, b"source").unwrap();
            for started in [true, false] {
                let source = FakeSource { generation: Arc::new(std::sync::atomic::AtomicU64::new(0)), file: file.clone() };
                let r = rig_with_source(Some((root.join("staging"), root.join("dest"))), Some(Box::new(source.clone())));
                r.send(enter(1));
                r.send(Msg::Button { seq: 1, button: 0, down: true });
                let deadline = Instant::now() + Duration::from_secs(2);
                while !ops(&r).contains(&Op::Button(WinButton::Left, true)) && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(5));
                }
                assert!(ops(&r).contains(&Op::Button(WinButton::Left, true)));
                if started { source.generation.store(1, std::sync::atomic::Ordering::SeqCst); }
                r.send(Msg::MotionRel { seq: 1, dx: -5000, dy: 0 });
                if started {
                    assert!(matches!(r.from_fwd.recv_timeout(Duration::from_secs(2)), Ok(Msg::DragSoon { .. })));
                    let mut left = false;
                    let mut began = false;
                    for _ in 0..4 {
                        match r.from_fwd.recv_timeout(Duration::from_secs(2)).unwrap() {
                            Msg::Leave { .. } => left = true,
                            Msg::DragBegin { files, .. } => {
                                assert_eq!(files[0].name, "out.txt");
                                began = true;
                            }
                            Msg::FileChunk { .. } => {}
                            other => panic!("unexpected message: {other:?}"),
                        }
                        if left && began { break; }
                    }
                    assert!(left && began, "expected Leave and DragBegin");
                    // Leave and DragBegin can reach the Mac before the button release is injected here.
                    let released = |ops: &[Op]| ops.contains(&Op::Button(WinButton::Left, false));
                    let deadline = Instant::now() + Duration::from_secs(2);
                    while !released(&ops(&r)) && Instant::now() < deadline {
                        thread::sleep(Duration::from_millis(5));
                    }
                    let events = ops(&r);
                    let escape = events.iter().position(|op| *op == Op::Key(0x01, true)).expect("Escape injected");
                    let release = events.iter().position(|op| *op == Op::Button(WinButton::Left, false)).expect("button released");
                    assert!(escape < release, "Escape must cancel before release: {events:?}");
                    assert!(file.exists());
                } else {
                    // Three times the 200 ms dwell: a wrong crossing would have sent Leave well within this.
                    assert!(r.from_fwd.recv_timeout(Duration::from_millis(600)).is_err(), "held press without drag must stay");
                    assert!(!ops(&r).contains(&Op::Key(0x01, true)));
                }
                r.end();
            }
            let _ = std::fs::remove_dir_all(&root);
        }

        #[test]
        fn the_mac_drives_this_machine_and_the_left_edge_hands_control_back() {
            let r = rig();
            r.send(enter(1));
            r.send(Msg::MotionRel { seq: 1, dx: 50, dy: 0 });
            r.send(Msg::Key { seq: 1, hid: 0x04, down: true, repeat: false });
            r.send(Msg::Key { seq: 1, hid: 0x04, down: false, repeat: false });
            // Push into the left wall and rest there: the dwell must hand control back.
            r.send(Msg::MotionRel { seq: 1, dx: -5000, dy: 0 });
            let leave = r.from_fwd.recv_timeout(Duration::from_secs(2)).expect("Leave after the dwell");
            let Msg::Leave { seq, edge, frac } = leave else { panic!("{leave:?}") };
            assert_eq!((seq, edge), (1, proto::Edge::Right), "Leave names the Mac's edge the cursor returns at");
            assert!((0x7000..0x9000).contains(&frac), "same height it left at: {frac:#x}");

            let entry = Point { x: flowseam_core::geometry::ENTRY_INSET, y: 540 };
            assert_eq!(
                ops(&r),
                vec![
                    Op::Move(entry),
                    Op::Move(Point { x: entry.x + 50, y: 540 }),
                    Op::Key(0x1E, true),
                    Op::Key(0x1E, false),
                    Op::Move(Point { x: 0, y: 540 }),
                ]
            );
            r.end();
        }

        #[test]
        fn a_session_that_drops_while_driven_lets_go_of_everything_held() {
            let r = rig();
            r.send(enter(1));
            r.send(Msg::Key { seq: 1, hid: 0xE1, down: true, repeat: false });
            r.send(Msg::Button { seq: 1, button: 0, down: true });
            thread::sleep(Duration::from_millis(50));
            let rec = r.end(); // the link went away mid-drag with Shift held
            let ops = rec.0.lock().unwrap().0.clone();
            assert!(ops.contains(&Op::Key(0x2A, false)), "Shift left down: {ops:?}");
            assert!(ops.contains(&Op::Button(WinButton::Left, false)), "button left down: {ops:?}");
        }
    }
}
