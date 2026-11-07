//! Windows OLE virtual-file drag for files arriving from the peer.

#[cfg(any(windows, test))]
fn read_len(at: u64, have: u64, size: u64, wanted: u32) -> usize {
    (size.saturating_sub(at).min(have.saturating_sub(at)).min(wanted as u64)) as usize
}

#[cfg(any(windows, test))]
fn drag_decision_code(cancelled: bool, escape: bool, buttons: u32) -> i32 {
    if cancelled || escape { 0x40101 } else if buttons & 1 == 0 { 0x40100 } else { 0 }
}

#[cfg(any(windows, test))]
fn descriptor_fields(name: &str, size: u64) -> (u32, u32, [u16; 260]) {
    let mut filename = [0u16; 260];
    for (to, ch) in filename.iter_mut().take(259).zip(name.encode_utf16()) { *to = ch; }
    ((size >> 32) as u32, size as u32, filename)
}

#[cfg(windows)]
mod platform {
use super::{descriptor_fields, drag_decision_code, read_len};

use std::ffi::c_void;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::mem::size_of;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use flowseam_net::files::{NativeDrop, NativeSession, Staged};
use windows::core::{implement, Error, HRESULT, Result as WinResult};
use windows::Win32::Foundation::{GlobalFree, DATA_S_SAMEFORMATETC, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, DV_E_FORMATETC, E_NOTIMPL, OLE_E_ADVISENOTSUPPORTED, S_FALSE, S_OK, STG_E_READFAULT};
use windows::Win32::System::Com::{DATADIR_GET, DVASPECT_CONTENT, FORMATETC, IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA, ISequentialStream, ISequentialStream_Impl, IStream, IStream_Impl, LOCKTYPE, STATFLAG, STATSTG, STGC, STGMEDIUM, STGMEDIUM_0, STREAM_SEEK, STREAM_SEEK_CUR, STREAM_SEEK_END, STREAM_SEEK_SET, TYMED_HGLOBAL, TYMED_ISTREAM};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE, GMEM_ZEROINIT};
use windows::Win32::System::Ole::{DoDragDrop, IDropSource, IDropSource_Impl, OleInitialize, OleUninitialize, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Shell::{SHCreateStdEnumFmtEtc, FD_FILESIZE, FD_PROGRESSUI, FILEDESCRIPTORW};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEINPUT, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP};
use windows_sys::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos, GetWindowLongPtrW, PostMessageW, RegisterClassW, SetLayeredWindowAttributes, SetWindowLongPtrW, SetWindowPos, ShowWindow, TranslateMessage, WNDCLASSW, GWLP_USERDATA, HWND_TOPMOST, LWA_ALPHA, MSG, SWP_NOACTIVATE, SWP_NOSIZE, SW_SHOWNOACTIVATE, WM_APP, WM_LBUTTONDOWN, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP};

const WAKE: u32 = WM_APP + 17;
const TIMEOUT: u32 = WM_APP + 18;
/// Registered once: a class per drag would never be unregistered and would fill the process's class table.
static CLASS: OnceLock<Result<Vec<u16>, String>> = OnceLock::new();

fn window_class() -> Result<&'static [u16], String> {
    CLASS
        .get_or_init(|| {
            let class = wide("FlowseamNativeDropWindow");
            let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), lpszClassName: class.as_ptr(), ..unsafe { std::mem::zeroed() } };
            if unsafe { RegisterClassW(&wc) } == 0 { Err(format!("RegisterClassW HRESULT={}", std::io::Error::last_os_error())) } else { Ok(class) }
        })
        .as_ref()
        .map(|c| c.as_slice())
        .map_err(Clone::clone)
}

fn log(msg: &str) { flowseam_core::log::line(&format!("[flowseam-win] native drop: {msg}")); }

fn wide(s: &str) -> Vec<u16> { s.encode_utf16().chain(Some(0)).collect() }

fn format_ids() -> (u16, u16) {
    let a = wide("FileGroupDescriptorW");
    let b = wide("FileContents");
    unsafe { (RegisterClipboardFormatW(a.as_ptr()) as u16, RegisterClipboardFormatW(b.as_ptr()) as u16) }
}

