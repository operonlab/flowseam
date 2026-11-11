//! flowseam_mac: macOS platform backend (design §4–§5). Implements flowseam_core's
//! `InputBackend` over CGEventTap / CGEventPost / CGDisplay*.
//!
//! Pure logic (`keymap`, `state`, `gesture`) compiles everywhere and runs without OS
//! permissions. Everything that touches the OS is macOS-only.

pub mod ddc;
pub mod front;
pub mod gesture;
pub mod grab;
pub mod inject_mark;
pub mod keymap;
pub mod kvm;
#[cfg(target_os = "macos")]
pub mod links;
pub mod sink;
pub mod state;
pub mod supervisor;
pub mod switch_gate;

#[cfg(target_os = "macos")]
mod ffi;
#[cfg(target_os = "macos")]
mod heartbeat;
#[cfg(target_os = "macos")]
mod signal_pipe;
#[cfg(target_os = "macos")]
mod ifaces;
#[cfg(target_os = "macos")]
pub mod backend;
#[cfg(target_os = "macos")]
pub mod native;
#[cfg(target_os = "macos")]
pub mod sys;
#[cfg(target_os = "macos")]
pub mod tap;
#[cfg(target_os = "macos")]
pub mod wire;
#[cfg(target_os = "macos")]
pub mod worker;

#[cfg(target_os = "macos")]
pub use backend::MacBackend;
