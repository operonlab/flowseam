//! Peer links for the worker: per peer one dial thread, plus one accept thread for the listener.
//! A session that reaches Hello registers a route (peer name -> outgoing channel) and calls
//! `Forwarder::link_up`; its end calls `link_down`. Inbound messages go to `Forwarder::on_msg`.
//!
//! Peers are identified by the certificate fingerprint prefix (`PeerInfo.node`), not by the name the
//! peer announces. `node` arrives in the peer's Hello, so on its own it is only a claim; flowseam_net's link
//! refuses any Hello whose `node` differs from the certificate the pinned handshake verified
//! (`link::cert_node`), which is what makes keying on it safe here.
//!
//! The same layer runs on every platform; what it needs from the machine it runs on is in [`LinkHost`].

use crate::ifaces::{self, LocalIf};
use flowseam_core::clipboard::ClipboardOs;
use flowseam_core::forward::{Forwarder, InputSink, Outbox};
use flowseam_core::proto::{Kind, MonitorId, Msg, PathAddr, MAX_PATH_ADDRS, MIN_PROTO_VERSION, PROTO_VERSION};
use flowseam_core::status::{self, LinkStatus, PeerStatus};
use crate::paths::{path_rank, Candidates, SessionKey};
use crate::clipboard::{run_poll, ClipboardPump};
use crate::files::{DragHub, DragPump, SendFn};
use crate::identity::Identity;
use crate::link::{run_client_once_with, serve_stream_with, Endpoint, LinkUp};
use crate::pins::PinSet;
use crate::session::{PeerInfo, PeerPaths as AdvertisedPaths, Timing};
use crate::NetError;
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::io;
use std::path::{Path, PathBuf};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Sender};
use crossbeam_channel::{unbounded, Receiver as OutReceiver, Sender as OutSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

type Routes = Arc<Mutex<HashMap<String, (u64, OutSender<Msg>, SessionKey, SocketAddr)>>>;
/// The forwarder every session feeds, whatever input sink this platform gives it.
pub type Shared<S> = Arc<Mutex<Forwarder<S, RouteOutbox>>>;
/// Milliseconds on the forwarder's clock.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// What the link layer needs from the machine it runs on.
#[derive(Clone, Copy)]
pub struct LinkHost {
    /// This machine's interfaces, each with the kind of network it is on.
    pub interfaces: fn() -> Vec<LocalIf>,
    /// Every local address, or `None` when they cannot be listed: a path is then never judged lost, since
    /// an empty list would make every live link look as if its address went away.
    pub addrs: fn() -> Option<Vec<IpAddr>>,
    /// Free space on the volume holding a path, for an arriving file.
    pub free_bytes: fn(&Path) -> Result<u64, String>,
    /// The forwarder needs ticks (an edge dwell is running): wake whatever ticks it, if it is idle.
    pub wake: fn(),
}
pub type PeerMonitors = Arc<Mutex<HashMap<String, Option<Vec<MonitorId>>>>>;
/// Peers whose "back" button asked for their monitors to come home (`Msg::MonitorsHome`), oldest first;
/// the worker takes them.
pub type MonitorsHome = Arc<Mutex<Vec<String>>>;

struct PeerPaths { cands: Candidates, speak: u16, attempt: Option<SocketAddr> }

#[derive(Debug, PartialEq, Eq)]
enum Reg { New, Replaced, Refused }
fn decide(existing: Option<SessionKey>, new: SessionKey, drag_busy: bool) -> Reg {
    match existing { None => Reg::New, Some(old) if new < old && !drag_busy => Reg::Replaced, _ => Reg::Refused }
}
fn key_for(up: &LinkUp, ifs: &[LocalIf], i_am_lower: bool) -> SessionKey {
    SessionKey::new(path_rank(ifaces::kind_of_local(ifs, up.local.ip()), up.paths.as_ref().map_or(Kind::Other, |p| p.kind)), up.dialed == i_am_lower)
}
fn path_lost(local: IpAddr, addrs: &[IpAddr]) -> bool { !addrs.contains(&local) }
fn status_kind(local: Kind, remote: Option<Kind>) -> Option<Kind> {
    match (local, remote) {
        (Kind::Other, _) | (_, None | Some(Kind::Other)) => None,
        (a, Some(b)) => Some(if a.rank() >= b.rank() { a } else { b }),
    }
}
fn describe(e: &NetError) -> String {
    match e {
        NetError::Io(err) if err.kind() == io::ErrorKind::ConnectionRefused => "對方沒有開啟 Flowseam".into(),
        NetError::Io(err) if err.kind() == io::ErrorKind::TimedOut => "連線逾時".into(),
        NetError::Io(err) if matches!(err.kind(), io::ErrorKind::HostUnreachable | io::ErrorKind::NetworkUnreachable) || matches!(err.raw_os_error(), Some(64 | 65)) => "找不到對方（不在網路上）".into(),
        NetError::VersionMismatch { .. } => "版本不同，正在改用相容版本".into(),
        NetError::Timeout => "對方沒有回應（程式可能卡住）".into(),
        NetError::Rejected(reason) => format!("對方拒絕：{reason}"),
        _ => format!("無法連線：{e:?}").chars().take(60).collect(),
    }
}
fn mark_down(board: &Mutex<HashMap<String, PeerStatus>>, name: &str) {
    if let Some(peer) = board.lock().unwrap_or_else(|e| e.into_inner()).get_mut(name) {
        peer.up = false;
        peer.kind = None;
        peer.remote = None;
        peer.since_ms = status::now_ms();
        peer.reason = Some("連線中斷".into());
    }
}
fn drag_busy(drags: Option<&DragCfg>, name: &str) -> bool {
    drags.is_some_and(|d| d.hub.lock().unwrap_or_else(|e| e.into_inner()).busy(name))
}

/// Sends to the peer's live session; a peer with no session drops the message loudly.
#[derive(Clone, Default)]
pub struct RouteOutbox(Routes);

impl Outbox for RouteOutbox {
    fn send(&mut self, peer: &str, msg: Msg) {
        let routes = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match routes.get(peer) {
            Some((_, tx, ..)) => {
                if tx.send(msg).is_err() {
                    flowseam_core::log::line(&format!("flowseam: link to {peer} closing; message dropped"));
                }
            }
            None => flowseam_core::log::line(&format!("flowseam: no live link to {peer}; message dropped")),
        }
    }
}

pub struct LinkParams {
    pub identity: Identity,
    pub me: PeerInfo,
    pub monitors: Vec<MonitorId>,
    pub peer_monitors: PeerMonitors,
    pub monitors_home: MonitorsHome,
    pub pins: PinSet,
    /// (configured name, dial address, node id = first 16 bytes of the fingerprint)
    pub peers: Vec<(String, SocketAddr, [u8; 16])>,
    pub listen: SocketAddr,
    pub timing: Timing,
    pub status: Option<PathBuf>,
    pub host: LinkHost,
}

/// File drags: every session registers its pump with `hub`. Unfinished files wait in `staging`; a drop
/// moves them to `dest`.
#[derive(Clone)]
pub struct DragCfg {
    pub hub: Arc<Mutex<DragHub>>,
    pub staging: PathBuf,
    pub dest: PathBuf,
    /// Carries an arriving drag on as a system drag here; `None` drops on release into `dest`.
    pub native: Option<Arc<dyn crate::files::NativeDrop>>,
}

struct Ctx<S: InputSink + Send + 'static> {
    p: LinkParams,
    fwd: Shared<S>,
    routes: Routes,
    paths: Mutex<HashMap<String, PeerPaths>>,
    board: Board,
    clock: Clock,
    stop: Arc<AtomicBool>,
    gens: AtomicU64,
    /// Called when a link thread panics (after it was caught).
    on_panic: Arc<dyn Fn() + Send + Sync>,
    clip: Option<Arc<dyn ClipboardOs + Send + Sync>>,
    drags: Option<DragCfg>,
}

