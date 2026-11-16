//! flowseam_core: pure protocol types, framing, boundary geometry, key tracking and
//! the platform backend trait surface. No OS calls live in this crate.

pub mod backend;
pub mod chord;
pub mod clipboard;
pub mod config;
pub mod cursor;
pub mod ddc;
pub mod emergency;
pub mod framing;
pub mod geometry;
pub mod gesture;
pub mod kvm;
pub mod kvm_side;
pub mod log;
pub mod menu;
pub mod mac_shortcuts;
pub mod modifiers;
pub mod proto;
pub mod switch;
pub mod touch;
pub mod forward;
pub mod status;
