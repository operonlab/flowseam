//! Real TLS over 127.0.0.1: handshake, pinning in both directions, pin
//! mismatch refusal, and reconnect after the peer drops the link.
//!
//! Every session ends when its outgoing sender is dropped, so each test
//! drops senders explicitly before joining, and joins are bounded.

use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use flowseam_core::proto::{Msg, Rect};
use flowseam_net::backoff::Backoff;
use flowseam_net::identity::{fingerprint, Identity};
use flowseam_net::link::{self, Endpoint, Outcome};
use flowseam_net::pins::PinSet;
use flowseam_net::session::{End, PeerInfo, Timing};
use flowseam_net::NetError;

fn timing() -> Timing {
    Timing {
        ping_every: Duration::from_millis(50),
        dead_after: Duration::from_secs(2),
        handshake_timeout: Duration::from_secs(3),
    }
}

fn peer(name: &str, id: &Identity) -> PeerInfo {
    PeerInfo {
        node: id.fingerprint()[..16].try_into().unwrap(),
        name: name.into(),
        displays: vec![Rect { x: 0, y: 0, w: 1440, h: 900 }],
    }
}

fn bind() -> (TcpListener, SocketAddr) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let a = l.local_addr().unwrap();
    (l, a)
}

fn clip(gen: u64, text: &str) -> Msg {
    Msg::ClipboardText { gen, text: text.into() }
}

/// Join with a deadline so a deadlock fails the test instead of hanging it.
fn join_within<T: Send + 'static>(h: JoinHandle<T>, secs: u64) -> T {
    let start = Instant::now();
    while !h.is_finished() {
        assert!(start.elapsed() < Duration::from_secs(secs), "thread still running after {secs}s");
        thread::sleep(Duration::from_millis(10));
    }
    h.join().unwrap()
}

fn recv_clip(rx: &Receiver<Msg>) -> Msg {
    rx.recv_timeout(Duration::from_secs(5)).expect("message within 5s")
}

#[test]
fn pinned_handshake_then_hello_and_data_both_ways() {
    let server_id = Identity::generate().unwrap();
    let client_id = Identity::generate().unwrap();
    let server_info = peer("studio-mac", &server_id);
    let client_info = peer("win-laptop", &client_id);
    let server_pins = PinSet::new([fingerprint(&client_id.cert_der)]);
    let client_pins = PinSet::new([fingerprint(&server_id.cert_der)]);
    let (listener, addr) = bind();

    let (s_out_tx, s_out_rx) = crossbeam_channel::unbounded::<Msg>();
    let (s_in_tx, s_in_rx) = channel::<Msg>();
    let (c_out_tx, c_out_rx) = crossbeam_channel::unbounded::<Msg>();
    let (c_in_tx, c_in_rx) = channel::<Msg>();

    let (sid, spins, sinfo) = (server_id.clone(), server_pins, server_info.clone());
    let server = thread::spawn(move || {
        let ep = Endpoint { identity: &sid, pins: &spins, me: &sinfo, timing: timing(), speak_for: None, paths_for: None, monitors: &[] };
        link::serve_once(&listener, &ep, &s_out_rx, &s_in_tx)
    });
    let (cid, cpins, cinfo) = (client_id.clone(), client_pins, client_info.clone());
    let client = thread::spawn(move || {
        let ep = Endpoint { identity: &cid, pins: &cpins, me: &cinfo, timing: timing(), speak_for: None, paths_for: None, monitors: &[] };
        link::run_client_once(addr, &ep, &c_out_rx, &c_in_tx)
    });

    c_out_tx.send(clip(1, "from windows")).unwrap();
    assert_eq!(recv_clip(&s_in_rx), clip(1, "from windows"));
    s_out_tx.send(clip(7, "from mac")).unwrap();
    assert_eq!(recv_clip(&c_in_rx), clip(7, "from mac"));

    // Dropping each side's sender ends that side's session; the other side
    // then sees the close.
    drop(s_out_tx);
    drop(c_out_tx);
    let server_outcome: Outcome = join_within(server, 10).expect("server session");
    let client_outcome: Outcome = join_within(client, 10).expect("client session");
    assert_eq!(server_outcome.peer, client_info, "server learned the client's Hello");
    assert_eq!(client_outcome.peer, server_info, "client learned the server's Hello");
    // Whichever side dropped its sender first ends its session first; the other
    // side then sees the socket close. Either order is a clean end.
    assert!(
        matches!(client_outcome.end, End::LocalClosed | End::PeerClosed),
        "{:?}",
        client_outcome.end
    );
}