fn descriptor_bytes(files: &[flowseam_core::proto::FileMeta]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + files.len() * size_of::<FILEDESCRIPTORW>());
    out.extend_from_slice(&(files.len() as u32).to_le_bytes());
    for meta in files {
        let (high, low, filename) = descriptor_fields(&meta.name, meta.size);
        let d = FILEDESCRIPTORW {
            dwFlags: (FD_FILESIZE.0 | FD_PROGRESSUI.0) as u32,
            nFileSizeHigh: high,
            nFileSizeLow: low,
            cFileName: filename,
            ..Default::default()
        };
        let bytes = unsafe { std::slice::from_raw_parts((&d as *const FILEDESCRIPTORW).cast::<u8>(), size_of::<FILEDESCRIPTORW>()) };
        out.extend_from_slice(bytes);
    }
    out
}

#[implement(ISequentialStream, IStream)]
struct FileStream { staged: Arc<Staged>, index: usize, pos: Mutex<u64> }

impl ISequentialStream_Impl for FileStream_Impl {
    fn Read(&self, pv: *mut c_void, cb: u32, pcbread: *mut u32) -> HRESULT {
        if !pcbread.is_null() { unsafe { *pcbread = 0 }; }
        if cb == 0 { return S_OK; }
        if pv.is_null() { return STG_E_READFAULT; }
        let mut pos = self.pos.lock().unwrap_or_else(|e| e.into_inner());
        let size = self.staged.files()[self.index].size;
        // S_OK promises all `cb` bytes and S_FALSE means the end was reached, so a target may stop at a short S_OK
        // read: wait for the bytes still on their way instead of handing back what has arrived so far.
        let mut file = None;
        let mut done = 0usize;
        while (done as u64) < cb as u64 && *pos < size {
            let have = match self.staged.wait_past(self.index, *pos) {
                Ok(n) => n,
                Err(e) => { log(&format!("stream wait failed HRESULT={STG_E_READFAULT:?}: {e}")); return STG_E_READFAULT; }
            };
            let n = read_len(*pos, have, size, cb - done as u32);
            if n == 0 { break; }
            if file.is_none() {
                match File::open(self.staged.path(self.index)) {
                    Ok(f) => file = Some(f),
                    Err(e) => { log(&format!("stream open failed HRESULT={STG_E_READFAULT:?}: {e}")); return STG_E_READFAULT; }
                }
            }
            let f = file.as_mut().expect("opened above");
            let out = unsafe { std::slice::from_raw_parts_mut(pv.cast::<u8>().add(done), n) };
            if let Err(e) = f.seek(SeekFrom::Start(*pos)).and_then(|_| f.read_exact(out)) {
                log(&format!("stream read failed HRESULT={STG_E_READFAULT:?}: {e}"));
                return STG_E_READFAULT;
            }
            *pos += n as u64;
            done += n;
        }
        if !pcbread.is_null() { unsafe { *pcbread = done as u32 }; }
        if done as u64 == cb as u64 { S_OK } else { S_FALSE }
    }
    fn Write(&self, _: *const c_void, _: u32, _: *mut u32) -> HRESULT { STG_E_READFAULT }
}

