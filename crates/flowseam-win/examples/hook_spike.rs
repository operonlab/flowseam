//! Spike: what a low-level hook on this machine reports for injected input, and what swallowing does.
//!
//! It swallows only events carrying its own mark and passes everything else on untouched, so the
//! person at the machine and a Mac driving it are not affected. It moves the cursor for its
//! measurements, puts it back, and exits within seconds; a timer thread ends the process (which
//! removes the hooks) if anything hangs. Run it in the logged-in desktop session.

#[cfg(windows)]
fn main() {
    imp::main()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("hook_spike runs on Windows only");
}

#[cfg(windows)]
mod imp {
    use std::mem::size_of;
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{LPARAM, LRESULT, POINT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP, MOUSEEVENTF_MOVE, MOUSEINPUT,
        VK_F24,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, DispatchMessageW, GetCursorPos, PeekMessageW, SetCursorPos, SetWindowsHookExW,
        SystemParametersInfoW, TranslateMessage, UnhookWindowsHookEx, HC_ACTION, KBDLLHOOKSTRUCT, MSG,
        MSLLHOOKSTRUCT, PM_REMOVE, SPI_GETMOUSE, SPI_GETMOUSESPEED, WH_KEYBOARD_LL, WH_MOUSE_LL,
    };

    const SPIKE_MARK: usize = 0x5350_494B; // "SPIK"

