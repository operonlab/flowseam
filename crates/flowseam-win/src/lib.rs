//! flowseam_win: the Windows side. `app` runs the worker on the shared links; `capture` and `hook` read this
//! PC's own input, `inject` puts a peer's input here, and the clipboard is a `Clipboard` (Win32 in `win32`,
//! a stdin/stdout stand-in in `stdio`) that `clip_os` serves to the shared clipboard pump.

pub mod app;
pub mod capture;
pub mod clip_os;
pub mod ddc;
pub mod kvm;
pub mod netifs;
pub mod edid;
pub mod inject;
pub mod keymap;
pub mod stdio;
pub mod touchpad;
pub mod native_drop;
#[cfg(windows)]
pub mod edge_drop;
#[cfg(windows)]
pub mod hook;
#[cfg(windows)]
pub mod win32;
#[cfg(windows)]
pub mod win_input;

/// Platform clipboard. Errors are text: they are logged, never swallowed.
pub trait Clipboard {
    /// Changes whenever the clipboard content changes (GetClipboardSequenceNumber).
    fn seq(&mut self) -> Result<u32, String>;
    /// `Ok(None)` means the clipboard holds no text, which is not a failure.
    fn read_text(&mut self) -> Result<Option<String>, String>;
    fn write_text(&mut self, text: &str) -> Result<(), String>;
}
