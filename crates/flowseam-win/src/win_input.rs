//! SendInput injection, cursor position and the display list, in physical pixels.
//!
//! Call [`dpi_aware`] before anything else: without per-monitor DPI awareness Windows hands a scaled
//! cursor position and scaled monitor rects to this process, and absolute moves land in the wrong place.

use std::mem::size_of;

use windows_sys::Win32::Foundation::{BOOL, LPARAM, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::{EnumDisplayMonitors, HDC, HMONITOR};
use windows_sys::Win32::UI::HiDpi::{
    GetAwarenessFromDpiAwarenessContext, GetThreadDpiAwarenessContext, SetProcessDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, DPI_AWARENESS_PER_MONITOR_AWARE,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
    KEYEVENTF_SCANCODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, VK_NUMLOCK,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

use flowseam_core::proto::{Point, Rect};

use crate::inject::{Injector, WinButton};
use crate::keymap::Scan;

/// Tags every event we inject ("kmsh"), so a future capture hook on this machine can skip its own output.
pub const INJECT_MARK: usize = 0x6B6D_7368;
const WHEEL_DELTA: i32 = 120;
const XBUTTON1: u32 = 1;
const XBUTTON2: u32 = 2;

const ERROR_ACCESS_DENIED: i32 = 5;

/// Per-monitor DPI awareness. `ERROR_ACCESS_DENIED` means the awareness was already fixed (by a manifest
/// or by Windows) before we got here, which is documented, not a failure: then it checks what it is.
/// `Err` means positions may be scaled on monitors that are not at 100 %; the caller decides how loud.
pub fn dpi_aware() -> Result<(), String> {
    // SAFETY: plain call with a predefined context constant.
    if unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) } != 0 {
        return Ok(());
    }
    let e = std::io::Error::last_os_error();
    if e.raw_os_error() != Some(ERROR_ACCESS_DENIED) {
        return Err(format!("SetProcessDpiAwarenessContext failed: {e}"));
    }
    // SAFETY: queries on this thread's own context.
    let current = unsafe { GetAwarenessFromDpiAwarenessContext(GetThreadDpiAwarenessContext()) };
    if current == DPI_AWARENESS_PER_MONITOR_AWARE {
        return Ok(());
    }
    Err(format!(
        "DPI awareness was already set to {current} (0 unaware, 1 system) before flowseam-win could choose \
         per-monitor; on a monitor scaled above 100 % the cursor lands off by the scale factor"
    ))
}

unsafe extern "system" fn collect_monitor(_m: HMONITOR, _dc: HDC, r: *mut RECT, data: LPARAM) -> BOOL {
    // SAFETY: `data` is the &mut Vec passed by `displays`, alive for the whole EnumDisplayMonitors call;
    // `r` is the monitor rect Windows passes in.
    let out = unsafe { &mut *(data as *mut Vec<Rect>) };
    let r = unsafe { &*r };
    out.push(Rect { x: r.left, y: r.top, w: r.right - r.left, h: r.bottom - r.top });
    1
}

/// Every monitor, in virtual-screen coordinates.
pub fn displays() -> Result<Vec<Rect>, String> {
    let mut out: Vec<Rect> = Vec::new();
    // SAFETY: the callback only runs during this call and writes into `out` through the LPARAM.
    let ok = unsafe {
        EnumDisplayMonitors(std::ptr::null_mut(), std::ptr::null(), Some(collect_monitor), &mut out as *mut Vec<Rect> as LPARAM)
    };
    if ok == 0 || out.is_empty() {
        return Err(format!("EnumDisplayMonitors found no monitors: {}", std::io::Error::last_os_error()));
    }
    Ok(out)
}

fn virtual_screen() -> Rect {
    // SAFETY: plain metric queries.
    unsafe {
        Rect {
            x: GetSystemMetrics(SM_XVIRTUALSCREEN),
            y: GetSystemMetrics(SM_YVIRTUALSCREEN),
            w: GetSystemMetrics(SM_CXVIRTUALSCREEN),
            h: GetSystemMetrics(SM_CYVIRTUALSCREEN),
        }
    }
}

/// A virtual-screen pixel to SendInput's 0..=65535 absolute range for `MOUSEEVENTF_VIRTUALDESK`.
///
/// Windows maps a normalized value back with `floor(n * len / 65536)`, so the value here is the
/// smallest `n` that lands on the pixel (a ceiling). Rounding down instead lands one pixel short for
/// most positions, and then a one-pixel move right or down maps back onto the same pixel: slow mouse
/// motion stalls. Measured on the test machine before the fix: +50 from x=-2536 landed on -2487.
pub fn normalize(p: Point, vs: Rect) -> (i32, i32) {
    let scale = |v: i32, origin: i32, len: i32| -> i32 {
        let len = len.max(1) as i64;
        let off = ((v - origin) as i64).clamp(0, len - 1);
        ((off * 65536 + len - 1) / len).min(65535) as i32
    };
    (scale(p.x, vs.x, vs.w), scale(p.y, vs.y, vs.h))
}

