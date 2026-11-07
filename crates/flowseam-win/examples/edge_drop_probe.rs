#[cfg(windows)]
mod win {

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use flowseam_core::geometry::exit_frac;
use flowseam_core::proto::{Edge, Point};
use flowseam_net::files::DragSource;
use flowseam_win::edge_drop::EdgeDropSource;
use flowseam_win::inject::{Injector, WinButton};
use flowseam_win::keymap::Scan;
use flowseam_win::win_input::{self, SendInputInjector};
use windows::core::{implement, w, BOOL, HRESULT, PCWSTR};
use windows::Win32::Foundation::{DRAGDROP_S_CANCEL, DRAGDROP_S_USEDEFAULTCURSORS};
use windows::Win32::System::Com::{CoTaskMemFree, IDataObject, FORMATETC, DVASPECT_CONTENT, TYMED_HGLOBAL};
use windows::Win32::System::Ole::{DoDragDrop, OleInitialize, OleUninitialize, IDropSource, IDropSource_Impl, CF_HDROP, DROPEFFECT, DROPEFFECT_COPY};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
use windows::Win32::UI::Shell::{ILFindLastID, SHCreateDataObject, SHParseDisplayName};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, ShowWindow, SW_SHOWNOACTIVATE, WS_POPUP};

fn pause(ms: u64) { thread::sleep(Duration::from_millis(ms)); }
fn esc() {
    let mut inj = SendInputInjector;
    let scan = Scan { code: 0x01, extended: false };
    let _ = inj.key(scan, true);
    pause(80);
    let _ = inj.key(scan, false);
}
fn move_to(p: Point) -> Result<(), String> { SendInputInjector.move_to(p) }
fn button(down: bool) -> Result<(), String> { SendInputInjector.button(WinButton::Left, down) }
fn cursor() -> Result<Point, String> { SendInputInjector.cursor() }
fn released() -> bool { (unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) }) >= 0 }

#[implement(IDropSource)]
struct ProbeSource { escape_seen: Arc<AtomicBool>, cancel: Arc<AtomicBool> }

impl IDropSource_Impl for ProbeSource_Impl {
    fn QueryContinueDrag(&self, escape: BOOL, _: MODIFIERKEYS_FLAGS) -> HRESULT {
        if escape.as_bool() {
            self.escape_seen.store(true, Ordering::Release);
            DRAGDROP_S_CANCEL
        }
        else if self.cancel.load(Ordering::Acquire) { DRAGDROP_S_CANCEL }
        else { HRESULT(0) }
    }
    fn GiveFeedback(&self, _: DROPEFFECT) -> HRESULT { DRAGDROP_S_USEDEFAULTCURSORS }
}

struct Restore(Point);
impl Drop for Restore {
    fn drop(&mut self) {
        esc();
        let _ = button(false);
        let _ = move_to(self.0);
    }
}

fn temp_case(name: &str) -> Result<(PathBuf, PathBuf), String> {
    let folder = std::env::temp_dir().join(format!("flowseam-edge-{name}-{}-{}", std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|e| e.to_string())?.as_millis()));
    fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
    let file = folder.join("drag.txt");
    fs::write(&file, b"flowseam edge probe").map_err(|e| e.to_string())?;
    Ok((folder, file))
}

fn edge_point() -> Result<Point, String> {
    let displays = win_input::displays()?;
    for r in &displays {
        for y in r.y.saturating_add(20)..r.y.saturating_add(r.h).saturating_sub(20) {
            if exit_frac(&displays, Point { x: r.x, y }, Edge::Left).is_some() {
                return Ok(Point { x: r.x + 1, y });
            }
        }
    }
    Err("no left outer edge".into())
}

fn observed(source: &EdgeDropSource, base: u64, file: &Path) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && source.generation()? == base { pause(20); }
    let generation = source.generation()?;
    let files = source.files()?;
    if generation == base || files.len() != 1 || files[0] != file {
        return Err(format!("generation {base}->{generation}; files={files:?}; expected={file:?}"));
    }
    Ok(())
}

