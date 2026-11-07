#[cfg(windows)]
mod windows_probe {
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use flowseam_core::proto::{FileMeta, Point};
use flowseam_net::files::{Incoming, NativeDrop};
use flowseam_win::inject::Injector;
use flowseam_win::native_drop::WinNativeDrop;
use flowseam_win::win_input::SendInputInjector;
use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM, RECT};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
use windows_sys::Win32::UI::WindowsAndMessaging::{EnumWindows, GetClassNameW, GetWindowRect, GetWindowTextW, IsHungAppWindow, IsWindowVisible, PostMessageW, SetForegroundWindow, WM_CLOSE};

unsafe extern "system" fn find_window(hwnd: HWND, arg: LPARAM) -> BOOL {
    let found = unsafe { &mut *(arg as *mut (String, HWND)) };
    let mut class = [0u16; 100];
    let mut title = [0u16; 300];
    let cn = unsafe { GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32) };
    let tn = unsafe { GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32) };
    if unsafe { IsWindowVisible(hwnd) } != 0 && String::from_utf16_lossy(&class[..cn as usize]) == "CabinetWClass" && String::from_utf16_lossy(&title[..tn as usize]).contains(&found.0) {
        found.1 = hwnd;
        return 0;
    }
    1
}

unsafe extern "system" fn print_explorer(hwnd: HWND, _: LPARAM) -> BOOL {
    let mut class = [0u16; 100];
    let mut title = [0u16; 300];
    let cn = unsafe { GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32) };
    if String::from_utf16_lossy(&class[..cn as usize]) == "CabinetWClass" {
        let tn = unsafe { GetWindowTextW(hwnd, title.as_mut_ptr(), title.len() as i32) };
        println!("Explorer window: {:?}", String::from_utf16_lossy(&title[..tn as usize]));
    }
    1
}

fn explorer(title: &str) -> Option<HWND> {
    let mut found: (String, HWND) = (title.to_owned(), std::ptr::null_mut());
    unsafe { EnumWindows(Some(find_window), (&mut found as *mut (String, HWND)) as LPARAM) };
    (!found.1.is_null()).then_some(found.1)
}

fn wait_explorer(title: &str) -> Result<HWND, String> {
    let start = Instant::now();
    loop {
        if let Some(hwnd) = explorer(title) { return Ok(hwnd); }
        if start.elapsed() > Duration::from_secs(8) { unsafe { EnumWindows(Some(print_explorer), 0) }; return Err(format!("Explorer window {title} not found")); }
        thread::sleep(Duration::from_millis(100));
    }
}

fn payload() -> Vec<u8> {
    let mut bytes = vec![b'x'; 200_000];
    bytes.extend_from_slice(b"\r\nend of the probe file\r\n");
    bytes
}

fn case(mode: &str, root: &Path, input: &mut SendInputInjector) -> Result<(), String> {
    let dir = root.join(format!("recv-{mode}-{}", std::process::id()));
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let title = dir.file_name().unwrap().to_string_lossy().to_string();
    Command::new("explorer.exe").arg(dir.to_string_lossy().replace('/', "\\")).spawn().map_err(|e| e.to_string())?;
    let hwnd = wait_explorer(&title)?;
    let result = run_drag(mode, &dir, hwnd, input);
    unsafe { PostMessageW(hwnd, WM_CLOSE, 0, 0) };
    result
}

fn run_drag(mode: &str, dir: &Path, hwnd: HWND, input: &mut SendInputInjector) -> Result<(), String> {
    unsafe { SetForegroundWindow(hwnd) };
    let mut rect: RECT = unsafe { std::mem::zeroed() };
    if unsafe { GetWindowRect(hwnd, &mut rect) } == 0 { return Err("GetWindowRect failed".into()); }
    let target = Point { x: (rect.left + rect.right) / 2, y: rect.top + (rect.bottom - rect.top) * 3 / 5 };
    let start = Point { x: rect.left - 100, y: target.y };
    input.move_to(start)?;
    let bytes = payload();
    let filename = format!("{mode}.txt");
    let mut inc = Incoming::begin(dir, dir, 100, &[FileMeta { name: filename.clone(), size: bytes.len() as u64 }])?;
    if mode == "fast" { inc.on_chunk(0, &bytes)?; }
    else { inc.on_chunk(0, &bytes[..bytes.len() / 2])?; }
    let staged = inc.shared();
    let (tx, rx) = channel();
    let session = WinNativeDrop::new().begin(100, staged, Box::new(move |took| { let _ = tx.send(took); }))?;
    let feeder = if mode == "slow" {
        let rest = bytes[bytes.len() / 2..].to_vec();
        Some(thread::spawn(move || { thread::sleep(Duration::from_secs(6)); inc.on_chunk(0, &rest).map(|_| ()).map_err(|e| e.to_string()) }))
    } else { None };
    thread::sleep(Duration::from_millis(700));
    if mode == "cancel" { session.cancel(); }
    else {
        for step in 1..=25 {
            input.move_to(Point { x: start.x + (target.x - start.x) * step / 25, y: start.y + (target.y - start.y) * step / 25 })?;
            thread::sleep(Duration::from_millis(20));
        }
        thread::sleep(Duration::from_millis(400));
        input.button(flowseam_win::inject::WinButton::Left, false)?;
    }
    let hung = if mode == "slow" { (unsafe { IsHungAppWindow(hwnd) }) != 0 } else { false };
    let took = match rx.recv_timeout(Duration::from_secs(20)) {
        Ok(value) => value,
        Err(RecvTimeoutError::Timeout) => return Err("ended callback timeout".into()),
        Err(e) => return Err(format!("ended callback: {e}")),
    };
    if let Some(feeder) = feeder { feeder.join().map_err(|_| "feeder panicked".to_string())??; }
    let landed = dir.join(filename);
    if mode == "cancel" {
        if took || landed.exists() || unsafe { GetAsyncKeyState(VK_LBUTTON as i32) } < 0 { return Err(format!("cancel: took={took}, landed={}, left_down={}", landed.exists(), unsafe { GetAsyncKeyState(VK_LBUTTON as i32) } < 0)); }
    } else {
        let actual = fs::read(&landed).map_err(|e| format!("read {}: {e}", landed.display()))?;
        if !took || actual != bytes || hung { return Err(format!("drop: took={took}, bytes={}/{}, hung={hung}", actual.len(), bytes.len())); }
    }
    Ok(())
}

pub fn run() {
    thread::spawn(|| { thread::sleep(Duration::from_secs(59)); eprintln!("dead-man timeout"); std::process::exit(2); });
    let _ = flowseam_win::win_input::dpi_aware();
    let mut input = SendInputInjector;
    let original = input.cursor().ok();
    let root = std::env::temp_dir().join("flowseam-native-drop-probe");
    let mut failures = 0;
    for mode in ["fast", "slow", "cancel"] {
        match case(mode, &root, &mut input) {
            Ok(()) => println!("RESULT PASS {mode}"),
            Err(e) => { println!("RESULT FAIL {mode}: {e}"); failures += 1; }
        }
        if let Some(p) = original { let _ = input.move_to(p); }
    }
    std::process::exit(if failures == 0 { 0 } else { 1 });
}

}

#[cfg(windows)]
fn main() { windows_probe::run(); }

#[cfg(not(windows))]
fn main() { eprintln!("Windows desktop only"); }