fn send(inputs: &[INPUT]) -> Result<(), String> {
    // SAFETY: a slice of fully initialised INPUTs; cbSize is the struct size SendInput checks.
    let n = unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), size_of::<INPUT>() as i32) };
    if n as usize != inputs.len() {
        // UIPI blocks injection into an elevated foreground window without setting an error code.
        return Err(format!(
            "SendInput inserted {n} of {} ({}; an elevated window in front blocks injection)",
            inputs.len(),
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn mouse(dx: i32, dy: i32, data: u32, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data, dwFlags: flags, time: 0, dwExtraInfo: INJECT_MARK } },
    }
}

pub struct SendInputInjector;

impl Injector for SendInputInjector {
    fn key(&mut self, scan: Scan, down: bool) -> Result<(), String> {
        let mut flags = KEYEVENTF_SCANCODE;
        if scan.extended {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        if !down {
            flags |= KEYEVENTF_KEYUP;
        }
        send(&[INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT { wVk: 0, wScan: scan.code, dwFlags: flags, time: 0, dwExtraInfo: INJECT_MARK },
            },
        }])
    }

    fn move_to(&mut self, p: Point) -> Result<(), String> {
        let (x, y) = normalize(p, virtual_screen());
        send(&[mouse(x, y, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK)])
    }

    fn cursor(&mut self) -> Result<Point, String> {
        let mut p = POINT { x: 0, y: 0 };
        // SAFETY: writes one POINT.
        if unsafe { GetCursorPos(&mut p) } == 0 {
            return Err(format!("GetCursorPos failed: {}", std::io::Error::last_os_error()));
        }
        Ok(Point { x: p.x, y: p.y })
    }

    fn button(&mut self, b: WinButton, down: bool) -> Result<(), String> {
        let (flags, data) = match (b, down) {
            (WinButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (WinButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
            (WinButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (WinButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (WinButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (WinButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (WinButton::X1, true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
            (WinButton::X1, false) => (MOUSEEVENTF_XUP, XBUTTON1),
            (WinButton::X2, true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
            (WinButton::X2, false) => (MOUSEEVENTF_XUP, XBUTTON2),
        };
        send(&[mouse(0, 0, data, flags)])
    }

    fn wheel(&mut self, v: i32, h: i32) -> Result<(), String> {
        let mut ev = Vec::with_capacity(2);
        if v != 0 {
            ev.push(mouse(0, 0, v.saturating_mul(WHEEL_DELTA) as u32, MOUSEEVENTF_WHEEL));
        }
        if h != 0 {
            ev.push(mouse(0, 0, h.saturating_mul(WHEEL_DELTA) as u32, MOUSEEVENTF_HWHEEL));
        }
        if ev.is_empty() {
            return Ok(());
        }
        send(&ev)
    }

    fn num_lock(&mut self) -> Result<bool, String> {
        // SAFETY: plain query; the low bit is the toggle state.
        Ok(unsafe { GetKeyState(VK_NUMLOCK as i32) } & 1 != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How Windows turns a normalized absolute coordinate back into a pixel offset.
    fn back(n: i32, len: i32) -> i32 {
        (n as i64 * len as i64 / 65536) as i32
    }

    #[test]
    fn every_pixel_round_trips_exactly() {
        // The test machine's desktop (2560 left of a 1920 primary), plus odd and tiny widths.
        for (origin, len) in [(-2560, 4480), (-356, 1440), (0, 1920), (0, 1081), (0, 3), (0, 1)] {
            for off in 0..len {
                let vs = Rect { x: origin, y: origin, w: len, h: len };
                let (n, _) = normalize(Point { x: origin + off, y: origin }, vs);
                assert!((0..=65535).contains(&n), "{n} out of range");
                assert_eq!(back(n, len), off, "len {len}: pixel {off} came back as {}", back(n, len));
            }
        }
    }

    #[test]
    fn normalize_clamps_outside_the_desktop() {
        let vs = Rect { x: -1280, y: 0, w: 3200, h: 1080 };
        assert_eq!(normalize(Point { x: -1280, y: 0 }, vs), (0, 0));
        let (x, y) = normalize(Point { x: 5000, y: -10 }, vs);
        assert_eq!((back(x, 3200), back(y, 1080)), (3199, 0));
    }

    /// Reads real state only (no injection): the monitor list and the cursor must be readable here.
    /// Needs the logged-in desktop: over SSH, GetCursorPos fails with 1459 (no interactive window
    /// station), and so does injection, which is why flowseam-win has to run in the user's session.
    #[test]
    #[ignore = "needs the interactive desktop session; run with --ignored from a logged-in session"]
    fn displays_and_cursor_are_readable() {
        let d = displays().expect("monitors");
        assert!(d.iter().all(|r| r.w > 0 && r.h > 0), "{d:?}");
        SendInputInjector.cursor().expect("cursor position");
    }
}
