//! Forwarders over real pinned TLS links on 127.0.0.1: a driven machine can also relay
//! its controller's input to its own peer, then resume injecting on return.

use std::net::TcpListener;
use std::sync::mpsc::{channel, Receiver};
use crossbeam_channel::Sender;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use flowseam_core::backend::{CapturedEvent, Disposition};
use flowseam_core::forward::{FwdConfig, Forwarder, InputSink, Mode, Outbox, PeerSpec};
use flowseam_core::proto::{Edge, Msg, Point, Rect};
use flowseam_core::switch::SwitchPolicy;
use flowseam_net::identity::{fingerprint, Identity};
use flowseam_net::link::{self, Endpoint};
use flowseam_net::pins::PinSet;
use flowseam_net::session::{PeerInfo, Timing};

fn timing() -> Timing {
    Timing {
        ping_every: Duration::from_millis(50),
        dead_after: Duration::from_secs(2),
        handshake_timeout: Duration::from_secs(3),
    }
}

fn info(name: &str, id: &Identity) -> PeerInfo {
    PeerInfo {
        node: id.fingerprint()[..16].try_into().unwrap(),
        name: name.into(),
        displays: vec![Rect { x: 0, y: 0, w: 1440, h: 900 }],
    }
}

#[derive(Default)]
struct Rec(Vec<String>);
impl InputSink for Rec {
    fn motion_rel(&mut self, dx: i32, dy: i32) {
        self.0.push(format!("motion {dx} {dy}"));
    }
    fn button(&mut self, b: u8, d: bool) {
        self.0.push(format!("button {b} {d}"));
    }
    fn key(&mut self, h: u16, d: bool, _: bool) {
        self.0.push(format!("key {h} {d}"));
    }
    fn wheel(&mut self, dx: i32, dy: i32) {
        self.0.push(format!("wheel {dx} {dy}"));
    }
    fn warp(&mut self, p: Point) {
        self.0.push(format!("warp {} {}", p.x, p.y));
    }
    fn set_grab(&mut self, on: bool) {
        self.0.push(format!("grab {on}"));
    }
    fn hide(&mut self) {
        self.0.push("hide".into());
    }
    fn show(&mut self) {
        self.0.push("show".into());
    }
    fn set_association(&mut self, c: bool) {
        self.0.push(format!("assoc {c}"));
    }
}

struct Chan(Sender<Msg>);
impl Outbox for Chan {
    fn send(&mut self, _peer: &str, msg: Msg) {
        self.0.send(msg).expect("link outgoing open");
    }
}

struct Routes { a: Sender<Msg>, c: Sender<Msg> }
impl Outbox for Routes {
    fn send(&mut self, peer: &str, msg: Msg) {
        match peer {
            "a" => self.a.send(msg).expect("A link outgoing open"),
            "c" => self.c.send(msg).expect("C link outgoing open"),
            _ => panic!("unknown route {peer}"),
        }
    }
}

fn cfg(peer: &str, edge: Edge) -> FwdConfig {
    FwdConfig {
        peers: vec![PeerSpec { name: peer.into(), edge, keys: flowseam_core::config::KeyMap::None, gestures: Default::default() }],
        policy: SwitchPolicy::default(),
        boundary_px: 2,
    }
}

fn recv(rx: &Receiver<Msg>) -> Msg {
    rx.recv_timeout(Duration::from_secs(5)).expect("message within 5s")
}

fn join_within<T: Send + 'static>(h: JoinHandle<T>, secs: u64) -> T {
    let start = Instant::now();
    while !h.is_finished() {
        assert!(start.elapsed() < Duration::from_secs(secs), "thread still running after {secs}s");
        thread::sleep(Duration::from_millis(10));
    }
    h.join().unwrap()
}