const CLIPBOARD_POLL: Duration = Duration::from_millis(500);

type Board = Arc<Mutex<HashMap<String, PeerStatus>>>;

/// Each configured peer's link as the window shows it, for a platform that writes status.toml itself.
#[derive(Clone)]
pub struct StatusBoard(Board);

impl StatusBoard {
    /// Every configured peer, by name.
    pub fn peers(&self) -> Vec<PeerStatus> {
        let mut peers: Vec<_> = self.0.lock().unwrap_or_else(|e| e.into_inner()).values().cloned().collect();
        peers.sort_by(|a, b| a.name.cmp(&b.name));
        peers
    }
}

pub struct Links {
    stop: Arc<AtomicBool>,
    routes: Routes,
    wake: SocketAddr,
    threads: Vec<JoinHandle<()>>,
    board: Board,
}

impl Links {
    pub fn start<S: InputSink + Send + 'static>(
        p: LinkParams,
        fwd: Shared<S>,
        routes: RouteOutbox,
        clock: Clock,
        on_panic: Arc<dyn Fn() + Send + Sync>,
        clip: Option<Arc<dyn ClipboardOs + Send + Sync>>,
        drags: Option<DragCfg>,
    ) -> Result<Links, NetError> {
        let listener = TcpListener::bind(p.listen).map_err(NetError::Io)?;
        let wake = listener.local_addr().map_err(NetError::Io)?;
        let stop = Arc::new(AtomicBool::new(false));
        let paths = p.peers.iter().map(|(name, addr, _)| { let mut cands = Candidates::new(); cands.add_hint(*addr); (name.clone(), PeerPaths { cands, speak: PROTO_VERSION, attempt: None }) }).collect();
        let board: Board = Arc::new(Mutex::new(p.peers.iter().map(|(name, _, _)| (name.clone(), PeerStatus { name: name.clone(), up: false, kind: None, remote: None, since_ms: status::now_ms(), reason: None })).collect()));
        let ctx = Arc::new(Ctx { p, fwd, routes: routes.0.clone(), paths: Mutex::new(paths), board: board.clone(), clock, stop: stop.clone(), gens: AtomicU64::new(0), on_panic, clip, drags });
        let mut threads = Vec::new();
        {
            let ctx = ctx.clone();
            threads.push(guarded("flowseam-link-accept", ctx.clone(), move || accept_loop(&ctx, listener)));
        }
        for i in 0..ctx.p.peers.len() {
            let ctx = ctx.clone();
            threads.push(guarded("flowseam-link-dial", ctx.clone(), move || dial_loop(&ctx, i)));
        }
        if let Some(path) = ctx.p.status.clone() {
            let ctx = ctx.clone();
            threads.push(guarded("flowseam-link-status", ctx.clone(), move || {
                let mut logged_errors = HashSet::new();
                while !ctx.stop.load(Ordering::SeqCst) {
                    let mut peers: Vec<_> = ctx.board.lock().unwrap_or_else(|e| e.into_inner()).values().cloned().collect();
                    peers.sort_by(|a, b| a.name.cmp(&b.name));
                    let driven_by = crate::node::driven_by(ctx.fwd.lock().unwrap_or_else(|e| e.into_inner()).mode());
                    match status::write(&path, &LinkStatus { written_ms: status::now_ms(), peers, driven_by }) {
                        Ok(()) => {}
                        Err(e) => {
                            let error = e.to_string();
                            if logged_errors.insert(error.clone()) { flowseam_core::log::line(&format!("flowseam: status write: {error}")); }
                        }
                    }
                    thread::sleep(Duration::from_secs(1));
                }
            }));
        }
        Ok(Links { stop, routes: routes.0, wake, threads, board })
    }

    /// The address the listener took (`listen` may name port 0).
    pub fn listening(&self) -> SocketAddr {
        self.wake
    }

    /// Peers with a live session now.
    pub fn live(&self) -> usize {
        self.routes.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn status_board(&self) -> StatusBoard {
        StatusBoard(self.board.clone())
    }

    /// Ends every session (dropping the route senders closes them) and joins the threads.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.routes.lock().unwrap_or_else(|e| e.into_inner()).clear();
        // Wake a blocked accept(); the bogus connection fails its handshake at once.
        let _ = TcpStream::connect_timeout(&self.wake, Duration::from_millis(500));
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// Run `f`; a panic is caught, reported, and handed to `on_panic`. Returns false if it panicked.
pub fn run_guarded(f: impl FnOnce(), on_panic: &dyn Fn()) -> bool {
    if catch_unwind(AssertUnwindSafe(f)).is_err() {
        flowseam_core::log::line("flowseam: link thread panicked");
        on_panic();
        return false;
    }
    true
}

fn guarded<S: InputSink + Send + 'static>(name: &str, ctx: Arc<Ctx<S>>, f: impl FnOnce() + Send + 'static) -> JoinHandle<()> {
    thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            run_guarded(f, &*ctx.on_panic);
        })
        .expect("spawn link thread")
}

