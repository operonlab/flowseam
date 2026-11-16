//! The node logic every worker shares, whatever it runs on: config, identity, pairing, the forwarder's
//! config and link peers, the wake registry for clock threads, and the clock loop that ticks the forwarder.
//! What a platform adds on top (Windows: hook probes, cursor drift, display refresh) goes through `ClockHooks`.

use std::net::{SocketAddr, TcpListener, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use flowseam_core::config::{Config, Edge, KeyMap, Peer};
use flowseam_core::forward::{Forwarder, FwdConfig, InputSink, Mode, Outbox, PeerSpec};
use flowseam_core::proto::{self, Msg, Rect};
use flowseam_core::switch::SwitchPolicy;

use crate::identity::{from_hex, to_hex, Identity};
use crate::links::{Clock, MonitorsHome};
use crate::pair::{self, PairGuard};
use crate::pins::PinSet;
use crate::session::PeerInfo;

/// Router wake-up while a peer drives this machine: the return-edge dwell is measured on these ticks.
pub const TICK_DRIVEN: Duration = Duration::from_millis(15);
pub const TICK_IDLE: Duration = Duration::from_secs(1);

/// How a platform words its log lines: where they go, what it calls itself, and how a peer "connects" to it.
#[derive(Clone, Copy)]
pub struct Names {
    pub log: fn(&str),
    /// "this PC", "this peer"
    pub me: &'static str,
    /// "to this PC", "here": completes "it can still connect ..."
    pub connect_to: &'static str,
}

pub fn identity_dir(config: &Path) -> PathBuf {
    config.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")).join("identity")
}

pub fn load(config: &Path) -> Result<Config, String> {
    Config::load(config).map_err(|e| e.to_string())
}

pub fn resolve(addr: &str) -> Result<SocketAddr, String> {
    addr.to_socket_addrs()
        .map_err(|e| format!("cannot resolve {addr}: {e}"))?
        .next()
        .ok_or_else(|| format!("{addr} resolved to nothing"))
}

/// Store `fp` under `name`; a new peer sends its keys as `keys` (how this machine's keys reach it when it
/// drives that peer, see `fwd_config`).
pub fn upsert_peer(cfg: &mut Config, name: &str, addr: &str, edge: Edge, fp: &[u8; 32], keys: KeyMap) {
    let fingerprint = to_hex(fp);
    match cfg.peers.iter_mut().find(|p| p.name == name) {
        Some(p) => {
            p.addr = addr.into();
            p.fingerprint = fingerprint;
        }
        None => cfg.peers.push(Peer { name: name.into(), addr: addr.into(), edge, fingerprint, keys, gestures: Default::default(), displays: Vec::new() }),
    }
}

pub fn pins_of(cfg: &Config) -> PinSet {
    PinSet::new(cfg.peers.iter().filter_map(|p| from_hex(&p.fingerprint)))
}

/// What this node announces: its identity, `host` (cut to the 32 chars the wire allows) and its displays.
pub fn node_info(id: &Identity, displays: Vec<Rect>, host: &str) -> PeerInfo {
    let name: String = host.chars().take(32).collect();
    let displays = displays.into_iter().take(proto::MAX_DISPLAYS).collect();
    PeerInfo { node: id.fingerprint()[..16].try_into().expect("16 of 32 bytes"), name, displays }
}

/// What a cancelled pairing server returns, so the window can say "cancelled" rather than "failed".
pub const PAIR_CANCELLED: &str = "pairing cancelled";

/// Serve one pairing: `on_code` gets the code to show before a client is accepted, `on_paired` the
/// stored fingerprint (hex). A new peer sends its keys as `keys`.
pub fn pair_serve_with(
    names: Names,
    config: &Path,
    peer: &str,
    peer_addr: &str,
    keys: KeyMap,
    on_code: impl FnOnce(&str),
    on_paired: impl FnOnce(&str),
) -> Result<(), String> {
    pair_serve_cancellable(names, config, peer, peer_addr, keys, &AtomicBool::new(false), on_code, on_paired)
}

/// `pair_serve_with` that ends with `Err(PAIR_CANCELLED)` once `cancel` is raised (`pair_serve_cancel` wakes
/// the blocked accept). The flag is also read before each accept, since a cancel raised before the port was
/// bound had nothing to wake. A pairing that succeeds as the flag goes up still counts.
#[allow(clippy::too_many_arguments)]
pub fn pair_serve_cancellable(
    names: Names,
    config: &Path,
    peer: &str,
    peer_addr: &str,
    keys: KeyMap,
    cancel: &AtomicBool,
    on_code: impl FnOnce(&str),
    on_paired: impl FnOnce(&str),
) -> Result<(), String> {
    let log = names.log;
    let mut cfg = load(config)?;
    let listener = TcpListener::bind(&cfg.listen).map_err(|e| format!("bind {}: {e}", cfg.listen))?;
    let code = pair::generate_code();
    on_code(&code);
    log(&format!("waiting for a client on {}", cfg.listen));
    let guard = PairGuard::default();
    let stop = || cancel.load(Ordering::SeqCst);
    let fp = match pair::serve_code(&listener, &code, &identity_dir(config), &guard, stop, log) {
        Ok(Some(fp)) => fp,
        Ok(None) => return Err(PAIR_CANCELLED.into()),
        Err(e) => return Err(e.to_string()),
    };
    upsert_peer(&mut cfg, peer, peer_addr, Edge::Left, &fp, keys);
    cfg.save(config).map_err(|e| e.to_string())?;
    on_paired(&to_hex(&fp));
    Ok(())
}

/// End a `pair_serve_cancellable` that is waiting: raise the flag, then wake its blocked accept with a
/// connection that closes at once.
pub fn pair_serve_cancel(config: &Path, cancel: &AtomicBool) {
    cancel.store(true, Ordering::SeqCst);
    let Ok(cfg) = load(config) else { return };
    let Ok(mut addr) = cfg.listen.parse::<SocketAddr>() else { return };
    if addr.ip().is_unspecified() {
        addr.set_ip(if addr.is_ipv4() { std::net::Ipv4Addr::LOCALHOST.into() } else { std::net::Ipv6Addr::LOCALHOST.into() });
    }
    let _ = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(500));
}

