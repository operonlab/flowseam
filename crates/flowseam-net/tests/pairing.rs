//! Pairing over real TLS on 127.0.0.1: a correct code pins both ways, a wrong
//! code fails the same way whichever digit is wrong, and the normal pinned
//! link refuses everyone while the pin sets are empty.

use std::net::{SocketAddr, TcpListener};
use std::sync::mpsc::channel;
use std::thread;
use std::time::Duration;

use flowseam_core::proto::{Msg, Rect};
use flowseam_net::identity::Identity;
use flowseam_net::link::{self, Endpoint};
use flowseam_net::pair;
use flowseam_net::pins::{PinSet, PinStore};
use flowseam_net::session::{PeerInfo, Timing};
use flowseam_net::NetError;

const TIMEOUT: Duration = Duration::from_secs(3);

fn bind() -> (TcpListener, SocketAddr) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let a = l.local_addr().unwrap();
    (l, a)
}

/// Run one pairing with the server holding `server_code` and the client
/// typing `client_code`. Returns both results and both pin stores.
fn run_pair(
    server_code: &str,
    client_code: &str,
) -> (Result<(), NetError>, Result<(), NetError>, PinStore, PinStore) {
    let s_id = Identity::generate().unwrap();
    let c_id = Identity::generate().unwrap();
    let (listener, addr) = bind();
    let (s_code, c_code) = (server_code.to_string(), client_code.to_string());
    let s_handle = thread::spawn(move || {
        let mut store = PinStore::default();
        let r = pair::pair_server(&listener, &s_id, &s_code, &mut store, "win", TIMEOUT);
        (r, store)
    });
    let c_handle = thread::spawn(move || {
        let mut store = PinStore::default();
        let r = pair::pair_client(addr, &c_id, &c_code, &mut store, "mac", TIMEOUT);
        (r, store)
    });
    let (c_res, c_store) = c_handle.join().unwrap();
    let (s_res, s_store) = s_handle.join().unwrap();
    (c_res, s_res, s_store, c_store)
}

#[test]
fn correct_code_pins_each_side_to_the_other() {
    let (c_res, s_res, s_store, c_store) = run_pair("482913", "482913");
    c_res.expect("client pairs");
    s_res.expect("server pairs");
    assert_eq!(c_store.entries.len(), 1, "client pins the server exactly once");
    assert_eq!(s_store.entries.len(), 1, "server pins the client exactly once");
    assert_eq!(c_store.entries[0].name, "mac");
    assert_eq!(s_store.entries[0].name, "win");
}

#[test]
fn wrong_code_fails_on_both_sides_and_writes_no_pin() {
    let (c_res, s_res, s_store, c_store) = run_pair("482913", "482914");
    assert!(c_res.is_err() && s_res.is_err(), "both sides must refuse");
    assert!(c_store.entries.is_empty() && s_store.entries.is_empty(), "no pin on failure");
}

// 錯在第一位與錯在最後一位，對外的錯誤文字必須完全相同：不能讓攻擊者知道哪一位猜對。
#[test]
fn wrong_digit_position_does_not_change_the_error() {
    let (first_wrong, _, _, _) = run_pair("482913", "182913");
    let (last_wrong, _, _, _) = run_pair("482913", "482912");
    let a = first_wrong.unwrap_err().to_string();
    let b = last_wrong.unwrap_err().to_string();
    assert_eq!(a, b, "error must not reveal which digit was wrong");
    assert!(a.starts_with("pairing:"), "got {a}");
}

#[test]
fn normal_link_refuses_everyone_while_pin_sets_are_empty() {
    let s_id = Identity::generate().unwrap();
    let c_id = Identity::generate().unwrap();
    let (listener, addr) = bind();
    let timing = Timing {
        ping_every: Duration::from_millis(50),
        dead_after: Duration::from_secs(2),
        handshake_timeout: Duration::from_secs(3),
    };
    let info = |name: &str| PeerInfo {
        node: [0; 16],
        name: name.into(),
        displays: vec![Rect { x: 0, y: 0, w: 1, h: 1 }],
    };
    let (s_info, c_info) = (info("win"), info("mac"));
    let (s_out_tx, s_out_rx) = crossbeam_channel::unbounded::<Msg>();
    let (s_in_tx, _s_in_rx) = channel::<Msg>();
    let (c_out_tx, c_out_rx) = crossbeam_channel::unbounded::<Msg>();
    let (c_in_tx, _c_in_rx) = channel::<Msg>();
    let (sid, cid) = (s_id.clone(), c_id.clone());
    let server = thread::spawn(move || {
        let ep = Endpoint { identity: &sid, pins: &PinSet::default(), me: &s_info, timing, speak_for: None, paths_for: None, monitors: &[] };
        link::serve_once(&listener, &ep, &s_out_rx, &s_in_tx)
    });
    let client = thread::spawn(move || {
        let ep = Endpoint { identity: &cid, pins: &PinSet::default(), me: &c_info, timing, speak_for: None, paths_for: None, monitors: &[] };
        link::run_client_once(addr, &ep, &c_out_rx, &c_in_tx)
    });
    let c = client.join().unwrap();
    let s = server.join().unwrap();
    drop(s_out_tx);
    drop(c_out_tx);
    assert!(c.is_err(), "client with empty pins must be refused");
    assert!(s.is_err(), "server with empty pins must be refused");
}