#[test]
fn a_switches_out_b_injects_then_b_returns_the_cursor() {
    let a_id = Identity::generate().unwrap();
    let b_id = Identity::generate().unwrap();
    let (a_info, b_info) = (info("a", &a_id), info("b", &b_id));
    let a_pins = PinSet::new([fingerprint(&b_id.cert_der)]);
    let b_pins = PinSet::new([fingerprint(&a_id.cert_der)]);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let (a_out_tx, a_out_rx) = crossbeam_channel::unbounded::<Msg>();
    let (a_in_tx, a_in_rx) = channel::<Msg>();
    let (b_out_tx, b_out_rx) = crossbeam_channel::unbounded::<Msg>();
    let (b_in_tx, b_in_rx) = channel::<Msg>();

    let (id, pins, me) = (b_id.clone(), b_pins, b_info);
    let server = thread::spawn(move || {
        let ep = Endpoint { identity: &id, pins: &pins, me: &me, timing: timing(), speak_for: None, paths_for: None, monitors: &[] };
        link::serve_once(&listener, &ep, &b_out_rx, &b_in_tx)
    });
    let (id, pins, me) = (a_id.clone(), a_pins, a_info);
    let client = thread::spawn(move || {
        let ep = Endpoint { identity: &id, pins: &pins, me: &me, timing: timing(), speak_for: None, paths_for: None, monitors: &[] };
        link::run_client_once(addr, &ep, &a_out_rx, &a_in_tx)
    });

    let screen = vec![Rect { x: 0, y: 0, w: 1440, h: 900 }];
    let mut a = Forwarder::new(cfg("b", Edge::Right), screen.clone(), Rec::default(), Chan(a_out_tx));
    let mut b = Forwarder::new(cfg("a", Edge::Left), screen, Rec::default(), Chan(b_out_tx));
    a.link_up("b");
    b.link_up("a");

    // A: dwell at its right edge.
    let edge = Point { x: 1439, y: 450 };
    a.on_cursor(edge, 0);
    a.on_cursor(edge, 200);
    assert!(matches!(a.mode(), Mode::RemoteOut { .. }));
    let enter = recv(&b_in_rx);
    assert!(matches!(enter, Msg::Enter { .. }), "{enter:?}");
    b.on_msg("a", enter, 0);
    assert!(matches!(b.mode(), Mode::RemoteIn { .. }));

    // A's input now arrives at B in order.
    assert_eq!(a.on_captured(CapturedEvent::Motion { dx: 7, dy: -3 }, 300), Disposition::Swallow);
    a.on_captured(CapturedEvent::Button { button: 1, down: true }, 300);
    a.on_captured(CapturedEvent::Key { hid: 0x04, down: true }, 300);
    for _ in 0..3 {
        let m = recv(&b_in_rx);
        b.on_msg("a", m, 300);
    }
    let fx = &b.sink().0;
    assert_eq!(fx[fx.len() - 3..], ["motion 7 -3", "button 1 true", "key 4 true"]);

    // B pushes into its left wall and dwells: it sends Leave and A comes back.
    a.on_captured(CapturedEvent::Motion { dx: -200, dy: 0 }, 400);
    b.on_msg("a", recv(&b_in_rx), 1000);
    b.tick(1200);
    assert!(matches!(b.mode(), Mode::Local));
    let leave = recv(&a_in_rx);
    assert!(matches!(leave, Msg::Leave { .. }), "{leave:?}");
    a.on_msg("b", leave, 1300);
    assert!(matches!(a.mode(), Mode::Local));
    assert_eq!(a.sink().0.last().unwrap(), "warp 1415 447"); // y carries the -3 that B injected
    assert!(b.sink().0.contains(&"key 4 false".to_string()), "B released A's held key");

    drop(a);
    drop(b);
    join_within(server, 10).expect("server session");
    join_within(client, 10).expect("client session");
}

