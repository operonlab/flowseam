//! Connection establishment on top of `tls` and `session`: a client that
//! dials and a server that accepts, both pinned, plus a reconnect loop.
//!
//! Only transient transport failures are retried. Pin, certificate, version
//! and rejection failures are returned at once so they stay visible.

use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::ops::DerefMut;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use crossbeam_channel::Receiver;
use std::thread;
use std::time::{Duration, Instant};

use flowseam_core::proto::{Kind, MonitorId, Msg, PROTO_VERSION};
use rustls::pki_types::ServerName;
use rustls::{ClientConnection, ConnectionCommon, ServerConnection, StreamOwned};

use crate::backoff::Backoff;
use crate::identity::Identity;
use crate::pins::PinSet;
use crate::session::{Channel, End, GreetOpts, PeerInfo, PeerPaths, Timing};
use crate::{tls, NetError};

/// Socket read timeout. Lets the session loop turn over often enough to
/// send heartbeats and drain outgoing messages.
const POLL: Duration = Duration::from_millis(50);
/// Socket write timeout, per call. A peer that stops reading would otherwise
/// block `write` forever and the dead-peer check in `Channel::run` never runs.
/// Kept well under `session::WRITE_BUDGET` so one stalled call cannot overrun
/// the per-frame budget by much; the budget is the bound that matters.
const WRITE_TIMEOUT: Duration = Duration::from_millis(250);
/// The project's former name. Part of the wire protocol and of every identity certificate already
/// issued, so it stays: changing it would force every device to pair again.
const SERVER_NAME: &str = "kmshare.local";

pub struct Endpoint<'a> {
    pub identity: &'a Identity,
    pub pins: &'a PinSet,
    pub me: &'a PeerInfo,
    pub monitors: &'a [MonitorId],
    pub timing: Timing,
    pub speak_for: Option<&'a (dyn Fn([u8; 16]) -> u16 + Sync)>,
    pub paths_for: Option<&'a (dyn Fn(SocketAddr) -> PeerPaths + Sync)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkUp {
    pub peer: PeerInfo,
    pub ver: u16,
    pub paths: Option<PeerPaths>,
    pub monitors: Option<Vec<MonitorId>>,
    pub local: SocketAddr,
    pub remote: SocketAddr,
    pub dialed: bool,
}

#[derive(Debug)]
pub struct Outcome {
    pub peer: PeerInfo,
    pub end: End,
}

pub fn run_client_once(
    addr: SocketAddr,
    ep: &Endpoint<'_>,
    outgoing: &Receiver<Msg>,
    incoming: &Sender<Msg>,
) -> Result<Outcome, NetError> {
    run_client_once_with(addr, ep, outgoing, incoming, &|_| {})
}

/// Like [`run_client_once`]; `on_up` runs once the Hello exchange succeeded, before traffic flows.
pub fn run_client_once_with(
    addr: SocketAddr,
    ep: &Endpoint<'_>,
    outgoing: &Receiver<Msg>,
    incoming: &Sender<Msg>,
    on_up: &dyn Fn(&LinkUp),
) -> Result<Outcome, NetError> {
    let mut sock = TcpStream::connect_timeout(&addr, ep.timing.handshake_timeout).map_err(NetError::Io)?;
    prepare(&sock)?;
    let cfg = tls::client_config(ep.identity, ep.pins.clone())?;
    let name = ServerName::try_from(SERVER_NAME).map_err(|e| NetError::Cert(e.to_string()))?;
    let mut conn = ClientConnection::new(cfg, name).map_err(NetError::Tls)?;
    finish_handshake(&mut conn, &mut sock, ep.timing.handshake_timeout)?;
    let node = cert_node(&conn)?;
    let local = sock.local_addr().map_err(NetError::Io)?;
    let remote = sock.peer_addr().map_err(NetError::Io)?;
    let probe = sock.try_clone().map_err(NetError::Io)?;
    run_channel(StreamOwned::new(conn, sock), Probe { socket: probe, local, remote, dialed: true }, node, ep, outgoing, incoming, on_up)
}

pub fn serve_once(
    listener: &TcpListener,
    ep: &Endpoint<'_>,
    outgoing: &Receiver<Msg>,
    incoming: &Sender<Msg>,
) -> Result<Outcome, NetError> {
    serve_once_with(listener, ep, outgoing, incoming, &|_| {})
}