/// Dial a pairing server and store its fingerprint under `peer`.
pub fn pair_dial(config: &Path, peer: &str, addr: &str, code: &str, keys: KeyMap) -> Result<(), String> {
    let mut cfg = load(config)?;
    let sock = resolve(addr)?;
    let fp = pair::pair_as_client(sock, code, &identity_dir(config)).map_err(|e| e.to_string())?;
    upsert_peer(&mut cfg, peer, addr, Edge::Right, &fp, keys);
    cfg.save(config).map_err(|e| e.to_string())?;
    println!("PAIRED {peer} {}", to_hex(&fp));
    Ok(())
}

/// Every peer's edge, keys and the switch policy. `keys_for` is the platform's last word on a peer's key map
/// (a Mac keyboard's "mac" keys, for one, mean nothing when this machine is the one sending).
pub fn fwd_config_all(cfg: &Config, keys_for: impl Fn(&Peer) -> KeyMap) -> Result<FwdConfig, String> {
    let peers = cfg.peers.iter().map(|p| PeerSpec { name: p.name.clone(), edge: p.edge.into(), keys: keys_for(p), gestures: p.gestures }).collect();
    Ok(FwdConfig { peers, policy: SwitchPolicy::from_config(&cfg.switch).map_err(|e| format!("switch chord: {e:?}"))?, boundary_px: cfg.switch.boundary_px })
}

