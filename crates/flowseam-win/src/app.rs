//! Process-level wiring: config, identity, pairing and the dial/serve loops.
//!
//! Each session runs three threads: the network link, the clipboard pump, and a router that hands
//! clipboard messages to the pump and everything else to a flowseam_core `Forwarder`, which injects the peer's
//! input here and sends `Leave` when the cursor dwells at the edge facing the peer.

use std::net::{SocketAddr, TcpListener, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use flowseam_core::config::{Config, Edge, KeyMap, Peer};
use flowseam_core::forward::{Forwarder, FwdConfig, Mode, PeerSpec};
use flowseam_core::proto::{self, Msg, Rect};
use flowseam_core::switch::SwitchPolicy;
use flowseam_core::status::{self, LinkStatus};
use flowseam_net::backoff::Backoff;
use flowseam_net::files::{DragHub, DragInbox, DragPump, DragSource, SendFn};
#[cfg(windows)]
use crate::edge_drop::EdgeDropSource;
use flowseam_net::identity::{from_hex, to_hex, Identity};
use flowseam_net::link::{self, Endpoint};
use flowseam_net::pair::{self, PairGuard};
use flowseam_net::pins::PinSet;
use flowseam_net::session::{End, PeerInfo, Timing};
use flowseam_net::NetError;

use crate::inject::{ChanOut, InjectSink, Injector};
use crate::{pump_loop, Clipboard, Pump};

pub const POLL: Duration = Duration::from_millis(500);
/// Router wake-up while the peer drives this machine: the return-edge dwell is measured on these ticks.
const TICK_DRIVEN: Duration = Duration::from_millis(15);
const TICK_IDLE: Duration = Duration::from_secs(1);
const DISPLAY_REFRESH: Duration = Duration::from_secs(2);
/// The forwarder's name for the one peer a session talks to. A session is one link, so the name only
/// has to be the same on every call; which configured peer it is does not matter for receiving.
const SESSION_PEER: &str = "peer";
static ROUTER_HEARTBEAT: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();

fn route_heartbeat(value: Option<Instant>) {
    *ROUTER_HEARTBEAT.get_or_init(|| Mutex::new(None)).lock().unwrap_or_else(|e| e.into_inner()) = value;
}

fn status_heartbeat(config: &Path) {
    let path = status::status_path(config);
    thread::spawn(move || loop {
        let active = *ROUTER_HEARTBEAT.get_or_init(|| Mutex::new(None)).lock().unwrap_or_else(|e| e.into_inner());
        if active.is_none_or(|at| at.elapsed() <= Duration::from_secs(5)) {
            if let Err(e) = status::write(&path, &LinkStatus { written_ms: status::now_ms(), peers: Vec::new() }) { log(&format!("status write: {e}")); }
        }
        thread::sleep(Duration::from_secs(1));
    });
}

/// Builds a clipboard on the thread that will use it (Win32 windows are thread-bound).
pub type ClipFactory<'a> = &'a (dyn Fn() -> Result<Box<dyn Clipboard>, String> + Sync);
/// Builds the input injector for one session.
pub type InjectFactory<'a> = &'a (dyn Fn() -> Box<dyn Injector> + Sync);
/// This machine's displays, in the coordinates the injector moves in.
pub type DisplaySource<'a> = &'a (dyn Fn() -> Result<Vec<Rect>, String> + Sync);

/// What a run needs from the host besides the config.
pub struct Host<'a> {
    pub clipboard: ClipFactory<'a>,
    pub injector: InjectFactory<'a>,
    pub displays: DisplaySource<'a>,
}

pub fn log(msg: &str) {
    flowseam_core::log::line(&format!("[flowseam-win] {msg}"));
}

pub fn identity_dir(config: &Path) -> PathBuf {
    config.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")).join("identity")
}

fn load(config: &Path) -> Result<Config, String> {
    Config::load(config).map_err(|e| e.to_string())
}