/// The route for `name` is gone: tell the forwarder, unless a session for `name` registered since. Checked
/// under the forwarder's lock, in the order the forwarder itself takes them (forwarder, then routes), so a
/// session that registers afterwards finds the forwarder already down and its link_up brings it back. A
/// link_down landing after a new session's link_up would mark a live link down until that session ended.
fn link_down_unless_relinked<F>(fwd: &Mutex<F>, routes: &Routes, name: &str, down: impl FnOnce(&mut F)) -> bool {
    let mut f = fwd.lock().unwrap_or_else(|e| e.into_inner());
    if routes.lock().unwrap_or_else(|e| e.into_inner()).contains_key(name) {
        return false;
    }
    down(&mut f);
    true
}

fn dial_loop<S: InputSink + Send + 'static>(ctx: &Arc<Ctx<S>>, i: usize) {
    let (name, _, peer_node) = &ctx.p.peers[i];
    let i_am_lower = ctx.p.me.node < *peer_node;
    while !ctx.stop.load(Ordering::SeqCst) {
        let ifs = (ctx.p.host.interfaces)();
        let addrs = (ctx.p.host.addrs)();
        let lost = {
            let mut routes = ctx.routes.lock().unwrap_or_else(|e| e.into_inner());
            let lost = addrs.as_ref().and_then(|addrs| routes.get(name).and_then(|(gen, _, _, local)| path_lost(local.ip(), addrs).then_some((*gen, *local))));
            if let Some((gen, _)) = lost { if routes.get(name).is_some_and(|(g, ..)| *g == gen) { routes.remove(name); ctx.p.peer_monitors.lock().unwrap_or_else(|e| e.into_inner()).remove(name); mark_down(&ctx.board, name); } }
            lost
        };
        if let Some((_, local)) = lost {
            flowseam_core::log::line(&format!("flowseam: {name}: path via {local} lost"));
            link_down_unless_relinked(&*ctx.fwd, &ctx.routes, name, |f| f.link_down(name));
        }
        let current = ctx.routes.lock().unwrap_or_else(|e| e.into_inner()).get(name).map(|(_, _, key, _)| *key);
        let busy = drag_busy(ctx.drags.as_ref(), name);
        let addr = {
            let mut paths = ctx.paths.lock().unwrap_or_else(|e| e.into_inner());
            let state = paths.get_mut(name).expect("configured peer has paths");
            if state.attempt.is_some() || busy { None } else {
                let addr = state.cands.next_dial(Instant::now(), &|a| ifaces::kind_toward(&ifs, a.ip()), current, i_am_lower);
                state.attempt = addr;
                addr
            }
        };
        if let Some(addr) = addr {
            let ctx = ctx.clone();
            let name = name.clone();
            let manager = thread::current();
            guarded("flowseam-link-attempt", ctx.clone(), move || {
                let result = session(&ctx, |ep, o, inc, up| run_client_once_with(addr, ep, o, inc, up));
                let mut paths = ctx.paths.lock().unwrap_or_else(|e| e.into_inner());
                let state = paths.get_mut(&name).expect("configured peer has paths");
                if let Err(e) = &result {
                    let mut board = ctx.board.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(peer) = board.get_mut(&name) { if !peer.up { peer.reason = Some(describe(e)); } }
                }
                match result {
                    Err(NetError::VersionMismatch { theirs, .. }) if theirs >= MIN_PROTO_VERSION => { state.speak = theirs; }
                    Err(e) => { flowseam_core::log::line(&format!("flowseam: dial {addr}: {e:?}")); state.cands.dial_failed(addr, Instant::now()); }
                    Ok(true) => state.cands.dial_ok(addr),
                    Ok(false) => state.cands.dial_failed(addr, Instant::now()),
                }
                if state.attempt == Some(addr) { state.attempt = None; }
                manager.unpark();
            });
        }
        thread::park_timeout(Duration::from_millis(500));
    }
}

/// Each accepted connection is served on its own thread: a peer that can only connect to this machine (its
/// address here is a placeholder) must not wait for another peer's session to end, nor behind a handshake
/// that stalls.
fn accept_loop<S: InputSink + Send + 'static>(ctx: &Arc<Ctx<S>>, listener: TcpListener) {
    while !ctx.stop.load(Ordering::SeqCst) {
        let sock = match listener.accept() {
            Ok((sock, _)) => sock,
            Err(e) => {
                if !ctx.stop.load(Ordering::SeqCst) {
                    flowseam_core::log::line(&format!("flowseam: accept: {e:?}"));
                    thread::sleep(Duration::from_millis(100));
                }
                continue;
            }
        };
        if ctx.stop.load(Ordering::SeqCst) {
            break;
        }
        let inner = ctx.clone();
        guarded("flowseam-link-serve", ctx.clone(), move || {
            if let Err(e) = session(&inner, |ep, o, inc, up| serve_stream_with(sock, ep, o, inc, up)) {
                if !inner.stop.load(Ordering::SeqCst) {
                    flowseam_core::log::line(&format!("flowseam: accept: {e:?}"));
                }
            }
        });
    }
}