fn own_case(armed: bool) -> Result<(), String> {
    let (folder, file) = temp_case(if armed { "own" } else { "disarmed" })?;
    let result = (|| {
        let edge = edge_point()?;
        let source = EdgeDropSource::new(Edge::Left, win_input::displays()?);
        source.arm(armed);
        pause(300);
        let base = source.generation()?;
        let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
        let mut parent: *mut ITEMIDLIST = std::ptr::null_mut();
        let wide: Vec<u16> = file.as_os_str().to_string_lossy().encode_utf16().chain(Some(0)).collect();
        unsafe { SHParseDisplayName(PCWSTR(wide.as_ptr()), None, &mut pidl, 0, None) }.map_err(|e| format!("SHParseDisplayName HRESULT={:#x}", e.code().0))?;
        let parent_wide: Vec<u16> = folder.as_os_str().to_string_lossy().encode_utf16().chain(Some(0)).collect();
        unsafe { SHParseDisplayName(PCWSTR(parent_wide.as_ptr()), None, &mut parent, 0, None) }
            .map_err(|e| format!("parent SHParseDisplayName HRESULT={:#x}", e.code().0))?;
        let child = unsafe { ILFindLastID(pidl) };
        let data: IDataObject = unsafe { SHCreateDataObject(Some(parent), Some(&[child as *const ITEMIDLIST]), None::<&IDataObject>) }
            .map_err(|e| format!("SHCreateDataObject HRESULT={:#x}", e.code().0))?;
        let format = FORMATETC { cfFormat: CF_HDROP.0, dwAspect: DVASPECT_CONTENT.0, lindex: -1, tymed: TYMED_HGLOBAL.0 as u32, ..Default::default() };
        let query = unsafe { data.QueryGetData(&format) };
        println!("OWN FORMAT query_hresult={:#x}", query.0);
        query.ok().map_err(|e| format!("own source lacks CF_HDROP HRESULT={:#x}", e.code().0))?;
        unsafe { CoTaskMemFree(Some(pidl.cast())); }
        unsafe { CoTaskMemFree(Some(parent.cast())); }
        let origin = Point { x: edge.x + 220, y: edge.y };
        let hwnd = unsafe { CreateWindowExW(Default::default(), w!("STATIC"), w!("Flowseam edge probe"), WS_POPUP,
            origin.x - 30, origin.y - 30, 60, 60, None, None, None, None) }
            .map_err(|e| format!("probe window HRESULT={:#x}", e.code().0))?;
        let _ = unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) };
        let done = Arc::new(AtomicBool::new(false));
        let mover_done = done.clone();
        let escape_seen = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(AtomicBool::new(false));
        let mover_cancel = cancel.clone();
        let mover = thread::spawn(move || {
            pause(250);
            let _ = move_to(edge);
            pause(500);
            esc();
            pause(200);
            mover_cancel.store(true, Ordering::Release);
            let _ = button(false);
            mover_done.store(true, Ordering::Release);
        });
        move_to(origin)?;
        button(true)?;
        pause(100);
        let drop_source: IDropSource = ProbeSource { escape_seen: escape_seen.clone(), cancel }.into();
        let mut effect = DROPEFFECT_COPY;
        let drag = unsafe { DoDragDrop(&data, &drop_source, DROPEFFECT_COPY, &mut effect) };
        println!("OWN DETAIL armed={armed} drag_hresult={:#x} effect={} button_down={}", drag.0, effect.0, !released());
        let _ = mover.join();
        let _ = unsafe { DestroyWindow(hwnd) };
        drag.ok().map_err(|e| format!("DoDragDrop HRESULT={:#x}", e.code().0))?;
        if !done.load(Ordering::Acquire) { return Err("mover did not finish".into()); }
        if !escape_seen.load(Ordering::Acquire) { return Err("OLE did not observe injected Escape".into()); }
        if armed { observed(&source, base, &file)?; }
        else if source.generation()? != base { return Err("disarmed edge observed DragEnter".into()); }
        if !file.exists() { return Err("source file disappeared".into()); }
        if !released() { return Err("left button remained down".into()); }
        Ok(())
    })();
    esc();
    let _ = button(false);
    let _ = fs::remove_dir_all(folder);
    result
}

