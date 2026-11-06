//! Pairing with a 6-digit code, the only way a peer enters the pin store.
//!
//! The code is the SPAKE2 password. Each side's identity string carries the
//! SHA-256 fingerprints of both TLS certificates as seen on this connection,
//! so the shared key is bound to those fingerprints: a man in the middle
//! presenting different certificates to the two legs ends up with different
//! keys. Each side then proves the key with an HMAC over a fixed label. A
//! wrong code, a swapped fingerprint and a bad MAC all produce the same
//! `NetError::Pair` text and no pin is written.
//!
//! A 6-digit code allows online guessing, one guess per connection, so the
//! server side counts failures in a [`PairGuard`] and locks out for a cooldown.

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use flowseam_core::proto::Msg;
use rand_core::{OsRng, RngCore};
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConnection, ServerConnection, StreamOwned};
use sha2::Sha256;
use spake2::{Ed25519Group, Identity as SpakeId, Password, Spake2};

use crate::identity::{fingerprint, Fingerprint, Identity};
use crate::link;
use crate::pins::PinStore;
use crate::session::Channel;
use crate::{tls, NetError};

pub const CODE_DIGITS: usize = 6;
/// `kmshare` is the project's former name; these labels are protocol constants and stay (see link.rs).
const SERVER_NAME: &str = "kmshare.local";
const PAIR_FAILED: &str = "配對失敗：配對碼或雙方指紋不符";
const ID_CLIENT: &[u8] = b"kmshare pair client";
const ID_SERVER: &[u8] = b"kmshare pair server";
const LABEL_CLIENT: &[u8] = b"kmshare confirm client";
const LABEL_SERVER: &[u8] = b"kmshare confirm server";

/// Per-exchange time limit used by `pair_as_client` / `pair_as_server`.
pub const PAIR_TIMEOUT: Duration = Duration::from_secs(10);
/// Consecutive wrong codes tolerated before the server locks out.
pub const MAX_FAILURES: u32 = 3;
pub const LOCKOUT: Duration = Duration::from_secs(30);

/// Counts wrong codes on the pairing server and refuses attempts during the
/// cooldown, so a 6-digit code cannot be brute-forced online.
///
/// ponytail: in-memory only, a restart resets it, and it is per guard not per
/// source address (a LAN attacker can lock the owner out for 30s). Persist the
/// state or key it by peer address if either matters.
pub struct PairGuard {
    max_failures: u32,
    lockout: Duration,
    state: Mutex<GuardState>,
}

#[derive(Default)]
struct GuardState {
    failures: u32,
    locked_until: Option<Instant>,
}

impl Default for PairGuard {
    fn default() -> Self {
        Self::with_policy(MAX_FAILURES, LOCKOUT)
    }
}

impl PairGuard {
    pub fn with_policy(max_failures: u32, lockout: Duration) -> Self {
        PairGuard { max_failures, lockout, state: Mutex::new(GuardState::default()) }
    }

    fn locked(&self) -> bool {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match s.locked_until {
            Some(t) if Instant::now() < t => true,
            Some(_) => {
                s.locked_until = None;
                false
            }
            None => false,
        }
    }

    fn record(&self, r: &Result<Fingerprint, NetError>) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match r {
            Ok(_) => s.failures = 0,
            // Only a wrong code / fingerprint / MAC is a guess; a dropped
            // connection or timeout tells the attacker nothing.
            Err(NetError::Pair(_)) => {
                s.failures += 1;
                if s.failures >= self.max_failures {
                    s.failures = 0;
                    s.locked_until = Some(Instant::now() + self.lockout);
                }
            }
            Err(_) => {}
        }
    }
}