// ---- production entry points: pair_as_client / pair_as_server ----

fn id_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("flowseam-net-pairing-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// One pairing through the public entry points. The server thread serves
/// exactly one connection; `guard` carries lockout state across calls.
fn attempt(
    guard: &std::sync::Arc<pair::PairGuard>,
    s_dir: &std::path::Path,
    c_dir: &std::path::Path,
    server_code: &str,
    client_code: &str,
) -> (Result<flowseam_net::identity::Fingerprint, NetError>, Result<flowseam_net::identity::Fingerprint, NetError>) {
    let (listener, addr) = bind();
    let (g, sd, code) = (guard.clone(), s_dir.to_path_buf(), server_code.to_string());
    let server = thread::spawn(move || pair::pair_as_server(&listener, &code, &sd, &g));
    let c = pair::pair_as_client(addr, client_code, c_dir);
    (c, server.join().unwrap())
}

#[test]
fn pair_as_entry_points_return_each_others_fingerprint_as_config_hex() {
    let (sd, cd) = (id_dir("fp-s"), id_dir("fp-c"));
    let guard = std::sync::Arc::new(pair::PairGuard::default());
    let (c, s) = attempt(&guard, &sd, &cd, "482913", "482913");
    let (server_fp_seen_by_client, client_fp_seen_by_server) = (c.unwrap(), s.unwrap());
    let s_id = Identity::load_or_create(&sd).unwrap();
    let c_id = Identity::load_or_create(&cd).unwrap();
    assert_eq!(server_fp_seen_by_client, s_id.fingerprint());
    assert_eq!(client_fp_seen_by_server, c_id.fingerprint());
    let hex = flowseam_net::identity::to_hex(&server_fp_seen_by_client);
    assert_eq!(hex.len(), 64);
    assert!(hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)), "{hex}");
}

// 連續 3 次錯碼 -> 鎖定。鎖定中即使碼正確也要被拒；冷卻過後恢復。
#[test]
fn three_wrong_codes_lock_the_server_out_until_the_cooldown_passes() {
    let (sd, cd) = (id_dir("lock-s"), id_dir("lock-c"));
    let cooldown = Duration::from_millis(700);
    let guard = std::sync::Arc::new(pair::PairGuard::with_policy(3, cooldown));
    for i in 0..3 {
        let (c, s) = attempt(&guard, &sd, &cd, "482913", "000000");
        assert!(c.is_err(), "wrong code #{i} client");
        assert!(matches!(s, Err(NetError::Pair(_))), "wrong code #{i} server: {s:?}");
    }
    let locked_at = std::time::Instant::now();
    let (c, s) = attempt(&guard, &sd, &cd, "482913", "482913");
    assert!(c.is_err(), "correct code during lockout must fail on the client");
    let msg = s.unwrap_err().to_string();
    assert!(msg.contains("鎖定"), "server should say it is locked: {msg}");

    thread::sleep(cooldown.saturating_sub(locked_at.elapsed()) + Duration::from_millis(150));
    let (c, s) = attempt(&guard, &sd, &cd, "482913", "482913");
    c.expect("pairing works again after the cooldown");
    s.expect("pairing works again after the cooldown (server)");
}

// 失敗計數是「連續」的：中間成功一次要歸零，否則偶爾打錯的正常使用者會被鎖。
#[test]
fn a_success_resets_the_failure_count() {
    let (sd, cd) = (id_dir("reset-s"), id_dir("reset-c"));
    let guard = std::sync::Arc::new(pair::PairGuard::with_policy(3, Duration::from_secs(30)));
    for _ in 0..2 {
        let (c, _) = attempt(&guard, &sd, &cd, "482913", "000000");
        assert!(c.is_err());
    }
    let (c, s) = attempt(&guard, &sd, &cd, "482913", "482913");
    assert!(c.is_ok() && s.is_ok(), "2 failures must not lock");
    for _ in 0..2 {
        let (c, _) = attempt(&guard, &sd, &cd, "482913", "000000");
        assert!(c.is_err());
    }
    let (c, s) = attempt(&guard, &sd, &cd, "482913", "482913");
    assert!(c.is_ok() && s.is_ok(), "count restarted after the success; 2 more failures must not lock");
}
