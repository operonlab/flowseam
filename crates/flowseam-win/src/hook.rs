//! The low-level keyboard and mouse hooks: this PC's own input, for the session that may drive a peer.
//!
//! One thread installs both hooks and pumps their messages for the life of the process; Windows calls the
//! hooks on it. Each call asks the attached session (`capture::target`) for a verdict and returns at once:
//! Windows holds back all input while a hook runs, and removes a hook that keeps it waiting. Our own
//! injection is passed before anything is locked, and between sessions everything passes. The hooks end
//! with the process, so a crash leaves no input swallowed.
#![cfg(windows)]

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::time::Duration;

use flowseam_core::backend::Disposition;
use flowseam_core::proto::Point;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::HRAWINPUT;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetCursorPos, GetMessageW, KillTimer,
    PostMessageW, PostThreadMessageW, RegisterClassW, SetCursor, SetCursorPos, SetLayeredWindowAttributes, SetTimer,
    SetWindowPos, SetWindowsHookExW, ShowWindow, TranslateMessage, UnhookWindowsHookEx, HC_ACTION, HHOOK,
    HWND_TOPMOST, KBDLLHOOKSTRUCT, LWA_ALPHA, MSG, MSLLHOOKSTRUCT, SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_HIDE,
    WH_KEYBOARD_LL, WH_MOUSE_LL, WM_APP, WM_INPUT, WM_INPUT_DEVICE_CHANGE, WM_SETCURSOR, WM_TIMER, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP, MOUSEEVENTF_MOVE, MOUSEINPUT,
};

use crate::capture::{self, Target, Translator, INJECT_MARK, PROBE_MARK};
use crate::inject::Grab;

/// How often a dwell at the edge is checked while the mouse rests there (the router's driven tick).
const TICK_MS: u32 = 15;
/// Asks the capture thread to put the hooks back.
const REINSTALL: u32 = WM_APP + 7;
/// The capture thread, for `reinstall`; 0 until it runs.
static THREAD: AtomicU32 = AtomicU32::new(0);

thread_local! {
    static TRANSLATOR: RefCell<Translator> = RefCell::new(Translator::default());
    /// The running dwell timer's id, 0 when none runs.
    static TIMER: Cell<usize> = const { Cell::new(0) };
}

fn log(m: &str) {
    crate::app::log(&format!("capture: {m}"));
}

/// Install the hooks on a thread of their own. Returns once they are in, or with why they are not.
pub fn start() -> Result<(), String> {
    let (tx, rx) = channel();
    std::thread::Builder::new()
        .name("flowseam-capture".into())
        .spawn(move || run(tx))
        .map_err(|e| format!("capture thread: {e}"))?;
    rx.recv_timeout(Duration::from_secs(5)).map_err(|_| "the capture thread did not report".to_string())?
}

/// Both hooks, or why one could not go in (the other is then taken out again).
fn install() -> Result<(HHOOK, HHOOK), String> {
    // SAFETY: plain calls. The procedures are 'static functions, and a low-level hook is global (thread 0)
    // and called on the installing thread, which pumps messages for as long as the hooks are in.
    let module = unsafe { GetModuleHandleW(std::ptr::null()) };
    let keyboard = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), module, 0) };
    if keyboard.is_null() {
        return Err(format!("keyboard hook: {}", std::io::Error::last_os_error()));
    }
    let mouse = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), module, 0) };
    if mouse.is_null() {
        let e = std::io::Error::last_os_error();
        // SAFETY: the hook installed just above.
        unsafe { UnhookWindowsHookEx(keyboard) };
        return Err(format!("mouse hook: {e}"));
    }
    Ok((keyboard, mouse))
}

fn uninstall((keyboard, mouse): (HHOOK, HHOOK)) {
    // SAFETY: hooks `install` returned. Windows may have taken them out already; that only fails the call.
    unsafe {
        UnhookWindowsHookEx(mouse);
        UnhookWindowsHookEx(keyboard);
    }
}

/// Puts the hooks back after Windows took them out (the router saw the cursor move while it should be held).
pub fn reinstall() {
    let thread = THREAD.load(Ordering::SeqCst);
    // SAFETY: posts to the capture thread's queue; a thread id of 0 or a gone thread only fails the call.
    if thread == 0 || unsafe { PostThreadMessageW(thread, REINSTALL, 0, 0) } == 0 {
        log("the hooks could not be put back: the capture thread is not running");
    }
}

