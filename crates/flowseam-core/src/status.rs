use crate::proto::Kind;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct LinkStatus { pub written_ms: u64, pub peers: Vec<PeerStatus> }

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PeerStatus { pub name: String, pub up: bool, pub kind: Option<Kind>, pub remote: Option<String>, pub since_ms: u64, pub reason: Option<String> }

pub fn status_path(config: &Path) -> PathBuf { config.with_file_name("status.toml") }

pub fn write(path: &Path, s: &LinkStatus) -> io::Result<()> {
    let text = toml::to_string(s).map_err(io::Error::other)?;
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
    }
    fs::rename(tmp, path)
}

pub fn read(path: &Path) -> io::Result<LinkStatus> {
    toml::from_str(&fs::read_to_string(path)?).map_err(io::Error::other)
}

pub fn now_ms() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64 }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_missing_file() {
        let path = std::env::temp_dir().join(format!("flowseam-status-{}-{}.toml", std::process::id(), now_ms()));
        assert_eq!(read(&path).unwrap_err().kind(), io::ErrorKind::NotFound);
        let status = LinkStatus { written_ms: 42, peers: vec![PeerStatus { name: "Mac".into(), up: true, kind: Some(Kind::WiFi), remote: Some("192.168.1.2:24800".into()), since_ms: 40, reason: None }] };
        write(&path, &status).unwrap();
        assert_eq!(read(&path).unwrap(), status);
        fs::remove_file(path).unwrap();
    }
}