/// Like [`serve_once`]; `on_up` runs once the Hello exchange succeeded, before traffic flows.
pub fn serve_once_with(
    listener: &TcpListener,
    ep: &Endpoint<'_>,
    outgoing: &Receiver<Msg>,
    incoming: &Sender<Msg>,
    on_up: &dyn Fn(&LinkUp),
) -> Result<Outcome, NetError> {
    let (mut sock, _) = listener.accept().map_err(NetError::Io)?;
    prepare(&sock)?;
    let cfg = tls::server_config(ep.identity, ep.pins.clone())?;
    let mut conn = ServerConnection::new(cfg).map_err(NetError::Tls)?;
    finish_handshake(&mut conn, &mut sock, ep.timing.handshake_timeout)?;
    let node = cert_node(&conn)?;
    let local = sock.local_addr().map_err(NetError::Io)?;
    let remote = sock.peer_addr().map_err(NetError::Io)?;
    let probe = sock.try_clone().map_err(NetError::Io)?;
    run_channel(StreamOwned::new(conn, sock), Probe { socket: probe, local, remote, dialed: false }, node, ep, outgoing, incoming, on_up)
}

/// First 16 bytes of the fingerprint of the certificate the handshake verified against the pins: the
/// only identity a peer has proved. Its Hello `node` must equal this.
fn cert_node(conn: &rustls::CommonState) -> Result<[u8; 16], NetError> {
    let cert = conn
        .peer_certificates()
        .and_then(|c| c.first())
        .ok_or_else(|| NetError::Cert("peer presented no certificate".into()))?;
    let fp = crate::identity::fingerprint(cert.as_ref());
    Ok(fp[..16].try_into().expect("16 of 32 bytes"))
}

/// Dial and keep the link up. Returns `Ok` on a deliberate local stop (the
/// outgoing sender was dropped) or when `stop` is set between sessions. The
/// stop flag is checked between sessions, not in the middle of one.
pub fn run_client(
    addr: SocketAddr,
    ep: &Endpoint<'_>,
    outgoing: &Receiver<Msg>,
    incoming: &Sender<Msg>,
    backoff: &mut Backoff,
    stop: &AtomicBool,
) -> Result<(), NetError> {
    loop {
        if stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        match run_client_once(addr, ep, outgoing, incoming) {
            Ok(outcome) => match outcome.end {
                End::LocalClosed => return Ok(()),
                End::Faulted(m) => return Err(NetError::Proto(m)),
                _ => backoff.reset(),
            },
            Err(e) if e.is_transient() => {}
            Err(e) => return Err(e),
        }
        if !sleep_unless_stopped(backoff.next_delay(), stop) {
            return Ok(());
        }
    }
}

/// The socket a session waits on for readability, and the endpoints it was opened between.
struct Probe {
    socket: TcpStream,
    local: SocketAddr,
    remote: SocketAddr,
    dialed: bool,
}

/// `probe.socket` is a clone of the socket under `stream`: the evented session waits on it for readability so
/// outgoing input never queues behind a blocked read. `cert_node` is from [`cert_node`]; the peer's
/// Hello must claim it, so `PeerInfo.node` handed to `on_up` is authenticated, not self-asserted.
fn run_channel<S>(
    stream: S,
    probe: Probe,
    cert_node: [u8; 16],
    ep: &Endpoint<'_>,
    outgoing: &Receiver<Msg>,
    incoming: &Sender<Msg>,
    on_up: &dyn Fn(&LinkUp),
) -> Result<Outcome, NetError>
where
    S: std::io::Read + std::io::Write + crate::session::Buffered + Send,
{
    let mut ch = Channel::new(stream);
    let opts = GreetOpts {
        speak: ep.speak_for.map_or(PROTO_VERSION, |f| f(cert_node)),
        monitors: ep.monitors.to_vec(),
        paths: ep.paths_for.map_or(
            PeerPaths {
                kind: Kind::Other,
                addrs: vec![],
            },
            |f| f(probe.local),
        ),
    };
    let greeting = ch.greet(ep.me, &opts, ep.timing.handshake_timeout, Some(cert_node))?;
    let peer = greeting.peer;
    on_up(&LinkUp {
        peer: peer.clone(),
        ver: greeting.ver,
        paths: greeting.paths,
        monitors: greeting.monitors,
        local: probe.local,
        remote: probe.remote,
        dialed: probe.dialed,
    });
    let end = ch.run_evented(outgoing, incoming, ep.timing, probe.socket);
    Ok(Outcome { peer, end })
}

pub(crate) fn prepare(sock: &TcpStream) -> Result<(), NetError> {
    sock.set_nodelay(true).map_err(NetError::Io)?;
    sock.set_read_timeout(Some(POLL)).map_err(NetError::Io)?;
    sock.set_write_timeout(Some(WRITE_TIMEOUT)).map_err(NetError::Io)
}

