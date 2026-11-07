//! OLE drop targets at the outer edge facing the peer. All HWND and COM work stays on one STA.
#![cfg(windows)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use flowseam_core::geometry::exit_frac;
use flowseam_core::proto::{Edge, Point, Rect};
use flowseam_net::files::DragSource;
use windows::core::{implement, w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, POINTL, WPARAM};
use windows::Win32::System::Com::{IDataObject, FORMATETC, DVASPECT_CONTENT, TYMED_HGLOBAL};
use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::{
    IDropTarget, IDropTarget_Impl, OleInitialize, OleUninitialize, RegisterDragDrop, ReleaseStgMedium,
    RevokeDragDrop, CF_HDROP, DROPEFFECT, DROPEFFECT_NONE,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, PeekMessageW,
    PostThreadMessageW, RegisterClassW, SetLayeredWindowAttributes, ShowWindow, TranslateMessage,
    WNDCLASSW, LWA_ALPHA, MSG, PM_NOREMOVE, SW_HIDE, SW_SHOWNOACTIVATE, WM_APP, WM_QUIT,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

const REFRESH: u32 = WM_APP + 1;
const WIDTH: i32 = 4;

#[link(name = "kernel32")]
unsafe extern "system" { fn GetCurrentThreadId() -> u32; }

fn log(message: &str) {
    flowseam_core::log::line(&format!("[flowseam-win] edge drop: {message}"));
}

struct State {
    generation: AtomicU64,
    files: Mutex<Vec<PathBuf>>,
    armed: AtomicBool,
    settings: Mutex<(Edge, Vec<Rect>)>,
    thread_id: AtomicU32,
}

impl State {
    fn wake(&self) {
        let id = self.thread_id.load(Ordering::Acquire);
        if id != 0 {
            if let Err(e) = unsafe { PostThreadMessageW(id, REFRESH, WPARAM(0), LPARAM(0)) } {
                log(&format!("post refresh failed HRESULT={:#x}", e.code().0));
            }
        }
    }
}

// ponytail: a session that ends before the STA thread has stored its id posts no WM_QUIT, so that thread waits in
// GetMessageW for good (sessions last seconds, the window is microseconds); hand the thread a quit flag it checks
// before blocking if one is ever seen leaking.
impl Drop for State {
    fn drop(&mut self) {
        let id = self.thread_id.load(Ordering::Acquire);
        if id != 0 {
            let _ = unsafe { PostThreadMessageW(id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        }
    }
}

#[derive(Clone)]
pub struct EdgeDropSource(Arc<State>);

impl EdgeDropSource {
    pub fn new(edge: Edge, displays: Vec<Rect>) -> Self {
        let state = Arc::new(State {
            generation: AtomicU64::new(0), files: Mutex::new(Vec::new()),
            armed: AtomicBool::new(false), settings: Mutex::new((edge, displays)), thread_id: AtomicU32::new(0),
        });
        let weak = Arc::downgrade(&state);
        if let Err(e) = std::thread::Builder::new().name("flowseam-edge-drop".into()).spawn(move || sta(weak)) {
            log(&format!("spawn STA failed: {e}"));
        }
        Self(state)
    }

    pub fn arm(&self, down: bool) {
        self.0.armed.store(down, Ordering::Release);
        self.0.wake();
    }

    pub fn set_edge(&self, edge: Edge, displays: Vec<Rect>) {
        let mut settings = self.0.settings.lock().unwrap_or_else(|e| e.into_inner());
        settings.0 = edge;
        settings.1 = displays;
        drop(settings);
        self.0.wake();
    }
}

impl DragSource for EdgeDropSource {
    fn generation(&self) -> Result<u64, String> { Ok(self.0.generation.load(Ordering::Acquire)) }
    fn files(&self) -> Result<Vec<PathBuf>, String> {
        Ok(self.0.files.lock().unwrap_or_else(|e| e.into_inner()).clone())
    }
}

#[implement(IDropTarget)]
struct Target { state: Weak<State> }

impl IDropTarget_Impl for Target_Impl {
    fn DragEnter(&self, data: windows::core::Ref<IDataObject>, _: MODIFIERKEYS_FLAGS, _: &POINTL, effect: *mut DROPEFFECT) -> windows::core::Result<()> {
        unsafe { *effect = DROPEFFECT_NONE; }
        let files = match data.as_ref() {
            Some(data) => match read_files(data) {
                Ok(files) => files,
                Err(e) => { log(&format!("DragEnter GetData failed HRESULT={:#x}", e.code().0)); Vec::new() }
            },
            None => Vec::new(),
        };
        log(&format!("DragEnter files={}", files.len()));
        if !files.is_empty() {
            if let Some(state) = self.state.upgrade() {
                *state.files.lock().unwrap_or_else(|e| e.into_inner()) = files;
                state.generation.fetch_add(1, Ordering::Release);
            }
        }
        Ok(())
    }
    fn DragOver(&self, _: MODIFIERKEYS_FLAGS, _: &POINTL, effect: *mut DROPEFFECT) -> windows::core::Result<()> {
        unsafe { *effect = DROPEFFECT_NONE; }
        Ok(())
    }
    fn DragLeave(&self) -> windows::core::Result<()> { Ok(()) }
    fn Drop(&self, _: windows::core::Ref<IDataObject>, _: MODIFIERKEYS_FLAGS, _: &POINTL, effect: *mut DROPEFFECT) -> windows::core::Result<()> {
        unsafe { *effect = DROPEFFECT_NONE; }
        Ok(())
    }
}

fn read_files(data: &IDataObject) -> windows::core::Result<Vec<PathBuf>> {
    let format = FORMATETC { cfFormat: CF_HDROP.0, dwAspect: DVASPECT_CONTENT.0, lindex: -1, tymed: TYMED_HGLOBAL.0 as u32, ..Default::default() };
    let mut medium = unsafe { data.GetData(&format)? };
    let result = unsafe {
        let hglobal = medium.u.hGlobal;
        let locked = GlobalLock(hglobal);
        if locked.is_null() {
            Err(windows::core::Error::from_thread())
        } else {
            let drop = HDROP(hglobal.0);
            let count = DragQueryFileW(drop, u32::MAX, None);
            let mut files = Vec::with_capacity(count as usize);
            for i in 0..count {
                let len = DragQueryFileW(drop, i, None);
                let mut buf = vec![0u16; len as usize + 1];
                let got = DragQueryFileW(drop, i, Some(&mut buf));
                files.push(PathBuf::from(String::from_utf16_lossy(&buf[..got as usize])));
            }
            let _ = GlobalUnlock(hglobal);
            Ok(files)
        }
    };
    unsafe { ReleaseStgMedium(&mut medium); }
    result
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> windows::Win32::Foundation::LRESULT {
    DefWindowProcW(hwnd, msg, w, l)
}

fn strips(displays: &[Rect], edge: Edge) -> Vec<(i32, i32, i32, i32)> {
    let mut out = Vec::new();
    for r in displays {
        let (start, end) = match edge {
            Edge::Left | Edge::Right => (r.y, r.y.saturating_add(r.h)),
            Edge::Top | Edge::Bottom => (r.x, r.x.saturating_add(r.w)),
        };
        let boundary = match edge {
            Edge::Left => r.x,
            Edge::Right => r.x.saturating_add(r.w).saturating_sub(1),
            Edge::Top => r.y,
            Edge::Bottom => r.y.saturating_add(r.h).saturating_sub(1),
        };
        let mut run = None;
        for along in start..=end {
            let valid = along < end && {
                let p = match edge {
                    Edge::Left | Edge::Right => Point { x: boundary, y: along },
                    Edge::Top | Edge::Bottom => Point { x: along, y: boundary },
                };
                exit_frac(displays, p, edge).is_some()
            };
            if valid && run.is_none() { run = Some(along); }
            if !valid {
                if let Some(s) = run.take() {
                    let span = along - s;
                    out.push(match edge {
                        Edge::Left => (boundary, s, WIDTH.min(r.w), span),
                        Edge::Right => (boundary - WIDTH.min(r.w) + 1, s, WIDTH.min(r.w), span),
                        Edge::Top => (s, boundary, span, WIDTH.min(r.h)),
                        Edge::Bottom => (s, boundary - WIDTH.min(r.h) + 1, span, WIDTH.min(r.h)),
                    });
                }
            }
        }
    }
    out
}

fn sta(state: Weak<State>) {
    if let Err(e) = unsafe { OleInitialize(None) } {
        log(&format!("OleInitialize failed HRESULT={:#x}", e.code().0));
        return;
    }
    let class = w!("FlowseamEdgeDrop");
    let wc = WNDCLASSW { lpfnWndProc: Some(window_proc), lpszClassName: class, ..Default::default() };
    if unsafe { RegisterClassW(&wc) } == 0 {
        let code = windows::core::Error::from_thread().code().0;
        if code != 0x80070582u32 as i32 {
            log(&format!("RegisterClassW failed HRESULT={code:#x}"));
            unsafe { OleUninitialize(); }
            return;
        }
    }
    let mut msg = MSG::default();
    let _ = unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE) };
    if let Some(s) = state.upgrade() { s.thread_id.store(unsafe { GetCurrentThreadId() }, Ordering::Release); }
    let mut windows = Vec::new();
    let mut geometry = None;
    loop {
        let Some(s) = state.upgrade() else { break };
        let settings = s.settings.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let armed = s.armed.load(Ordering::Acquire);
        drop(s);
        let next_geometry = settings.clone();
        if geometry.as_ref() != Some(&next_geometry) {
            for hwnd in windows.drain(..) {
                if let Err(e) = unsafe { RevokeDragDrop(hwnd) } { log(&format!("RevokeDragDrop failed HRESULT={:#x}", e.code().0)); }
                let _ = unsafe { DestroyWindow(hwnd) };
            }
            for (x, y, w, h) in strips(&settings.1, settings.0) {
                let ex = WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED;
                match unsafe { CreateWindowExW(ex, class, PCWSTR::null(), WS_POPUP, x, y, w, h, None, None, None, None) } {
                    Ok(hwnd) => {
                        if let Err(e) = unsafe { SetLayeredWindowAttributes(hwnd, COLORREF(0), 1, LWA_ALPHA) } {
                            log(&format!("SetLayeredWindowAttributes failed HRESULT={:#x}", e.code().0));
                        }
                        let target: IDropTarget = Target { state: state.clone() }.into();
                        match unsafe { RegisterDragDrop(hwnd, &target) } {
                            Ok(()) => windows.push(hwnd),
                            Err(e) => {
                                log(&format!("RegisterDragDrop failed HRESULT={:#x}", e.code().0));
                                let _ = unsafe { DestroyWindow(hwnd) };
                            }
                        }
                    }
                    Err(e) => log(&format!("CreateWindowExW failed HRESULT={:#x}", e.code().0)),
                }
            }
            geometry = Some(next_geometry);
        }
        for &hwnd in &windows { let _ = unsafe { ShowWindow(hwnd, if armed { SW_SHOWNOACTIVATE } else { SW_HIDE }) }; }
        let got = unsafe { GetMessageW(&mut msg, None, 0, 0) }.0;
        if got == -1 {
            log(&format!("GetMessageW failed HRESULT={:#x}", windows::core::Error::from_thread().code().0));
            break;
        }
        if got == 0 { break; }
        if msg.message != REFRESH {
            unsafe { let _ = TranslateMessage(&msg); DispatchMessageW(&msg); }
        }
    }
    for hwnd in windows {
        let _ = unsafe { RevokeDragDrop(hwnd) };
        let _ = unsafe { DestroyWindow(hwnd) };
    }
    unsafe { OleUninitialize(); }
}