#[test]
fn client_refuses_a_server_whose_fingerprint_is_not_pinned() {
    let server_id = Identity::generate().unwrap();
    let client_id = Identity::generate().unwrap();
    let client_info = peer("win-laptop", &client_id);
    let server_info = peer("studio-mac", &server_id);
    // The client pins a different certificate than the one the server shows.
    let wrong = Identity::generate().unwrap();
    let client_pins = PinSet::new([fingerprint(&wrong.cert_der)]);
    let server_pins = PinSet::new([fingerprint(&client_id.cert_der)]);
    let (listener, addr) = bind();

    let (s_out_tx, s_out_rx) = crossbeam_channel::unbounded::<Msg>();
    let (s_in_tx, _s_in_rx) = channel::<Msg>();
    let (sid, sinfo) = (server_id.clone(), server_info.clone());
    let server = thread::spawn(move || {
        let ep = Endpoint { identity: &sid, pins: &server_pins, me: &sinfo, timing: timing(), speak_for: None, paths_for: None, monitors: &[] };
        link::serve_once(&listener, &ep, &s_out_rx, &s_in_tx)
    });

    let (c_out_tx, c_out_rx) = crossbeam_channel::unbounded::<Msg>();
    let (c_in_tx, _c_in_rx) = channel::<Msg>();
    // Run the client on its own thread: if verification is broken the session
    // stays healthy on heartbeats, and the test must fail rather than hang.
    let (cid, cpins, cinfo) = (client_id.clone(), client_pins, client_info.clone());
    let client = thread::spawn(move || {
        let ep = Endpoint { identity: &cid, pins: &cpins, me: &cinfo, timing: timing(), speak_for: None, paths_for: None, monitors: &[] };
        link::run_client_once(addr, &ep, &c_out_rx, &c_in_tx)
    });
    let err = join_within(client, 10).expect_err("must refuse");
    match &err {
        NetError::Tls(e) => assert!(e.to_string().contains("not pinned"), "got: {e}"),
        other => panic!("expected a TLS pin failure, got {other}"),
    }
    assert!(!err.is_transient(), "a pin failure must not be retried");
    drop((s_out_tx, c_out_tx));
    let _ = join_within(server, 10);
}

#[test]
fn server_refuses_a_client_whose_fingerprint_is_not_pinned() {
    let server_id = Identity::generate().unwrap();
    let client_id = Identity::generate().unwrap();
    let client_info = peer("win-laptop", &client_id);
    let server_info = peer("studio-mac", &server_id);
    // The server pins nobody: an empty set must reject the client.
    let server_pins = PinSet::default();
    let client_pins = PinSet::new([fingerprint(&server_id.cert_der)]);
    let (listener, addr) = bind();

    let (s_out_tx, s_out_rx) = crossbeam_channel::unbounded::<Msg>();
    let (s_in_tx, _s_in_rx) = channel::<Msg>();
    let (sid, sinfo) = (server_id.clone(), server_info.clone());
    let server = thread::spawn(move || {
        let ep = Endpoint { identity: &sid, pins: &server_pins, me: &sinfo, timing: timing(), speak_for: None, paths_for: None, monitors: &[] };
        link::serve_once(&listener, &ep, &s_out_rx, &s_in_tx)
    });

    let (c_out_tx, c_out_rx) = crossbeam_channel::unbounded::<Msg>();
    let (c_in_tx, _c_in_rx) = channel::<Msg>();
    let (cid, cpins, cinfo) = (client_id.clone(), client_pins, client_info.clone());
    let client = thread::spawn(move || {
        let ep = Endpoint { identity: &cid, pins: &cpins, me: &cinfo, timing: timing(), speak_for: None, paths_for: None, monitors: &[] };
        link::run_client_once(addr, &ep, &c_out_rx, &c_in_tx)
    });
    // TLS 1.3 lets the client finish before the server checks its certificate,
    // so the client sees an error or a dead link; the server is the authority.
    let client_result = join_within(client, 10);
    let server_result = join_within(server, 10);
    match server_result {
        Err(NetError::Tls(e)) => assert!(e.to_string().contains("not pinned"), "got: {e}"),
        other => panic!("server must reject with a pin failure, got {other:?}"),
    }
    if let Ok(outcome) = client_result {
        assert!(!matches!(outcome.end, End::LocalClosed), "client must not look healthy");
    }
    drop((s_out_tx, c_out_tx));
}