/// Drive the TLS handshake to completion, tolerating idle reads. The deadline
/// bounds a peer that stalls mid-handshake.
pub(crate) fn finish_handshake<C, D>(conn: &mut C, sock: &mut TcpStream, timeout: Duration) -> Result<(), NetError>
where
    C: DerefMut<Target = ConnectionCommon<D>>,
{
    let deadline = Instant::now() + timeout;
    while conn.is_handshaking() {
        // 每輪都檢查 deadline。不能用 complete_io：它在 rustls 內部迴圈裡讀到握手推進為止，
        // 涓流對端會把它困在單次呼叫裡，外層的 deadline 永遠到不了（實測 12 秒才返回）。
        // 這裡每輪只做一次 read_tls／write_tls，socket 有 POLL 讀取逾時，粒度約 50ms。
        if Instant::now() >= deadline {
            return Err(NetError::Timeout);
        }
        if !conn.wants_read() && !conn.wants_write() {
            return Err(NetError::Proto("handshake stalled: neither read nor write wanted".into()));
        }
        if conn.wants_write() {
            match conn.write_tls(sock) {
                Ok(_) => {}
                Err(e) if is_idle(&e) => {}
                Err(e) => return Err(map_tls_io(e)),
            }
        }
        if conn.wants_read() {
            match conn.read_tls(sock) {
                Ok(0) => return Err(NetError::Io(io::Error::from(io::ErrorKind::UnexpectedEof))),
                Ok(_) => conn.process_new_packets().map(|_| ()).map_err(NetError::Tls)?,
                Err(e) if is_idle(&e) => {}
                Err(e) => return Err(map_tls_io(e)),
            }
        }
    }
    Ok(())
}

/// rustls surfaces its own errors (pin mismatch, bad signature) wrapped in an
/// io::Error. Unwrap them so callers can tell TLS failures from socket ones.
fn map_tls_io(e: io::Error) -> NetError {
    match e.get_ref().and_then(|inner| inner.downcast_ref::<rustls::Error>()) {
        Some(tls) => NetError::Tls(tls.clone()),
        None => NetError::Io(e),
    }
}

fn is_idle(e: &io::Error) -> bool {
    matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
}

