//! Peer links for the worker: per peer one dial thread, plus one accept thread for the listener.
//! A session that reaches Hello registers a route (peer name -> outgoing channel) and calls
//! `Forwarder::link_up`; its end calls `link_down`. Inbound messages go to `Forwarder::on_msg`.
//!
//! Peers are identified by the certificate fingerprint prefix (`PeerInfo.node`), not by the name the
//! peer announces. `node` arrives in the peer's Hello, so on its own it is only a claim; flowseam_net's link
//! refuses any Hello whose `node` differs from the certificate the pinned handshake verified
//! (`link::cert_node`), which is what makes keying on it safe here.

use crate::wire::{Clock, Shared};
use flowseam_core::backend::InputEmitter;
use flowseam_core::clipboard::ClipboardOs;
use flowseam_core::forward::Outbox;
use flowseam_core::proto::Msg;
use flowseam_net::backoff::Backoff;
use flowseam_net::clipboard::{run_poll, ClipboardPump};
use flowseam_net::files::{DragHub, DragPump, SendFn};
use flowseam_net::identity::Identity;
use flowseam_net::link::{run_client_once_with, serve_once_with, Endpoint};
use flowseam_net::pins::PinSet;
use flowseam_net::session::{PeerInfo, Timing};
use flowseam_net::NetError;
use std::collections::HashMap;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

type Routes = Arc<Mutex<HashMap<String, (u64, Sender<Msg>)>>>;

/// Sends to the peer's live session; a peer with no session drops the message loudly.
#[derive(Clone, Default)]
pub struct RouteOutbox(Routes);

impl Outbox for RouteOutbox {
    fn send(&mut self, peer: &str, msg: Msg) {
        let routes = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match routes.get(peer) {
            Some((_, tx)) => {
                if tx.send(msg).is_err() {
                    eprintln!("flowseam: link to {peer} closing; message dropped");
                }
            }
            None => eprintln!("flowseam: no live link to {peer}; message dropped"),
        }
    }
}

pub struct LinkParams {
    pub identity: Identity,
    pub me: PeerInfo,
    pub pins: PinSet,
    /// (configured name, dial address, node id = first 16 bytes of the fingerprint)
    pub peers: Vec<(String, SocketAddr, [u8; 16])>,
    pub listen: SocketAddr,
    pub timing: Timing,
}

/// File drags: every session registers its pump with `hub`. Unfinished files wait in `staging`; a drop
/// moves them to `dest`.
#[derive(Clone)]
pub struct DragCfg {
    pub hub: Arc<Mutex<DragHub>>,
    pub staging: PathBuf,
    pub dest: PathBuf,
}

struct Ctx<E: InputEmitter + Send + 'static> {
    p: LinkParams,
    fwd: Shared<E, RouteOutbox>,
    routes: Routes,
    clock: Clock,
    stop: Arc<AtomicBool>,
    gens: AtomicU64,
    /// Called when a link thread panics (after it was caught).
    on_panic: Arc<dyn Fn() + Send + Sync>,
    clip: Option<Arc<dyn ClipboardOs + Send + Sync>>,
    drags: Option<DragCfg>,
}

const CLIPBOARD_POLL: Duration = Duration::from_millis(500);

pub struct Links {
    stop: Arc<AtomicBool>,
    routes: Routes,
    wake: SocketAddr,
    threads: Vec<JoinHandle<()>>,
}

