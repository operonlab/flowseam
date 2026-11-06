//! Framed message channel over any blocking byte stream: version-checked
//! Hello exchange, then a loop that carries messages, answers Ping, and
//! declares the peer dead after `dead_after` without any inbound traffic.
//!
//! The caller owns socket timeouts. The loop assumes the stream returns
//! `WouldBlock` / `TimedOut` on an idle read (set a short read timeout).
//!
//! Two loops: [`Channel::run`] turns over on the read timeout, so a message queued while it blocks in a
//! read waits for that timeout. [`Channel::run_evented`] (what links use) never blocks the sender on a
//! read: a reader thread waits for the socket to turn readable, and the caller's thread sends the moment
//! a message is queued.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use flowseam_core::framing::{encode_frame, FrameDecoder};
use flowseam_core::proto::{self, Kind, Msg, PathAddr, Rect, MIN_PROTO_VERSION, PROTO_VERSION};
use rustls::{ClientConnection, ServerConnection, StreamOwned};

use crate::NetError;

/// Plaintext a stream already holds in memory. A readiness probe on the socket cannot see it, so the
/// evented reader drains it after each read instead of waiting for more bytes on the wire.
pub trait Buffered {
    fn has_buffered(&mut self) -> bool;
}

impl Buffered for TcpStream {
    fn has_buffered(&mut self) -> bool {
        false
    }
}

impl Buffered for StreamOwned<ClientConnection, TcpStream> {
    fn has_buffered(&mut self) -> bool {
        plaintext_pending(self.conn.process_new_packets())
    }
}

impl Buffered for StreamOwned<ServerConnection, TcpStream> {
    fn has_buffered(&mut self) -> bool {
        plaintext_pending(self.conn.process_new_packets())
    }
}

/// A TLS error claims "pending" so that the next read runs and reports it, rather than hiding it here.
fn plaintext_pending(st: Result<rustls::IoState, rustls::Error>) -> bool {
    st.map_or(true, |s| s.plaintext_bytes_to_read() > 0)
}

