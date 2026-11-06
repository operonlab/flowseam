//! Discovery beacons (docs/multipath.md §3): a 24-byte UDP datagram naming a node and its TCP port.
//! A beacon is only a hint; the address it yields is dialled and must still pass the pinned TLS handshake.
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub const BEACON_PORT: u16 = 24790;
pub const BEACON_LEN: usize = 24;
const MAGIC: &[u8; 4] = b"FSB1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Beacon { pub ver: u16, pub node: [u8; 16], pub port: u16 }

pub fn encode(b: &Beacon) -> [u8; BEACON_LEN] {
    let mut bytes = [0; BEACON_LEN];
    bytes[..4].copy_from_slice(MAGIC);
    bytes[4..6].copy_from_slice(&b.ver.to_be_bytes());
    bytes[6..22].copy_from_slice(&b.node);
    bytes[22..24].copy_from_slice(&b.port.to_be_bytes());
    bytes
}

/// None unless exactly BEACON_LEN bytes with the magic. A port of 0 is None too.
pub fn decode(bytes: &[u8]) -> Option<Beacon> {
    if bytes.len() != BEACON_LEN || &bytes[..4] != MAGIC { return None; }
    let port = u16::from_be_bytes([bytes[22], bytes[23]]);
    if port == 0 { return None; }
    let mut node = [0; 16];
    node.copy_from_slice(&bytes[6..22]);
    Some(Beacon { ver: u16::from_be_bytes([bytes[4], bytes[5]]), node, port })
}

/// The IPv4 broadcast address of an interface: addr | !mask.
pub fn broadcast(addr: Ipv4Addr, mask: Ipv4Addr) -> Ipv4Addr {
    Ipv4Addr::from(u32::from(addr) | !u32::from(mask))
}

/// Sends beacons from one unbound UDP socket with broadcast enabled.
pub struct Sender { socket: UdpSocket }

impl Sender {
    pub fn new() -> io::Result<Self> {
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        socket.set_broadcast(true)?;
        Ok(Self { socket })
    }

    /// Sends one datagram per target, collecting the first error without stopping later sends.
    pub fn send(&self, b: &Beacon, targets: &[Ipv4Addr], port: u16) -> (usize, Option<io::Error>) {
        let bytes = encode(&Beacon { ver: flowseam_core::proto::PROTO_VERSION, ..*b });
        let mut sent = 0;
        let mut first_error = None;
        for &target in targets {
            match self.socket.send_to(&bytes, SocketAddrV4::new(target, port)) {
                Ok(BEACON_LEN) => sent += 1,
                Ok(_) => { if first_error.is_none() { first_error = Some(io::Error::new(io::ErrorKind::WriteZero, "short beacon send")); } }
                Err(error) => { if first_error.is_none() { first_error = Some(error); } }
            }
        }
        (sent, first_error)
    }
}

/// Receives beacons on all IPv4 interfaces.
pub struct Listener { socket: UdpSocket, invalid: AtomicU64 }