/// Dial `addr` with the 6-digit `code` and return the server's fingerprint.
/// Nothing is pinned here: the caller stores the result, e.g.
/// `Peer.fingerprint = identity::to_hex(&fp)`.
pub fn pair_as_client(addr: SocketAddr, code: &str, identity_dir: &Path) -> Result<Fingerprint, NetError> {
    let id = Identity::load_or_create(identity_dir)?;
    client_exchange(addr, &id, code, PAIR_TIMEOUT)
}

/// Accept one connection and return the client's fingerprint. During a
/// lockout the connection is dropped unread and `NetError::Pair` is returned,
/// even if the code would have been right.
pub fn pair_as_server(
    listener: &TcpListener,
    code: &str,
    identity_dir: &Path,
    guard: &PairGuard,
) -> Result<Fingerprint, NetError> {
    let id = Identity::load_or_create(identity_dir)?;
    serve_once(listener, code, &id, guard)
}

fn serve_once(listener: &TcpListener, code: &str, id: &Identity, guard: &PairGuard) -> Result<Fingerprint, NetError> {
    let (sock, _) = listener.accept().map_err(NetError::Io)?;
    if guard.locked() {
        drop(sock);
        return Err(NetError::Pair("配對暫時鎖定：失敗次數過多，請稍後再試".into()));
    }
    let r = server_exchange(sock, id, code, PAIR_TIMEOUT);
    guard.record(&r);
    r
}

/// Show `code` on `listener` until a client pairs and return its fingerprint, or `Ok(None)` once `stop()`
/// is true. A connection that does not pair is skipped, not fatal: a wrong code (the guard counts those),
/// a paired peer whose sharing worker dials in with a Hello, a port scan, a dropped link. Only our own
/// identity failing to load ends the wait; it is loaded once, so nothing in the loop can fail that way.
///
/// ponytail: an accept() that keeps failing (EMFILE) spins this loop; back off if that is ever seen.
pub fn serve_code(
    listener: &TcpListener,
    code: &str,
    identity_dir: &Path,
    guard: &PairGuard,
    stop: impl Fn() -> bool,
    mut log: impl FnMut(&str),
) -> Result<Option<Fingerprint>, NetError> {
    // A wait cancelled before it started reports cancelled, even when the identity would not load.
    if stop() {
        return Ok(None);
    }
    let id = Identity::load_or_create(identity_dir)?;
    loop {
        if stop() {
            return Ok(None);
        }
        match serve_once(listener, code, &id, guard) {
            Ok(fp) => return Ok(Some(fp)),
            Err(_) if stop() => return Ok(None),
            Err(e @ NetError::Pair(_)) => log(&format!("rejected: {e}")),
            Err(e) => log(&format!("skipped a connection: {e}")),
        }
    }
}

/// A uniformly random 6-digit code. Rejection sampling avoids the modulo bias
/// a plain `u32 % 1_000_000` would carry.
pub fn generate_code() -> String {
    let limit = (u32::MAX as u64 + 1) / 1_000_000 * 1_000_000;
    loop {
        let r = OsRng.next_u32() as u64;
        if r < limit {
            return format!("{:0width$}", r % 1_000_000, width = CODE_DIGITS);
        }
    }
}

/// Dial `addr`, run the pairing exchange, and pin the server on success.
pub fn pair_client(
    addr: SocketAddr,
    id: &Identity,
    code: &str,
    store: &mut PinStore,
    name: &str,
    timeout: Duration,
) -> Result<(), NetError> {
    let peer = client_exchange(addr, id, code, timeout)?;
    store.add(name, peer);
    Ok(())
}