impl IStream_Impl for FileStream_Impl {
    fn Seek(&self, delta: i64, origin: STREAM_SEEK, new: *mut u64) -> WinResult<()> {
        let mut p = self.pos.lock().unwrap_or_else(|e| e.into_inner());
        let base = if origin == STREAM_SEEK_SET { 0 } else if origin == STREAM_SEEK_CUR { *p } else if origin == STREAM_SEEK_END { self.staged.files()[self.index].size } else { return Err(Error::from_hresult(STG_E_READFAULT)); };
        *p = base.checked_add_signed(delta).ok_or_else(|| Error::from_hresult(STG_E_READFAULT))?;
        if !new.is_null() { unsafe { *new = *p }; }
        Ok(())
    }
    fn SetSize(&self, _: u64) -> WinResult<()> { Err(Error::from_hresult(E_NOTIMPL)) }
    fn CopyTo(&self, _: windows::core::Ref<IStream>, _: u64, _: *mut u64, _: *mut u64) -> WinResult<()> { Err(Error::from_hresult(E_NOTIMPL)) }
    fn Commit(&self, _: &STGC) -> WinResult<()> { Ok(()) }
    fn Revert(&self) -> WinResult<()> { Err(Error::from_hresult(E_NOTIMPL)) }
    fn LockRegion(&self, _: u64, _: u64, _: &LOCKTYPE) -> WinResult<()> { Err(Error::from_hresult(E_NOTIMPL)) }
    fn UnlockRegion(&self, _: u64, _: u64, _: u32) -> WinResult<()> { Err(Error::from_hresult(E_NOTIMPL)) }
    fn Stat(&self, out: *mut STATSTG, _: &STATFLAG) -> WinResult<()> {
        if out.is_null() { return Err(Error::from_hresult(STG_E_READFAULT)); }
        let stat = STATSTG { r#type: 2, cbSize: self.staged.files()[self.index].size, ..Default::default() };
        unsafe { *out = stat }; Ok(())
    }
    fn Clone(&self) -> WinResult<IStream> { Ok(FileStream { staged: self.staged.clone(), index: self.index, pos: Mutex::new(*self.pos.lock().unwrap_or_else(|e| e.into_inner())) }.into()) }
}

#[implement(IDataObject)]
struct VirtualFiles { staged: Arc<Staged>, descriptors: u16, contents: u16 }

impl VirtualFiles {
    fn valid(&self, f: &FORMATETC) -> bool {
        f.dwAspect == DVASPECT_CONTENT.0 && ((f.cfFormat == self.descriptors && f.lindex == -1 && f.tymed & TYMED_HGLOBAL.0 as u32 != 0) || (f.cfFormat == self.contents && f.lindex >= 0 && (f.lindex as usize) < self.staged.files().len() && f.tymed & TYMED_ISTREAM.0 as u32 != 0))
    }
    fn formats(&self) -> [FORMATETC; 2] {
        [FORMATETC { cfFormat: self.descriptors, dwAspect: DVASPECT_CONTENT.0, lindex: -1, tymed: TYMED_HGLOBAL.0 as u32, ..Default::default() }, FORMATETC { cfFormat: self.contents, dwAspect: DVASPECT_CONTENT.0, lindex: 0, tymed: TYMED_ISTREAM.0 as u32, ..Default::default() }]
    }
}

impl IDataObject_Impl for VirtualFiles_Impl {
    fn GetData(&self, f: *const FORMATETC) -> WinResult<STGMEDIUM> {
        let Some(f) = (unsafe { f.as_ref() }) else { return Err(Error::from_hresult(DV_E_FORMATETC)); };
        if !self.valid(f) { return Err(Error::from_hresult(DV_E_FORMATETC)); }
        if f.cfFormat == self.contents {
            let stream: IStream = FileStream { staged: self.staged.clone(), index: f.lindex as usize, pos: Mutex::new(0) }.into();
            return Ok(STGMEDIUM { tymed: TYMED_ISTREAM.0 as u32, u: STGMEDIUM_0 { pstm: std::mem::ManuallyDrop::new(Some(stream)) }, ..Default::default() });
        }
        let bytes = descriptor_bytes(self.staged.files());
        let h = unsafe { GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes.len()) }?;
        let p = unsafe { GlobalLock(h) };
        if p.is_null() { let e = Error::from_hresult(STG_E_READFAULT); log(&format!("GlobalLock failed HRESULT={:?}", e.code())); unsafe { let _ = GlobalFree(Some(h)); } return Err(e); }
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), p.cast::<u8>(), bytes.len()); let _ = GlobalUnlock(h); }
        Ok(STGMEDIUM { tymed: TYMED_HGLOBAL.0 as u32, u: STGMEDIUM_0 { hGlobal: h }, ..Default::default() })
    }
    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> WinResult<()> { Err(Error::from_hresult(DV_E_FORMATETC)) }
    fn QueryGetData(&self, f: *const FORMATETC) -> HRESULT { if unsafe { f.as_ref() }.is_some_and(|f| self.valid(f)) { S_OK } else { DV_E_FORMATETC } }
    fn GetCanonicalFormatEtc(&self, input: *const FORMATETC, output: *mut FORMATETC) -> HRESULT { if !input.is_null() && !output.is_null() { unsafe { *output = *input }; } DATA_S_SAMEFORMATETC }
    fn SetData(&self, _: *const FORMATETC, _: *const STGMEDIUM, _: windows::core::BOOL) -> WinResult<()> { Ok(()) }
    fn EnumFormatEtc(&self, direction: u32) -> WinResult<IEnumFORMATETC> {
        if direction != DATADIR_GET.0 as u32 { return Err(Error::from_hresult(E_NOTIMPL)); }
        unsafe { SHCreateStdEnumFmtEtc(&self.formats()) }
    }
    fn DAdvise(&self, _: *const FORMATETC, _: u32, _: windows::core::Ref<IAdviseSink>) -> WinResult<u32> { Err(Error::from_hresult(OLE_E_ADVISENOTSUPPORTED)) }
    fn DUnadvise(&self, _: u32) -> WinResult<()> { Err(Error::from_hresult(OLE_E_ADVISENOTSUPPORTED)) }
    fn EnumDAdvise(&self) -> WinResult<IEnumSTATDATA> { Err(Error::from_hresult(OLE_E_ADVISENOTSUPPORTED)) }
}