/// Returns false if `stop` was raised during the wait.
fn sleep_unless_stopped(total: Duration, stop: &AtomicBool) -> bool {
    let end = Instant::now() + total;
    loop {
        if stop.load(Ordering::SeqCst) {
            return false;
        }
        let now = Instant::now();
        if now >= end {
            return true;
        }
        thread::sleep((end - now).min(Duration::from_millis(20)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // 對端連上後從不讀取。灌進超過核心緩衝的資料後，寫入必須在 WRITE_TIMEOUT
    // 附近失敗並結束 session（End::Dead），而不是卡在 write_all 裡。
    #[test]
    fn peer_that_stops_reading_ends_session_instead_of_blocking_writes() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let sock = TcpStream::connect(addr).unwrap();
        let (peer, _) = listener.accept().unwrap();
        prepare(&sock).unwrap();

        let (tx, rx) = std::sync::mpsc::channel::<Msg>();
        let (in_tx, _in_rx) = std::sync::mpsc::channel::<Msg>();
        for _ in 0..200 {
            tx.send(Msg::ClipboardText { gen: 1, text: "x".repeat(64 * 1024) }).unwrap();
        }
        let timing = Timing {
            ping_every: Duration::from_millis(20),
            dead_after: Duration::from_secs(60),
            handshake_timeout: Duration::from_secs(2),
        };
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let end = Channel::new(sock).run(&rx, &in_tx, timing);
            let _ = done_tx.send(format!("{end:?}"));
        });

        let got = done_rx.recv_timeout(Duration::from_secs(10));
        // 失敗時丟掉對端，讓卡住的寫入退出，測試程序才不會被掛住。
        drop(peer);
        drop(tx);
        match got {
            Ok(end) => assert!(end.starts_with("Dead"), "應為 Dead，實得 {end}"),
            Err(_) => panic!("寫入卡住：對端不讀取時 run() 未在限時內結束"),
        }
    }

    // 對端送出一筆湊不齊的 TLS record 標頭，之後每 10ms 補一個位元組。
    // 每次讀取都有資料（Ok），舊實作只在 idle 時看 deadline，會一直卡住；
    // 修正後必須在 timeout 附近回 Timeout。
    #[test]
    fn trickling_peer_cannot_outlive_handshake_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let peer = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            // 合法長度（16384）但之後不足，讓 rustls 一直等下一段資料。
            let _ = s.write_all(&[0x16, 0x03, 0x01, 0x40, 0x00]);
            for _ in 0..400 {
                if s.write_all(&[0u8]).is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        });

        let dir = crate::test_dir(&format!("link-stall-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let id = Identity::load_or_create(&dir).unwrap();
        let cfg = tls::client_config(&id, PinSet::new(std::iter::empty::<crate::identity::Fingerprint>())).unwrap();
        let name = ServerName::try_from(SERVER_NAME).unwrap();
        let mut conn = ClientConnection::new(cfg, name).unwrap();
        let mut sock = TcpStream::connect(addr).unwrap();
        prepare(&sock).unwrap();

        let started = Instant::now();
        let r = finish_handshake(&mut conn, &mut sock, Duration::from_millis(300));
        let took = started.elapsed();
        drop(sock);
        let _ = peer.join();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(matches!(r, Err(NetError::Timeout)), "應為 Timeout，實得 {r:?}");
        assert!(took < Duration::from_millis(1500), "deadline 300ms 卻花了 {took:?}");
    }

    // TLS 路徑的寫入停滯：對端完成握手後永遠不讀。寫入必須在 WRITE_TIMEOUT 附近失敗，
    // 讓 session 以 Dead 結束，不能卡在 StreamOwned::write 裡。
    #[test]
    fn tls_peer_that_stops_reading_ends_session_as_dead() {
        let pid = std::process::id();
        let dir_s = crate::test_dir(&format!("link-wstall-s-{pid}"));
        let dir_c = crate::test_dir(&format!("link-wstall-c-{pid}"));
        let _ = std::fs::remove_dir_all(&dir_s);
        let _ = std::fs::remove_dir_all(&dir_c);
        let id_s = Identity::load_or_create(&dir_s).unwrap();
        let id_c = Identity::load_or_create(&dir_c).unwrap();
        let pins_c = PinSet::new([id_s.fingerprint()]);
        let pins_s = PinSet::new([id_c.fingerprint()]);
        let cfg_c = tls::client_config(&id_c, pins_c).unwrap();
        let cfg_s = tls::server_config(&id_s, pins_s).unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let server = thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            prepare(&sock).unwrap();
            let mut conn = ServerConnection::new(cfg_s).unwrap();
            finish_handshake(&mut conn, &mut sock, Duration::from_secs(5)).unwrap();
            // 握手後不讀；等客戶端回報結果才放手，避免測試程序被掛住。
            let _ = release_rx.recv_timeout(Duration::from_secs(20));
            drop(sock);
        });

        let (done_tx, done_rx) = std::sync::mpsc::channel::<(String, Duration)>();
        thread::spawn(move || {
            let mut sock = TcpStream::connect(addr).unwrap();
            prepare(&sock).unwrap();
            let name = ServerName::try_from(SERVER_NAME).unwrap();
            let mut conn = ClientConnection::new(cfg_c, name).unwrap();
            finish_handshake(&mut conn, &mut sock, Duration::from_secs(5)).unwrap();
            let (tx, rx) = std::sync::mpsc::channel::<Msg>();
            let (in_tx, _in_rx) = std::sync::mpsc::channel::<Msg>();
            // 約 12MB，遠超 TLS 與核心緩衝，確保寫入真的停滯。
            for _ in 0..200 {
                tx.send(Msg::ClipboardText { gen: 1, text: "x".repeat(64 * 1024) }).unwrap();
            }
            let timing = Timing {
                ping_every: Duration::from_millis(20),
                dead_after: Duration::from_secs(60),
                handshake_timeout: Duration::from_secs(5),
            };
            let started = Instant::now();
            let end = Channel::new(StreamOwned::new(conn, sock)).run(&rx, &in_tx, timing);
            let _ = done_tx.send((format!("{end:?}"), started.elapsed()));
        });

        let got = done_rx.recv_timeout(Duration::from_secs(10));
        let _ = release_tx.send(());
        let _ = server.join();
        let _ = std::fs::remove_dir_all(&dir_s);
        let _ = std::fs::remove_dir_all(&dir_c);
        match got {
            Ok((end, took)) => {
                assert!(end.starts_with("Dead"), "應為 Dead，實得 {end}（花了 {took:?}）");
                // 單筆寫入預算 WRITE_BUDGET（1s）附近結束；逐次計時的舊行為約 8s，在這裡必須紅。
                assert!(took >= Duration::from_millis(500), "過早結束：{took:?}");
                assert!(took < Duration::from_millis(1500), "寫入時限應約 1s，實得 {took:?}");
            }
            Err(_) => panic!("TLS 寫入卡住：對端不讀取時 session 未在限時內結束"),
        }
    }
}