fn resolve(addr: &str) -> Result<SocketAddr, String> {
    addr.to_socket_addrs()
        .map_err(|e| format!("cannot resolve {addr}: {e}"))?
        .next()
        .ok_or_else(|| format!("{addr} resolved to nothing"))
}

fn upsert_peer(cfg: &mut Config, name: &str, addr: &str, edge: Edge, fp: &[u8; 32]) {
    let fingerprint = to_hex(fp);
    match cfg.peers.iter_mut().find(|p| p.name == name) {
        Some(p) => {
            p.addr = addr.into();
            p.fingerprint = fingerprint;
        }
        // This side only receives input, so how it would map its keys is moot; a Windows controller
        // will need a PC-keyboard mapping of its own.
        None => cfg.peers.push(Peer { name: name.into(), addr: addr.into(), edge, fingerprint, keys: KeyMap::None }),
    }
}

fn node_info(id: &Identity, displays: Vec<Rect>) -> PeerInfo {
    let name = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "flowseam-win".into());
    let name: String = name.chars().take(32).collect();
    let displays = displays.into_iter().take(proto::MAX_DISPLAYS).collect();
    PeerInfo { node: id.fingerprint()[..16].try_into().expect("16 of 32 bytes"), name, displays }
}

fn fwd_config(cfg: &Config) -> Result<FwdConfig, String> {
    let edge = cfg.peers.first().map_or(proto::Edge::Left, |p| p.edge.into());
    Ok(FwdConfig {
        peers: vec![PeerSpec { name: SESSION_PEER.into(), edge, keys: KeyMap::None }],
        policy: SwitchPolicy::from_config(&cfg.switch).map_err(|e| format!("switch chord: {e:?}"))?,
        boundary_px: cfg.switch.boundary_px,
    })
}

/// Print the code, wait for one successful pairing, store the client's fingerprint.
pub fn pair_serve(config: &Path, peer: &str, peer_addr: &str) -> Result<(), String> {
    pair_serve_with(config, peer, peer_addr, |code| println!("PAIR CODE: {code}"), |fp| println!("PAIRED {peer} {fp}"))
}

/// Serve one pairing: `on_code` gets the code to show before a client is accepted, `on_paired` the
/// stored fingerprint (hex). The CLI prints both; the GUI shows them.
pub fn pair_serve_with(config: &Path, peer: &str, peer_addr: &str, on_code: impl FnOnce(&str), on_paired: impl FnOnce(&str)) -> Result<(), String> {
    let mut cfg = load(config)?;
    let listener = TcpListener::bind(&cfg.listen).map_err(|e| format!("bind {}: {e}", cfg.listen))?;
    let code = pair::generate_code();
    on_code(&code);
    log(&format!("waiting for a client on {}", cfg.listen));
    let guard = PairGuard::default();
    let fp = pair::serve_code(&listener, &code, &identity_dir(config), &guard, || false, log)
        .map_err(|e| e.to_string())?
        .expect("serve_code returns None only when stop() is true");
    upsert_peer(&mut cfg, peer, peer_addr, Edge::Left, &fp);
    cfg.save(config).map_err(|e| e.to_string())?;
    on_paired(&to_hex(&fp));
    Ok(())
}

/// Dial a pairing server and store its fingerprint under `peer`.
pub fn pair_dial(config: &Path, peer: &str, addr: &str, code: &str) -> Result<(), String> {
    let mut cfg = load(config)?;
    let sock = resolve(addr)?;
    let fp = pair::pair_as_client(sock, code, &identity_dir(config)).map_err(|e| e.to_string())?;
    upsert_peer(&mut cfg, peer, addr, Edge::Right, &fp);
    cfg.save(config).map_err(|e| e.to_string())?;
    println!("PAIRED {peer} {}", to_hex(&fp));
    Ok(())
}

fn pins_of(cfg: &Config) -> PinSet {
    PinSet::new(cfg.peers.iter().filter_map(|p| from_hex(&p.fingerprint)))
}

type WinForwarder = Forwarder<InjectSink<Box<dyn Injector>>, ChanOut>;