impl Links {
    pub fn start<E: InputEmitter + Send + 'static>(
        p: LinkParams,
        fwd: Shared<E, RouteOutbox>,
        routes: RouteOutbox,
        clock: Clock,
        on_panic: Arc<dyn Fn() + Send + Sync>,
        clip: Option<Arc<dyn ClipboardOs + Send + Sync>>,
        drags: Option<DragCfg>,
    ) -> Result<Links, NetError> {
        let listener = TcpListener::bind(p.listen).map_err(NetError::Io)?;
        let wake = listener.local_addr().map_err(NetError::Io)?;
        let stop = Arc::new(AtomicBool::new(false));
        let ctx = Arc::new(Ctx { p, fwd, routes: routes.0.clone(), clock, stop: stop.clone(), gens: AtomicU64::new(0), on_panic, clip, drags });
        let mut threads = Vec::new();
        {
            let ctx = ctx.clone();
            threads.push(guarded("flowseam-mac-accept", ctx.clone(), move || accept_loop(&ctx, listener)));
        }
        for i in 0..ctx.p.peers.len() {
            let ctx = ctx.clone();
            threads.push(guarded("flowseam-mac-dial", ctx.clone(), move || dial_loop(&ctx, i)));
        }
        Ok(Links { stop, routes: routes.0, wake, threads })
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
pub(crate) fn run_guarded(f: impl FnOnce(), on_panic: &dyn Fn()) -> bool {
    if catch_unwind(AssertUnwindSafe(f)).is_err() {
        eprintln!("flowseam: link thread panicked");
        on_panic();
        return false;
    }
    true
}

fn guarded<E: InputEmitter + Send + 'static>(name: &str, ctx: Arc<Ctx<E>>, f: impl FnOnce() + Send + 'static) -> JoinHandle<()> {
    thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            run_guarded(f, &*ctx.on_panic);
        })
        .expect("spawn link thread")
}

fn ep<'a, E: InputEmitter + Send + 'static>(ctx: &'a Ctx<E>) -> Endpoint<'a> {
    Endpoint { identity: &ctx.p.identity, pins: &ctx.p.pins, me: &ctx.p.me, timing: ctx.p.timing }
}

fn dial_loop<E: InputEmitter + Send + 'static>(ctx: &Ctx<E>, i: usize) {
    let addr = ctx.p.peers[i].1;
    let mut backoff = Backoff::new(Duration::from_millis(200), Duration::from_secs(5));
    while !ctx.stop.load(Ordering::SeqCst) {
        match session(ctx, |ep, o, inc, up| run_client_once_with(addr, ep, o, inc, up)) {
            Ok(true) => backoff.reset(),
            Ok(false) => {}
            Err(e) => eprintln!("flowseam: dial {addr}: {e:?}"),
        }
        sleep_unless_stopped(backoff.next_delay(), &ctx.stop);
    }
}

fn accept_loop<E: InputEmitter + Send + 'static>(ctx: &Ctx<E>, listener: TcpListener) {
    while !ctx.stop.load(Ordering::SeqCst) {
        if let Err(e) = session(ctx, |ep, o, inc, up| serve_once_with(&listener, ep, o, inc, up)) {
            if !ctx.stop.load(Ordering::SeqCst) {
                eprintln!("flowseam: accept: {e:?}");
            }
        }
    }
}

fn sleep_unless_stopped(total: Duration, stop: &AtomicBool) {
    let end = std::time::Instant::now() + total;
    while !stop.load(Ordering::SeqCst) && std::time::Instant::now() < end {
        thread::sleep(Duration::from_millis(20));
    }
}