/// (name, dial address, node) for every paired peer. A peer whose address does not resolve is still
/// listed, so it can connect to this machine; dialing it fails and is retried with the links' backoff.
pub fn link_peers(cfg: &Config, names: Names) -> Vec<(String, SocketAddr, [u8; 16])> {
    let log = names.log;
    cfg.peers
        .iter()
        .filter_map(|p| {
            let Some(fp) = from_hex(&p.fingerprint) else {
                log(&format!("{}: not paired (no readable fingerprint); left out", p.name));
                return None;
            };
            let addr = resolve(&p.addr).unwrap_or_else(|e| {
                log(&format!("{}: {e}; it can still connect {}", p.name, names.connect_to));
                SocketAddr::from(([0, 0, 0, 0], 0))
            });
            Some((p.name.clone(), addr, fp[..16].try_into().expect("16 of 32 bytes")))
        })
        .collect()
}

/// "back" from the window asks each linked peer that can read it to bring home the monitors it switched
/// for this machine. Returns the line to log, or `None` for a peer with no link.
pub fn home_request_to(peer: &str, ver: Option<u16>, out: &mut impl Outbox) -> Option<String> {
    let ver = ver?;
    if ver < 6 {
        return Some(format!("{peer} speaks protocol {ver}: it cannot be asked for its monitors (update it)"));
    }
    out.send(peer, Msg::MonitorsHome);
    Some(format!("asked {peer} to bring its monitors home"))
}

/// The peer driving this machine in `mode` (`None`: nobody does), as status.toml reports it.
pub fn driven_by(mode: &Mode) -> Option<String> {
    match mode {
        Mode::RemoteIn { peer, .. } => Some(peer.clone()),
        Mode::Relay { from, .. } => Some(from.clone()),
        _ => None,
    }
}

/// The window's "back": hands control back to whoever drives this machine (`release_control`), then asks
/// each linked peer that can read it to bring its monitors home. The clock loops of every platform share it.
pub fn hand_back<S: InputSink, O: Outbox>(f: &mut Forwarder<S, O>, peers: &[String], routes: &mut impl Outbox, me: &str, log: &dyn Fn(&str)) {
    log(&if f.release_control() { "control handed back from the menu".to_string() } else { format!("the menu asked to hand control back; {me} was not being driven") });
    for p in peers {
        if let Some(line) = home_request_to(p, f.speaks(p), routes) {
            log(&line);
        }
    }
}

/// Bytes an unprivileged process may still write on the volume holding `p`.
#[cfg(unix)]
pub fn free_bytes(p: &Path) -> Result<u64, String> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).map_err(|_| "path contains NUL".to_string())?;
    // SAFETY: `c` is a valid C string and `st` a correctly sized out-parameter.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return Err(format!("statvfs {}: {}", p.display(), std::io::Error::last_os_error()));
    }
    Ok((st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
}

/// Clock threads to wake when a forwarder needs ticks: `LinkHost::wake` is a plain function, and a test
/// runs two workers in one process.
static CLOCKS: Mutex<Vec<thread::Thread>> = Mutex::new(Vec::new());

pub fn register_clock() {
    CLOCKS.lock().unwrap_or_else(|e| e.into_inner()).push(thread::current());
}

pub fn unregister_clock() {
    let me = thread::current().id();
    CLOCKS.lock().unwrap_or_else(|e| e.into_inner()).retain(|t| t.id() != me);
}

pub fn wake_clocks() {
    for t in CLOCKS.lock().unwrap_or_else(|e| e.into_inner()).iter() {
        t.unpark();
    }
}

/// Peers whose "back" button asked for their monitors to come home, oldest first. Taken so they are logged once.
fn take_home_asks(home_asks: &MonitorsHome) -> Vec<String> {
    std::mem::take(&mut *home_asks.lock().unwrap_or_else(|e| e.into_inner()))
}

/// What a platform adds to each turn of the clock loop. Every method has a do-nothing default.
pub trait ClockHooks<S: InputSink, O: Outbox> {
    /// Once, after the thread registered for wake-ups.
    fn start(&mut self) {}
    /// Right after the park, before the forwarder is locked.
    fn woke(&mut self) {}
    /// With the forwarder locked, right after it ticked.
    fn after_tick(&mut self, _f: &mut Forwarder<S, O>) {}
    /// Whether the window's "back" button was pressed since the last turn (taken: asks once).
    fn take_back(&mut self) -> bool {
        false
    }
    /// The peer driving this machine changed (`None`: nobody drives it now).
    fn driven_changed(&mut self, _now: &Option<String>) {}
    /// Last of the turn, forwarder still locked.
    fn end_turn(&mut self, _f: &mut Forwarder<S, O>) {}
    /// Once, after the loop ended and the thread left the wake registry.
    fn finish(&mut self) {}
}

/// What the clock loop reads that is not the forwarder.
pub struct ClockEnv<'a> {
    pub clock: &'a Clock,
    pub stop: &'a AtomicBool,
    /// The peers a "back" press asks to bring the monitors home.
    pub peers: &'a [String],
    pub home_asks: &'a MonitorsHome,
    pub names: Names,
}