/// Unfinished files wait in %LOCALAPPDATA%\Flowseam\drags; dropped ones land in Downloads. `None` when
/// the environment names no user profile.
fn drag_dirs() -> Option<(PathBuf, PathBuf)> {
    // ponytail: %USERPROFILE%\Downloads misses a relocated Downloads folder; ask SHGetKnownFolderPath(FOLDERID_Downloads) if one shows up.
    let home = PathBuf::from(std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?);
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| home.join("AppData").join("Local"));
    Some((local.join("Flowseam").join("drags"), home.join("Downloads")))
}

/// A drag hub for one session's one peer, and the queue the router hands drag messages to.
fn session_drags(out: &Sender<Msg>, dirs: Option<(PathBuf, PathBuf)>, source: Option<Box<dyn DragSource>>) -> Option<(Arc<Mutex<DragHub>>, DragInbox)> {
    let Some((staging, dest)) = dirs else {
        log("no user profile in the environment: file drags are off");
        return None;
    };
    let pump = DragPump::new(staging, dest);
    #[cfg(windows)]
    let pump = pump.with_native(Arc::new(crate::native_drop::WinNativeDrop::new()));
    #[cfg(windows)]
    let pump = pump.with_free_space(Arc::new(|p: &Path| crate::win32::free_bytes(p)));
    let pump = Arc::new(Mutex::new(pump));
    let tx = out.clone();
    let send: SendFn = Arc::new(move |m| tx.send(m).is_ok());
    let mut hub = DragHub::new(source);
    hub.attach(SESSION_PEER, pump, send);
    let inbox = hub.inbox(SESSION_PEER).expect("just attached");
    Some((Arc::new(Mutex::new(hub)), inbox))
}

/// Run `link` for exactly one session with a fresh clipboard pump and a fresh forwarder. Fresh per
/// session: a restarted peer's clipboard generation and Enter seq both start over at 1.
fn with_session<R>(host: &Host<'_>, fwd_cfg: &FwdConfig, link: impl FnOnce(&Receiver<Msg>, &Sender<Msg>) -> R) -> Result<R, String> {
    let (out_tx, out_rx) = channel();
    let (in_tx, in_rx) = channel();
    let (clip_tx, clip_rx) = channel();
    let displays = (host.displays)()?;
    let dirs = drag_dirs();
    #[cfg(windows)]
    let edge_drop = dirs.as_ref().map(|_| EdgeDropSource::new(fwd_cfg.peers[0].edge, displays.clone()));
    #[cfg(windows)]
    let source: Option<Box<dyn DragSource>> = edge_drop.clone().map(|s| Box::new(s) as Box<dyn DragSource>);
    #[cfg(not(windows))]
    let source: Option<Box<dyn DragSource>> = None;
    let drags = session_drags(&out_tx, dirs, source);
    let mut sink = InjectSink::new((host.injector)(), displays.clone(), log);
    #[cfg(windows)]
    if let Some(source) = edge_drop { sink = sink.with_edge_drop(source, fwd_cfg.peers[0].edge); }
    if let Some((hub, _)) = &drags {
        sink = sink.with_drags(hub.clone());
    }
    let fwd: WinForwarder = Forwarder::new(fwd_cfg.clone(), displays, sink, ChanOut(out_tx.clone()));
    let drags = drags.map(|(_, d)| d);
    thread::scope(|s| {
        let make = host.clipboard;
        let pump_thread = s.spawn(move || -> Result<(), String> {
            let pump = Pump::new(make()?);
            pump_loop(pump, &clip_rx, &out_tx, POLL, &log);
            Ok(())
        });
        let displays = host.displays;
        let router = s.spawn(move || route(in_rx, clip_tx, fwd, displays, drags));
        let r = link(&out_rx, &in_tx);
        drop(in_tx);
        router.join().map_err(|_| "input router panicked".to_string())?;
        pump_thread.join().map_err(|_| "pump thread panicked".to_string())??;
        Ok(r)
    })
}

