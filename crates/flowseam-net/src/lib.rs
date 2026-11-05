//! flowseam_net: TLS transport, fingerprint pinning, framed session and clipboard
//! text sync for Flowseam (design §2, §5, §7). Blocking std I/O on purpose:
//! one session is one thread, which keeps the failure surface small.

use std::fmt;
use std::io;

pub mod backoff;
pub mod clipboard;
pub mod files;
pub mod identity;
pub mod link;
pub mod pair;
pub mod pins;
pub mod session;
pub mod tls;

#[derive(Debug)]
pub enum NetError {
    Io(io::Error),
    /// Handshake or certificate failure. Never transient, never retried.
    Tls(rustls::Error),
    Cert(String),
    Identity(String),
    Pins(String),
    /// Pairing failed. The text is deliberately the same for a wrong code, a
    /// swapped fingerprint and a mismatched MAC, so it leaks nothing about which.
    Pair(String),
    Proto(String),
    VersionMismatch { ours: u16, theirs: u16 },
    /// The peer read our Hello and answered `accepted = false`.
    Rejected(String),
    Oversize { len: usize, limit: usize },
    Timeout,
}

impl fmt::Display for NetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NetError::Io(e) => write!(f, "io: {e}"),
            NetError::Tls(e) => write!(f, "tls: {e}"),
            NetError::Cert(m) => write!(f, "certificate: {m}"),
            NetError::Identity(m) => write!(f, "identity: {m}"),
            NetError::Pins(m) => write!(f, "pin store: {m}"),
            NetError::Pair(m) => write!(f, "pairing: {m}"),
            NetError::Proto(m) => write!(f, "protocol: {m}"),
            NetError::VersionMismatch { ours, theirs } => {
                write!(f, "protocol version mismatch: ours {ours}, peer {theirs}")
            }
            NetError::Rejected(m) => write!(f, "peer rejected hello: {m}"),
            NetError::Oversize { len, limit } => write!(f, "payload {len} B exceeds {limit} B"),
            NetError::Timeout => write!(f, "timed out"),
        }
    }
}

impl std::error::Error for NetError {}

impl NetError {
    /// Only transport-level failures are worth reconnecting for. Pin, cert,
    /// version and rejection failures are deliberate and must stay loud.
    pub fn is_transient(&self) -> bool {
        matches!(self, NetError::Io(_) | NetError::Timeout)
    }
}

/// Scratch directory for tests, kept under the workspace target dir.
#[cfg(test)]
pub(crate) fn test_dir(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/flowseam-net-tests")
        .join(name)
}