fn client_exchange(
    addr: SocketAddr,
    id: &Identity,
    code: &str,
    timeout: Duration,
) -> Result<Fingerprint, NetError> {
    let mut sock = TcpStream::connect_timeout(&addr, timeout).map_err(NetError::Io)?;
    link::prepare(&sock)?;
    let cfg = tls::pairing_client_config(id)?;
    let server_name = ServerName::try_from(SERVER_NAME).map_err(|e| NetError::Cert(e.to_string()))?;
    let mut conn = ClientConnection::new(cfg, server_name).map_err(NetError::Tls)?;
    link::finish_handshake(&mut conn, &mut sock, timeout)?;
    let peer = peer_fingerprint(conn.peer_certificates())?;
    let mine = id.fingerprint();
    let deadline = Instant::now() + timeout;
    let mut ch = Channel::new(StreamOwned::new(conn, sock));

    let (spake, out) = start(code, &mine, &peer, Role::Client);
    ch.send(&Msg::PairInit { node: node_of(&mine), msg: out })?;
    let their = match ch.next_msg(deadline)? {
        Msg::PairResp { msg, .. } => msg,
        Msg::Fault { .. } => return Err(pair_failed()),
        other => return Err(unexpected("PairResp", &other)),
    };
    let key = spake.finish(&their).map_err(|_| pair_failed())?;
    ch.send(&Msg::PairConfirm { mac: mac(&key, LABEL_CLIENT) })?;
    match ch.next_msg(deadline)? {
        Msg::PairConfirm { mac: got } if verify(&key, LABEL_SERVER, &got) => {}
        Msg::PairConfirm { .. } | Msg::Fault { .. } => return Err(pair_failed()),
        other => return Err(unexpected("PairConfirm", &other)),
    }
    Ok(peer)
}

/// Accept one pairing connection and pin the client on success.
pub fn pair_server(
    listener: &TcpListener,
    id: &Identity,
    code: &str,
    store: &mut PinStore,
    name: &str,
    timeout: Duration,
) -> Result<(), NetError> {
    let (sock, _) = listener.accept().map_err(NetError::Io)?;
    let peer = server_exchange(sock, id, code, timeout)?;
    store.add(name, peer);
    Ok(())
}

fn server_exchange(
    mut sock: TcpStream,
    id: &Identity,
    code: &str,
    timeout: Duration,
) -> Result<Fingerprint, NetError> {
    link::prepare(&sock)?;
    let cfg = tls::pairing_server_config(id)?;
    let mut conn = ServerConnection::new(cfg).map_err(NetError::Tls)?;
    link::finish_handshake(&mut conn, &mut sock, timeout)?;
    let peer = peer_fingerprint(conn.peer_certificates())?;
    let mine = id.fingerprint();
    let deadline = Instant::now() + timeout;
    let mut ch = Channel::new(StreamOwned::new(conn, sock));

    let init = match ch.next_msg(deadline)? {
        Msg::PairInit { msg, .. } => msg,
        other => return Err(unexpected("PairInit", &other)),
    };
    let (spake, out) = start(code, &peer, &mine, Role::Server);
    let key = spake.finish(&init).map_err(|_| NetError::Proto("bad pairing message".into()))?;
    ch.send(&Msg::PairResp { node: node_of(&mine), msg: out })?;
    match ch.next_msg(deadline)? {
        Msg::PairConfirm { mac: got } if verify(&key, LABEL_CLIENT, &got) => {}
        Msg::PairConfirm { .. } => {
            // Same text the client sees for any failure; the client learns nothing extra.
            let _ = ch.send(&Msg::Fault { code: 2, msg: "pairing failed".into() });
            return Err(pair_failed());
        }
        other => return Err(unexpected("PairConfirm", &other)),
    }
    ch.send(&Msg::PairConfirm { mac: mac(&key, LABEL_SERVER) })?;
    Ok(peer)
}

#[derive(Clone, Copy)]
enum Role {
    Client,
    Server,
}

/// Both sides build the same identity pair: the client's id is
/// `ID_CLIENT || client_fp`, the server's is `ID_SERVER || server_fp`.
fn start(
    code: &str,
    client_fp: &Fingerprint,
    server_fp: &Fingerprint,
    role: Role,
) -> (Spake2<Ed25519Group>, Vec<u8>) {
    let id_a = SpakeId::new(&join(ID_CLIENT, client_fp));
    let id_b = SpakeId::new(&join(ID_SERVER, server_fp));
    let pw = Password::new(code.as_bytes());
    match role {
        Role::Client => Spake2::<Ed25519Group>::start_a(&pw, &id_a, &id_b),
        Role::Server => Spake2::<Ed25519Group>::start_b(&pw, &id_a, &id_b),
    }
}