/// Clipboard to the pump, input to the forwarder; tick the forwarder so the return dwell fires while the
/// mouse rests at the edge. When the session ends, release everything the peer was holding down here.
fn route(incoming: Receiver<Msg>, clip: Sender<Msg>, mut fwd: WinForwarder, displays: DisplaySource<'_>, drags: Option<DragInbox>) {
    route_heartbeat(Some(Instant::now()));
    let started = Instant::now();
    let now = || started.elapsed().as_millis() as u64;
    let mut refreshed = Instant::now();
    loop {
        let driven = matches!(fwd.mode(), Mode::RemoteIn { .. });
        match incoming.recv_timeout(if driven { TICK_DRIVEN } else { TICK_IDLE }) {
            Ok(m @ Msg::ClipboardText { .. }) => {
                let _ = clip.send(m);
            }
            Ok(m @ (Msg::DragSoon { .. } | Msg::DragBegin { .. } | Msg::FileChunk { .. } | Msg::DragAck { .. } | Msg::DragCancel { .. })) => {
                // In order with the button releases this session injects, which reach the same queue.
                if let Some(inbox) = &drags {
                    inbox.deliver(m);
                }
            }
            Ok(m) => fwd.on_msg(SESSION_PEER, m, now()),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        fwd.tick(now());
        route_heartbeat(Some(Instant::now()));
        if driven != matches!(fwd.mode(), Mode::RemoteIn { .. }) {
            log(if driven { "control returned to the peer" } else { "peer took control" });
        }
        if refreshed.elapsed() >= DISPLAY_REFRESH {
            refreshed = Instant::now();
            match displays() {
                Ok(d) => {
                    fwd.sink_mut().set_displays(d.clone());
                    fwd.set_displays(d);
                }
                Err(e) => log(&format!("display refresh failed: {e}")),
            }
        }
    }
    if matches!(fwd.mode(), Mode::RemoteIn { .. }) {
        log("session ended while driven: releasing held keys and buttons");
    }
    fwd.link_down(SESSION_PEER);
    route_heartbeat(None);
}

/// One session or connection attempt ended. Nothing here ends the process: a stray LAN connection with
/// a bad certificate, a peer that faulted, or a peer that is not paired yet are logged and retried.
/// Only a session that ended cleanly resets the backoff, so a peer that keeps failing is not hammered.
fn report(r: &Result<link::Outcome, NetError>, backoff: &mut Backoff) {
    match r {
        Ok(o) => {
            log(&format!("session with {} ended: {:?}", o.peer.name, o.end));
            if !matches!(o.end, End::Faulted(_)) {
                backoff.reset();
            }
        }
        Err(e) if e.is_transient() => log(&format!("connect failed (will retry): {e}")),
        Err(e) => log(&format!("connection refused (will retry; re-pair if this keeps happening): {e}")),
    }
}

/// Dial the first peer in the config and keep the link up.
pub fn run_dial(config: &Path, host: &Host<'_>) -> Result<(), String> {
    let cfg = load(config)?;
    cfg.validate_for_run().map_err(|e| e.to_string())?;
    let fwd_cfg = fwd_config(&cfg)?;
    let peer = cfg.peers.first().ok_or("config has no peers; pair first")?;
    let addr = resolve(&peer.addr)?;
    let id = Identity::load_or_create(&identity_dir(config)).map_err(|e| e.to_string())?;
    let (pins, me) = (pins_of(&cfg), node_info(&id, (host.displays)()?));
    let ep = Endpoint { identity: &id, pins: &pins, me: &me, timing: Timing::default(), speak_for: None, paths_for: None };
    let mut backoff = Backoff::new(Duration::from_secs(1), Duration::from_secs(30));
    loop {
        log(&format!("dialing {} ({addr})", peer.name));
        let r = with_session(host, &fwd_cfg, |out, inc| {
            link::run_client_once_with(addr, &ep, out, inc, &|p| log(&format!("link up with {}", p.peer.name)))
        })?;
        report(&r, &mut backoff);
        thread::sleep(backoff.next_delay());
    }
}

/// Listen on `config.listen` and serve one session at a time.
pub fn run_serve(config: &Path, host: &Host<'_>) -> Result<(), String> {
    let cfg = load(config)?;
    cfg.validate_for_run().map_err(|e| e.to_string())?;
    let fwd_cfg = fwd_config(&cfg)?;
    let listener = TcpListener::bind(&cfg.listen).map_err(|e| format!("bind {}: {e}", cfg.listen))?;
    let id = Identity::load_or_create(&identity_dir(config)).map_err(|e| e.to_string())?;
    let (pins, me) = (pins_of(&cfg), node_info(&id, (host.displays)()?));
    let ep = Endpoint { identity: &id, pins: &pins, me: &me, timing: Timing::default(), speak_for: None, paths_for: None };
    status_heartbeat(config);
    let mut backoff = Backoff::new(Duration::from_millis(200), Duration::from_secs(5));
    log(&format!("listening on {}", cfg.listen));
    loop {
        let r = with_session(host, &fwd_cfg, |out, inc| {
            link::serve_once_with(&listener, &ep, out, inc, &|p| log(&format!("link up with {}", p.peer.name)))
        })?;
        report(&r, &mut backoff);
        thread::sleep(backoff.next_delay());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(end: End) -> Result<link::Outcome, NetError> {
        Ok(link::Outcome { peer: PeerInfo { node: [0; 16], name: "mac".into(), displays: vec![] }, end })
    }

    /// The listener is a long-lived service: one bad connection must not take it down for every
    /// later legitimate one.
    #[test]
    fn nothing_a_peer_or_a_stray_connection_does_ends_the_loop() {
        let mut b = Backoff::new(Duration::from_millis(100), Duration::from_secs(5));
        // Each of these used to return Err, and `?` in run_serve/run_dial ended the process.
        report(&Err(NetError::Proto("garbage hello".into())), &mut b);
        report(&Err(NetError::VersionMismatch { ours: 1, theirs: 9 }), &mut b);
        report(&Err(NetError::Rejected("not paired".into())), &mut b);
        report(&outcome(End::Faulted("peer fault 1: bad frame".into())), &mut b);
    }

    mod routing {
        use super::super::*;
        use crate::inject::WinButton;
        use crate::keymap::Scan;
        use flowseam_core::proto::{ModifierSet, Point};
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

        struct Rig {
            rec: Rec,
            to_router: Sender<Msg>,
            from_fwd: Receiver<Msg>,
            clip: Receiver<Msg>,
            router: thread::JoinHandle<()>,
        }

        fn rig() -> Rig {
            rig_with(None)
        }

        /// `dirs`: (staging, destination) for file drags, as `drag_dirs` gives them.
        fn rig_with(dirs: Option<(PathBuf, PathBuf)>) -> Rig {
            rig_with_source(dirs, None)
        }

        fn rig_with_source(dirs: Option<(PathBuf, PathBuf)>, source: Option<Box<dyn DragSource>>) -> Rig {
            let rec = Rec(Arc::new(Mutex::new((vec![], Point { x: 960, y: 540 }))));
            let (to_router, in_rx) = channel();
            let (out_tx, from_fwd) = channel();
            let (clip_tx, clip) = channel();
            let cfg = fwd_config(&Config::default()).unwrap();
            let drags = dirs.and_then(|d| session_drags(&out_tx, Some(d), source));
            let mut sink = InjectSink::new(Box::new(rec.clone()) as Box<dyn Injector>, screen().unwrap(), |_| {});
            if let Some((hub, _)) = &drags {
                sink = sink.with_drags(hub.clone());
            }
            let fwd: WinForwarder = Forwarder::new(cfg, screen().unwrap(), sink, ChanOut(out_tx));
            let drags = drags.map(|(_, d)| d);
            let router = thread::spawn(move || route(in_rx, clip_tx, fwd, &screen, drags));
            Rig { rec, to_router, from_fwd, clip, router }
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
            r.to_router.send(Msg::DragSoon { drag: 1 }).unwrap();
            r.to_router.send(Msg::DragBegin { drag: 1, files: vec![proto::FileMeta { name: "報告.pdf".into(), size: 5 }] }).unwrap();
            r.to_router.send(Msg::FileChunk { drag: 1, file: 0, data: b"%PDF-".to_vec() }).unwrap();
            r.to_router.send(enter(1)).unwrap();
            let ack = r.from_fwd.recv_timeout(Duration::from_secs(2)).expect("ack");
            assert_eq!(ack, Msg::DragAck { drag: 1, bytes: 5 });
            assert!(std::fs::read_dir(&dest).unwrap().next().is_none(), "nothing before the drop");
            r.to_router.send(Msg::Button { seq: 1, button: 0, down: false }).unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline && !dest.join("報告.pdf").exists() {
                thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(std::fs::read(dest.join("報告.pdf")).unwrap(), b"%PDF-");
            drop(r.to_router);
            r.router.join().unwrap();
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
                r.to_router.send(enter(1)).unwrap();
                r.to_router.send(Msg::Button { seq: 1, button: 0, down: true }).unwrap();
                let deadline = Instant::now() + Duration::from_secs(2);
                while !ops(&r).contains(&Op::Button(WinButton::Left, true)) && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(5));
                }
                assert!(ops(&r).contains(&Op::Button(WinButton::Left, true)));
                if started { source.generation.store(1, std::sync::atomic::Ordering::SeqCst); }
                r.to_router.send(Msg::MotionRel { seq: 1, dx: -5000, dy: 0 }).unwrap();
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
                drop(r.to_router);
                r.router.join().unwrap();
            }
            let _ = std::fs::remove_dir_all(&root);
        }

        #[test]
        fn the_mac_drives_this_machine_and_the_left_edge_hands_control_back() {
            let r = rig();
            r.to_router.send(enter(1)).unwrap();
            r.to_router.send(Msg::MotionRel { seq: 1, dx: 50, dy: 0 }).unwrap();
            r.to_router.send(Msg::Key { seq: 1, hid: 0x04, down: true, repeat: false }).unwrap();
            r.to_router.send(Msg::Key { seq: 1, hid: 0x04, down: false, repeat: false }).unwrap();
            r.to_router.send(Msg::ClipboardText { gen: 1, text: "via router".into() }).unwrap();
            // Push into the left wall and rest there: the dwell must hand control back.
            r.to_router.send(Msg::MotionRel { seq: 1, dx: -5000, dy: 0 }).unwrap();
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
            assert_eq!(r.clip.recv_timeout(Duration::from_secs(1)).unwrap(), Msg::ClipboardText { gen: 1, text: "via router".into() });
            drop(r.to_router);
            r.router.join().unwrap();
        }

        #[test]
        fn a_session_that_drops_while_driven_lets_go_of_everything_held() {
            let r = rig();
            r.to_router.send(enter(1)).unwrap();
            r.to_router.send(Msg::Key { seq: 1, hid: 0xE1, down: true, repeat: false }).unwrap();
            r.to_router.send(Msg::Button { seq: 1, button: 0, down: true }).unwrap();
            thread::sleep(Duration::from_millis(50));
            let Rig { rec, to_router, router, .. } = r;
            drop(to_router); // the link went away mid-drag with Shift held
            router.join().unwrap();
            let ops = rec.0.lock().unwrap().0.clone();
            assert!(ops.contains(&Op::Key(0x2A, false)), "Shift left down: {ops:?}");
            assert!(ops.contains(&Op::Button(WinButton::Left, false)), "button left down: {ops:?}");
        }
    }

    #[test]
    fn only_a_clean_session_resets_the_backoff() {
        let mut b = Backoff::new(Duration::from_millis(100), Duration::from_secs(5));
        let first = b.next_delay();
        b.next_delay();
        report(&outcome(End::Faulted("x".into())), &mut b);
        assert!(b.next_delay() > first, "a faulting peer must not be redialled at the base delay");
        report(&outcome(End::PeerClosed), &mut b);
        assert_eq!(b.next_delay(), first);
    }
}
