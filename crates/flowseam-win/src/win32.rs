//! Win32 text clipboard: OpenClipboard + CF_UNICODETEXT.
//!
//! Text is passed through byte-for-byte (no LF/CRLF normalising) so the
//! equality-based loop prevention in `ClipboardSync` keeps working.
//! `EmptyClipboard` needs an owner window, otherwise `SetClipboardData` fails,
//! so a message-only window is created per instance; create it on the thread
//! that uses it.

use std::ptr::{null, null_mut};
use std::thread::sleep;
use std::time::Duration;

use windows_sys::Win32::Foundation::{GlobalFree, HWND};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
    IsClipboardFormatAvailable, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, HWND_MESSAGE};

use crate::Clipboard;

const CF_UNICODETEXT: u32 = 13;

pub struct WinClipboard {
    hwnd: HWND,
}

impl WinClipboard {
    pub fn new() -> Result<Self, String> {
        let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
        // SAFETY: predefined STATIC class, message-only parent, no user data.
        let hwnd = unsafe {
            CreateWindowExW(0, class.as_ptr(), null(), 0, 0, 0, 0, 0, HWND_MESSAGE, null_mut(), null_mut(), null())
        };
        if hwnd.is_null() {
            return Err(format!("CreateWindowExW failed: {}", std::io::Error::last_os_error()));
        }
        Ok(WinClipboard { hwnd })
    }

    /// The clipboard can be briefly held by another process: retry a few times, then report.
    fn open(&self) -> Result<Guard, String> {
        let mut last = String::new();
        for _ in 0..10 {
            // SAFETY: hwnd is our live window.
            if unsafe { OpenClipboard(self.hwnd) } != 0 {
                return Ok(Guard);
            }
            last = std::io::Error::last_os_error().to_string();
            sleep(Duration::from_millis(10));
        }
        Err(format!("OpenClipboard failed: {last}"))
    }
}

impl Drop for WinClipboard {
    fn drop(&mut self) {
        // SAFETY: window created by new() on this thread.
        unsafe { DestroyWindow(self.hwnd) };
    }
}

/// Closes the clipboard on every exit path.
struct Guard;
impl Drop for Guard {
    fn drop(&mut self) {
        // SAFETY: only constructed after a successful OpenClipboard.
        unsafe { CloseClipboard() };
    }
}

impl Clipboard for WinClipboard {
    fn seq(&mut self) -> Result<u32, String> {
        // SAFETY: no arguments. 0 means no window station access, which is a failure.
        match unsafe { GetClipboardSequenceNumber() } {
            0 => Err("GetClipboardSequenceNumber returned 0 (no clipboard access)".into()),
            n => Ok(n),
        }
    }

    fn read_text(&mut self) -> Result<Option<String>, String> {
        let _g = self.open()?;
        // SAFETY: clipboard is open for the whole block; the locked pointer is only read inside it.
        unsafe {
            if IsClipboardFormatAvailable(CF_UNICODETEXT) == 0 {
                return Ok(None);
            }
            let h = GetClipboardData(CF_UNICODETEXT);
            if h.is_null() {
                return Err(format!("GetClipboardData failed: {}", std::io::Error::last_os_error()));
            }
            let max = GlobalSize(h) / 2;
            let p = GlobalLock(h) as *const u16;
            if p.is_null() {
                return Err(format!("GlobalLock failed: {}", std::io::Error::last_os_error()));
            }
            let mut n = 0;
            while n < max && *p.add(n) != 0 {
                n += 1;
            }
            let units = std::slice::from_raw_parts(p, n).to_vec();
            GlobalUnlock(h);
            String::from_utf16(&units).map(Some).map_err(|e| format!("clipboard text is not valid UTF-16: {e}"))
        }
    }

    /// Everything that can fail without touching the user's clipboard (allocate, fill, open) runs before
    /// `EmptyClipboard`, so a failure there leaves the current contents in place.
    fn write_text(&mut self, text: &str) -> Result<(), String> {
        let units: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes = units.len() * 2;
        // SAFETY: a fresh moveable block of `bytes` bytes, filled with exactly `units.len()` u16s.
        let h = unsafe {
            let h = GlobalAlloc(GMEM_MOVEABLE, bytes);
            if h.is_null() {
                return Err(format!("GlobalAlloc failed: {}", std::io::Error::last_os_error()));
            }
            let p = GlobalLock(h) as *mut u16;
            if p.is_null() {
                let e = std::io::Error::last_os_error();
                GlobalFree(h);
                return Err(format!("GlobalLock failed: {e}"));
            }
            std::ptr::copy_nonoverlapping(units.as_ptr(), p, units.len());
            GlobalUnlock(h);
            h
        };
        let _g = match self.open() {
            Ok(g) => g,
            Err(e) => {
                // SAFETY: never handed to the system.
                unsafe { GlobalFree(h) };
                return Err(e);
            }
        };
        // SAFETY: clipboard open; on SetClipboardData success the system owns the HGLOBAL, otherwise we free it.
        unsafe {
            if EmptyClipboard() == 0 {
                let e = std::io::Error::last_os_error();
                GlobalFree(h);
                return Err(format!("EmptyClipboard failed: {e}"));
            }
            if SetClipboardData(CF_UNICODETEXT, h).is_null() {
                let e = std::io::Error::last_os_error();
                GlobalFree(h);
                return Err(format!("SetClipboardData failed: {e}"));
            }
        }
        Ok(())
    }
}

/// Bytes the current user may still write on the volume holding `p`.
pub fn free_bytes(p: &std::path::Path) -> Result<u64, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = p.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut avail: u64 = 0;
    // SAFETY: `wide` is NUL-terminated; the two totals we do not need may be null.
    if unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut avail, null_mut(), null_mut()) } == 0 {
        return Err(format!("GetDiskFreeSpaceExW {}: {}", p.display(), std::io::Error::last_os_error()));
    }
    Ok(avail)
}