/// How often each evented thread looks up from its wait to check whether the session has ended.
/// Bounds how long a peer-side end takes to reach the caller, and how long shutdown joins take.
const STOP_CHECK: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerInfo {
    pub node: [u8; 16],
    pub name: String,
    pub displays: Vec<Rect>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerPaths {
    pub kind: Kind,
    pub addrs: Vec<PathAddr>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GreetOpts {
    pub speak: u16,
    pub paths: PeerPaths,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Greeting {
    pub peer: PeerInfo,
    pub ver: u16,
    pub paths: Option<PeerPaths>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    pub ping_every: Duration,
    pub dead_after: Duration,
    pub handshake_timeout: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            ping_every: Duration::from_secs(1),
            dead_after: Duration::from_secs(3),
            handshake_timeout: Duration::from_secs(5),
        }
    }
}

/// Why a session ended. Only `LocalClosed` means we chose to stop.
#[derive(Debug)]
pub enum End {
    /// Peer closed the connection (EOF or close without close_notify).
    PeerClosed,
    /// No inbound traffic within `dead_after`.
    Dead,
    /// Our outgoing sender was dropped: deliberate local shutdown.
    LocalClosed,
    /// Peer sent `Fault`, or sent a frame we cannot decode.
    Faulted(String),
    Io(io::Error),
}

enum ReadErr {
    Idle,
    Eof,
    Io(io::Error),
    Proto(String),
}

pub struct Channel<S> {
    stream: S,
    dec: FrameDecoder,
    queued: VecDeque<Msg>,
    /// Protocol fault raised by the decoder after `queued` frames; surfaces
    /// once `queued` is drained.
    fault: Option<String>,
}

impl<S: Read + Write> Channel<S> {
    pub fn new(stream: S) -> Self {
        Channel { stream, dec: FrameDecoder::new(), queued: VecDeque::new(), fault: None }
    }

    pub fn send(&mut self, m: &Msg) -> Result<(), NetError> {
        let body = proto::encode(m).map_err(|e| NetError::Proto(format!("{e:?}")))?;
        let frame = encode_frame(&body).map_err(|e| NetError::Proto(format!("{e:?}")))?;
        // 整筆寫入共用一個時限。socket 的 write timeout 是「單次呼叫」的逾時；
        // TLS 的 write 每次只要有推進一點位元組就回 Ok，逐次計時會把停滯拉長到數秒。
        let deadline = Instant::now() + WRITE_BUDGET;
        write_all_by(&mut self.stream, &frame, deadline)?;
        flush_by(&mut self.stream, deadline)
    }

    /// Exchange Hello and HelloAck. Both sides send Hello first, then answer
    /// the peer's Hello with HelloAck, then wait for the peer's HelloAck.
    pub fn hello(&mut self, me: &PeerInfo, timeout: Duration) -> Result<PeerInfo, NetError> {
        self.hello_bound(me, timeout, None)
    }

    /// [`Channel::hello`], with the peer's `node` tied to its certificate. `cert_node` is the first 16
    /// bytes of the fingerprint the TLS handshake verified; a Hello claiming any other node is refused
    /// before it is accepted, because callers key peers on `node` and the Hello itself is just a claim.
    pub fn hello_bound(
        &mut self,
        me: &PeerInfo,
        timeout: Duration,
        cert_node: Option<[u8; 16]>,
    ) -> Result<PeerInfo, NetError> {
        self.greet(
            me,
            &GreetOpts {
                speak: PROTO_VERSION,
                paths: PeerPaths {
                    kind: Kind::Other,
                    addrs: vec![],
                },
            },
            timeout,
            cert_node,
        )
        .map(|g| g.peer)
    }

    pub fn greet(
        &mut self,
        me: &PeerInfo,
        opts: &GreetOpts,
        timeout: Duration,
        cert_node: Option<[u8; 16]>,
    ) -> Result<Greeting, NetError> {
        let deadline = Instant::now() + timeout;
        self.send(&Msg::Hello {
            ver: opts.speak,
            node: me.node,
            name: me.name.clone(),
            displays: me.displays.clone(),
        })?;
        let (peer_ver, peer) = match self.next_msg(deadline)? {
            Msg::Hello {
                ver,
                node,
                name,
                displays,
            } => {
                if ver < MIN_PROTO_VERSION {
                    let reason =
                        format!("protocol {ver} unsupported, need at least {MIN_PROTO_VERSION}");
                    let _ = self.send(&Msg::HelloAck {
                        ver: opts.speak,
                        accepted: false,
                        reason: Some(reason),
                    });
                    return Err(NetError::VersionMismatch {
                        ours: opts.speak,
                        theirs: ver,
                    });
                }
                if cert_node.is_some_and(|n| n != node) {
                    let reason = "Hello node does not match the certificate".to_string();
                    let _ = self.send(&Msg::HelloAck {
                        ver: opts.speak,
                        accepted: false,
                        reason: Some(reason.clone()),
                    });
                    return Err(NetError::Proto(reason));
                }
                (
                    ver,
                    PeerInfo {
                        node,
                        name,
                        displays,
                    },
                )
            }
            other => return Err(NetError::Proto(format!("expected Hello, got {other:?}"))),
        };
        self.send(&Msg::HelloAck {
            ver: opts.speak,
            accepted: true,
            reason: None,
        })?;
        match self.next_msg(deadline)? {
            Msg::HelloAck { accepted: true, .. } => {}
            Msg::HelloAck {
                accepted: false, ..
            } if peer_ver < opts.speak => {
                return Err(NetError::VersionMismatch {
                    ours: opts.speak,
                    theirs: peer_ver,
                });
            }
            Msg::HelloAck {
                accepted: false,
                reason,
                ..
            } => return Err(NetError::Rejected(reason.unwrap_or_default())),
            other => return Err(NetError::Proto(format!("expected HelloAck, got {other:?}"))),
        }
        let ver = opts.speak.min(peer_ver);
        let paths = if ver >= 4 {
            self.send(&Msg::Paths {
                kind: opts.paths.kind,
                addrs: opts.paths.addrs.clone(),
            })?;
            match self.next_msg(deadline)? {
                Msg::Paths { kind, addrs } => Some(PeerPaths { kind, addrs }),
                other => return Err(NetError::Proto(format!("expected Paths, got {other:?}"))),
            }
        } else {
            None
        };
        Ok(Greeting { peer, ver, paths })
    }

    pub(crate) fn next_msg(&mut self, deadline: Instant) -> Result<Msg, NetError> {
        loop {
            if let Some(m) = self.queued.pop_front() {
                return Ok(m);
            }
            if let Some(m) = self.fault.take() {
                return Err(NetError::Proto(m));
            }
            // 每輪都檢查，不只在 Idle 時檢查：對端持續涓流時 read_some 一直回 Ok，
            // 只看 Idle 的話 deadline 會被繞過（同 link.rs finish_handshake）。
            if Instant::now() >= deadline {
                return Err(NetError::Timeout);
            }
            match self.read_some() {
                Ok(()) => {}
                Err(ReadErr::Idle) => {}
                Err(ReadErr::Eof) => {
                    return Err(NetError::Io(io::Error::from(io::ErrorKind::UnexpectedEof)))
                }
                Err(ReadErr::Io(e)) => return Err(NetError::Io(e)),
                Err(ReadErr::Proto(m)) => return Err(NetError::Proto(m)),
            }
        }
    }

    /// One read. Decoded messages go to `queued`; nothing is returned.
    fn read_some(&mut self) -> Result<(), ReadErr> {
        let mut buf = [0u8; 8192];
        let n = match self.stream.read(&mut buf) {
            Ok(0) => return Err(ReadErr::Eof),
            Ok(n) => n,
            Err(e) if is_idle(&e) => return Err(ReadErr::Idle),
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Err(ReadErr::Eof),
            Err(e) => return Err(ReadErr::Io(e)),
        };
        let pushed = self.dec.push(&buf[..n]);
        for body in pushed.frames {
            let m = proto::decode(&body).map_err(|e| ReadErr::Proto(format!("{e:?}")))?;
            self.queued.push_back(m);
        }
        // 超長標頭的 fault 排在它前面的完整框架之後才生效：先讓 queued 送完。
        if let Some(e) = pushed.error {
            self.fault = Some(format!("{e:?}"));
        }
        Ok(())
    }

    /// Run until the session ends. Inbound data messages go to `incoming`;
    /// `outgoing` is drained whenever the loop turns.
    pub fn run(&mut self, outgoing: &Receiver<Msg>, incoming: &Sender<Msg>, timing: Timing) -> End {
        let mut last_seen = Instant::now();
        let mut last_ping = Instant::now();
        loop {
            while let Some(m) = self.queued.pop_front() {
                match m {
                    Msg::Ping { t } => {
                        if let Err(e) = self.send(&Msg::Pong { t }) {
                            return send_end(e);
                        }
                    }
                    Msg::Pong { .. } => {}
                    Msg::Fault { code, msg } => {
                        return End::Faulted(format!("peer fault {code}: {msg}"));
                    }
                    data => {
                        if incoming.send(data).is_err() {
                            return End::LocalClosed;
                        }
                    }
                }
            }
            if let Some(m) = self.fault.take() {
                let _ = self.send(&Msg::Fault { code: 1, msg: m.clone() });
                return End::Faulted(m);
            }

            loop {
                match outgoing.try_recv() {
                    Ok(m) => {
                        if let Err(e) = self.send(&m) {
                            return send_end(e);
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return End::LocalClosed,
                }
            }

            match self.read_some() {
                Ok(()) => last_seen = Instant::now(),
                Err(ReadErr::Idle) => {}
                Err(ReadErr::Eof) => return End::PeerClosed,
                Err(ReadErr::Io(e)) => return End::Io(e),
                Err(ReadErr::Proto(m)) => {
                    let _ = self.send(&Msg::Fault { code: 1, msg: m.clone() });
                    return End::Faulted(m);
                }
            }

            if last_ping.elapsed() >= timing.ping_every {
                let t = now_millis();
                if let Err(e) = self.send(&Msg::Ping { t }) {
                    return send_end(e);
                }
                last_ping = Instant::now();
            }
            if last_seen.elapsed() > timing.dead_after {
                return End::Dead;
            }
        }
    }
}

/// State the evented reader and sender share. The first end recorded wins.
struct Shared {
    done: AtomicBool,
    end: Mutex<Option<End>>,
    last_seen: Mutex<Instant>,
}

impl Shared {
    fn finish(&self, e: End) {
        let mut slot = self.end.lock().unwrap_or_else(|p| p.into_inner());
        if slot.is_none() {
            *slot = Some(e);
        }
        self.done.store(true, Ordering::SeqCst);
    }
    fn is_done(&self) -> bool {
        self.done.load(Ordering::SeqCst)
    }
    fn seen_now(&self) {
        *self.last_seen.lock().unwrap_or_else(|p| p.into_inner()) = Instant::now();
    }
    fn since_seen(&self) -> Duration {
        self.last_seen.lock().unwrap_or_else(|p| p.into_inner()).elapsed()
    }
}

impl<S: Read + Write + Buffered + Send> Channel<S> {
    /// Like [`Channel::run`], but the caller's thread never blocks in a read. A reader thread waits for
    /// `probe` (a clone of the underlying socket, sharing its read timeout) to turn readable and then reads
    /// under a short lock; this thread waits on `outgoing` and sends the moment a message is queued.
    pub fn run_evented(&mut self, outgoing: &Receiver<Msg>, incoming: &Sender<Msg>, timing: Timing, probe: TcpStream) -> End {
        let shared = Shared { done: AtomicBool::new(false), end: Mutex::new(None), last_seen: Mutex::new(Instant::now()) };
        let ch = Mutex::new(self);
        thread::scope(|s| {
            s.spawn(|| read_loop(&ch, &probe, incoming, &shared));
            send_loop(&ch, outgoing, timing, &shared);
        });
        let end = shared.end.into_inner().unwrap_or_else(|p| p.into_inner());
        end.unwrap_or(End::LocalClosed)
    }
}

fn lock<'a, 'b, S>(ch: &'a Mutex<&'b mut Channel<S>>) -> MutexGuard<'a, &'b mut Channel<S>> {
    ch.lock().unwrap_or_else(|p| p.into_inner())
}

/// Wait for the socket to turn readable, then read everything available, the stream's buffered
/// plaintext included, and deliver it. The probe's read timeout bounds each wait so a session that the
/// sender ended is noticed.
fn read_loop<S: Read + Write + Buffered>(ch: &Mutex<&mut Channel<S>>, probe: &TcpStream, incoming: &Sender<Msg>, sh: &Shared) {
    let mut one = [0u8; 1];
    while !sh.is_done() {
        match probe.peek(&mut one) {
            Ok(_) => {} // data, or EOF (0): the read reports which
            Err(e) if is_idle(&e) => continue,
            Err(_) => {} // let the read surface the error
        }
        let mut g = lock(ch);
        let mut read = g.read_some();
        while read.is_ok() && g.stream.has_buffered() {
            read = g.read_some();
        }
        match read {
            Ok(()) => sh.seen_now(),
            Err(ReadErr::Idle) => {} // a partial TLS record; the rest is on its way
            Err(ReadErr::Eof) => return sh.finish(End::PeerClosed),
            Err(ReadErr::Io(e)) => return sh.finish(End::Io(e)),
            Err(ReadErr::Proto(m)) => {
                let _ = g.send(&Msg::Fault { code: 1, msg: m.clone() });
                return sh.finish(End::Faulted(m));
            }
        }
        if let Some(end) = g.deliver(incoming) {
            return sh.finish(end);
        }
    }
}

/// Send each queued message at once, and the heartbeat when due; end the session on a dead peer.
fn send_loop<S: Read + Write>(ch: &Mutex<&mut Channel<S>>, outgoing: &Receiver<Msg>, timing: Timing, sh: &Shared) {
    let mut last_ping = Instant::now();
    while !sh.is_done() {
        let until_ping = timing.ping_every.saturating_sub(last_ping.elapsed());
        match outgoing.recv_timeout(until_ping.min(STOP_CHECK)) {
            Ok(m) => {
                let mut g = lock(ch);
                let mut next = Some(m);
                // A burst (a fast mouse) goes out under one lock.
                while let Some(m) = next {
                    if let Err(e) = g.send(&m) {
                        return sh.finish(send_end(e));
                    }
                    next = outgoing.try_recv().ok();
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return sh.finish(End::LocalClosed),
        }
        if last_ping.elapsed() >= timing.ping_every {
            if let Err(e) = lock(ch).send(&Msg::Ping { t: now_millis() }) {
                return sh.finish(send_end(e));
            }
            last_ping = Instant::now();
        }
        if sh.since_seen() > timing.dead_after {
            return sh.finish(End::Dead);
        }
    }
}

impl<S: Read + Write> Channel<S> {
    /// Hand decoded messages on: answer Ping, end on Fault, pass data to `incoming`.
    fn deliver(&mut self, incoming: &Sender<Msg>) -> Option<End> {
        while let Some(m) = self.queued.pop_front() {
            match m {
                Msg::Ping { t } => {
                    if let Err(e) = self.send(&Msg::Pong { t }) {
                        return Some(send_end(e));
                    }
                }
                Msg::Pong { .. } => {}
                Msg::Fault { code, msg } => return Some(End::Faulted(format!("peer fault {code}: {msg}"))),
                data => {
                    if incoming.send(data).is_err() {
                        return Some(End::LocalClosed);
                    }
                }
            }
        }
        if let Some(m) = self.fault.take() {
            let _ = self.send(&Msg::Fault { code: 1, msg: m.clone() });
            return Some(End::Faulted(m));
        }
        None
    }
}

fn is_idle(e: &io::Error) -> bool {
    matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
}

/// Total time one outgoing frame may take to leave the process. A peer that
/// stops reading ends the session as `Dead` after about this long.
pub const WRITE_BUDGET: Duration = Duration::from_secs(1);

/// `write_all` under one deadline. The deadline is checked after every call,
/// so a stall that trickles bytes still ends at the budget.
fn write_all_by<W: Write>(w: &mut W, mut buf: &[u8], deadline: Instant) -> Result<(), NetError> {
    while !buf.is_empty() {
        if Instant::now() >= deadline {
            return Err(NetError::Timeout);
        }
        match w.write(buf) {
            Ok(0) => return Err(NetError::Io(io::Error::from(io::ErrorKind::WriteZero))),
            Ok(n) => buf = &buf[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(write_err(e)),
        }
    }
    Ok(())
}

fn flush_by<W: Write>(w: &mut W, deadline: Instant) -> Result<(), NetError> {
    if Instant::now() >= deadline {
        return Err(NetError::Timeout);
    }
    w.flush().map_err(write_err)
}

/// A write that hits the socket write timeout surfaces as `WouldBlock` or
/// `TimedOut`. Map it to `Timeout` so `send_end` can end the session as Dead.
fn write_err(e: io::Error) -> NetError {
    if is_idle(&e) {
        NetError::Timeout
    } else {
        NetError::Io(e)
    }
}

/// A stalled write means the peer stopped reading: the link is dead, so end
/// the session as `Dead` (transient, reconnects). Other send failures stay `Io`.
fn send_end(e: NetError) -> End {
    match e {
        NetError::Timeout => End::Dead,
        other => End::Io(into_io(other)),
    }
}

fn into_io(e: NetError) -> io::Error {
    match e {
        NetError::Io(e) => e,
        other => io::Error::other(other.to_string()),
    }
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc::channel;
    use std::thread;

    fn pair() -> (TcpStream, TcpStream) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        let (server, _) = l.accept().unwrap();
        for s in [&client, &server] {
            s.set_read_timeout(Some(Duration::from_millis(20))).unwrap();
        }
        (client, server)
    }

    fn info(name: &str, b: u8) -> PeerInfo {
        PeerInfo {
            node: [b; 16],
            name: name.into(),
            displays: vec![Rect { x: 0, y: 0, w: 1920, h: 1080 }],
        }
    }

    fn fast() -> Timing {
        Timing {
            ping_every: Duration::from_millis(20),
            dead_after: Duration::from_millis(300),
            handshake_timeout: Duration::from_secs(2),
        }
    }

    /// Accepts one byte per `write` after a 20ms pause, forever: a peer that
    /// is alive but nearly stopped reading. Every call returns `Ok`, so only the
    /// per-frame deadline can end the send. After 3s it errors out so a broken
    /// deadline fails the test instead of hanging it.
    struct Trickle(Instant);

    impl Write for Trickle {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            if self.0.elapsed() > Duration::from_secs(3) {
                return Err(io::Error::other("test safety valve"));
            }
            thread::sleep(Duration::from_millis(20));
            Ok(b.len().min(1))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Read for Trickle {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        }
    }

    // 對端活著但幾乎不讀（每次 write 只吃 1 byte，且每次都回 Ok）。
    // send 必須在 WRITE_BUDGET（1s）附近回 Timeout：預算值錯（放寬）或迴圈內不檢查 deadline 都會讓它拖到安全閥。
    #[test]
    fn send_to_a_trickling_peer_times_out_at_the_write_budget() {
        let mut ch = Channel::new(Trickle(Instant::now()));
        let started = Instant::now();
        let r = ch.send(&Msg::ClipboardText { gen: 1, text: "x".repeat(2000) });
        let took = started.elapsed();
        assert!(matches!(r, Err(NetError::Timeout)), "應為 Timeout，實得 {r:?}");
        assert!(
            took >= WRITE_BUDGET && took < WRITE_BUDGET + Duration::from_millis(700),
            "預算 {WRITE_BUDGET:?} 卻花了 {took:?}"
        );
    }

    #[test]
    fn hello_exchange_gives_each_side_the_other_peer() {
        let (a, b) = pair();
        let (ia, ib) = (info("mac", 1), info("win", 2));
        let (ia2, ib2) = (ia.clone(), ib.clone());
        let t = thread::spawn(move || Channel::new(b).hello(&ib2, Duration::from_secs(2)));
        let got_b_from_a = Channel::new(a).hello(&ia2, Duration::from_secs(2)).unwrap();
        assert_eq!(got_b_from_a, ib);
        assert_eq!(t.join().unwrap().unwrap(), ia);
    }

    fn opts(speak: u16, kind: Kind, port: u16) -> GreetOpts {
        GreetOpts {
            speak,
            paths: PeerPaths {
                kind,
                addrs: vec![PathAddr {
                    addr: format!("127.0.0.1:{port}").parse().unwrap(),
                    kind,
                }],
            },
        }
    }

    #[test]
    fn v4_exchanges_paths() {
        let (a, b) = pair();
        let (ia, ib) = (info("mac", 1), info("win", 2));
        let (oa, ob) = (opts(4, Kind::Wired, 1), opts(4, Kind::WiFi, 2));
        let expected_a = oa.paths.clone();
        let expected_b = ob.paths.clone();
        let t =
            thread::spawn(move || Channel::new(b).greet(&ib, &ob, Duration::from_secs(2), None));
        let ga = Channel::new(a)
            .greet(&ia, &oa, Duration::from_secs(2), None)
            .unwrap();
        let gb = t.join().unwrap().unwrap();
        assert_eq!((ga.ver, ga.paths), (4, Some(expected_b)));
        assert_eq!((gb.ver, gb.paths), (4, Some(expected_a)));
    }

    #[test]
    fn speaking_three_skips_paths() {
        let (a, b) = pair();
        let t = thread::spawn(move || {
            Channel::new(b).greet(
                &info("win", 2),
                &opts(4, Kind::WiFi, 2),
                Duration::from_secs(2),
                None,
            )
        });
        let ga = Channel::new(a)
            .greet(
                &info("mac", 1),
                &opts(3, Kind::Wired, 1),
                Duration::from_secs(2),
                None,
            )
            .unwrap();
        let gb = t.join().unwrap().unwrap();
        assert_eq!((ga.ver, ga.paths), (3, None));
        assert_eq!((gb.ver, gb.paths), (3, None));
    }

    #[test]
    fn old_peer_refused() {
        let (a, b) = pair();
        let t = thread::spawn(move || {
            let mut ch = Channel::new(b);
            ch.send(&Msg::Hello {
                ver: 2,
                node: [2; 16],
                name: "old".into(),
                displays: vec![],
            })
            .unwrap();
            assert!(matches!(
                ch.next_msg(Instant::now() + Duration::from_secs(2))
                    .unwrap(),
                Msg::Hello { .. }
            ));
            assert!(matches!(
                ch.next_msg(Instant::now() + Duration::from_secs(2))
                    .unwrap(),
                Msg::HelloAck {
                    accepted: false,
                    ..
                }
            ));
        });
        assert!(matches!(
            Channel::new(a).greet(
                &info("mac", 1),
                &opts(4, Kind::Other, 1),
                Duration::from_secs(2),
                None
            ),
            Err(NetError::VersionMismatch { ours: 4, theirs: 2 })
        ));
        t.join().unwrap();
    }

    #[test]
    fn expected_paths_is_protocol_error() {
        let (a, b) = pair();
        let t = thread::spawn(move || {
            let mut ch = Channel::new(b);
            ch.send(&Msg::Hello {
                ver: 4,
                node: [2; 16],
                name: "peer".into(),
                displays: vec![],
            })
            .unwrap();
            assert!(matches!(
                ch.next_msg(Instant::now() + Duration::from_secs(2))
                    .unwrap(),
                Msg::Hello { .. }
            ));
            ch.send(&Msg::HelloAck {
                ver: 4,
                accepted: true,
                reason: None,
            })
            .unwrap();
            assert!(matches!(
                ch.next_msg(Instant::now() + Duration::from_secs(2))
                    .unwrap(),
                Msg::HelloAck { .. }
            ));
            assert!(matches!(
                ch.next_msg(Instant::now() + Duration::from_secs(2))
                    .unwrap(),
                Msg::Paths { .. }
            ));
            ch.send(&Msg::Ping { t: 1 }).unwrap();
        });
        assert!(matches!(
            Channel::new(a).greet(
                &info("mac", 1),
                &opts(4, Kind::Other, 1),
                Duration::from_secs(2),
                None
            ),
            Err(NetError::Proto(_))
        ));
        t.join().unwrap();
    }

    #[test]
    fn v3_refusal_then_retry() {
        let (a, b) = pair();
        let t = thread::spawn(move || {
            let mut ch = Channel::new(b);
            for turn in 0..2 {
                ch.send(&Msg::Hello {
                    ver: 3,
                    node: [2; 16],
                    name: "v3".into(),
                    displays: vec![],
                })
                .unwrap();
                let hello = ch
                    .next_msg(Instant::now() + Duration::from_secs(2))
                    .unwrap();
                assert!(
                    matches!(hello, Msg::Hello { ver, .. } if ver == if turn == 0 { 4 } else { 3 })
                );
                assert!(matches!(
                    ch.next_msg(Instant::now() + Duration::from_secs(2))
                        .unwrap(),
                    Msg::HelloAck { accepted: true, .. }
                ));
                ch.send(&Msg::HelloAck {
                    ver: 3,
                    accepted: turn == 1,
                    reason: Some("version".into()),
                })
                .unwrap();
            }
        });
        let mut ch = Channel::new(a);
        assert!(matches!(
            ch.greet(
                &info("mac", 1),
                &opts(4, Kind::Other, 1),
                Duration::from_secs(2),
                None
            ),
            Err(NetError::VersionMismatch { ours: 4, theirs: 3 })
        ));
        let g = ch
            .greet(
                &info("mac", 1),
                &opts(3, Kind::Other, 1),
                Duration::from_secs(2),
                None,
            )
            .unwrap();
        assert_eq!((g.ver, g.paths), (3, None));
        t.join().unwrap();
    }

    #[test]
    fn data_flows_both_ways_and_ping_is_answered_internally() {
        let (a, b) = pair();
        let (tx_a, rx_a) = channel::<Msg>();
        let (in_a, out_a) = channel::<Msg>();
        let (tx_b, rx_b) = channel::<Msg>();
        let (in_b, out_b) = channel::<Msg>();
        let ta = thread::spawn(move || Channel::new(a).run(&rx_a, &in_a, fast()));
        let tb = thread::spawn(move || Channel::new(b).run(&rx_b, &in_b, fast()));
        tx_a.send(Msg::ClipboardText { gen: 1, text: "from a".into() }).unwrap();
        tx_b.send(Msg::Key { seq: 1, hid: 4, down: true, repeat: false }).unwrap();
        let at_b = out_b.recv_timeout(Duration::from_secs(2)).unwrap();
        let at_a = out_a.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(at_b, Msg::ClipboardText { gen: 1, text: "from a".into() });
        assert_eq!(at_a, Msg::Key { seq: 1, hid: 4, down: true, repeat: false });
        // Ping/Pong must never reach the data channel; ~10 ping intervals pass here.
        thread::sleep(Duration::from_millis(200));
        assert!(out_a.try_recv().is_err() && out_b.try_recv().is_err());
        // One side at a time: dropping both at once races each side's own drop against the other's close.
        drop(tx_a);
        assert!(matches!(ta.join().unwrap(), End::LocalClosed));
        // A closed first, so B may see that before its own drop. Windows reports a close with unread data
        // as a reset (Io); any of these is a clean end for B.
        drop(tx_b);
        let end_b = tb.join().unwrap();
        assert!(matches!(end_b, End::LocalClosed | End::PeerClosed | End::Io(_)), "{end_b:?}");
    }

    #[test]
    fn silent_peer_is_declared_dead_within_bound() {
        let (a, b) = pair();
        let (_tx, rx) = channel::<Msg>();
        let (in_tx, _in_rx) = channel::<Msg>();
        // Peer holds the socket open and never reads or answers.
        let quiet = thread::spawn(move || {
            thread::sleep(Duration::from_secs(2));
            drop(b);
        });
        let start = Instant::now();
        let end = Channel::new(a).run(&rx, &in_tx, fast());
        let took = start.elapsed();
        assert!(matches!(end, End::Dead), "got {end:?}");
        assert!(took >= Duration::from_millis(300) && took < Duration::from_secs(2), "took {took:?}");
        quiet.join().unwrap();
    }

    #[test]
    fn peer_close_is_reported_as_peer_closed() {
        let (a, b) = pair();
        let (_tx, rx) = channel::<Msg>();
        let (in_tx, _in_rx) = channel::<Msg>();
        drop(b);
        assert!(matches!(Channel::new(a).run(&rx, &in_tx, fast()), End::PeerClosed));
    }

    #[test]
    fn peer_fault_ends_the_session_with_its_message() {
        let (a, b) = pair();
        let (_tx, rx) = channel::<Msg>();
        let (in_tx, _in_rx) = channel::<Msg>();
        let mut raw = Channel::new(b);
        raw.send(&Msg::Fault { code: 7, msg: "uipi blocked".into() }).unwrap();
        match Channel::new(a).run(&rx, &in_tx, fast()) {
            End::Faulted(m) => assert!(m.contains("uipi blocked"), "got {m}"),
            other => panic!("expected Faulted, got {other:?}"),
        }
    }

    // 對端只送 Hello 框架的前半（長度標頭宣告 256 bytes，之後只給 3 bytes）就停住。
    // hello() 必須在 handshake timeout 附近回 Timeout，不能無限等待。
    #[test]
    fn hello_stalled_mid_frame_times_out() {
        let (a, mut b) = pair();
        let peer = thread::spawn(move || {
            b.write_all(&[0, 0, 1, 0, 1, 2, 3]).unwrap();
            thread::sleep(Duration::from_secs(2));
            drop(b);
        });
        let started = Instant::now();
        let r = Channel::new(a).hello(&info("mac", 1), Duration::from_millis(300));
        let took = started.elapsed();
        peer.join().unwrap();
        assert!(matches!(r, Err(NetError::Timeout)), "應為 Timeout，實得 {r:?}");
        assert!(took >= Duration::from_millis(300) && took < Duration::from_millis(1500), "took {took:?}");
    }

    // 對端宣告 4096 bytes 的框架，之後每 1ms 補一個位元組（nodelay，間隔短於讀取逾時）。每次讀取都成功（Ok），
    // 舊實作只在 Idle 時檢查 deadline，會一直等到對端停止才結束。修正後必須在 timeout 附近回 Timeout。
    #[test]
    fn hello_trickled_one_byte_at_a_time_times_out_on_deadline() {
        let (a, mut b) = pair();
        // 關掉 Nagle，確保每個位元組各自送出，讀端才是真的「持續有資料」。
        b.set_nodelay(true).unwrap();
        let peer = thread::spawn(move || {
            let _ = b.write_all(&[0, 0, 0x10, 0x00]);
            for _ in 0..4000 {
                if b.write_all(&[0u8]).is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(1));
            }
        });
        let started = Instant::now();
        let r = Channel::new(a).hello(&info("mac", 1), Duration::from_millis(300));
        let took = started.elapsed();
        peer.join().unwrap();
        assert!(matches!(r, Err(NetError::Timeout)), "應為 Timeout，實得 {r:?}");
        assert!(took < Duration::from_millis(1500), "timeout 300ms 卻花了 {took:?}");
    }

    #[test]
    fn undecodable_frame_faults_instead_of_being_skipped() {
        let (a, mut b) = pair();
        let (_tx, rx) = channel::<Msg>();
        let (in_tx, _in_rx) = channel::<Msg>();
        // Frame of length 1 whose body is an enum tag no Msg variant uses.
        b.write_all(&[0, 0, 0, 1, 0xFF]).unwrap();
        assert!(matches!(Channel::new(a).run(&rx, &in_tx, fast()), End::Faulted(_)));
    }

    // 同一次讀取裡，超長標頭前面的完整框架必須照常送達，之後才因 fault 結束。
    #[test]
    fn frames_before_an_oversize_header_are_delivered_before_fault() {
        let (a, b) = pair();
        let (_tx, rx) = channel::<Msg>();
        let (in_tx, in_rx) = channel::<Msg>();
        let key = Msg::Key { seq: 9, hid: 4, down: true, repeat: false };
        let mut raw = Channel::new(b);
        raw.send(&key).unwrap();
        raw.stream.write_all(&[0xFF, 0xFF, 0xFF, 0xFF]).unwrap();
        let end = Channel::new(a).run(&rx, &in_tx, fast());
        assert_eq!(in_rx.try_recv().unwrap(), key, "frame before the fault was lost");
        assert!(matches!(end, End::Faulted(_)), "got {end:?}");
    }
}
