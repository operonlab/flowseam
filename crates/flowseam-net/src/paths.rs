//! Pure candidate ranking for one peer.
use crate::session::PeerPaths;
use flowseam_core::proto::Kind;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

pub fn path_rank(a: Kind, b: Kind) -> u8 {
    a.rank().max(b.rank())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SessionKey {
    pub rank: u8,
    pub not_lower_dialer: bool,
}
impl SessionKey {
    pub fn new(rank: u8, dialed_by_lower_id: bool) -> Self {
        Self {
            rank,
            not_lower_dialer: !dialed_by_lower_id,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Hint,
    Learned,
}

struct Entry {
    addr: SocketAddr,
    kind: Kind,
    source: Source,
    learned_at: Option<Instant>,
    failures: u32,
    ready_at: Option<Instant>,
}

pub struct Candidates {
    entries: Vec<Entry>,
}
impl Default for Candidates {
    fn default() -> Self {
        Self::new()
    }
}
impl Candidates {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
    pub fn add_hint(&mut self, addr: SocketAddr) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.addr == addr) {
            e.source = Source::Hint;
            return;
        }
        if self.entries.len() < 8 {
            self.entries.push(Entry {
                addr,
                kind: Kind::Other,
                source: Source::Hint,
                learned_at: None,
                failures: 0,
                ready_at: None,
            });
        }
    }
    pub fn learn(&mut self, paths: &PeerPaths, now: Instant) {
        for path in &paths.addrs {
            if let Some(e) = self.entries.iter_mut().find(|e| e.addr == path.addr) {
                e.kind = path.kind;
                if e.source == Source::Learned {
                    e.learned_at = Some(now);
                }
                continue;
            }
            let new = Entry {
                addr: path.addr,
                kind: path.kind,
                source: Source::Learned,
                learned_at: Some(now),
                failures: 0,
                ready_at: None,
            };
            if self.entries.len() < 8 {
                self.entries.push(new);
            } else if let Some((i, _)) = self
                .entries
                .iter()
                .enumerate()
                .filter(|(_, e)| e.source == Source::Learned)
                .min_by_key(|(_, e)| e.learned_at)
            {
                self.entries[i] = new;
            }
        }
    }
    pub fn next_dial(
        &self,
        now: Instant,
        local_kind: &dyn Fn(SocketAddr) -> Option<Kind>,
        current: Option<SessionKey>,
        i_am_lower: bool,
    ) -> Option<SocketAddr> {
        self.entries
            .iter()
            .filter(|e| e.ready_at.is_none_or(|until| now >= until))
            .map(|e| {
                (
                    SessionKey::new(local_kind(e.addr).map_or(3, |kind| path_rank(kind, e.kind)), i_am_lower),
                    e.addr,
                )
            })
            .filter(|(key, _)| current.is_none_or(|c| *key < c))
            .min_by_key(|(key, addr)| (*key, *addr))
            .map(|(_, addr)| addr)
    }
    pub fn dial_failed(&mut self, addr: SocketAddr, now: Instant) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.addr == addr) {
            let delay = 200u64
                .saturating_mul(1u64.checked_shl(e.failures.min(8)).unwrap_or(u64::MAX))
                .min(30_000);
            e.failures = e.failures.saturating_add(1);
            e.ready_at = Some(now + Duration::from_millis(delay));
        }
    }
    pub fn dial_ok(&mut self, addr: SocketAddr) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.addr == addr) {
            e.failures = 0;
            e.ready_at = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flowseam_core::proto::PathAddr;
    fn addr(port: u16) -> SocketAddr {
        format!("127.0.0.1:{port}").parse().unwrap()
    }
    #[test]
    fn ranking_and_ties() {
        let now = Instant::now();
        let mut c = Candidates::new();
        c.learn(
            &PeerPaths {
                kind: Kind::Other,
                addrs: vec![
                    PathAddr {
                        addr: addr(1),
                        kind: Kind::WiFi,
                    },
                    PathAddr {
                        addr: addr(2),
                        kind: Kind::Wired,
                    },
                ],
            },
            now,
        );
        assert_eq!(
            c.next_dial(now, &|_| Some(Kind::Wired), None, true),
            Some(addr(2))
        );
        assert_eq!(path_rank(Kind::Wired, Kind::WiFi), 2);
        assert!(SessionKey::new(0, true) < SessionKey::new(0, false));
        assert_eq!(
            c.next_dial(now, &|_| Some(Kind::Wired), Some(SessionKey::new(0, true)), false),
            None
        );
    }
    #[test]
    fn unreachable_wired_loses_to_reachable_wifi() {
        let now = Instant::now();
        let wired: SocketAddr = "169.254.1.2:9".parse().unwrap();
        let wifi: SocketAddr = "192.0.2.2:9".parse().unwrap();
        let mut c = Candidates::new();
        c.learn(&PeerPaths { kind: Kind::Other, addrs: vec![PathAddr { addr: wired, kind: Kind::Wired }, PathAddr { addr: wifi, kind: Kind::WiFi }] }, now);
        let local_kind = |addr: SocketAddr| if addr == wifi { Some(Kind::WiFi) } else { None };
        assert_eq!(c.next_dial(now, &local_kind, None, true), Some(wifi));
        assert_eq!(c.next_dial(now, &local_kind, Some(SessionKey::new(2, true)), true), None);
    }
    #[test]
    fn backoff_and_cap() {
        let now = Instant::now();
        let mut c = Candidates::new();
        c.add_hint(addr(1));
        c.dial_failed(addr(1), now);
        assert_eq!(
            c.next_dial(
                now + Duration::from_millis(100),
                &|_| Some(Kind::Other),
                None,
                true
            ),
            None
        );
        assert_eq!(
            c.next_dial(
                now + Duration::from_millis(200),
                &|_| Some(Kind::Other),
                None,
                true
            ),
            Some(addr(1))
        );
        c.dial_failed(addr(1), now + Duration::from_millis(200));
        assert_eq!(
            c.next_dial(
                now + Duration::from_millis(599),
                &|_| Some(Kind::Other),
                None,
                true
            ),
            None
        );
        assert_eq!(
            c.next_dial(
                now + Duration::from_millis(600),
                &|_| Some(Kind::Other),
                None,
                true
            ),
            Some(addr(1))
        );
        for i in 2..=8 {
            c.learn(
                &PeerPaths {
                    kind: Kind::Other,
                    addrs: vec![PathAddr {
                        addr: addr(i),
                        kind: Kind::Other,
                    }],
                },
                now + Duration::from_millis(u64::from(i)),
            );
        }
        c.learn(
            &PeerPaths {
                kind: Kind::Other,
                addrs: vec![PathAddr {
                    addr: addr(9),
                    kind: Kind::Other,
                }],
            },
            now + Duration::from_secs(1),
        );
        for _ in 0..10 { c.dial_failed(addr(1), now); }
        assert_eq!(c.entries.iter().find(|e| e.addr == addr(1)).unwrap().ready_at,
            Some(now + Duration::from_secs(30)));
        c.dial_ok(addr(1));
        assert_eq!(c.entries.iter().find(|e| e.addr == addr(1)).unwrap().ready_at, None);
        assert_eq!(c.entries.len(), 8);
        assert!(c
            .entries
            .iter()
            .any(|e| e.addr == addr(1) && e.source == Source::Hint));
        assert!(!c.entries.iter().any(|e| e.addr == addr(2)));
    }
}