fn run(ready: Sender<Result<(), String>>) {
    let mut hooks = match install() {
        Ok(h) => h,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    // SAFETY: plain query.
    THREAD.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
    match make_veil() {
        Ok(hwnd) => {
            VEIL.store(hwnd as isize, Ordering::SeqCst);
            crate::touchpad::start(hwnd);
        }
        Err(e) => log(&format!("the cursor stays visible while this PC drives the peer, and the touchpad gestures are not read: {e}")),
    }
    let _ = ready.send(Ok(()));
    // SAFETY: MSG is plain data; GetMessageW fills it.
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    loop {
        // >0 a message, 0 WM_QUIT, -1 an error.
        let r = unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) };
        if r == 0 || r == -1 {
            break;
        }
        if msg.hwnd.is_null() && msg.message == WM_TIMER {
            on_timer();
            continue;
        }
        if msg.hwnd.is_null() && msg.message == REINSTALL {
            uninstall(hooks);
            match install() {
                Ok(h) => {
                    hooks = h;
                    log("the input hooks are back");
                }
                Err(e) => {
                    log(&format!("the input hooks could not be put back ({e}): this PC no longer drives a peer"));
                    THREAD.store(0, Ordering::SeqCst);
                    return;
                }
            }
            continue;
        }
        // SAFETY: a message GetMessageW returned.
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    log("the capture thread stopped: this PC's keyboard and mouse no longer reach a peer");
    THREAD.store(0, Ordering::SeqCst);
    uninstall(hooks);
}

/// The translator, or a fresh one if a hook call arrives while the last one is still running on this thread.
fn translate(f: impl FnOnce(&mut Translator) -> Disposition) -> Disposition {
    TRANSLATOR.with(|tr| match tr.try_borrow_mut() {
        Ok(mut tr) => f(&mut tr),
        Err(_) => f(&mut Translator::default()),
    })
}

/// The dwell at the edge completes on the clock while the mouse rests: run it while the session needs it.
fn after(t: &dyn Target) {
    if TIMER.get() == 0 && t.needs_tick() {
        // SAFETY: a thread timer (no window): WM_TIMER comes to this thread's queue.
        TIMER.set(unsafe { SetTimer(std::ptr::null_mut(), 0, TICK_MS, None) });
    }
}

fn on_timer() {
    if capture::target().is_some_and(|t| t.tick()) {
        return;
    }
    let id = TIMER.replace(0);
    if id != 0 {
        // SAFETY: the thread timer started in `after`.
        unsafe { KillTimer(std::ptr::null_mut(), id) };
    }
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && code as u32 == HC_ACTION {
        // SAFETY: with HC_ACTION, lparam points at this event's KBDLLHOOKSTRUCT for the duration of the call.
        let k = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        if k.dwExtraInfo == PROBE_MARK {
            capture::key_probe_seen();
            return 1;
        }
        // Our own injection passes before anything is locked: the router may inject while it holds the forwarder.
        if k.dwExtraInfo != INJECT_MARK {
            let t = capture::target();
            let d = translate(|tr| tr.key(t.as_deref(), k.vkCode, k.scanCode, k.flags, k.dwExtraInfo));
            if let Some(t) = &t {
                after(&**t);
            }
            if d == Disposition::Swallow {
                return 1;
            }
        }
    }
    // SAFETY: passes the event on unchanged.
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && code as u32 == HC_ACTION {
        // SAFETY: with HC_ACTION, lparam points at this event's MSLLHOOKSTRUCT for the duration of the call.
        let m = unsafe { &*(lparam as *const MSLLHOOKSTRUCT) };
        if m.dwExtraInfo == PROBE_MARK {
            capture::mouse_probe_seen();
            return 1;
        }
        if m.dwExtraInfo != INJECT_MARK {
            let t = capture::target();
            let pt = Point { x: m.pt.x, y: m.pt.y };
            let mut c = POINT { x: 0, y: 0 };
            // Where the cursor is before this event; unreadable, the event is taken as no move.
            // SAFETY: writes one POINT.
            let cursor = if unsafe { GetCursorPos(&mut c) } != 0 { Point { x: c.x, y: c.y } } else { pt };
            let d = translate(|tr| tr.mouse(t.as_deref(), wparam as u32, pt, m.mouseData, m.dwExtraInfo, cursor));
            if let Some(t) = &t {
                after(&**t);
            }
            if d == Disposition::Swallow {
                return 1;
            }
        }
    }
    // SAFETY: passes the event on unchanged.
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

/// Whether the capture thread runs with its hooks in. After a failed reinstall it does not, and a session
/// must not count on driving a peer.
pub fn alive() -> bool {
    THREAD.load(Ordering::SeqCst) != 0
}

/// One probe for each hook, with [`PROBE_MARK`]: a key that is no key (virtual key 0xFF) down and up, and a
/// move of one count right and back (a move of nothing never reaches the hooks, measured). The hooks swallow
/// and count them; should a hook be gone, Windows gets a no-op key and a jiggle of at most a pixel. `Err`
/// when Windows would not take them (an elevated window in front blocks injection), which says nothing about
/// the hooks.
pub fn send_probe() -> Result<(), String> {
    let key = |up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: 0xFF, wScan: 0, dwFlags: if up { KEYEVENTF_KEYUP } else { 0 }, time: 0, dwExtraInfo: PROBE_MARK } },
    };
    let nudge = |dx: i32| INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy: 0, mouseData: 0, dwFlags: MOUSEEVENTF_MOVE, time: 0, dwExtraInfo: PROBE_MARK } },
    };
    let inputs = [key(false), key(true), nudge(1), nudge(-1)];
    // SAFETY: fully initialised INPUTs and their size.
    let n = unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32) };
    if n as usize != inputs.len() {
        return Err(format!("SendInput took {n} of {} probe events: {}", inputs.len(), std::io::Error::last_os_error()));
    }
    Ok(())
}