/// `PeerInfo.node` is what callers key peers on, so it must be the certificate the handshake proved,
/// not whatever the Hello says: a paired machine must not pass itself off as another paired machine.
fn forged_node_hello(server_forges: bool) -> (Result<Outcome, NetError>, Result<Outcome, NetError>, bool, bool) {
    let server_id = Identity::generate().unwrap();
    let client_id = Identity::generate().unwrap();
    let other = Identity::generate().unwrap(); // also paired; its node is the one being claimed
    let forged = |name: &str, id: &Identity| {
        let mut p = peer(name, id);
        p.node = other.fingerprint()[..16].try_into().unwrap();
        p
    };
    let server_info = if server_forges { forged("studio-mac", &server_id) } else { peer("studio-mac", &server_id) };
    let client_info = if server_forges { peer("win-laptop", &client_id) } else { forged("win-laptop", &client_id) };
    let server_pins = PinSet::new([fingerprint(&client_id.cert_der), fingerprint(&other.cert_der)]);
    let client_pins = PinSet::new([fingerprint(&server_id.cert_der), fingerprint(&other.cert_der)]);
    let (listener, addr) = bind();
    let server_up = Arc::new(AtomicBool::new(false));
    let client_up = Arc::new(AtomicBool::new(false));

    let (s_out_tx, s_out_rx) = crossbeam_channel::unbounded::<Msg>();
    let (s_in_tx, _s_in_rx) = channel::<Msg>();
    let (sid, sinfo, sup) = (server_id.clone(), server_info, server_up.clone());
    let server = thread::spawn(move || {
        let ep = Endpoint { identity: &sid, pins: &server_pins, me: &sinfo, timing: timing(), speak_for: None, paths_for: None, monitors: &[] };
        link::serve_once_with(&listener, &ep, &s_out_rx, &s_in_tx, &|_| sup.store(true, Ordering::SeqCst))
    });
    let (c_out_tx, c_out_rx) = crossbeam_channel::unbounded::<Msg>();
    let (c_in_tx, _c_in_rx) = channel::<Msg>();
    let (cid, cinfo, cup) = (client_id.clone(), client_info, client_up.clone());
    let client = thread::spawn(move || {
        let ep = Endpoint { identity: &cid, pins: &client_pins, me: &cinfo, timing: timing(), speak_for: None, paths_for: None, monitors: &[] };
        link::run_client_once_with(addr, &ep, &c_out_rx, &c_in_tx, &|_| cup.store(true, Ordering::SeqCst))
    });
    // A session that wrongly came up would run on heartbeats; end it so the joins return.
    thread::sleep(Duration::from_millis(500));
    drop((s_out_tx, c_out_tx));
    let (s, c) = (join_within(server, 10), join_within(client, 10));
    (s, c, server_up.load(Ordering::SeqCst), client_up.load(Ordering::SeqCst))
}

#[test]
fn server_refuses_a_paired_client_claiming_another_peers_node() {
    let (server, _client, server_up, _) = forged_node_hello(false);
    assert!(!server_up, "the server registered a session under a node the client does not hold");
    assert!(server.is_err(), "server must refuse the Hello, got {:?}", server.map(|o| o.peer.node));
}

