//! Pinned peer fingerprints. Pairing is an explicit act: a fingerprint enters
//! the store only when a human (or the pairing flow) adds it. Unknown peers
//! are refused, never trusted on first use.

use std::fs;
use std::path::Path;

use crate::identity::{from_hex, to_hex, Fingerprint};
use crate::NetError;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PinSet(Vec<Fingerprint>);

impl PinSet {
    pub fn new(fps: impl IntoIterator<Item = Fingerprint>) -> Self {
        let mut v: Vec<Fingerprint> = fps.into_iter().collect();
        v.sort();
        v.dedup();
        PinSet(v)
    }

    /// An empty set trusts nobody; that is the default, not a fallback.
    pub fn contains(&self, fp: &Fingerprint) -> bool {
        self.0.binary_search(fp).is_ok()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinEntry {
    pub name: String,
    pub fp: Fingerprint,
}

/// On-disk pin list: one `<64-hex-fingerprint> <name>` per line. A malformed
/// line fails the whole load; silently dropping a pin would weaken trust.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PinStore {
    pub entries: Vec<PinEntry>,
}

impl PinStore {
    /// A missing file is an empty store (first run). Anything else unreadable
    /// or malformed is an error.
    pub fn load(path: &Path) -> Result<Self, NetError> {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(NetError::Io(e)),
        };
        let mut entries = Vec::new();
        for (n, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let mut parts = line.splitn(2, ' ');
            let hex = parts.next().unwrap_or("");
            let name = parts.next().unwrap_or("").to_string();
            let fp = from_hex(hex).ok_or_else(|| {
                NetError::Pins(format!("{}:{}: bad fingerprint", path.display(), n + 1))
            })?;
            entries.push(PinEntry { name, fp });
        }
        Ok(PinStore { entries })
    }

    pub fn save(&self, path: &Path) -> Result<(), NetError> {
        let mut out = String::new();
        for e in &self.entries {
            out.push_str(&to_hex(&e.fp));
            out.push(' ');
            out.push_str(&e.name.replace(['\n', '\r'], " "));
            out.push('\n');
        }
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(NetError::Io)?;
        }
        // Write-then-rename so a crash never leaves a truncated pin list.
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, out).map_err(NetError::Io)?;
        fs::rename(&tmp, path).map_err(NetError::Io)
    }

    pub(crate) fn add(&mut self, name: &str, fp: Fingerprint) {
        if !self.entries.iter().any(|e| e.fp == fp) {
            self.entries.push(PinEntry { name: name.to_string(), fp });
        }
    }

    pub fn pin_set(&self) -> PinSet {
        PinSet::new(self.entries.iter().map(|e| e.fp))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_set_trusts_nobody() {
        assert!(!PinSet::default().contains(&[0u8; 32]));
    }

    #[test]
    fn membership_is_exact() {
        let set = PinSet::new([[1u8; 32], [2u8; 32]]);
        assert!(set.contains(&[1u8; 32]));
        assert!(!set.contains(&[3u8; 32]));
        let mut near = [1u8; 32];
        near[31] ^= 1;
        assert!(!set.contains(&near), "one flipped bit must not match");
    }

    #[test]
    fn store_round_trips_through_disk() {
        let dir = crate::test_dir("pins-roundtrip");
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("pins.txt");
        let mut s = PinStore::default();
        s.add("studio mac", [0xAB; 32]);
        s.add("mbp", [0x01; 32]);
        s.add("dup", [0xAB; 32]);
        assert_eq!(s.entries.len(), 2, "same fingerprint is added once");
        s.save(&path).unwrap();
        let back = PinStore::load(&path).unwrap();
        assert_eq!(back, s);
        assert!(back.pin_set().contains(&[0x01; 32]));
    }

    #[test]
    fn missing_file_is_empty_store() {
        let path = crate::test_dir("no-such-pins.txt");
        let _ = fs::remove_file(&path);
        assert!(PinStore::load(&path).unwrap().entries.is_empty());
    }

    #[test]
    fn malformed_line_fails_loud() {
        let dir = crate::test_dir(&format!("pins-bad-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pins.txt");
        fs::write(&path, format!("{} ok\nzz not-hex\n", to_hex(&[7u8; 32]))).unwrap();
        let got = PinStore::load(&path);
        assert!(matches!(got, Err(NetError::Pins(_))), "got {got:?}");
    }
}
