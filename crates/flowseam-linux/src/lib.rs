//! flowseam_linux: the Linux side. This slice is a headless peer: it links, pairs and is driven, and what
//! would be injected is printed. Real capture and injection (portals, evdev) come in later slices.

pub mod app;
pub mod host;
pub mod netifs;
pub mod sink;
pub mod stdio;