#[implement(IDropSource)]
struct Source { cancelled: Arc<AtomicBool> }
impl IDropSource_Impl for Source_Impl {
    fn QueryContinueDrag(&self, escape: windows::core::BOOL, buttons: MODIFIERKEYS_FLAGS) -> HRESULT { HRESULT(drag_decision_code(self.cancelled.load(Ordering::Acquire), escape.as_bool(), buttons.0)) }
    fn GiveFeedback(&self, _: DROPEFFECT) -> HRESULT { DRAGDROP_S_USEDEFAULTCURSORS }
}

struct Shared { cancelled: Arc<AtomicBool>, hwnd: AtomicIsize, pressed: AtomicBool, started: AtomicBool }
struct Context { drag: u32, staged: Arc<Staged>, shared: Arc<Shared>, ended: Option<Box<dyn FnOnce(bool) + Send>> }

fn input_mouse(flags: u32) -> bool {
    let i = INPUT { r#type: INPUT_MOUSE, Anonymous: INPUT_0 { mi: MOUSEINPUT { dx: 0, dy: 0, mouseData: 0, dwFlags: flags, time: 0, dwExtraInfo: crate::win_input::INJECT_MARK } } };
    unsafe { SendInput(1, &i, size_of::<INPUT>() as i32) == 1 }
}
fn input_escape() -> bool {
    let i = INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: 0, wScan: 0x01, dwFlags: KEYEVENTF_SCANCODE, time: 0, dwExtraInfo: crate::win_input::INJECT_MARK } } };
    let up = INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { dwFlags: KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP, ..unsafe { i.Anonymous.ki } } } };
    unsafe { SendInput(2, [i, up].as_ptr(), size_of::<INPUT>() as i32) == 2 }
}
fn release_if_ours(shared: &Shared) {
    if shared.pressed.swap(false, Ordering::AcqRel)
        && unsafe { windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(0x01) } < 0
        && !input_mouse(MOUSEEVENTF_LEFTUP)
    {
        log("release SendInput failed HRESULT=0x80004005");
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let p = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Context };
    if !p.is_null() {
        let ctx = unsafe { &mut *p };
        if msg == WM_LBUTTONDOWN && !ctx.shared.started.swap(true, Ordering::AcqRel) {
            if ctx.shared.cancelled.load(Ordering::Acquire) {
                // The cancel's WAKE and the watchdog's TIMEOUT both skip a started window, so this is the last chance
                // to end the session; returning without it would leave the window, the thread and `ended` hanging.
                log(&format!("cancel drag {} at the press", ctx.drag));
                release_if_ours(&ctx.shared);
                if let Some(end) = ctx.ended.take() { end(false); }
                unsafe { DestroyWindow(hwnd) };
                return 0;
            }
            let ids = format_ids();
            let data: IDataObject = VirtualFiles { staged: ctx.staged.clone(), descriptors: ids.0, contents: ids.1 }.into();
            let source: IDropSource = Source { cancelled: ctx.shared.cancelled.clone() }.into();
            let shared = ctx.shared.clone();
            let drag = ctx.drag;
            let ended = ctx.ended.take();
            let mut effect = DROPEFFECT_NONE;
            log(&format!("start drag {drag}"));
            let hr = unsafe { DoDragDrop(&data, &source, DROPEFFECT_COPY, &mut effect) };
            let took = !shared.cancelled.load(Ordering::Acquire) && hr == DRAGDROP_S_DROP && effect != DROPEFFECT_NONE;
            log(&format!("drop drag {drag} HRESULT={hr:?} effect={effect:?} took={took}"));
            if hr == DRAGDROP_S_DROP {
                shared.pressed.store(false, Ordering::Release); // a drop happens on the release: the button is up
            } else {
                // Escape or a refusal ends the drag with our press still down; nothing else would let it go.
                release_if_ours(&shared);
            }
            if let Some(end) = ended { end(took); }
            unsafe { DestroyWindow(hwnd) }; return 0;
        }
        if msg == TIMEOUT && !ctx.shared.started.load(Ordering::Acquire) {
            log(&format!("fallback drag {}: start timeout HRESULT=0x800705B4", ctx.drag));
            release_if_ours(&ctx.shared);
            if let Some(end) = ctx.ended.take() { end(false); }
            unsafe { DestroyWindow(hwnd) }; return 0;
        }
        if msg == WAKE && !ctx.shared.started.load(Ordering::Acquire) {
            log(&format!("cancel drag {} before start", ctx.drag));
            release_if_ours(&ctx.shared);
            if let Some(end) = ctx.ended.take() { end(false); }
            unsafe { DestroyWindow(hwnd) }; return 0;
        }
    }
    unsafe { DefWindowProcW(hwnd, msg, w, l) }
}