/// Run one session. `Ok(true)` if it reached Hello and was registered.
fn session<S: InputSink + Send + 'static>(
    ctx: &Ctx<S>,
    run: impl FnOnce(&Endpoint<'_>, &OutReceiver<Msg>, &Sender<Msg>, &dyn Fn(&crate::link::LinkUp)) -> Result<crate::link::Outcome, NetError>,
) -> Result<bool, NetError> {
    let (out_tx, out_rx) = unbounded::<Msg>();
    let (in_tx, in_rx) = channel::<Msg>();
    let slot = Mutex::new(Some((out_tx, in_rx)));
    let reg: Mutex<Option<(String, u64)>> = Mutex::new(None);
    let drag_pump: Mutex<Option<Arc<Mutex<DragPump>>>> = Mutex::new(None);
    let speak_for = |node| ctx.p.peers.iter().find(|p| p.2 == node).and_then(|p| ctx.paths.lock().unwrap_or_else(|e| e.into_inner()).get(&p.0).map(|s| s.speak)).unwrap_or(PROTO_VERSION);
    let paths_for = |local: SocketAddr| AdvertisedPaths { kind: ifaces::kind_of_local(&(ctx.p.host.interfaces)(), local.ip()), addrs: (ctx.p.host.interfaces)().into_iter().take(MAX_PATH_ADDRS).map(|i| PathAddr { addr: (i.addr, ctx.p.listen.port()).into(), kind: i.kind }).collect() };
    let ep = Endpoint { identity: &ctx.p.identity, pins: &ctx.p.pins, me: &ctx.p.me, monitors: &ctx.p.monitors, timing: ctx.p.timing, speak_for: Some(&speak_for), paths_for: Some(&paths_for) };
    let up = |up: &crate::link::LinkUp| {
        let Some((out_tx, in_rx)) = slot.lock().unwrap_or_else(|e| e.into_inner()).take() else { return };
        let Some((name, ..)) = ctx.p.peers.iter().find(|p| p.2 == up.peer.node) else {
            flowseam_core::log::line("flowseam: link from unknown node; closing");
            return;
        };
        let name = name.clone();
        let key = key_for(up, &(ctx.p.host.interfaces)(), ctx.p.me.node < up.peer.node);
        let busy = drag_busy(ctx.drags.as_ref(), &name);
        let outcome = {
            let mut routes = ctx.routes.lock().unwrap_or_else(|e| e.into_inner());
            let old = routes.get(&name).map(|(_, _, k, _)| *k);
            let outcome = decide(old, key, busy);
            match outcome {
                Reg::New | Reg::Replaced => {
                    let gen = ctx.gens.fetch_add(1, Ordering::SeqCst);
                    routes.insert(name.clone(), (gen, out_tx, key, up.local));
                    ctx.p.peer_monitors.lock().unwrap_or_else(|e| e.into_inner()).insert(name.clone(), up.monitors.clone());
                    if let Some(peer) = ctx.board.lock().unwrap_or_else(|e| e.into_inner()).get_mut(&name) {
                        peer.up = true;
                        peer.kind = status_kind(ifaces::kind_of_local(&(ctx.p.host.interfaces)(), up.local.ip()), up.paths.as_ref().map(|p| p.kind));
                        peer.remote = Some(up.remote.to_string());
                        peer.since_ms = status::now_ms();
                        peer.reason = None;
                    }
                    *reg.lock().unwrap_or_else(|e| e.into_inner()) = Some((name.clone(), gen));
                }
                Reg::Refused => flowseam_core::log::line(&format!("flowseam: second link to {name}; closing it (old {old:?}, new {key:?})")),
            }
            outcome
        };
        if let Some(paths) = &up.paths { ctx.paths.lock().unwrap_or_else(|e| e.into_inner()).get_mut(&name).expect("configured peer has paths").cands.learn(paths, Instant::now()); }
        if outcome == Reg::Refused { return; }
        if outcome == Reg::New {
            let need = {
                let mut f = ctx.fwd.lock().unwrap_or_else(|e| e.into_inner());
                f.peer_speaks(&name, up.ver);
                f.link_up(&name);
                f.needs_tick()
            };
            if need { (ctx.p.host.wake)(); }
            flowseam_core::log::line(&format!("flowseam: link up with {name}"));
        }
        if outcome == Reg::Replaced {
            // The new session may speak another version: a peer rolled back would drop the link on a Gesture.
            ctx.fwd.lock().unwrap_or_else(|e| e.into_inner()).peer_speaks(&name, up.ver);
            flowseam_core::log::line(&format!("flowseam: {name}: path upgraded to {} ({:?})", up.remote, ifaces::kind_of_local(&(ctx.p.host.interfaces)(), up.local.ip())));
        }
        if up.dialed { let mut paths = ctx.paths.lock().unwrap_or_else(|e| e.into_inner()); if let Some(state) = paths.get_mut(&name) { state.attempt = None; } }
        let gen = reg.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|r| r.1);
        // Sends through the route table (not a held sender), so a replaced or removed route refuses.
        let route_send: SendFn = {
            let (routes, name) = (ctx.routes.clone(), name.clone());
            Arc::new(move |m: Msg| {
                let r = routes.lock().unwrap_or_else(|e| e.into_inner());
                match r.get(&name) {
                    Some((g, tx, ..)) if Some(*g) == gen => tx.send(m).is_ok(),
                    _ => false,
                }
            })
        };
        let drags = ctx.drags.as_ref().map(|d| {
            let pump = Arc::new(Mutex::new(
                match d.native.clone() {
                    Some(n) => DragPump::new(d.staging.clone(), d.dest.clone()).with_native(n),
                    None => DragPump::new(d.staging.clone(), d.dest.clone()),
                }
                .with_free_space(Arc::new(ctx.p.host.free_bytes)),
            ));
            let mut hub = d.hub.lock().unwrap_or_else(|e| e.into_inner());
            hub.attach(&name, pump.clone(), route_send.clone());
            *drag_pump.lock().unwrap_or_else(|e| e.into_inner()) = Some(pump);
            hub.inbox(&name).expect("just attached")
        });
        let pump = ctx.clip.as_ref().map(|os| Arc::new(Mutex::new(ClipboardPump::new(os.clone()).with_log(flowseam_core::log::line))));
        if let Some(pump) = &pump {
            let (pump, routes, stop, name) = (pump.clone(), ctx.routes.clone(), ctx.stop.clone(), name.clone());
            let gen = reg.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|r| r.1);
            let on_panic = ctx.on_panic.clone();
            thread::spawn(move || {
                // Sends through the route table (not a held sender) so the session can still end
                // when the route is removed; a replaced or removed route stops the poller.
                let send = |m: Msg| {
                    let r = routes.lock().unwrap_or_else(|e| e.into_inner());
                    match r.get(&name) {
                        Some((g, tx, ..)) if Some(*g) == gen => tx.send(m).is_ok(),
                        _ => false,
                    }
                };
                let ended = || {
                    stop.load(Ordering::SeqCst)
                        || !routes.lock().unwrap_or_else(|e| e.into_inner()).get(&name).is_some_and(|(g, ..)| Some(*g) == gen)
                };
                run_guarded(|| run_poll(&pump, &send, &ended, CLIPBOARD_POLL), &*on_panic);
            });
        }
        let (fwd, clock, on_panic, wake) = (ctx.fwd.clone(), ctx.clock.clone(), ctx.on_panic.clone(), ctx.p.host.wake);
        let home_asks = ctx.p.monitors_home.clone();
        thread::spawn(move || {
            run_guarded(
                || {
                    while let Ok(msg) = in_rx.recv() {
                        if matches!(msg, Msg::DragSoon { .. } | Msg::DragBegin { .. } | Msg::FileChunk { .. } | Msg::DragAck { .. } | Msg::DragCancel { .. }) {
                            // Queued, not handled here: a slow disk must not hold a lock the event tap waits on.
                            if let Some(inbox) = &drags {
                                inbox.deliver(msg);
                            }
                            continue;
                        }
                        if let Msg::ClipboardText { gen, text } = &msg {
                            if let Some(pump) = &pump {
                                pump.lock().unwrap_or_else(|e| e.into_inner()).on_remote(*gen, text);
                            }
                            continue;
                        }
                        if matches!(msg, Msg::MonitorsHome) {
                            flowseam_core::log::line(&format!("flowseam: {name} asked for its monitors to come home"));
                            home_asks.lock().unwrap_or_else(|e| e.into_inner()).push(name.clone());
                            wake();
                            continue;
                        }
                        let now = clock();
                        let need = {
                            let mut f = fwd.lock().unwrap_or_else(|e| e.into_inner());
                            f.on_msg(&name, msg, now);
                            f.needs_tick()
                        };
                        if need { wake(); }
                    }
                },
                &*on_panic,
            );
        });
    };
    let r = run(&ep, &out_rx, &in_tx, &up);
    drop(in_tx);
    if let (Some(d), Some(pump)) = (&ctx.drags, drag_pump.lock().unwrap_or_else(|e| e.into_inner()).take()) {
        // Unregistering drops the hub's handle; the router thread's handle goes when it drains, and
        // with the last one the staged files are deleted and a running send is cancelled.
        if let Some(name) = reg.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|r| r.0.clone()) {
            d.hub.lock().unwrap_or_else(|e| e.into_inner()).detach(&name, &pump);
        }
    }
    let registered = reg.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some((name, gen)) = &registered {
        let mut routes = ctx.routes.lock().unwrap_or_else(|e| e.into_inner());
        if routes.get(name).is_some_and(|(g, ..)| g == gen) {
            routes.remove(name);
            ctx.p.peer_monitors.lock().unwrap_or_else(|e| e.into_inner()).remove(name);
            mark_down(&ctx.board, name);
            drop(routes);
            if link_down_unless_relinked(&*ctx.fwd, &ctx.routes, name, |f| f.link_down(name)) {
                flowseam_core::log::line(&format!("flowseam: link down with {name}"));
            }
        }
    }
    r.map(|_| registered.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flowseam_core::forward::{FwdConfig, Mode, PeerSpec};
    use flowseam_core::proto::{Edge, Point, Rect};
    use flowseam_core::switch::SwitchPolicy;
    use crate::identity::fingerprint;
    use std::net::Ipv4Addr;

    /// A route removed, and a session for the same peer registered before the forwarder heard: the late
    /// link_down must leave the new session's link up. With no session since, it goes through.
    #[test]
    fn a_late_link_down_spares_a_session_that_registered_since() {
        let routes: Routes = Arc::new(Mutex::new(HashMap::new()));
        let fwd = Mutex::new(Vec::<&str>::new());
        assert!(link_down_unless_relinked(&fwd, &routes, "mbp", |f| f.push("down")));
        let (tx, _rx) = unbounded();
        routes.lock().unwrap().insert("mbp".into(), (7, tx, SessionKey::new(2, true), "192.0.2.1:24800".parse().unwrap()));
        assert!(!link_down_unless_relinked(&fwd, &routes, "mbp", |f| f.push("down")));
        assert_eq!(*fwd.lock().unwrap(), ["down"], "the late link_down reached the forwarder");
    }

    #[test]
    fn path_lost_checks_all_local_address_kinds() {
        let addrs: Vec<IpAddr> = vec!["127.0.0.1".parse().unwrap(), "10.8.0.2".parse().unwrap(), "fe80::1234".parse().unwrap()];
        assert!(!path_lost("127.0.0.1".parse().unwrap(), &addrs));
        assert!(!path_lost("10.8.0.2".parse().unwrap(), &addrs));
        assert!(!path_lost("fe80::1234".parse().unwrap(), &addrs));
        assert!(path_lost("169.254.192.119".parse().unwrap(), &addrs));
    }

    #[test]
    fn status_kind_uses_worse_known_path() {
        assert_eq!(status_kind(Kind::Wired, Some(Kind::Wired)), Some(Kind::Wired));
        assert_eq!(status_kind(Kind::Wired, Some(Kind::WiFi)), Some(Kind::WiFi));
        assert_eq!(status_kind(Kind::WiFi, Some(Kind::Wired)), Some(Kind::WiFi));
        assert_eq!(status_kind(Kind::Other, Some(Kind::Wired)), None);
        assert_eq!(status_kind(Kind::Wired, None), None);
    }

    #[test]
    fn describe_dial_errors() {
        assert_eq!(describe(&NetError::Io(io::Error::from(io::ErrorKind::ConnectionRefused))), "對方沒有開啟 Flowseam");
        assert_eq!(describe(&NetError::Io(io::Error::from(io::ErrorKind::TimedOut))), "連線逾時");
        for kind in [io::ErrorKind::HostUnreachable, io::ErrorKind::NetworkUnreachable] {
            assert_eq!(describe(&NetError::Io(io::Error::from(kind))), "找不到對方（不在網路上）");
        }
        for code in [64, 65] { assert_eq!(describe(&NetError::Io(io::Error::from_raw_os_error(code))), "找不到對方（不在網路上）"); }
        assert_eq!(describe(&NetError::VersionMismatch { ours: 2, theirs: 1 }), "版本不同，正在改用相容版本");
        assert_eq!(describe(&NetError::Rejected("busy".into())), "對方拒絕：busy");
        assert_eq!(describe(&NetError::Timeout), "對方沒有回應（程式可能卡住）");
        let fallback = describe(&NetError::Proto("x".repeat(100)));
        assert!(fallback.starts_with("無法連線："));
        assert_eq!(fallback.chars().count(), 60);
    }

    #[test]
    fn registration_prefers_better_unless_drag_busy() {
        let best = SessionKey::new(0, true);
        let worse = SessionKey::new(2, false);
        assert_eq!(decide(None, worse, false), Reg::New);
        assert_eq!(decide(Some(worse), best, false), Reg::Replaced);
        assert_eq!(decide(Some(worse), best, true), Reg::Refused);
        assert_eq!(decide(Some(best), worse, false), Reg::Refused);
        assert_eq!(decide(Some(best), best, false), Reg::Refused);
    }

    #[test]
    fn session_key_uses_worse_interface_and_lower_dialer() {
        let ifs = vec![LocalIf { name: "bridge0".into(), addr: "169.254.192.119".parse().unwrap(), mask: "255.255.0.0".parse().unwrap(), kind: Kind::Wired },
            LocalIf { name: "en1".into(), addr: "192.0.2.60".parse().unwrap(), mask: "255.255.255.0".parse().unwrap(), kind: Kind::WiFi }];
        let mut up = LinkUp { peer: PeerInfo { node: [0; 16], name: "p".into(), displays: vec![] }, ver: PROTO_VERSION,
            paths: Some(AdvertisedPaths { kind: Kind::Wired, addrs: vec![] }), monitors: None, local: "169.254.192.119:12".parse().unwrap(), remote: "169.254.1.2:13".parse().unwrap(), dialed: true };
        assert_eq!(key_for(&up, &ifs, true), SessionKey::new(0, true));
        assert_eq!(key_for(&up, &ifs, false), SessionKey::new(0, false));
        up.paths.as_mut().unwrap().kind = Kind::WiFi;
        assert_eq!(key_for(&up, &ifs, true).rank, 2);
        up.paths.as_mut().unwrap().kind = Kind::Wired;
        up.local = "192.0.2.60:12".parse().unwrap();
        assert_eq!(key_for(&up, &ifs, true).rank, 2);
        up.dialed = false;
        assert_eq!(key_for(&up, &ifs, false), SessionKey::new(2, true));
    }

    /// The status file names the peer driving this machine while one does, and nobody after.
    #[test]
    fn the_status_file_reports_who_drives_this_machine() {
        use flowseam_core::proto::{ModifierSet, Msg};
        let id = Identity::generate().unwrap();
        let screen = Rect { x: 0, y: 0, w: 1000, h: 800 };
        let routes = RouteOutbox::default();
        let cfg = FwdConfig { peers: vec![PeerSpec { name: "ctl".into(), edge: Edge::Left, keys: flowseam_core::config::KeyMap::None, gestures: Default::default() }], policy: SwitchPolicy::default(), boundary_px: 2 };
        let fwd = Arc::new(Mutex::new(Forwarder::new(cfg, vec![screen], NullSink, routes.clone())));
        fwd.lock().unwrap().link_up("ctl");
        let path = std::env::temp_dir().join(format!("flowseam-drivenby-{}-{}.toml", std::process::id(), status::now_ms()));
        let params = LinkParams {
            identity: id.clone(),
            me: PeerInfo { node: id.fingerprint()[..16].try_into().unwrap(), name: "me".into(), displays: vec![screen] },
            monitors: vec![],
            peer_monitors: PeerMonitors::default(),
            monitors_home: MonitorsHome::default(),
            pins: PinSet::new([fingerprint(&Identity::generate().unwrap().cert_der)]),
            peers: vec![("ctl".into(), "127.0.0.1:1".parse().unwrap(), [1; 16])],
            listen: "127.0.0.1:0".parse().unwrap(),
            timing: timing(),
            status: Some(path.clone()),
            host: loopback(),
        };
        let t0 = Instant::now();
        let clock: Clock = Arc::new(move || t0.elapsed().as_millis() as u64);
        let links = Links::start(params, fwd.clone(), routes, clock, Arc::new(|| {}), None, None).unwrap();
        let driven = || status::read(&path).ok().and_then(|s| s.driven_by);
        assert!(wait(5, || status::read(&path).is_ok()), "no status written");
        assert_eq!(driven(), None, "nobody drives it yet");
        fwd.lock().unwrap().on_msg("ctl", Msg::Enter { seq: 7, edge: Edge::Left, frac: 0x8000, mods: ModifierSet(0), pressed: vec![] }, 0);
        assert!(matches!(fwd.lock().unwrap().mode(), Mode::RemoteIn { .. }));
        assert!(wait(5, || driven().as_deref() == Some("ctl")), "status never named the driver: {:?}", status::read(&path));
        assert!(fwd.lock().unwrap().release_control());
        assert!(wait(5, || driven().is_none()), "status kept naming the driver after control was released");
        links.stop();
        let _ = std::fs::remove_file(&path);
    }

    /// An input sink that does nothing: these tests are about the links, on any platform.
    struct NullSink;
    impl InputSink for NullSink {
        fn motion_rel(&mut self, _: i32, _: i32) {}
        fn button(&mut self, _: u8, _: bool) {}
        fn key(&mut self, _: u16, _: bool, _: bool) {}
        fn wheel(&mut self, _: i32, _: i32) {}
        fn warp(&mut self, _: Point) {}
        fn set_grab(&mut self, _: bool) {}
        fn hide(&mut self) {}
        fn show(&mut self) {}
        fn set_association(&mut self, _: bool) {}
    }

    struct Node {
        fwd: Shared<NullSink>,
        links: Links,
        routes: RouteOutbox,
        home_asks: MonitorsHome,
    }

    fn timing() -> Timing {
        Timing { ping_every: Duration::from_millis(50), dead_after: Duration::from_secs(2), handshake_timeout: Duration::from_secs(3) }
    }

    /// Loopback only: the sessions run on 127.0.0.1, which is always among the addresses.
    fn loopback() -> LinkHost {
        LinkHost { interfaces: Vec::new, addrs: || Some(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]), free_bytes: |_| Ok(u64::MAX), wake: || {} }
    }

    fn node(name: &str, id: &Identity, other: &Identity, other_name: &str, edge: Edge, dial: SocketAddr) -> Node {
        node_on(name, id, other, other_name, edge, dial, None, loopback())
    }

    #[allow(clippy::too_many_arguments)]
    fn node_on(name: &str, id: &Identity, other: &Identity, other_name: &str, edge: Edge, dial: SocketAddr, clip: Option<Arc<dyn ClipboardOs + Send + Sync>>, host: LinkHost) -> Node {
        let screen = Rect { x: 0, y: 0, w: 1000, h: 800 };
        let routes = RouteOutbox::default();
        let cfg = FwdConfig { peers: vec![PeerSpec { name: other_name.into(), edge, keys: flowseam_core::config::KeyMap::None, gestures: Default::default() }], policy: SwitchPolicy::default(), boundary_px: 2 };
        let fwd = Arc::new(Mutex::new(Forwarder::new(cfg, vec![screen], NullSink, routes.clone())));
        let other_fp = fingerprint(&other.cert_der);
        let node_id: [u8; 16] = id.fingerprint()[..16].try_into().unwrap();
        let other_node: [u8; 16] = other_fp[..16].try_into().unwrap();
        let params = LinkParams {
            identity: id.clone(),
            me: PeerInfo { node: node_id, name: name.into(), displays: vec![screen] },
            monitors: vec![],
            peer_monitors: PeerMonitors::default(),
            monitors_home: MonitorsHome::default(),
            pins: PinSet::new([other_fp]),
            peers: vec![(other_name.into(), dial, other_node)],
            listen: "127.0.0.1:0".parse().unwrap(),
            timing: timing(),
            status: None,
            host,
        };
        let home_asks = params.monitors_home.clone();
        let t0 = Instant::now();
        let clock: Clock = Arc::new(move || t0.elapsed().as_millis() as u64);
        let links = Links::start(params, fwd.clone(), routes.clone(), clock, Arc::new(|| {}), clip, None).unwrap();
        Node { fwd, links, routes, home_asks }
    }

    fn wait(secs: u64, mut f: impl FnMut() -> bool) -> bool {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            if f() {
                return true;
            }
            thread::sleep(Duration::from_millis(30));
        }
        false
    }

    #[derive(Default)]
    struct FakeClip(Mutex<(u64, Option<String>, bool)>);
    impl FakeClip {
        fn copy(&self, t: &str) {
            let mut g = self.0.lock().unwrap();
            g.0 += 1;
            g.1 = Some(t.into());
        }
        fn text(&self) -> Option<String> {
            self.0.lock().unwrap().1.clone()
        }
    }
    impl ClipboardOs for FakeClip {
        fn change_count(&self) -> Result<u64, String> { Ok(self.0.lock().unwrap().0) }
        fn read_text(&self) -> Result<Option<String>, String> { Ok(self.0.lock().unwrap().1.clone()) }
        fn write_text(&self, s: &str) -> Result<(), String> {
            let mut g = self.0.lock().unwrap();
            if g.2 { return Err("denied".into()); }
            g.0 += 1;
            g.1 = Some(s.into());
            Ok(())
        }
    }

    /// Clipboard text crosses a real pinned TLS link both ways while the forwarder stays Local,
    /// and a failing write on one side leaves the link up.
    #[test]
    fn clipboard_text_syncs_over_the_link_in_both_directions() {
        let (a_id, b_id) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let (ca, cb) = (Arc::new(FakeClip::default()), Arc::new(FakeClip::default()));
        let b = node_on("b", &b_id, &a_id, "a", Edge::Left, "127.0.0.1:1".parse().unwrap(), Some(cb.clone()), loopback());
        let a = node_on("a", &a_id, &b_id, "b", Edge::Right, b.links.listening(), Some(ca.clone()), loopback());
        // Let the session come up before the first copy (the pump baselines at link-up).
        assert!(wait(8, || a.links.live() == 1 && b.links.live() == 1), "link never came up");
        ca.copy("from a");
        assert!(wait(5, || cb.text().as_deref() == Some("from a")), "A -> B did not arrive");
        cb.0.lock().unwrap().2 = true; // B's writes now fail
        ca.copy("lost on b");
        thread::sleep(Duration::from_millis(1200));
        assert_eq!(cb.text().as_deref(), Some("from a"));
        cb.0.lock().unwrap().2 = false;
        cb.copy("from b");
        assert!(wait(5, || ca.text().as_deref() == Some("from b")), "B -> A did not arrive after a failed write");
        assert_eq!(a.links.live(), 1, "link survived the failed write");
        a.links.stop();
        b.links.stop();
    }

    /// The driven side's "back" reaches the controller as a request for its monitors, over real pinned TLS.
    #[test]
    fn a_peers_back_button_reaches_the_worker_as_a_monitor_return() {
        let (a_id, b_id) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let b = node("b", &b_id, &a_id, "a", Edge::Left, "127.0.0.1:1".parse().unwrap());
        let a = node("a", &a_id, &b_id, "b", Edge::Right, b.links.listening());
        assert!(wait(8, || a.links.live() == 1 && b.links.live() == 1), "link never came up");
        b.routes.clone().send("a", Msg::MonitorsHome);
        assert!(wait(5, || *a.home_asks.lock().unwrap() == ["b"]), "the request never reached A");
        assert!(b.home_asks.lock().unwrap().is_empty());
        a.links.stop();
        b.links.stop();
    }

    /// Real pinned TLS on loopback: link_up lets A switch out and B follow; when B's links stop,
    /// A's link_down returns it to Local.
    #[test]
    fn link_up_enables_switch_and_link_down_returns_local() {
        let (a_id, b_id) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let b = node("b", &b_id, &a_id, "a", Edge::Left, "127.0.0.1:1".parse().unwrap());
        let a = node("a", &a_id, &b_id, "b", Edge::Right, b.links.listening());
        let edge = Point { x: 999, y: 400 };
        let t0 = Instant::now();
        let went_out = wait(8, || {
            let mut f = a.fwd.lock().unwrap();
            f.on_cursor(edge, t0.elapsed().as_millis() as u64);
            matches!(f.mode(), Mode::RemoteOut { .. })
        });
        assert!(went_out, "A never switched out: link_up not reached");
        assert!(wait(5, || matches!(b.fwd.lock().unwrap().mode(), Mode::RemoteIn { .. })), "B never received Enter");
        b.links.stop();
        assert!(wait(8, || matches!(a.fwd.lock().unwrap().mode(), Mode::Local)), "A stayed RemoteOut after the link dropped");
        a.links.stop();
    }

    /// Two peers that can only reach this machine by dialing it (its addresses for them are placeholders)
    /// are both linked at once: the second is not kept waiting until the first one's session ends.
    #[test]
    fn two_peers_that_dial_in_are_linked_at_once() {
        let ids: Vec<Identity> = (0..3).map(|_| Identity::generate().unwrap()).collect();
        let node_of = |i: &Identity| -> [u8; 16] { i.fingerprint()[..16].try_into().unwrap() };
        let screen = Rect { x: 0, y: 0, w: 1000, h: 800 };
        let start = |me: usize, peers: Vec<(&str, usize, Edge, SocketAddr)>| {
            let routes = RouteOutbox::default();
            let cfg = FwdConfig { peers: peers.iter().map(|(n, _, e, _)| PeerSpec { name: (*n).into(), edge: *e, keys: flowseam_core::config::KeyMap::None, gestures: Default::default() }).collect(), policy: SwitchPolicy::default(), boundary_px: 2 };
            let fwd = Arc::new(Mutex::new(Forwarder::new(cfg, vec![screen], NullSink, routes.clone())));
            let params = LinkParams {
                identity: ids[me].clone(),
                me: PeerInfo { node: node_of(&ids[me]), name: format!("n{me}"), displays: vec![screen] },
                monitors: vec![],
                peer_monitors: PeerMonitors::default(),
                monitors_home: MonitorsHome::default(),
                pins: PinSet::new(peers.iter().map(|(_, i, _, _)| fingerprint(&ids[*i].cert_der))),
                peers: peers.iter().map(|(n, i, _, a)| ((*n).into(), *a, node_of(&ids[*i]))).collect(),
                listen: "127.0.0.1:0".parse().unwrap(),
                timing: timing(),
                status: None,
                host: loopback(),
            };
            let t0 = Instant::now();
            let clock: Clock = Arc::new(move || t0.elapsed().as_millis() as u64);
            Links::start(params, fwd, routes, clock, Arc::new(|| {}), None, None).unwrap()
        };
        let nowhere: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let hub = start(0, vec![("a", 1, Edge::Left, nowhere), ("b", 2, Edge::Right, nowhere)]);
        let a = start(1, vec![("hub", 0, Edge::Right, hub.listening())]);
        assert!(wait(8, || a.live() == 1), "a never linked");
        let b = start(2, vec![("hub", 0, Edge::Left, hub.listening())]);
        assert!(wait(8, || b.live() == 1 && hub.live() == 2), "b waited behind a's session: hub has {} links", hub.live());
        for l in [a, b, hub] {
            l.stop();
        }
    }

    /// A machine that cannot list its addresses keeps its links: an unreadable list is not an empty one,
    /// which would make every live path look lost and drop the link twice a second.
    #[test]
    fn a_host_that_cannot_list_its_addresses_keeps_its_links() {
        let blind = LinkHost { addrs: || None, ..loopback() };
        let (a_id, b_id) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let b = node_on("b", &b_id, &a_id, "a", Edge::Left, "127.0.0.1:1".parse().unwrap(), None, blind);
        let a = node_on("a", &a_id, &b_id, "b", Edge::Right, b.links.listening(), None, blind);
        let edge = Point { x: 999, y: 400 };
        let t0 = Instant::now();
        assert!(wait(8, || {
            let mut f = a.fwd.lock().unwrap();
            f.on_cursor(edge, t0.elapsed().as_millis() as u64);
            matches!(f.mode(), Mode::RemoteOut { .. })
        }), "A never switched out");
        // Three of the dial loop's 500 ms turns: a path judged lost would end the session and bring A home.
        thread::sleep(Duration::from_millis(1600));
        assert!(matches!(a.fwd.lock().unwrap().mode(), Mode::RemoteOut { .. }), "the link was dropped as lost");
        a.links.stop();
        b.links.stop();
    }
}
