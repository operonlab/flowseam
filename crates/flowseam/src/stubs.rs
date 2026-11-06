#![cfg(target_os = "macos")]
//! STUBS for pieces owned by other branches. Each one fails loudly rather than pretending.
//! Swap points:
//!   - `run_worker`   → `flowseam_mac::worker::run(&Config)` + `flowseam_mac::worker::real_cursor_os()` (feature/mac2)
//!   - `pair_as_client` → `flowseam_net::pair::pair_as_client` (feature/net2)

use flowseam_core::config::Config;
use flowseam_net::identity::{Fingerprint, Identity};
use std::io::{Read, Write};

/// Stand-in worker: does nothing but wait for the GUI to close stdin, then reports CLEAN.
/// It never touches the cursor. The CLEAN marker is the exact bytes the supervisor expects.
pub fn run_worker(cfg: &Config) -> Result<(), String> {
    eprintln!("[stub worker] 尚未接上 flowseam_mac::worker::run；僅示範啟停。peers={}", cfg.peers.len());
    let mut sink = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut sink);
    let mut out = std::io::stdout();
    out.write_all(flowseam_mac::supervisor::CLEAN).map_err(|e| e.to_string())?;
    out.flush().map_err(|e| e.to_string())
}

pub fn pair_as_client(addr: &str, code: &str, _id: &Identity) -> Result<Fingerprint, String> {
    Err(format!("配對尚未接上（等 feature/net2 的 pair_as_client）。addr={addr} code 長度={}", code.len()))
}