    static T0: OnceLock<Instant> = OnceLock::new();
    static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());
    static OTHER_MOUSE: AtomicU32 = AtomicU32::new(0);
    static OTHER_KEYS: AtomicU32 = AtomicU32::new(0);
    static WARP_IN_HOOK: AtomicBool = AtomicBool::new(false);
    static WARP_X: AtomicI32 = AtomicI32::new(0);
    static WARP_Y: AtomicI32 = AtomicI32::new(0);

    fn us() -> u128 {
        T0.get_or_init(Instant::now).elapsed().as_micros()
    }

    fn log(s: String) {
        LOG.lock().unwrap().push(format!("{:>9}us {s}", us()));
    }

    fn cursor() -> (i32, i32) {
        let mut p = POINT { x: 0, y: 0 };
        // SAFETY: writes one POINT.
        unsafe { GetCursorPos(&mut p) };
        (p.x, p.y)
    }

    unsafe extern "system" fn mouse_hook(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32 {
            // SAFETY: for WH_MOUSE_LL with HC_ACTION, lParam points at an MSLLHOOKSTRUCT.
            let m = unsafe { &*(l as *const MSLLHOOKSTRUCT) };
            if m.dwExtraInfo == SPIKE_MARK {
                let (cx, cy) = cursor();
                log(format!("spike mouse msg={w:#x} pt=({},{}) flags={:#x} cursor_in_hook=({cx},{cy})", m.pt.x, m.pt.y, m.flags));
                if WARP_IN_HOOK.swap(false, Ordering::SeqCst) {
                    let (x, y) = (WARP_X.load(Ordering::SeqCst), WARP_Y.load(Ordering::SeqCst));
                    // SAFETY: plain call.
                    let ok = unsafe { SetCursorPos(x, y) };
                    log(format!("SetCursorPos({x},{y}) inside the hook returned {ok}"));
                }
                return 1;
            }
            let n = OTHER_MOUSE.fetch_add(1, Ordering::SeqCst);
            if n < 12 {
                log(format!("other mouse msg={w:#x} pt=({},{}) flags={:#x} extra={:#x}", m.pt.x, m.pt.y, m.flags, m.dwExtraInfo));
            }
        }
        // SAFETY: passes the event on unchanged.
        unsafe { CallNextHookEx(std::ptr::null_mut(), code, w, l) }
    }

    unsafe extern "system" fn key_hook(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32 {
            // SAFETY: for WH_KEYBOARD_LL with HC_ACTION, lParam points at a KBDLLHOOKSTRUCT.
            let k = unsafe { &*(l as *const KBDLLHOOKSTRUCT) };
            if k.dwExtraInfo == SPIKE_MARK {
                log(format!("spike key msg={w:#x} vk={:#x} scan={:#x} flags={:#x}", k.vkCode, k.scanCode, k.flags));
                return 1;
            }
            OTHER_KEYS.fetch_add(1, Ordering::SeqCst);
        }
        // SAFETY: passes the event on unchanged.
        unsafe { CallNextHookEx(std::ptr::null_mut(), code, w, l) }
    }

    /// Run the message pump for `ms`: low-level hooks are called only while this thread pumps.
    fn pump(ms: u64) {
        let end = Instant::now() + Duration::from_millis(ms);
        // SAFETY: MSG is plain data; PeekMessageW fills it.
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        while Instant::now() < end {
            // SAFETY: standard pump on this thread's queue.
            while unsafe { PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
                unsafe {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn send(input: INPUT, what: &str) {
        log(format!("SendInput {what}"));
        // SAFETY: one fully initialised INPUT.
        let n = unsafe { SendInput(1, &input, size_of::<INPUT>() as i32) };
        if n != 1 {
            log(format!("SendInput {what} inserted {n}: {}", std::io::Error::last_os_error()));
        }
    }

    fn rel(dx: i32, dy: i32) -> INPUT {
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: 0, dwFlags: MOUSEEVENTF_MOVE, time: 0, dwExtraInfo: SPIKE_MARK } },
        }
    }

    fn key(vk: u16, down: bool) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT { wVk: vk, wScan: 0, dwFlags: if down { 0 } else { KEYEVENTF_KEYUP }, time: 0, dwExtraInfo: SPIKE_MARK },
            },
        }
    }

    fn warp(x: i32, y: i32, what: &str) {
        let before = OTHER_MOUSE.load(Ordering::SeqCst);
        // SAFETY: plain call.
        let ok = unsafe { SetCursorPos(x, y) };
        pump(60);
        log(format!(
            "SetCursorPos({x},{y}) for {what} returned {ok}; cursor now {:?}; other mouse events during it: {}",
            cursor(),
            OTHER_MOUSE.load(Ordering::SeqCst) - before
        ));
    }

    pub fn main() {
        T0.get_or_init(Instant::now);
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_secs(15));
            eprintln!("dead-man timer: exiting");
            std::process::exit(2);
        });
        if let Err(e) = flowseam_win::win_input::dpi_aware() {
            log(format!("dpi: {e}"));
        }
        let displays = flowseam_win::win_input::displays().expect("displays");
        log(format!("displays {displays:?}"));
        let mut speed: u32 = 0;
        let mut accel = [0i32; 3];
        // SAFETY: each writes the documented out-parameter.
        unsafe {
            SystemParametersInfoW(SPI_GETMOUSESPEED, 0, &mut speed as *mut u32 as *mut _, 0);
            SystemParametersInfoW(SPI_GETMOUSE, 0, accel.as_mut_ptr() as *mut _, 0);
        }
        log(format!("mouse speed {speed}, thresholds/accel {accel:?}"));

        // SAFETY: LL hooks with this module's handle; both procedures live in this binary.
        let module = unsafe { GetModuleHandleW(std::ptr::null()) };
        let mh = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), module, 0) };
        let kh = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(key_hook), module, 0) };
        log(format!("hooks mouse={:?} key={:?} ({})", mh, kh, std::io::Error::last_os_error()));
        if mh.is_null() || kh.is_null() {
            print();
            return;
        }

        let home = cursor();
        log(format!("cursor at start {home:?}"));
        let primary = displays.iter().find(|r| r.x == 0 && r.y == 0).copied().unwrap_or(displays[0]);
        let center = (primary.x + primary.w / 2, primary.y + primary.h / 2);
        let right = displays.iter().map(|r| r.x + r.w).max().unwrap() - 1;

        warp(center.0, center.1, "A: center");
        send(rel(10, 5), "A: +10,+5 marked");
        pump(100);
        log(format!("A: cursor after a swallowed move {:?}", cursor()));
        send(rel(100, 0), "A2: +100,0 marked");
        pump(100);

        warp(right, center.1, "B: right edge");
        send(rel(10, 0), "B: +10 at the right edge");
        pump(100);
        log(format!("B: cursor {:?}", cursor()));

        warp(center.0, center.1, "C: center");
        WARP_X.store(center.0 - 200, Ordering::SeqCst);
        WARP_Y.store(center.1, Ordering::SeqCst);
        WARP_IN_HOOK.store(true, Ordering::SeqCst);
        let before = OTHER_MOUSE.load(Ordering::SeqCst);
        send(rel(3, 0), "C: +3 marked, the hook warps to center-200");
        pump(100);
        log(format!("C: cursor {:?}; other mouse events meanwhile: {}", cursor(), OTHER_MOUSE.load(Ordering::SeqCst) - before));

        let keys_before = OTHER_KEYS.load(Ordering::SeqCst);
        send(key(VK_F24, true), "D: F24 down marked");
        send(key(VK_F24, false), "D: F24 up marked");
        pump(100);
        log(format!("D: other key events meanwhile: {}", OTHER_KEYS.load(Ordering::SeqCst) - keys_before));

        warp(home.0, home.1, "restore");
        // SAFETY: the handles came from SetWindowsHookExW above.
        unsafe {
            UnhookWindowsHookEx(mh);
            UnhookWindowsHookEx(kh);
        }
        log(format!(
            "unhooked; other mouse events in total {}, other key events {}",
            OTHER_MOUSE.load(Ordering::SeqCst),
            OTHER_KEYS.load(Ordering::SeqCst)
        ));
        print();
    }

    fn print() {
        for line in LOG.lock().unwrap().iter() {
            println!("{line}");
        }
    }
}