fn join(prefix: &[u8], fp: &Fingerprint) -> Vec<u8> {
    let mut v = prefix.to_vec();
    v.extend_from_slice(fp);
    v
}

fn mac(key: &[u8], label: &[u8]) -> Vec<u8> {
    let mut m = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    m.update(label);
    m.finalize().into_bytes().to_vec()
}

fn verify(key: &[u8], label: &[u8], got: &[u8]) -> bool {
    let mut m = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    m.update(label);
    m.verify_slice(got).is_ok()
}

fn peer_fingerprint(certs: Option<&[CertificateDer<'static>]>) -> Result<Fingerprint, NetError> {
    certs
        .and_then(|c| c.first())
        .map(|c| fingerprint(c.as_ref()))
        .ok_or_else(|| NetError::Proto("peer presented no certificate".into()))
}

fn node_of(fp: &Fingerprint) -> [u8; 16] {
    let mut n = [0u8; 16];
    n.copy_from_slice(&fp[..16]);
    n
}

fn pair_failed() -> NetError {
    NetError::Pair(PAIR_FAILED.into())
}

fn unexpected(want: &str, got: &Msg) -> NetError {
    NetError::Proto(format!("expected {want}, got {got:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // 兩端指紋一致時金鑰相同、MAC 通過；中間人替換任一端的指紋時金鑰不同、MAC 必須失敗。
    #[test]
    fn swapped_fingerprint_breaks_key_agreement_and_the_mac() {
        let c = fingerprint(&Identity::generate().unwrap().cert_der);
        let s = fingerprint(&Identity::generate().unwrap().cert_der);
        let m = fingerprint(&Identity::generate().unwrap().cert_der);
        let code = "123456";

        let (cli, cli_msg) = start(code, &c, &s, Role::Client);
        let (srv, srv_msg) = start(code, &c, &s, Role::Server);
        let k_cli = cli.finish(&srv_msg).unwrap();
        let k_srv = srv.finish(&cli_msg).unwrap();
        assert_eq!(k_cli, k_srv, "same code and fingerprints must agree");
        assert!(verify(&k_cli, LABEL_SERVER, &mac(&k_srv, LABEL_SERVER)));

        // 中間人：客戶端看到的伺服器指紋是 m，伺服器端看到的是 s。
        let (cli, cli_msg) = start(code, &c, &m, Role::Client);
        let (srv, srv_msg) = start(code, &c, &s, Role::Server);
        let k_cli = cli.finish(&srv_msg).unwrap();
        let k_srv = srv.finish(&cli_msg).unwrap();
        assert_ne!(k_cli, k_srv, "a swapped server fingerprint must change the key");
        assert!(!verify(&k_cli, LABEL_SERVER, &mac(&k_srv, LABEL_SERVER)));
    }

    type ServerCh = Channel<StreamOwned<ServerConnection, TcpStream>>;

    /// Accept one TLS connection the way `server_exchange` does, returning the
    /// channel and the client's certificate fingerprint.
    fn accept_tls(listener: &TcpListener, id: &Identity) -> (ServerCh, Fingerprint) {
        let (mut sock, _) = listener.accept().unwrap();
        link::prepare(&sock).unwrap();
        let mut conn = ServerConnection::new(tls::pairing_server_config(id).unwrap()).unwrap();
        link::finish_handshake(&mut conn, &mut sock, T).unwrap();
        let peer = peer_fingerprint(conn.peer_certificates()).unwrap();
        (Channel::new(StreamOwned::new(conn, sock)), peer)
    }

    const T: Duration = Duration::from_secs(3);

    fn deadline() -> Instant {
        Instant::now() + T
    }

    /// A server that knows the code and runs SPAKE honestly, but answers the
    /// client's confirmation with `good_mac` or with garbage.
    fn rogue_server(listener: TcpListener, id: Identity, code: &str, good_mac: bool) {
        let (mut ch, peer) = accept_tls(&listener, &id);
        let mine = id.fingerprint();
        let init = match ch.next_msg(deadline()).unwrap() {
            Msg::PairInit { msg, .. } => msg,
            other => panic!("{other:?}"),
        };
        let (spake, out) = start(code, &peer, &mine, Role::Server);
        let key = spake.finish(&init).unwrap();
        ch.send(&Msg::PairResp { node: node_of(&mine), msg: out }).unwrap();
        let _ = ch.next_msg(deadline());
        let m = if good_mac { mac(&key, LABEL_SERVER) } else { vec![0u8; 32] };
        let _ = ch.send(&Msg::PairConfirm { mac: m });
    }

    fn run_against_rogue(good_mac: bool) -> Result<(), NetError> {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = Identity::generate().unwrap();
        let t = std::thread::spawn(move || rogue_server(listener, srv, "123456", good_mac));
        let mut store = PinStore::default();
        let r = pair_client(addr, &Identity::generate().unwrap(), "123456", &mut store, "s", T);
        t.join().unwrap();
        assert_eq!(store.entries.len(), usize::from(r.is_ok()), "pin written iff pairing succeeded");
        r
    }

    // 控制組：惡意伺服器送對的 MAC 時客戶端成功，證明下一個測試的失敗只來自 MAC。
    // 伺服器金鑰完全正確、只有確認 MAC 是垃圾時，客戶端必須拒絕且不寫 pin。
    #[test]
    fn client_rejects_a_server_whose_confirm_mac_is_wrong() {
        run_against_rogue(true).expect("control: honest server pairs");
        let err = run_against_rogue(false).expect_err("garbage server MAC must be refused");
        assert!(matches!(err, NetError::Pair(_)), "{err}");
    }

    // 中間人：兩條腿各自終止 TLS，並原樣轉送三則配對訊息。正確的碼也不能配成，
    // 因為兩條腿看到的憑證指紋不同，SPAKE 身分綁定使雙方金鑰不同。
    #[test]
    fn man_in_the_middle_relaying_the_right_code_cannot_pair() {
        let (s_id, c_id, m_id) =
            (Identity::generate().unwrap(), Identity::generate().unwrap(), Identity::generate().unwrap());
        let s_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let m_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let (s_addr, m_addr) = (s_listener.local_addr().unwrap(), m_listener.local_addr().unwrap());

        let server = std::thread::spawn(move || {
            let mut store = PinStore::default();
            let r = pair_server(&s_listener, &s_id, "123456", &mut store, "c", T);
            (r, store.entries.len())
        });
        let mitm = std::thread::spawn(move || {
            let (mut cli_leg, _) = accept_tls(&m_listener, &m_id);
            let mut sock = TcpStream::connect_timeout(&s_addr, T).unwrap();
            link::prepare(&sock).unwrap();
            let name = ServerName::try_from(SERVER_NAME).unwrap();
            let mut conn = ClientConnection::new(tls::pairing_client_config(&m_id).unwrap(), name).unwrap();
            link::finish_handshake(&mut conn, &mut sock, T).unwrap();
            let mut srv_leg = Channel::new(StreamOwned::new(conn, sock));
            let mut relayed = 0;
            let init = cli_leg.next_msg(deadline()).unwrap();
            srv_leg.send(&init).unwrap();
            relayed += 1;
            let resp = srv_leg.next_msg(deadline()).unwrap();
            cli_leg.send(&resp).unwrap();
            relayed += 1;
            let confirm = cli_leg.next_msg(deadline()).unwrap();
            srv_leg.send(&confirm).unwrap();
            relayed += 1;
            if let Ok(back) = srv_leg.next_msg(deadline()) {
                let _ = cli_leg.send(&back);
            }
            relayed
        });
        let mut c_store = PinStore::default();
        let c_res = pair_client(m_addr, &c_id, "123456", &mut c_store, "s", T);
        let relayed = mitm.join().unwrap();
        let (s_res, s_pins) = server.join().unwrap();
        assert_eq!(relayed, 3, "the relay must reach the confirmation step or the test proves nothing");
        assert!(matches!(c_res, Err(NetError::Pair(_))), "client: {c_res:?}");
        assert!(matches!(s_res, Err(NetError::Pair(_))), "server: {s_res:?}");
        assert!(c_store.entries.is_empty() && s_pins == 0, "no pin through a man in the middle");
    }

    /// Two Macs: the one showing the code still has the other as a paired peer, whose sharing worker
    /// keeps dialling the same port and opens with a Hello. A port scan connects and closes. Neither may end
    /// the wait; the peer that types the code must still pair.
    #[test]
    fn serve_code_skips_connections_that_do_not_pair() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (srv_dir, cli_dir) = (crate::test_dir("serve-code-srv"), crate::test_dir("serve-code-cli"));
        let _ = (std::fs::remove_dir_all(&srv_dir), std::fs::remove_dir_all(&cli_dir));
        let dir = srv_dir.clone();
        let server = std::thread::spawn(move || {
            let mut lines = Vec::new();
            let r = serve_code(&listener, "246810", &dir, &PairGuard::default(), || false, |l| lines.push(l.to_owned()));
            (r, lines)
        });

        let mut sock = TcpStream::connect_timeout(&addr, T).unwrap();
        link::prepare(&sock).unwrap();
        let name = ServerName::try_from(SERVER_NAME).unwrap();
        let mut conn = ClientConnection::new(tls::pairing_client_config(&Identity::generate().unwrap()).unwrap(), name).unwrap();
        link::finish_handshake(&mut conn, &mut sock, T).unwrap();
        let mut worker = Channel::new(StreamOwned::new(conn, sock));
        worker.send(&Msg::Fault { code: 0, msg: "not a pairing message".into() }).unwrap();
        assert!(worker.next_msg(deadline()).is_err(), "the server answers a non-pairing message by closing");
        drop(TcpStream::connect(addr).unwrap());

        let fp_server = pair_as_client(addr, "246810", &cli_dir).unwrap();
        let (r, lines) = server.join().unwrap();
        assert_eq!(r.unwrap(), Some(Identity::load_or_create(&cli_dir).unwrap().fingerprint()));
        assert_eq!(fp_server, Identity::load_or_create(&srv_dir).unwrap().fingerprint());
        assert_eq!(lines.iter().filter(|l| l.starts_with("skipped")).count(), 2, "{lines:?}");
    }

    /// Cancel pressed before the wait began: the window must say cancelled, not show an identity error.
    #[test]
    fn serve_code_stopped_before_it_starts_is_cancelled_even_without_an_identity() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let file = crate::test_dir("serve-code-no-identity");
        let _ = std::fs::remove_dir_all(&file);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"a file where the identity directory would go").unwrap();
        let unloadable = file.join("identity");
        assert!(Identity::load_or_create(&unloadable).is_err(), "the test needs an identity that cannot load");
        let r = serve_code(&listener, "246810", &unloadable, &PairGuard::default(), || true, |_| {});
        assert!(matches!(r, Ok(None)), "{r:?}");
        let _ = std::fs::remove_file(file);
    }

    #[test]
    fn generated_codes_are_six_digits() {
        for _ in 0..200 {
            let c = generate_code();
            assert_eq!(c.len(), CODE_DIGITS);
            assert!(c.bytes().all(|b| b.is_ascii_digit()), "{c}");
        }
    }
}
