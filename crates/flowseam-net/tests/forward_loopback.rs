//! Two forwarders over a real pinned TLS link on 127.0.0.1: A hands the cursor to B,
//! B receives the stream and injects it into its sink; B's return dwell hands it back.

use std::net::TcpListener;
use std::sync::mpsc::{channel, Receiver, Sender};
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

fn cfg(peer: &str, edge: Edge) -> FwdConfig {
    FwdConfig {
        peers: vec![PeerSpec { name: peer.into(), edge, keys: flowseam_core::config::KeyMap::None }],
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

    let (a_out_tx, a_out_rx) = channel::<Msg>();
    let (a_in_tx, a_in_rx) = channel::<Msg>();
    let (b_out_tx, b_out_rx) = channel::<Msg>();
    let (b_in_tx, b_in_rx) = channel::<Msg>();

    let (id, pins, me) = (b_id.clone(), b_pins, b_info);
    let server = thread::spawn(move || {
        let ep = Endpoint { identity: &id, pins: &pins, me: &me, timing: timing(), speak_for: None, paths_for: None };
        link::serve_once(&listener, &ep, &b_out_rx, &b_in_tx)
    });
    let (id, pins, me) = (a_id.clone(), a_pins, a_info);
    let client = thread::spawn(move || {
        let ep = Endpoint { identity: &id, pins: &pins, me: &me, timing: timing(), speak_for: None, paths_for: None };
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
