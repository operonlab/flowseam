//! Forwarded input must not wait for the session loop's read timeout. A mouse move queued while the
//! loop is blocked in a read has to leave at once, or the remote cursor moves in bursts.
//!
//! Real TLS over 127.0.0.1 on an idle link (pings far apart, so nothing else wakes the reader).

use std::net::TcpListener;
use std::sync::mpsc::channel;
use std::thread;
use std::time::{Duration, Instant};

use flowseam_core::proto::{Msg, Rect};
use flowseam_net::identity::{fingerprint, Identity};
use flowseam_net::link::{self, Endpoint};
use flowseam_net::pins::PinSet;
use flowseam_net::session::{PeerInfo, Timing};

fn timing() -> Timing {
    Timing {
        ping_every: Duration::from_secs(30),
        dead_after: Duration::from_secs(60),
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

#[test]
fn input_on_an_idle_link_arrives_without_waiting_for_the_read_timeout() {
    let server_id = Identity::generate().unwrap();
    let client_id = Identity::generate().unwrap();
    let (sinfo, cinfo) = (peer("win", &server_id), peer("mac", &client_id));
    let server_pins = PinSet::new([fingerprint(&client_id.cert_der)]);
    let client_pins = PinSet::new([fingerprint(&server_id.cert_der)]);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let (s_out_tx, s_out_rx) = channel::<Msg>();
    let (s_in_tx, s_in_rx) = channel::<Msg>();
    let (c_out_tx, c_out_rx) = channel::<Msg>();
    let (c_in_tx, _c_in_rx) = channel::<Msg>();

    let (sid, spins) = (server_id.clone(), server_pins);
    let server = thread::spawn(move || {
        let ep = Endpoint { identity: &sid, pins: &spins, me: &sinfo, timing: timing(), speak_for: None, paths_for: None };
        link::serve_once(&listener, &ep, &s_out_rx, &s_in_tx)
    });
    let (cid, cpins) = (client_id.clone(), client_pins);
    let client = thread::spawn(move || {
        let ep = Endpoint { identity: &cid, pins: &cpins, me: &cinfo, timing: timing(), speak_for: None, paths_for: None };
        link::run_client_once(addr, &ep, &c_out_rx, &c_in_tx)
    });

    // Warm up: the first message also waits for the handshake.
    c_out_tx.send(Msg::MotionRel { seq: 1, dx: 0, dy: 0 }).unwrap();
    s_in_rx.recv_timeout(Duration::from_secs(5)).expect("link up");

    let mut lat = Vec::new();
    for i in 0..24u32 {
        // Land at different phases of the reader's blocking read.
        thread::sleep(Duration::from_millis(3 + (i as u64 * 7) % 45));
        let t0 = Instant::now();
        c_out_tx.send(Msg::MotionRel { seq: 1, dx: i as i32, dy: 1 }).unwrap();
        let got = s_in_rx.recv_timeout(Duration::from_secs(2)).expect("motion delivered");
        lat.push(t0.elapsed());
        assert_eq!(got, Msg::MotionRel { seq: 1, dx: i as i32, dy: 1 });
    }
    lat.sort();
    let (median, p75, max) = (lat[lat.len() / 2], lat[lat.len() * 3 / 4], lat[lat.len() - 1]);
    eprintln!("idle-link input latency: median {median:?}, p75 {p75:?}, max {max:?}");

    drop(c_out_tx);
    drop(s_out_tx);
    let _ = client.join();
    let _ = server.join();

    // Loopback TLS itself is well under a millisecond; one frame at 120 Hz is 8.3 ms. Waiting on the read
    // timeout spreads latency evenly over 0..50 ms (measured: median 31 ms), which fails both bounds by
    // far. Not the single worst sample: one scheduler hiccup under a loaded machine reached 11 ms.
    assert!(median < Duration::from_millis(4), "median {median:?} (p75 {p75:?}, max {max:?}): input waited on the read timeout");
    assert!(p75 < Duration::from_millis(8), "p75 {p75:?} (median {median:?}, max {max:?}): input waited on the read timeout");
}