fn run(mut ctx: Context) {
    let shared = ctx.shared.clone();
    let mut ole_ready = false;
    let result = (|| -> Result<(), String> {
        unsafe { OleInitialize(None) }.map_err(|e| format!("OleInitialize HRESULT={:?}", e.code()))?;
        ole_ready = true;
        let class = window_class()?;
        let mut pt = POINT { x: 0, y: 0 };
        if unsafe { GetCursorPos(&mut pt) } == 0 { return Err(format!("GetCursorPos HRESULT={}", std::io::Error::last_os_error())); }
        let hwnd = unsafe { CreateWindowExW(WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED, class.as_ptr(), class.as_ptr(), WS_POPUP, pt.x - 32, pt.y - 32, 64, 64, ptr::null_mut(), ptr::null_mut(), ptr::null_mut(), ptr::null()) };
        if hwnd.is_null() { return Err(format!("CreateWindowExW HRESULT={}", std::io::Error::last_os_error())); }
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, (&mut ctx as *mut Context) as isize); SetLayeredWindowAttributes(hwnd, 0, 13, LWA_ALPHA); ShowWindow(hwnd, SW_SHOWNOACTIVATE); }
        shared.hwnd.store(hwnd as isize, Ordering::Release);
        if shared.cancelled.load(Ordering::Acquire) { unsafe { PostMessageW(hwnd, WAKE, 0, 0) }; }
        let watchdog = shared.clone();
        thread::spawn(move || { thread::sleep(Duration::from_millis(1500)); let hwnd = watchdog.hwnd.load(Ordering::Acquire) as HWND; if !hwnd.is_null() && !watchdog.started.load(Ordering::Acquire) { unsafe { PostMessageW(hwnd, TIMEOUT, 0, 0) }; } });
        thread::sleep(Duration::from_millis(150));
        if !shared.cancelled.load(Ordering::Acquire) {
            // The peer kept moving during the wait: put the window back under the cursor, or the press lands on
            // whatever window is there now.
            let mut now = POINT { x: 0, y: 0 };
            if unsafe { GetCursorPos(&mut now) } != 0 {
                unsafe { SetWindowPos(hwnd, HWND_TOPMOST, now.x - 32, now.y - 32, 0, 0, SWP_NOSIZE | SWP_NOACTIVATE) };
            }
            shared.pressed.store(true, Ordering::Release);
            if !input_mouse(MOUSEEVENTF_LEFTDOWN) { shared.pressed.store(false, Ordering::Release); log(&format!("SendInput press failed HRESULT={}", std::io::Error::last_os_error())); unsafe { PostMessageW(hwnd, TIMEOUT, 0, 0) }; }
        }
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        while unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetMessageW(&mut msg, ptr::null_mut(), 0, 0) } > 0 {
            unsafe { TranslateMessage(&msg); DispatchMessageW(&msg) };
            if shared.hwnd.load(Ordering::Acquire) == 0 { break; }
            if unsafe { windows_sys::Win32::UI::WindowsAndMessaging::IsWindow(hwnd) } == 0 { break; }
        }
        shared.hwnd.store(0, Ordering::Release);
        Ok(())
    })();
    if let Err(e) = result { log(&format!("fallback drag {}: {e}", ctx.drag)); release_if_ours(&shared); if let Some(end) = ctx.ended.take() { end(false); } }
    if ole_ready { unsafe { OleUninitialize() }; }
}