fn powershell(script: &str) -> Result<String, String> {
    let output = std::process::Command::new("powershell.exe").args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output().map_err(|e| e.to_string())?;
    if !output.status.success() { return Err(format!("PowerShell exit={}; {}", output.status, String::from_utf8_lossy(&output.stderr))); }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn explorer_case() -> Result<(), String> {
    let (folder, file) = temp_case("explorer")?;
    let result = (|| {
        let edge = edge_point()?;
        let source = EdgeDropSource::new(Edge::Left, win_input::displays()?);
        source.arm(true);
        pause(300);
        let base = source.generation()?;
        std::process::Command::new("explorer.exe").arg(&folder).spawn().map_err(|e| e.to_string())?;
        pause(1200);
        let script = r#"Add-Type -AssemblyName UIAutomationClient; $r=[System.Windows.Automation.AutomationElement]::RootElement; $c=[System.Windows.Automation.PropertyCondition]::new([System.Windows.Automation.AutomationElement]::NameProperty,'drag'); $a=$r.FindFirst([System.Windows.Automation.TreeScope]::Descendants,$c); if(!$a){$c=[System.Windows.Automation.PropertyCondition]::new([System.Windows.Automation.AutomationElement]::NameProperty,'drag.txt');$a=$r.FindFirst([System.Windows.Automation.TreeScope]::Descendants,$c)}; if(!$a){exit 2};$b=$a.Current.BoundingRectangle; Write-Output ([int]($b.X+$b.Width/2));Write-Output ([int]($b.Y+$b.Height/2))"#;
        let output = powershell(script)?;
        let coords: Vec<i32> = output.lines().filter_map(|x| x.trim().parse().ok()).collect();
        if coords.len() != 2 { return Err(format!("UI Automation item bounds unavailable: {output}")); }
        let start = Point { x: coords[0], y: coords[1] };
        move_to(start)?;
        button(true)?;
        pause(100);
        move_to(Point { x: start.x - 70, y: start.y })?;
        pause(200);
        move_to(edge)?;
        let found = observed(&source, base, &file);
        esc();
        button(false)?;
        found?;
        if !file.exists() { return Err("source file disappeared".into()); }
        if !released() { return Err("left button remained down".into()); }
        Ok(())
    })();
    esc();
    let _ = button(false);
    let title = folder.file_name().unwrap().to_string_lossy().replace('"', "");
    let close = format!("$s=New-Object -ComObject Shell.Application; $s.Windows() | Where-Object {{$_.LocationName -eq '{title}'}} | ForEach-Object {{$_.Quit()}}");
    let _ = powershell(&close);
    let _ = fs::remove_dir_all(folder);
    result
}

pub fn run() {
    let saved = match cursor() { Ok(p) => p, Err(e) => { eprintln!("RESULT FAIL cursor: {e}"); std::process::exit(1) } };
    let restore = Restore(saved);
    thread::spawn(move || { pause(58_000); eprintln!("dead-man timeout"); drop(Restore(saved)); std::process::exit(2); });
    if let Err(e) = win_input::dpi_aware() { eprintln!("DPI: {e}"); }
    if let Err(e) = unsafe { OleInitialize(None) } { eprintln!("OLE HRESULT={:#x}", e.code().0); drop(restore); std::process::exit(1); }
    let mut failed = false;
    type Probe = (&'static str, fn() -> Result<(), String>);
    let cases: [Probe; 5] = [
        ("own_drag", || own_case(true)),
        ("disarmed", || own_case(false)),
        ("explorer", explorer_case),
        ("explorer", explorer_case),
        ("explorer", explorer_case),
    ];
    for (name, run) in cases {
        match run() {
            Ok(()) => println!("RESULT PASS {name}"),
            Err(e) => { println!("RESULT FAIL {name}: {e}"); failed = true; }
        }
    }
    unsafe { OleUninitialize(); }
    drop(restore);
    if failed { std::process::exit(1); }
}

}

fn main() {
    #[cfg(windows)]
    win::run();
}
