//! What the shared link layer asks of this machine: interfaces, free space, and a way to wake the clock.

#[cfg(not(unix))]
use std::path::Path;

use flowseam_net::links::LinkHost;

use crate::netifs;

#[cfg(unix)]
pub use flowseam_net::node::free_bytes;
pub use flowseam_net::node::wake_clocks;

#[cfg(not(unix))]
pub fn free_bytes(_: &Path) -> Result<u64, String> {
    Err("free space is measured on unix hosts only".into())
}

pub fn link_host() -> LinkHost {
    LinkHost { interfaces: netifs::local_interfaces, addrs: netifs::local_addrs, free_bytes, wake: wake_clocks }
}