#[test]
fn client_refuses_a_paired_server_claiming_another_peers_node() {
    let (_server, client, _, client_up) = forged_node_hello(true);
    assert!(!client_up, "the client registered a session under a node the server does not hold");
    assert!(client.is_err(), "client must refuse the Hello, got {:?}", client.map(|o| o.peer.node));
}

#[test]
fn reconnects_after_the_peer_drops_the_link() {
    let server_id = Identity::generate().unwrap();
    let client_id = Identity::generate().unwrap();
    let server_info = peer("studio-mac", &server_id);
    let client_info = peer("win-laptop", &client_id);
    let server_pins = PinSet::new([fingerprint(&client_id.cert_der)]);
    let client_pins = PinSet::new([fingerprint(&server_id.cert_der)]);
    let (listener, addr) = bind();

    // Client side: one long-lived run_client loop with a feeder that keeps
    // offering messages. A message written into a dead socket is lost, and
    // that is fine: the test only needs one to arrive on the second link.
    let (c_out_tx, c_out_rx) = crossbeam_channel::unbounded::<Msg>();
    let (c_in_tx, _c_in_rx) = channel::<Msg>();
    let stop = Arc::new(AtomicBool::new(false));
    let (cid, cpins, cinfo, stop_c) = (client_id.clone(), client_pins, client_info.clone(), stop.clone());
    let client = thread::spawn(move || {
        let ep = Endpoint { identity: &cid, pins: &cpins, me: &cinfo, timing: timing(), speak_for: None, paths_for: None, monitors: &[] };
        let mut backoff = Backoff::new(Duration::from_millis(20), Duration::from_millis(200));
        link::run_client(addr, &ep, &c_out_rx, &c_in_tx, &mut backoff, &stop_c)
    });
    let feeder_stop = Arc::new(AtomicBool::new(false));
    let fs = feeder_stop.clone();
    let feeder = thread::spawn(move || {
        let mut gen = 0u64;
        while !fs.load(Ordering::SeqCst) {
            gen += 1;
            if c_out_tx.send(clip(gen, &format!("n{gen}"))).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
    });

    // Server, connection 1: complete Hello, then close at once (outgoing
    // sender dropped) so the client sees the link go away.
    let ep = Endpoint { identity: &server_id, pins: &server_pins, me: &server_info, timing: timing(), speak_for: None, paths_for: None, monitors: &[] };
    let (dead_tx, dead_rx) = crossbeam_channel::unbounded::<Msg>();
    drop(dead_tx);
    let (in1_tx, _in1_rx) = channel::<Msg>();
    let first = link::serve_once(&listener, &ep, &dead_rx, &in1_tx);
    assert!(first.is_ok(), "connection 1 reached Hello: {:?}", first.as_ref().err());

    // Server, connection 2: the client must come back on its own. Serve it on
    // a thread, read until a clipboard message arrives, then close.
    let (keep_tx, keep_rx) = crossbeam_channel::unbounded::<Msg>();
    let (in2_tx, in2_rx) = channel::<Msg>();
    let server2 = thread::spawn(move || {
        let ep = Endpoint { identity: &server_id, pins: &server_pins, me: &server_info, timing: timing(), speak_for: None, paths_for: None, monitors: &[] };
        link::serve_once(&listener, &ep, &keep_rx, &in2_tx)
    });
    let got = in2_rx.recv_timeout(Duration::from_secs(10));
    assert!(matches!(got, Ok(Msg::ClipboardText { .. })), "data after reconnect: {got:?}");
    drop(keep_tx);
    let second: Outcome = join_within(server2, 10).expect("connection 2 reached Hello");
    assert_eq!(second.peer, client_info, "reconnect completed a fresh Hello");

    feeder_stop.store(true, Ordering::SeqCst);
    let _ = feeder.join();
    stop.store(true, Ordering::SeqCst);
    let result = join_within(client, 15);
    assert!(result.is_ok(), "client loop ended cleanly: {result:?}");
}