/// Ticks the forwarder (15 ms while a peer drives this machine or a dwell runs, else once a second, sooner
/// when the links wake it), logs who drives whom, drains the peers' monitor requests and answers "back".
pub fn clock_loop<S: InputSink, O: Outbox>(fwd: &Arc<Mutex<Forwarder<S, O>>>, mut routes: O, env: &ClockEnv, hook: &mut impl ClockHooks<S, O>) {
    let (log, me) = (env.names.log, env.names.me);
    register_clock();
    hook.start();
    let lock = || fwd.lock().unwrap_or_else(|e| e.into_inner());
    let mut driven: Option<String> = None;
    while !env.stop.load(Ordering::SeqCst) {
        let wait = {
            let f = lock();
            if matches!(f.mode(), Mode::RemoteIn { .. } | Mode::Relay { .. }) || f.needs_tick() { TICK_DRIVEN } else { TICK_IDLE }
        };
        thread::park_timeout(wait);
        hook.woke();
        for name in take_home_asks(env.home_asks) {
            log(&format!("{name} asked for its monitors to come home; {me} switches none"));
        }
        let mut f = lock();
        f.tick((env.clock)());
        hook.after_tick(&mut f);
        if hook.take_back() {
            hand_back(&mut f, env.peers, &mut routes, me, &log);
        }
        let now_driven = driven_by(f.mode());
        if now_driven != driven {
            log(&match (&driven, &now_driven) {
                (_, Some(p)) => format!("{p} took control"),
                (Some(p), None) => format!("control returned to {p}"),
                (None, None) => unreachable!("they differ"),
            });
            hook.driven_changed(&now_driven);
            driven = now_driven;
        }
        hook.end_turn(&mut f);
    }
    unregister_clock();
    hook.finish();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shared_fwd_config_keeps_a_peers_gesture_habit() {
        use flowseam_core::config::GestureMap;
        let mut cfg = Config::default();
        cfg.peers.push(Peer { name: "p".into(), addr: "h:1".into(), edge: flowseam_core::config::Edge::Right, fingerprint: String::new(), keys: KeyMap::None, gestures: GestureMap::Windows, displays: Vec::new() });
        let f = fwd_config_all(&cfg, |p| p.keys).unwrap();
        assert_eq!(f.peers[0].gestures, GestureMap::Windows);
    }

    #[test]
    fn a_waiting_pairing_server_ends_when_cancelled_and_before_it_binds() {
        let dir = std::env::temp_dir().join(format!("flowseam-node-cancel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("config.toml");
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let cfg = Config { listen: format!("127.0.0.1:{port}"), ..Config::default() };
        cfg.save(&config).unwrap();
        fn quiet(_: &str) {}
        let names = Names { log: quiet, me: "me", connect_to: "here" };
        // A server that ignores the flag would block in accept for good: every wait here is bounded.
        let bounded = |config: PathBuf, flag: Arc<AtomicBool>, code_tx: Option<std::sync::mpsc::Sender<String>>| {
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            thread::spawn(move || {
                let r = pair_serve_cancellable(names, &config, "p", "h:1", KeyMap::None, &flag, |c| { if let Some(t) = &code_tx { let _ = t.send(c.to_string()); } }, |_| panic!("no pairing"));
                let _ = done_tx.send(r);
            });
            done_rx
        };
        // Raised before the server runs: nothing to wake, the flag alone ends it.
        let done = bounded(config.clone(), Arc::new(AtomicBool::new(true)), None);
        assert_eq!(done.recv_timeout(Duration::from_secs(3)), Ok(Err(PAIR_CANCELLED.to_string())), "a flag raised before the accept ends the server");
        // Raised while it blocks in accept: the wake connection ends it.
        let flag = Arc::new(AtomicBool::new(false));
        let (code_tx, code_rx) = std::sync::mpsc::channel();
        let done = bounded(config.clone(), flag.clone(), Some(code_tx));
        code_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        pair_serve_cancel(&config, &flag);
        assert_eq!(done.recv_timeout(Duration::from_secs(3)), Ok(Err(PAIR_CANCELLED.to_string())), "the wake ends a waiting server");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn home_asks_are_taken_once_oldest_first() {
        let asks = MonitorsHome::default();
        asks.lock().unwrap().extend(["b".to_string(), "c".to_string()]);
        assert_eq!(take_home_asks(&asks), ["b", "c"]);
        assert!(take_home_asks(&asks).is_empty(), "a second turn finds nothing to log again");
    }

    #[test]
    fn hand_back_releases_control_and_asks_peers_that_speak_protocol_6() {
        use flowseam_core::config::KeyMap;
        use flowseam_core::forward::PeerSpec;
        use flowseam_core::proto::{Edge as PEdge, ModifierSet};
        struct Null;
        impl InputSink for Null {
            fn motion_rel(&mut self, _: i32, _: i32) {}
            fn button(&mut self, _: u8, _: bool) {}
            fn key(&mut self, _: u16, _: bool, _: bool) {}
            fn wheel(&mut self, _: i32, _: i32) {}
            fn warp(&mut self, _: proto::Point) {}
            fn set_grab(&mut self, _: bool) {}
            fn hide(&mut self) {}
            fn show(&mut self) {}
            fn set_association(&mut self, _: bool) {}
        }
        #[derive(Default)]
        struct Mail(Vec<(String, Msg)>);
        impl Outbox for Mail {
            fn send(&mut self, peer: &str, msg: Msg) {
                self.0.push((peer.to_string(), msg));
            }
        }
        let cfg = FwdConfig { peers: vec![PeerSpec { name: "new".into(), edge: PEdge::Left, keys: KeyMap::None, gestures: Default::default() }], policy: Default::default(), boundary_px: 2 };
        let mut f = Forwarder::new(cfg, vec![Rect { x: 0, y: 0, w: 1440, h: 900 }], Null, Mail::default());
        f.link_up("new");
        f.peer_speaks("new", 6);
        f.on_msg("new", Msg::Enter { seq: 7, edge: PEdge::Left, frac: 0x8000, mods: ModifierSet(0), pressed: vec![] }, 0);
        assert_eq!(driven_by(f.mode()).as_deref(), Some("new"));
        let lines = Mutex::new(Vec::<String>::new());
        let mut routes = Mail::default();
        hand_back(&mut f, &["new".to_string(), "old".to_string()], &mut routes, "this Mac", &|m| lines.lock().unwrap().push(m.to_string()));
        assert!(matches!(f.mode(), Mode::Local), "control released");
        assert_eq!(driven_by(f.mode()), None);
        assert_eq!(routes.0, [("new".to_string(), Msg::MonitorsHome)], "only a peer speaking 6 is asked");
        assert_eq!(*lines.lock().unwrap(), ["control handed back from the menu", "asked new to bring its monitors home"], "no link to `old`: nothing to log for it");
        hand_back(&mut f, &["new".to_string()], &mut routes, "this Mac", &|m| lines.lock().unwrap().push(m.to_string()));
        assert!(lines.lock().unwrap().contains(&"the menu asked to hand control back; this Mac was not being driven".to_string()));
    }

    #[test]
    fn the_node_name_fits_the_wire() {
        let id = Identity::generate().unwrap();
        let n = node_info(&id, Vec::new(), &"h".repeat(40)).name;
        assert_eq!(n.chars().count(), 32);
        let many = vec![Rect { x: 0, y: 0, w: 1, h: 1 }; proto::MAX_DISPLAYS + 3];
        assert_eq!(node_info(&id, many, "h").displays.len(), proto::MAX_DISPLAYS);
    }

    #[cfg(unix)]
    #[test]
    fn free_bytes_measures_a_real_volume() {
        assert!(free_bytes(&std::env::temp_dir()).expect("statvfs") > 0);
        assert!(free_bytes(Path::new("/no/such/flowseam/path")).is_err());
    }

    #[test]
    fn wake_unparks_a_registered_clock() {
        let (tx, rx) = std::sync::mpsc::channel();
        let t = thread::spawn(move || {
            register_clock();
            tx.send(()).unwrap();
            let t0 = std::time::Instant::now();
            thread::park_timeout(Duration::from_secs(5));
            let woke_after = t0.elapsed();
            unregister_clock();
            woke_after
        });
        rx.recv().unwrap();
        wake_clocks();
        assert!(t.join().unwrap() < Duration::from_secs(4), "wake did not unpark the clock thread");
    }

    #[test]
    fn upsert_keeps_the_keys_of_a_known_peer_and_gives_a_new_one_the_default() {
        let mut cfg = Config::default();
        upsert_peer(&mut cfg, "a", "h:1", Edge::Left, &[1; 32], KeyMap::Position);
        upsert_peer(&mut cfg, "a", "h:2", Edge::Right, &[2; 32], KeyMap::None);
        assert_eq!((cfg.peers.len(), cfg.peers[0].keys, cfg.peers[0].addr.as_str(), cfg.peers[0].edge), (1, KeyMap::Position, "h:2", Edge::Left));
    }

    mod loop_contract {
        use super::super::*;
        use flowseam_core::proto::{Edge as PEdge, ModifierSet};
        use flowseam_core::switch::SwitchPolicy as Policy;
        use std::sync::atomic::AtomicU64;
        use std::sync::mpsc::{channel, Receiver, Sender};

        static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());
        fn tlog(m: &str) {
            LOG.lock().unwrap().push(m.to_string());
        }
        fn logged(needle: &str) -> bool {
            LOG.lock().unwrap().iter().any(|l| l.contains(needle))
        }
        const NAMES: Names = Names { log: tlog, me: "this test", connect_to: "here" };

        struct Null;
        impl InputSink for Null {
            fn motion_rel(&mut self, _: i32, _: i32) {}
            fn button(&mut self, _: u8, _: bool) {}
            fn key(&mut self, _: u16, _: bool, _: bool) {}
            fn wheel(&mut self, _: i32, _: i32) {}
            fn warp(&mut self, _: proto::Point) {}
            fn set_grab(&mut self, _: bool) {}
            fn hide(&mut self) {}
            fn show(&mut self) {}
            fn set_association(&mut self, _: bool) {}
        }

        #[derive(Clone, Default)]
        struct Mail(Arc<Mutex<Vec<(String, Msg)>>>);
        impl Outbox for Mail {
            fn send(&mut self, peer: &str, msg: Msg) {
                self.0.lock().unwrap().push((peer.to_string(), msg));
            }
        }

        /// Records every call; `ticks` counts the clock reads, one per forwarder tick.
        struct Rec {
            tx: Sender<String>,
            ticks: Arc<AtomicU64>,
            back: Arc<AtomicBool>,
        }
        impl Rec {
            fn n(&self) -> u64 {
                self.ticks.load(Ordering::SeqCst)
            }
        }
        impl ClockHooks<Null, Mail> for Rec {
            fn start(&mut self) {
                let registered = CLOCKS.lock().unwrap().iter().any(|t| t.id() == thread::current().id());
                self.tx.send(format!("start registered={registered}")).unwrap();
            }
            fn woke(&mut self) {
                self.tx.send(format!("woke {}", self.n())).unwrap();
            }
            fn after_tick(&mut self, f: &mut Forwarder<Null, Mail>) {
                f.needs_tick();
                self.tx.send(format!("after_tick {}", self.n())).unwrap();
            }
            fn take_back(&mut self) -> bool {
                let b = self.back.swap(false, Ordering::SeqCst);
                if b {
                    self.tx.send("take_back".into()).unwrap();
                }
                b
            }
            fn driven_changed(&mut self, now: &Option<String>) {
                self.tx.send(format!("driven {now:?}")).unwrap();
            }
            fn end_turn(&mut self, _: &mut Forwarder<Null, Mail>) {
                self.tx.send(format!("end_turn {}", self.n())).unwrap();
            }
            fn finish(&mut self) {
                let registered = CLOCKS.lock().unwrap().iter().any(|t| t.id() == thread::current().id());
                self.tx.send(format!("finish registered={registered}")).unwrap();
            }
        }

        struct Rig {
            fwd: Arc<Mutex<Forwarder<Null, Mail>>>,
            mail: Mail,
            back: Arc<AtomicBool>,
            stop: Arc<AtomicBool>,
            rx: Receiver<String>,
            seen: Vec<String>,
            handle: thread::JoinHandle<()>,
        }

        impl Rig {
            /// `peer` is the one configured link; `others` are only asked on "back".
            fn start(peer: &str, speaks: &[(&str, u16)], ask: &[&str]) -> Rig {
                let cfg = FwdConfig {
                    peers: vec![PeerSpec { name: peer.into(), edge: PEdge::Left, keys: KeyMap::None, gestures: Default::default() }],
                    policy: Policy::default(),
                    boundary_px: 2,
                };
                let mail = Mail::default();
                let mut f = Forwarder::new(cfg, vec![Rect { x: 0, y: 0, w: 1440, h: 900 }], Null, mail.clone());
                f.link_up(peer);
                for (p, v) in speaks {
                    f.peer_speaks(p, *v);
                }
                let fwd = Arc::new(Mutex::new(f));
                let (tx, rx) = channel();
                let (ticks, back, stop) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
                let hook = Rec { tx, ticks: ticks.clone(), back: back.clone() };
                let (fwd2, mail2, stop2, ask) = (fwd.clone(), mail.clone(), stop.clone(), ask.iter().map(|s| s.to_string()).collect::<Vec<_>>());
                let handle = thread::spawn(move || {
                    let clock: Clock = Arc::new(move || ticks.fetch_add(1, Ordering::SeqCst) + 1);
                    let mut hook = hook;
                    clock_loop(&fwd2, mail2, &ClockEnv { clock: &clock, stop: &stop2, peers: &ask, home_asks: &MonitorsHome::default(), names: NAMES }, &mut hook);
                });
                Rig { fwd, mail, back, stop, rx, seen: Vec::new(), handle }
            }

            /// Waits at most 5 s in all, however many other events arrive: a hook that is never called must
            /// fail the test, not keep it waiting on the events that do come.
            fn wait_for(&mut self, what: &str) {
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                while !self.seen.iter().any(|e| e == what) {
                    let left = deadline.saturating_duration_since(std::time::Instant::now());
                    match self.rx.recv_timeout(left) {
                        Ok(e) => self.seen.push(e),
                        Err(_) => panic!("never saw {what:?}; saw {:?}", self.seen),
                    }
                }
            }

            fn enter(&self, peer: &str) {
                let mut f = self.fwd.lock().unwrap();
                f.on_msg(peer, Msg::Enter { seq: 7, edge: PEdge::Left, frac: 0x8000, mods: ModifierSet(0), pressed: vec![] }, 0);
                assert!(matches!(f.mode(), Mode::RemoteIn { .. }), "{:?}", f.mode());
                drop(f);
                wake_clocks();
            }

            fn finish(mut self) -> Vec<String> {
                self.stop.store(true, Ordering::SeqCst);
                wake_clocks();
                let id = self.handle.thread().id();
                self.handle.join().unwrap();
                self.seen.extend(self.rx.try_iter());
                assert!(CLOCKS.lock().unwrap().iter().all(|t| t.id() != id), "the clock thread is still in the wake registry");
                self.seen
            }
        }

        #[test]
        fn each_turn_runs_the_hooks_in_order_on_a_ticked_forwarder_and_reports_who_drives() {
            let mut r = Rig::start("lc-peer", &[], &[]);
            r.wait_for("end_turn 1");
            r.enter("lc-peer");
            r.wait_for("driven Some(\"lc-peer\")");
            r.fwd.lock().unwrap().link_down("lc-peer");
            wake_clocks();
            r.wait_for("driven None");
            let seen = r.finish();

            assert_eq!(seen[0], "start registered=true", "start runs once, after the thread joined the wake registry");
            assert_eq!(seen.iter().filter(|e| e.starts_with("start")).count(), 1);
            assert_eq!(seen.last().unwrap(), "finish registered=false", "finish runs once, after the thread left the registry");
            assert_eq!(seen.iter().filter(|e| e.starts_with("finish")).count(), 1);

            // Every turn is woke, after_tick, end_turn, and the 1-based tick count shows the forwarder had ticked
            // by the time after_tick and end_turn ran, and had not yet when woke ran.
            let body = &seen[1..seen.len() - 1];
            let mut turn = 0;
            let mut it = body.iter().filter(|e| !e.starts_with("driven"));
            while let Some(w) = it.next() {
                assert_eq!(w, &format!("woke {turn}"), "{seen:?}");
                turn += 1;
                assert_eq!(it.next().map(String::as_str), Some(format!("after_tick {turn}").as_str()), "{seen:?}");
                assert_eq!(it.next().map(String::as_str), Some(format!("end_turn {turn}").as_str()), "{seen:?}");
            }
            assert!(turn >= 2, "{seen:?}");

            let driven: Vec<&String> = seen.iter().filter(|e| e.starts_with("driven")).collect();
            assert_eq!(driven, ["driven Some(\"lc-peer\")", "driven None"], "each change is reported once");
            assert!(logged("lc-peer took control") && logged("control returned to lc-peer"));
        }

        #[test]
        fn back_hands_control_home_and_asks_only_peers_that_can_read_it() {
            let mut r = Rig::start("bk-new", &[("bk-new", 6), ("bk-old", 5)], &["bk-new", "bk-old", "bk-gone"]);
            r.wait_for("end_turn 1");
            r.enter("bk-new");
            r.wait_for("driven Some(\"bk-new\")");
            r.back.store(true, Ordering::SeqCst);
            wake_clocks();
            r.wait_for("take_back");
            r.wait_for("driven None");
            assert!(matches!(r.fwd.lock().unwrap().mode(), Mode::Local), "back released control");
            assert!(logged("control handed back from the menu"));
            assert!(logged("bk-old speaks protocol 5: it cannot be asked for its monitors (update it)"));
            assert!(logged("asked bk-new to bring its monitors home"));
            let asked: Vec<(String, Msg)> = r.mail.0.lock().unwrap().iter().filter(|(_, m)| *m == Msg::MonitorsHome).cloned().collect();
            assert_eq!(asked, [("bk-new".to_string(), Msg::MonitorsHome)], "only the peer that speaks 6 is sent it");

            // A second press with nobody driving still reaches the peers, and says so.
            r.mail.0.lock().unwrap().clear();
            r.back.store(true, Ordering::SeqCst);
            wake_clocks();
            let before = r.seen.iter().filter(|e| *e == "take_back").count();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while r.seen.iter().filter(|e| *e == "take_back").count() == before {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                r.seen.push(r.rx.recv_timeout(left).expect("a second take_back"));
            }
            let seen = r.finish();
            assert!(seen.iter().filter(|e| *e == "take_back").count() >= 2);
            assert!(logged("the menu asked to hand control back; this test was not being driven"));
        }
    }
}