/// The veil's messages: cover the cursor where it is, or uncover it.
const HIDE: u32 = WM_APP + 8;
const SHOW: u32 = WM_APP + 9;
/// Half the veil's side: it only has to be under the cursor's hot spot, which the hooks keep still.
const VEIL_HALF: i32 = 4;
/// The veil window (an HWND), 0 until the capture thread has made it.
static VEIL: AtomicIsize = AtomicIsize::new(0);

fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A window of its own on the capture thread, which shows no cursor: while this PC drives a peer it sits,
/// nearly transparent, under the cursor, so the cursor does not show. It never takes focus, and it goes
/// with the process, so a crash cannot leave the cursor hidden. Only the window under the cursor sets it,
/// and the hooks keep the cursor still, so a few pixels are enough.
fn make_veil() -> Result<HWND, String> {
    let name = utf16("FlowseamVeil");
    // SAFETY: a class and window on this thread; `veil_proc` is a 'static function and `name` outlives both
    // calls (Windows copies the class name).
    unsafe {
        let module = GetModuleHandleW(std::ptr::null());
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(veil_proc);
        wc.hInstance = module;
        wc.lpszClassName = name.as_ptr();
        if RegisterClassW(&wc) == 0 {
            return Err(format!("RegisterClassW: {}", std::io::Error::last_os_error()));
        }
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            name.as_ptr(),
            name.as_ptr(),
            WS_POPUP,
            0,
            0,
            2 * VEIL_HALF,
            2 * VEIL_HALF,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            module,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            return Err(format!("CreateWindowExW: {}", std::io::Error::last_os_error()));
        }
        // Alpha 1, not 0: a fully transparent layered window is skipped by hit-testing, and then it is the
        // window below that sets the cursor.
        SetLayeredWindowAttributes(hwnd, 0, 1, LWA_ALPHA);
        Ok(hwnd)
    }
}

/// Moving the cursor onto the point it is at makes Windows ask the window under it for its cursor again: the
/// moves themselves are swallowed by the hooks, so nothing else would.
fn refresh_cursor() -> POINT {
    let mut p = POINT { x: 0, y: 0 };
    // SAFETY: writes one POINT; then a move to where the cursor already is.
    unsafe {
        GetCursorPos(&mut p);
        SetCursorPos(p.x, p.y);
    }
    p
}

unsafe extern "system" fn veil_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY (each call): `hwnd` is the veil, this thread's own window.
    match msg {
        WM_SETCURSOR => {
            unsafe { SetCursor(std::ptr::null_mut()) };
            1
        }
        HIDE => {
            let mut p = POINT { x: 0, y: 0 };
            unsafe {
                GetCursorPos(&mut p);
                SetWindowPos(hwnd, HWND_TOPMOST, p.x - VEIL_HALF, p.y - VEIL_HALF, 2 * VEIL_HALF, 2 * VEIL_HALF, SWP_NOACTIVATE | SWP_SHOWWINDOW);
            }
            refresh_cursor();
            0
        }
        SHOW => {
            unsafe { ShowWindow(hwnd, SW_HIDE) };
            refresh_cursor();
            0
        }
        WM_INPUT_DEVICE_CHANGE => {
            crate::touchpad::on_device_change(wparam, lparam);
            0
        }
        WM_INPUT => {
            crate::touchpad::on_input(lparam as HRAWINPUT);
            // Raw Input wants its cleanup: the default procedure does it.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// The cursor while this PC drives a peer: the veil covers it. Posted, never sent: the caller may be the
/// router holding the forwarder, which the capture thread may be waiting for.
struct Veil;

fn to_veil(msg: u32) -> Result<(), String> {
    let hwnd = VEIL.load(Ordering::SeqCst);
    if hwnd == 0 {
        return Err("no veil window: the cursor stays visible while this PC drives the peer".into());
    }
    // SAFETY: posts to the veil; a window that is gone only fails the call.
    if unsafe { PostMessageW(hwnd as HWND, msg, 0, 0) } == 0 {
        return Err(format!("PostMessageW: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

impl Grab for Veil {
    /// `SetCursorPos` reaches no low-level hook (measured in the 11-06 spike), so the hooks never see this
    /// as a move to send to the peer.
    fn park(&mut self, at: Point) -> Result<(), String> {
        // SAFETY: plain call.
        if unsafe { SetCursorPos(at.x, at.y) } == 0 {
            return Err(format!("SetCursorPos: {}", std::io::Error::last_os_error()));
        }
        Ok(())
    }
    fn hide(&mut self) -> Result<(), String> {
        to_veil(HIDE)
    }
    fn show(&mut self) -> Result<(), String> {
        to_veil(SHOW)
    }
}

/// A session's grab on a machine whose input is captured.
pub fn grab() -> Box<dyn Grab> {
    Box::new(Veil)
}