#[derive(Default)]
pub struct WinNativeDrop;
impl WinNativeDrop { pub fn new() -> Self { Self } }
struct Session { shared: Arc<Shared> }
impl NativeSession for Session {
    fn cancel(&self) {
        log("cancel requested");
        self.shared.cancelled.store(true, Ordering::Release);
        release_if_ours(&self.shared);
        let hwnd = self.shared.hwnd.load(Ordering::Acquire) as HWND;
        if !hwnd.is_null() { unsafe { PostMessageW(hwnd, WAKE, 0, 0) }; }
        if self.shared.started.load(Ordering::Acquire) && !input_escape() { log(&format!("cancel Escape failed HRESULT={}", std::io::Error::last_os_error())); }
    }
}
impl NativeDrop for WinNativeDrop {
    fn begin(&self, drag: u32, staged: Arc<Staged>, ended: Box<dyn FnOnce(bool) + Send>) -> Result<Box<dyn NativeSession>, String> {
        let shared = Arc::new(Shared { cancelled: Arc::new(AtomicBool::new(false)), hwnd: AtomicIsize::new(0), pressed: AtomicBool::new(false), started: AtomicBool::new(false) });
        let ctx = Context { drag, staged, shared: shared.clone(), ended: Some(ended) };
        thread::Builder::new().name(format!("flowseam-drop-{drag}")).spawn(move || run(ctx)).map_err(|e| format!("native drop thread: {e}"))?;
        Ok(Box::new(Session { shared }))
    }
}

}

#[cfg(windows)]
pub use platform::WinNativeDrop;

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn decision_covers_cancel_escape_and_release() {
        assert_eq!(drag_decision_code(false, false, 1), 0);
        assert_eq!(drag_decision_code(false, false, 0), 0x40100);
        assert_eq!(drag_decision_code(true, false, 0), 0x40101);
        assert_eq!(drag_decision_code(false, true, 1), 0x40101);
    }
    #[test] fn read_plan_never_reports_unstaged_bytes() {
        assert_eq!(read_len(10, 15, 20, 100), 5);
        assert_eq!(read_len(15, 15, 20, 100), 0);
        assert_eq!(read_len(20, 20, 20, 100), 0);
    }
    #[test] fn descriptor_fields_keep_size_and_terminate_name() {
        let (high, low, name) = descriptor_fields("sample.txt", 0x1_0000_0001);
        assert_eq!((high, low), (1, 1));
        assert_eq!(name[0], 's' as u16);
        assert_eq!(name[10], 0);
    }
}