#[test]
fn a_drives_b_through_c_then_c_returns_and_a_releases() {
    let a_id = Identity::generate().unwrap();
    let b_id = Identity::generate().unwrap();
    let c_id = Identity::generate().unwrap();
    let (a_info, b_info, c_info) = (info("a", &a_id), info("b", &b_id), info("c", &c_id));
    let ab_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let bc_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let ab_addr = ab_listener.local_addr().unwrap();
    let bc_addr = bc_listener.local_addr().unwrap();

    let (a_out_tx, a_out_rx) = crossbeam_channel::unbounded::<Msg>();
    let (a_in_tx, a_in_rx) = channel::<Msg>();
    let (b_a_out_tx, b_a_out_rx) = crossbeam_channel::unbounded::<Msg>();
    let (b_a_in_tx, b_a_in_rx) = channel::<Msg>();
    let (b_c_out_tx, b_c_out_rx) = crossbeam_channel::unbounded::<Msg>();
    let (b_c_in_tx, b_c_in_rx) = channel::<Msg>();
    let (c_out_tx, c_out_rx) = crossbeam_channel::unbounded::<Msg>();
    let (c_in_tx, c_in_rx) = channel::<Msg>();

    let ab_server = {
        let (id, me) = (b_id.clone(), b_info.clone());
        let pins = PinSet::new([fingerprint(&a_id.cert_der)]);
        thread::spawn(move || {
            let ep = Endpoint { identity: &id, pins: &pins, me: &me, timing: timing(), speak_for: None, paths_for: None, monitors: &[] };
            link::serve_once(&ab_listener, &ep, &b_a_out_rx, &b_a_in_tx)
        })
    };
    let ab_client = {
        let (id, me) = (a_id.clone(), a_info);
        let pins = PinSet::new([fingerprint(&b_id.cert_der)]);
        thread::spawn(move || {
            let ep = Endpoint { identity: &id, pins: &pins, me: &me, timing: timing(), speak_for: None, paths_for: None, monitors: &[] };
            link::run_client_once(ab_addr, &ep, &a_out_rx, &a_in_tx)
        })
    };
    let bc_server = {
        let (id, me) = (c_id.clone(), c_info);
        let pins = PinSet::new([fingerprint(&b_id.cert_der)]);
        thread::spawn(move || {
            let ep = Endpoint { identity: &id, pins: &pins, me: &me, timing: timing(), speak_for: None, paths_for: None, monitors: &[] };
            link::serve_once(&bc_listener, &ep, &c_out_rx, &c_in_tx)
        })
    };
    let bc_client = {
        let (id, me) = (b_id.clone(), b_info);
        let pins = PinSet::new([fingerprint(&c_id.cert_der)]);
        thread::spawn(move || {
            let ep = Endpoint { identity: &id, pins: &pins, me: &me, timing: timing(), speak_for: None, paths_for: None, monitors: &[] };
            link::run_client_once(bc_addr, &ep, &b_c_out_rx, &b_c_in_tx)
        })
    };

    let screen = vec![Rect { x: 0, y: 0, w: 1440, h: 900 }];
    let mut b_cfg = cfg("a", Edge::Left);
    b_cfg.peers.push(PeerSpec { name: "c".into(), edge: Edge::Right, keys: flowseam_core::config::KeyMap::None, gestures: Default::default() });
    let mut a = Forwarder::new(cfg("b", Edge::Right), screen.clone(), Rec::default(), Chan(a_out_tx));
    let mut b = Forwarder::new(b_cfg, screen.clone(), Rec::default(), Routes { a: b_a_out_tx, c: b_c_out_tx });
    let mut c = Forwarder::new(cfg("b", Edge::Left), screen, Rec::default(), Chan(c_out_tx));
    a.link_up("b");
    b.link_up("a");
    b.link_up("c");
    c.link_up("b");

    let edge = Point { x: 1439, y: 450 };
    a.on_cursor(edge, 0);
    a.on_cursor(edge, 200);
    b.on_msg("a", recv(&b_a_in_rx), 300);
    assert!(matches!(b.mode(), Mode::RemoteIn { .. }));
    a.on_captured(CapturedEvent::Motion { dx: 2000, dy: 0 }, 1000);
    b.on_msg("a", recv(&b_a_in_rx), 1000);
    b.tick(1200);
    assert!(matches!(b.mode(), Mode::Relay { from, to, .. } if from == "a" && to == "c"));
    c.on_msg("b", recv(&c_in_rx), 1200);
    assert!(matches!(c.mode(), Mode::RemoteIn { .. }));

    a.on_captured(CapturedEvent::Motion { dx: 7, dy: -3 }, 1300);
    a.on_captured(CapturedEvent::Key { hid: 0x04, down: true }, 1300);
    for _ in 0..2 { b.on_msg("a", recv(&b_a_in_rx), 1300); }
    for _ in 0..2 { c.on_msg("b", recv(&c_in_rx), 1300); }
    assert!(c.sink().0.contains(&"motion 7 -3".into()));
    assert!(c.sink().0.contains(&"key 4 true".into()));
    assert!(!b.sink().0.contains(&"key 4 true".into()));

    a.on_captured(CapturedEvent::Motion { dx: -200, dy: 0 }, 1400);
    b.on_msg("a", recv(&b_a_in_rx), 1400);
    c.on_msg("b", recv(&c_in_rx), 1400);
    c.tick(1600);
    assert!(matches!(c.mode(), Mode::Local));
    b.on_msg("c", recv(&b_c_in_rx), 1600);
    assert!(matches!(b.mode(), Mode::RemoteIn { peer, .. } if peer == "a"));

    for hid in [0xE0, 0xE2, 0xE3, 0x16] {
        a.on_captured(CapturedEvent::Key { hid, down: true }, 1700);
    }
    for _ in 0..4 { b.on_msg("a", recv(&b_a_in_rx), 1700); }
    assert!(matches!(a.mode(), Mode::Local));
    assert!(matches!(b.mode(), Mode::Local));
    assert!(matches!(c.mode(), Mode::Local));

    drop(a);
    drop(b);
    drop(c);
    for handle in [ab_server, ab_client, bc_server, bc_client] {
        join_within(handle, 10).expect("link session");
    }
    drop(a_in_rx);
}