impl Listener {
    pub fn bind(port: u16) -> io::Result<Self> {
        Ok(Self { socket: UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port))?, invalid: AtomicU64::new(0) })
    }

    /// Returns the next valid beacon before timeout, using its advertised TCP port for dialing.
    pub fn recv(&self, timeout: Duration) -> Option<(Beacon, SocketAddr)> {
        let start = Instant::now();
        let mut bytes = [0; BEACON_LEN + 1];
        loop {
            let remaining = timeout.checked_sub(start.elapsed())?;
            if remaining.is_zero() || self.socket.set_read_timeout(Some(remaining)).is_err() { return None; }
            match self.socket.recv_from(&mut bytes) {
                Ok((len, source)) => {
                    if let Some(beacon) = decode(&bytes[..len]) {
                        return Some((beacon, SocketAddr::new(source.ip(), beacon.port)));
                    }
                    self.invalid.fetch_add(1, Ordering::Relaxed);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => continue,
                Err(_) => return None,
            }
        }
    }

    pub fn invalid(&self) -> u64 { self.invalid.load(Ordering::Relaxed) }

    pub fn local_port(&self) -> io::Result<u16> { Ok(self.socket.local_addr()?.port()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn beacon() -> Beacon { Beacon { ver: flowseam_core::proto::PROTO_VERSION, node: [0x5a; 16], port: 31415 } }

    #[test]
    fn round_trip() {
        let b = beacon();
        let bytes = encode(&b);
        assert_eq!(&bytes[..4], b"FSB1");
        assert_eq!(&bytes[4..6], &b.ver.to_be_bytes());
        assert_eq!(&bytes[6..22], &b.node);
        assert_eq!(&bytes[22..24], &b.port.to_be_bytes());
        assert_eq!(decode(&bytes), Some(b));
    }

    #[test]
    fn reject_bad_datagrams() {
        let mut bytes = encode(&beacon());
        bytes[0] = b'X';
        assert_eq!(decode(&bytes), None);
        let bytes = encode(&beacon());
        assert_eq!(decode(&bytes[..23]), None);
        assert_eq!(decode(&[bytes.as_slice(), &[0]].concat()), None);
        let mut bytes = bytes;
        bytes[22..24].copy_from_slice(&0u16.to_be_bytes());
        assert_eq!(decode(&bytes), None);
    }

    #[test]
    fn broadcast_addresses() {
        assert_eq!(broadcast(Ipv4Addr::new(192, 168, 68, 60), Ipv4Addr::new(255, 255, 255, 0)), Ipv4Addr::new(192, 168, 68, 255));
        assert_eq!(broadcast(Ipv4Addr::new(169, 254, 192, 119), Ipv4Addr::new(255, 255, 0, 0)), Ipv4Addr::new(169, 254, 255, 255));
    }

    #[test]
    fn loopback_uses_advertised_port() {
        let listener = Listener::bind(0).unwrap();
        let sender = Sender::new().unwrap();
        let b = beacon();
        assert_eq!(sender.send(&b, &[Ipv4Addr::LOCALHOST], listener.local_port().unwrap()).0, 1);
        assert_eq!(listener.recv(Duration::from_secs(1)), Some((b, SocketAddr::from((Ipv4Addr::LOCALHOST, b.port)))));
        assert_eq!(listener.invalid(), 0);
    }

    #[test]
    fn sender_uses_current_protocol_version() {
        let listener = Listener::bind(0).unwrap();
        let sender = Sender::new().unwrap();
        let b = Beacon { ver: 0, ..beacon() };
        assert_eq!(sender.send(&b, &[Ipv4Addr::LOCALHOST], listener.local_port().unwrap()).0, 1);
        let (received, _) = listener.recv(Duration::from_secs(1)).unwrap();
        assert_eq!(received.ver, flowseam_core::proto::PROTO_VERSION);
        assert_eq!(received.node, b.node);
        assert_eq!(received.port, b.port);
    }

    #[test]
    fn skips_garbage() {
        let listener = Listener::bind(0).unwrap();
        let port = listener.local_port().unwrap();
        let junk = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        for _ in 0..5 { junk.send_to(b"junk", (Ipv4Addr::LOCALHOST, port)).unwrap(); }
        let b = beacon();
        let sender = Sender::new().unwrap();
        assert_eq!(sender.send(&b, &[Ipv4Addr::LOCALHOST], port).0, 1);
        assert_eq!(listener.recv(Duration::from_secs(1)), Some((b, SocketAddr::from((Ipv4Addr::LOCALHOST, b.port)))));
        assert_eq!(listener.invalid(), 5);
    }

    #[test]
    fn times_out() {
        let listener = Listener::bind(0).unwrap();
        let start = Instant::now();
        assert_eq!(listener.recv(Duration::from_millis(200)), None);
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