/// Run one session. `Ok(true)` if it reached Hello and was registered.
fn session<E: InputEmitter + Send + 'static>(
    ctx: &Ctx<E>,
    run: impl FnOnce(&Endpoint<'_>, &Receiver<Msg>, &Sender<Msg>, &dyn Fn(&PeerInfo)) -> Result<flowseam_net::link::Outcome, NetError>,
) -> Result<bool, NetError> {
    let (out_tx, out_rx) = channel::<Msg>();
    let (in_tx, in_rx) = channel::<Msg>();
    let slot = Mutex::new(Some((out_tx, in_rx)));
    let reg: Mutex<Option<(String, u64)>> = Mutex::new(None);
    let drag_pump: Mutex<Option<Arc<Mutex<DragPump>>>> = Mutex::new(None);
    let up = |info: &PeerInfo| {
        let Some((out_tx, in_rx)) = slot.lock().unwrap_or_else(|e| e.into_inner()).take() else { return };
        let Some((name, ..)) = ctx.p.peers.iter().find(|p| p.2 == info.node) else {
            eprintln!("flowseam: link from unknown node; closing");
            return;
        };
        let name = name.clone();
        {
            let mut routes = ctx.routes.lock().unwrap_or_else(|e| e.into_inner());
            if routes.contains_key(&name) {
                // The other direction already won; dropping out_tx ends this session.
                eprintln!("flowseam: second link to {name}; closing it");
                return;
            }
            let gen = ctx.gens.fetch_add(1, Ordering::SeqCst);
            routes.insert(name.clone(), (gen, out_tx));
            *reg.lock().unwrap_or_else(|e| e.into_inner()) = Some((name.clone(), gen));
        }
        ctx.fwd.lock().unwrap_or_else(|e| e.into_inner()).link_up(&name);
        let gen = reg.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|r| r.1);
        // Sends through the route table (not a held sender), so a replaced or removed route refuses.
        let route_send: SendFn = {
            let (routes, name) = (ctx.routes.clone(), name.clone());
            Arc::new(move |m: Msg| {
                let r = routes.lock().unwrap_or_else(|e| e.into_inner());
                match r.get(&name) {
                    Some((g, tx)) if Some(*g) == gen => tx.send(m).is_ok(),
                    _ => false,
                }
            })
        };
        let drags = ctx.drags.as_ref().map(|d| {
            let pump = Arc::new(Mutex::new(
                DragPump::new(d.staging.clone(), d.dest.clone()).with_free_space(Arc::new(|p: &std::path::Path| crate::sys::free_bytes(p))),
            ));
            d.hub.lock().unwrap_or_else(|e| e.into_inner()).attach(&name, pump.clone(), route_send.clone());
            *drag_pump.lock().unwrap_or_else(|e| e.into_inner()) = Some(pump.clone());
            pump
        });
        let pump = ctx.clip.as_ref().map(|os| Arc::new(Mutex::new(ClipboardPump::new(os.clone()))));
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
                        Some((g, tx)) if Some(*g) == gen => tx.send(m).is_ok(),
                        _ => false,
                    }
                };
                let ended = || {
                    stop.load(Ordering::SeqCst)
                        || !routes.lock().unwrap_or_else(|e| e.into_inner()).get(&name).is_some_and(|(g, _)| Some(*g) == gen)
                };
                run_guarded(|| run_poll(&pump, &send, &ended, CLIPBOARD_POLL), &*on_panic);
            });
        }
        let (fwd, clock, on_panic) = (ctx.fwd.clone(), ctx.clock.clone(), ctx.on_panic.clone());
        thread::spawn(move || {
            run_guarded(
                || {
                    while let Ok(msg) = in_rx.recv() {
                        if matches!(msg, Msg::DragBegin { .. } | Msg::FileChunk { .. } | Msg::DragAck { .. } | Msg::DragCancel { .. }) {
                            if let Some(d) = &drags {
                                for p in d.lock().unwrap_or_else(|e| e.into_inner()).on_msg(msg, &*route_send) {
                                    eprintln!("flowseam files: saved {}", p.display());
                                }
                            }
                            continue;
                        }
                        if let Msg::ClipboardText { gen, text } = &msg {
                            if let Some(pump) = &pump {
                                pump.lock().unwrap_or_else(|e| e.into_inner()).on_remote(*gen, text);
                            }
                            continue;
                        }
                        let now = clock();
                        fwd.lock().unwrap_or_else(|e| e.into_inner()).on_msg(&name, msg, now);
                    }
                },
                &*on_panic,
            );
        });
    };
    let r = run(&ep(ctx), &out_rx, &in_tx, &up);
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
        if routes.get(name).is_some_and(|(g, _)| g == gen) {
            routes.remove(name);
            drop(routes);
            ctx.fwd.lock().unwrap_or_else(|e| e.into_inner()).link_down(name);
        }
    }
    r.map(|_| registered.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grab::mock::{Log, MockOs};
    use crate::grab::GrabGuard;
    use crate::sink::EmitSink;
    use flowseam_core::backend::{BackendError, EmitOutcome, Permission, Preflight};
    use flowseam_core::forward::{FwdConfig, Forwarder, Mode, PeerSpec};
    use flowseam_core::proto::{Edge, ModifierSet, Point, Rect};
    use flowseam_core::switch::SwitchPolicy;
    use flowseam_net::identity::fingerprint;
    use std::time::Instant;

    struct NullEm;
    type R = Result<EmitOutcome, BackendError>;
    impl InputEmitter for NullEm {
        fn preflight(&self) -> Preflight { Preflight::Ok }
        fn move_rel(&mut self, _: i32, _: i32) -> R { Ok(EmitOutcome::Delivered) }
        fn move_abs(&mut self, _: Point) -> R { Ok(EmitOutcome::Delivered) }
        fn button(&mut self, _: u8, _: bool) -> R { Ok(EmitOutcome::Delivered) }
        fn wheel(&mut self, _: i32, _: i32) -> R { Ok(EmitOutcome::Delivered) }
        fn key(&mut self, _: u16, _: bool, _: bool) -> R { Ok(EmitOutcome::Delivered) }
        fn set_modifiers(&mut self, _: ModifierSet) -> R { Ok(EmitOutcome::Delivered) }
        fn release_all(&mut self) -> Result<(), BackendError> { Ok(()) }
        fn set_cursor_visible(&mut self, _: bool) -> Result<(), BackendError> { Ok(()) }
    }

    struct Node {
        fwd: Shared<NullEm, RouteOutbox>,
        links: Links,
        _log: Log,
    }

    impl Node {
        fn routes_len(&self) -> usize {
            self.links.routes.lock().unwrap().len()
        }
    }

    fn timing() -> Timing {
        Timing { ping_every: Duration::from_millis(50), dead_after: Duration::from_secs(2), handshake_timeout: Duration::from_secs(3) }
    }

    fn node(name: &str, id: &Identity, other: &Identity, other_name: &str, edge: Edge, dial: SocketAddr) -> Node {
        node_with_clip(name, id, other, other_name, edge, dial, None)
    }

    fn node_with_clip(name: &str, id: &Identity, other: &Identity, other_name: &str, edge: Edge, dial: SocketAddr, clip: Option<Arc<dyn ClipboardOs + Send + Sync>>) -> Node {
        node_full(name, id, other, other_name, edge, dial, clip, None)
    }

    #[allow(clippy::too_many_arguments)]
    fn node_full(name: &str, id: &Identity, other: &Identity, other_name: &str, edge: Edge, dial: SocketAddr, clip: Option<Arc<dyn ClipboardOs + Send + Sync>>, drags: Option<DragCfg>) -> Node {
        let screen = Rect { x: 0, y: 0, w: 1000, h: 800 };
        let routes = RouteOutbox::default();
        let log = Log::default();
        let os = MockOs { log: log.clone(), preflight: Preflight::Missing(Permission::Accessibility), secure: Ok(Some(false)), fail_show: false };
        let guard = GrabGuard::new(Box::new(os), std::env::temp_dir().join(format!("flowseam-mac-links-{name}-{}.lock", std::process::id())));
        let cfg = FwdConfig { peers: vec![PeerSpec { name: other_name.into(), edge, keys: flowseam_core::config::KeyMap::None }], policy: SwitchPolicy::default(), boundary_px: 2 };
        let mut sink = EmitSink::new(NullEm, guard);
        if let Some(d) = &drags {
            sink = sink.with_drags(d.hub.clone(), Box::new(|| {}));
        }
        let fwd = Arc::new(Mutex::new(Forwarder::new(cfg, vec![screen], sink, routes.clone())));
        let other_fp = fingerprint(&other.cert_der);
        let node_id: [u8; 16] = id.fingerprint()[..16].try_into().unwrap();
        let other_node: [u8; 16] = other_fp[..16].try_into().unwrap();
        let params = LinkParams {
            identity: id.clone(),
            me: PeerInfo { node: node_id, name: name.into(), displays: vec![screen] },
            pins: PinSet::new([other_fp]),
            peers: vec![(other_name.into(), dial, other_node)],
            listen: "127.0.0.1:0".parse().unwrap(),
            timing: timing(),
        };
        let t0 = Instant::now();
        let clock: Clock = Arc::new(move || t0.elapsed().as_millis() as u64);
        let links = Links::start(params, fwd.clone(), routes, clock, Arc::new(|| {}), clip, drags).unwrap();
        Node { fwd, links, _log: log }
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
        let b = node_with_clip("b", &b_id, &a_id, "a", Edge::Left, "127.0.0.1:1".parse().unwrap(), Some(cb.clone()));
        let a = node_with_clip("a", &a_id, &b_id, "b", Edge::Right, b.links.wake, Some(ca.clone()));
        // Let the session come up before the first copy (the pump baselines at link-up).
        assert!(wait(8, || a.routes_len() == 1 && b.routes_len() == 1), "link never came up");
        ca.copy("from a");
        assert!(wait(5, || cb.text().as_deref() == Some("from a")), "A -> B did not arrive");
        cb.0.lock().unwrap().2 = true; // B's writes now fail
        ca.copy("lost on b");
        thread::sleep(Duration::from_millis(1200));
        assert_eq!(cb.text().as_deref(), Some("from a"));
        cb.0.lock().unwrap().2 = false;
        cb.copy("from b");
        assert!(wait(5, || ca.text().as_deref() == Some("from b")), "B -> A did not arrive after a failed write");
        assert_eq!(a.routes_len(), 1, "link survived the failed write");
        a.links.stop();
        b.links.stop();
    }

    struct Gen(Arc<Mutex<u64>>, Vec<PathBuf>);
    impl flowseam_net::files::DragSource for Gen {
        fn generation(&self) -> Result<u64, String> {
            Ok(*self.0.lock().unwrap())
        }
        fn files(&self) -> Result<Vec<PathBuf>, String> {
            Ok(self.1.clone())
        }
    }

    /// The whole path over real pinned TLS: A presses, a file drag starts, A's cursor dwells out to B, the
    /// button is released on B, and the file is in B's destination with the same bytes. Nothing lands
    /// before the release.
    #[test]
    fn a_file_dragged_across_lands_where_it_is_dropped() {
        use flowseam_core::backend::CapturedEvent;
        use flowseam_core::proto::PRIMARY_BUTTON;
        let root = std::env::temp_dir().join(format!("flowseam-mac-links-drag-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["src", "sa", "da", "sb", "db"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 253) as u8).collect();
        std::fs::write(root.join("src/clip.mov"), &data).unwrap();
        let gen = Arc::new(Mutex::new(1));
        let cfg = |hub: DragHub, s: &str, d: &str| DragCfg { hub: Arc::new(Mutex::new(hub)), staging: root.join(s), dest: root.join(d) };
        let (a_id, b_id) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let b = node_full("b", &b_id, &a_id, "a", Edge::Left, "127.0.0.1:1".parse().unwrap(), None, Some(cfg(DragHub::new(None), "sb", "db")));
        let a_hub = DragHub::new(Some(Box::new(Gen(gen.clone(), vec![root.join("src/clip.mov")]))));
        let a = node_full("a", &a_id, &b_id, "b", Edge::Right, b.links.wake, None, Some(cfg(a_hub, "sa", "da")));
        assert!(wait(8, || a.routes_len() == 1 && b.routes_len() == 1), "link never came up");
        let t0 = Instant::now();
        let now = || t0.elapsed().as_millis() as u64;
        a.fwd.lock().unwrap().on_captured(CapturedEvent::Button { button: PRIMARY_BUTTON, down: true }, now());
        *gen.lock().unwrap() += 1; // the drag starts during the press
        let edge = Point { x: 999, y: 400 };
        assert!(wait(5, || {
            let mut f = a.fwd.lock().unwrap();
            f.on_cursor(edge, now());
            matches!(f.mode(), Mode::RemoteOut { .. })
        }));
        assert!(wait(5, || matches!(b.fwd.lock().unwrap().mode(), Mode::RemoteIn { .. })));
        thread::sleep(Duration::from_millis(500));
        assert!(std::fs::read_dir(root.join("db")).unwrap().next().is_none(), "nothing lands before the drop");
        a.fwd.lock().unwrap().on_captured(CapturedEvent::Button { button: PRIMARY_BUTTON, down: false }, now());
        assert!(
            wait(5, || std::fs::read(root.join("db/clip.mov")).map(|d| d == data).unwrap_or(false)),
            "file never landed: {:?}",
            std::fs::read_dir(root.join("db")).unwrap().map(|e| e.unwrap().file_name()).collect::<Vec<_>>()
        );
        a.links.stop();
        b.links.stop();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Real pinned TLS on loopback: link_up lets A switch out and B follow; when B's links stop,
    /// A's link_down returns it to Local.
    #[test]
    fn link_up_enables_switch_and_link_down_returns_local() {
        let (a_id, b_id) = (Identity::generate().unwrap(), Identity::generate().unwrap());
        let b = node("b", &b_id, &a_id, "a", Edge::Left, "127.0.0.1:1".parse().unwrap());
        let a = node("a", &a_id, &b_id, "b", Edge::Right, b.links.wake);
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
}
