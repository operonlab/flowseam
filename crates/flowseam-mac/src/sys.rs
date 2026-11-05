//! Safe wrappers over `ffi`. Every OS failure becomes a typed `BackendError`
//! (design §4), never a bare string. Each `unsafe` block is one FFI call.

use crate::ffi::{self, CGPoint};
use flowseam_core::backend::{BackendError, Permission, Preflight};
use flowseam_core::proto::{Point, Rect};
use std::ffi::c_void;

fn cg_check(code: i32, api: &'static str) -> Result<(), BackendError> {
    if code == 0 {
        Ok(())
    } else {
        Err(BackendError::Os { code, api })
    }
}

/// Capture needs Accessibility (to create an active tap) and Input Monitoring.
/// Both are answered exactly by the OS, so there is no `Unknown` here.
pub fn capture_preflight() -> Preflight {
    if !unsafe { ffi::AXIsProcessTrusted() } {
        return Preflight::Missing(Permission::Accessibility);
    }
    if !unsafe { ffi::CGPreflightListenEventAccess() } {
        return Preflight::Missing(Permission::InputMonitoring);
    }
    Preflight::Ok
}

pub fn emit_preflight() -> Preflight {
    if unsafe { ffi::CGPreflightPostEventAccess() } {
        Preflight::Ok
    } else {
        Preflight::Missing(Permission::PostEvent)
    }
}

pub fn secure_input_enabled() -> bool {
    unsafe { ffi::IsSecureEventInputEnabled() }
}

pub fn active_displays() -> Result<Vec<Rect>, BackendError> {
    const MAX: usize = 32;
    let mut ids = [0u32; MAX];
    let mut n: u32 = 0;
    let code = unsafe { ffi::CGGetActiveDisplayList(MAX as u32, ids.as_mut_ptr(), &mut n) };
    cg_check(code, "CGGetActiveDisplayList")?;
    Ok(ids[..(n as usize).min(MAX)]
        .iter()
        .map(|&id| {
            let b = unsafe { ffi::CGDisplayBounds(id) };
            Rect {
                x: b.origin.x.round() as i32,
                y: b.origin.y.round() as i32,
                w: b.size.width.round() as i32,
                h: b.size.height.round() as i32,
            }
        })
        .collect())
}

pub fn main_display_center() -> Point {
    let b = unsafe { ffi::CGDisplayBounds(ffi::CGMainDisplayID()) };
    Point {
        x: (b.origin.x + b.size.width / 2.0).round() as i32,
        y: (b.origin.y + b.size.height / 2.0).round() as i32,
    }
}

/// A windowless background process's `CGDisplayHideCursor` returns success and does nothing (measured:
/// the cursor stayed visible through a grab), so first ask the window server to honour it: the private
/// connection property `SetsCursorInBackground`, the one Deskflow and Synergy set. Also drop the 0.25 s
/// local-event suppression that follows each warp, or holding the cursor with warps would stutter.
/// Both are best effort and logged once if missing; the hide itself still runs. The foreground hide is
/// requested too (see `front`): this one alone failed when the crossing passed over the Dock.
pub fn hide_cursor() -> Result<(), BackendError> {
    crate::front::request(true);
    use std::sync::Once;
    static WARN: Once = Once::new();
    if let Err(e) = background_cursor_control() {
        WARN.call_once(|| eprintln!("flowseam: cannot hide the cursor from the background ({e}); it may stay visible"));
    }
    cg_check(unsafe { ffi::CGDisplayHideCursor(ffi::CGMainDisplayID()) }, "CGDisplayHideCursor")
}

fn sym(name: &std::ffi::CStr) -> Result<*mut c_void, String> {
    // SAFETY: dlsym with RTLD_DEFAULT and a NUL-terminated name only reads.
    let p = unsafe { ffi::dlsym(ffi::RTLD_DEFAULT, name.as_ptr()) };
    if p.is_null() {
        Err(format!("{} not found", name.to_string_lossy()))
    } else {
        Ok(p)
    }
}

fn background_cursor_control() -> Result<(), String> {
    type DefaultConnection = unsafe extern "C" fn() -> i32;
    type SetConnectionProperty = unsafe extern "C" fn(i32, i32, ffi::CFTypeRef, ffi::CFTypeRef) -> i32;
    type SetSuppression = unsafe extern "C" fn(f64) -> i32;
    let conn = sym(c"_CGSDefaultConnection")?;
    let set = sym(c"CGSSetConnectionProperty")?;
    let suppress = sym(c"CGSetLocalEventsSuppressionInterval")?;
    // SAFETY: the symbols have these C signatures (as declared by Deskflow and by Apple's deprecated header);
    // the CFString is created, used once and released here.
    unsafe {
        let conn: DefaultConnection = std::mem::transmute::<*mut c_void, DefaultConnection>(conn);
        let set: SetConnectionProperty = std::mem::transmute::<*mut c_void, SetConnectionProperty>(set);
        let suppress: SetSuppression = std::mem::transmute::<*mut c_void, SetSuppression>(suppress);
        let key = ffi::CFStringCreateWithCString(std::ptr::null(), c"SetsCursorInBackground".as_ptr(), ffi::kCFStringEncodingUTF8);
        if key.is_null() {
            return Err("CFStringCreateWithCString failed".into());
        }
        let cid = conn();
        let err = set(cid, cid, key, ffi::kCFBooleanTrue);
        ffi::CFRelease(key);
        if err != 0 {
            return Err(format!("CGSSetConnectionProperty returned {err}"));
        }
        let err = suppress(0.0);
        if err != 0 {
            return Err(format!("CGSetLocalEventsSuppressionInterval returned {err}"));
        }
    }
    Ok(())
}

pub fn show_cursor() -> Result<(), BackendError> {
    crate::front::request(false);
    cg_check(unsafe { ffi::CGDisplayShowCursor(ffi::CGMainDisplayID()) }, "CGDisplayShowCursor")
}

pub fn set_mouse_association(connected: bool) -> Result<(), BackendError> {
    cg_check(
        unsafe { ffi::CGAssociateMouseAndMouseCursorPosition(connected) },
        "CGAssociateMouseAndMouseCursorPosition",
    )
}

pub fn warp_cursor(p: Point) -> Result<(), BackendError> {
    let pt = CGPoint { x: p.x as f64, y: p.y as f64 };
    cg_check(unsafe { ffi::CGWarpMouseCursorPosition(pt) }, "CGWarpMouseCursorPosition")?;
    // A warp moves the cursor without telling any app, and an app that was tracking the cursor where it
    // was keeps it: crossing on the Dock's side, the Dock kept the cursor drawn through the whole grab,
    // hidden or not (measured). One marked move at the new spot tells it; the tap passes marked events,
    // so this is neither forwarded nor swallowed.
    post_mouse(ffi::EV_MOUSE_MOVED, p, 0)
}

pub fn cursor_position() -> Result<Point, BackendError> {
    let ev = unsafe { ffi::CGEventCreate(std::ptr::null()) };
    if ev.is_null() {
        return Err(BackendError::Os { code: -1, api: "CGEventCreate" });
    }
    let loc = unsafe { ffi::CGEventGetLocation(ev) };
    unsafe { ffi::CFRelease(ev) };
    Ok(pixel_at(loc.x, loc.y))
}

/// Subtype and data1 of a system-defined event, through NSEvent (CoreGraphics has no field for them).
/// `None` if NSEvent cannot read the event.
///
/// # Safety
/// `ev` must be a live CGEvent.
pub unsafe fn system_defined(ev: ffi::CGEventRef) -> Option<(i16, i64)> {
    type FromCg = unsafe extern "C" fn(*mut c_void, *mut c_void, ffi::CGEventRef) -> *mut c_void;
    type GetI16 = unsafe extern "C" fn(*mut c_void, *mut c_void) -> i16;
    type GetI64 = unsafe extern "C" fn(*mut c_void, *mut c_void) -> i64;
    type Untyped = unsafe extern "C" fn();
    let send = ffi::objc_msgSend as Untyped;
    // SAFETY: each cast matches the method's Objective-C signature on arm64 and x86_64 (integer
    // returns); the pool drains the autoreleased NSEvent on this non-main thread.
    unsafe {
        let pool = ffi::objc_autoreleasePoolPush();
        let class = ffi::objc_getClass(c"NSEvent".as_ptr());
        let ns = std::mem::transmute::<Untyped, FromCg>(send)(class, ffi::sel_registerName(c"eventWithCGEvent:".as_ptr()), ev);
        let out = (!ns.is_null()).then(|| {
            let subtype = std::mem::transmute::<Untyped, GetI16>(send)(ns, ffi::sel_registerName(c"subtype".as_ptr()));
            let data1 = std::mem::transmute::<Untyped, GetI64>(send)(ns, ffi::sel_registerName(c"data1".as_ptr()));
            (subtype, data1)
        });
        ffi::objc_autoreleasePoolPop(pool);
        out
    }
}

/// Whether the keyboard used last is ISO.
pub fn keyboard_is_iso() -> bool {
    // SAFETY: plain Carbon queries.
    unsafe { ffi::KBGetLayoutType(ffi::LMGetKbdType() as i16) == ffi::kKeyboardISO }
}

/// The pixel holding a cursor location. The OS keeps the cursor fractional.
fn pixel_at(x: f64, y: f64) -> Point {
    Point { x: x.floor() as i32, y: y.floor() as i32 }
}

/// Tag an event as ours; the tap lets tagged events through untouched.
pub(crate) fn mark(ev: ffi::CGEventRef) {
    unsafe { ffi::CGEventSetIntegerValueField(ev, crate::inject_mark::FIELD, crate::inject_mark::MARK) };
}

/// Post one event to the HID tap and release it.
///
/// `CGEventPost` returns no status, so success here means "posted without a
/// null event", not "the target app received it". The caller reports it as
/// such. Secure Input is checked separately by the emitter.
fn post(ev: ffi::CGEventRef, api: &'static str) -> Result<(), BackendError> {
    if ev.is_null() {
        return Err(BackendError::Os { code: -1, api });
    }
    mark(ev);
    unsafe {
        ffi::CGEventPost(ffi::TAP_LOCATION_HID, ev);
        ffi::CFRelease(ev as *const c_void);
    }
    Ok(())
}

pub fn post_mouse(ty: u32, p: Point, button: u32) -> Result<(), BackendError> {
    let ev = unsafe {
        ffi::CGEventCreateMouseEvent(
            std::ptr::null(),
            ty,
            CGPoint { x: p.x as f64, y: p.y as f64 },
            button,
        )
    };
    if !ev.is_null() {
        // Non-left/right buttons need the button number field set explicitly.
        unsafe { ffi::CGEventSetIntegerValueField(ev, ffi::F_MOUSE_BUTTON_NUMBER, button as i64) };
    }
    post(ev, "CGEventCreateMouseEvent")
}

pub fn post_key(keycode: u16, down: bool, repeat: bool, flags: u64, flags_changed: bool) -> Result<(), BackendError> {
    let ev = unsafe { ffi::CGEventCreateKeyboardEvent(std::ptr::null(), keycode, down) };
    if !ev.is_null() {
        unsafe {
            if flags_changed {
                ffi::CGEventSetType(ev, ffi::EV_FLAGS_CHANGED);
            }
            ffi::CGEventSetFlags(ev, flags);
            ffi::CGEventSetIntegerValueField(ev, ffi::F_KEYBOARD_AUTOREPEAT, repeat as i64);
        }
    }
    post(ev, "CGEventCreateKeyboardEvent")
}

pub fn post_scroll(dx: i32, dy: i32) -> Result<(), BackendError> {
    let ev = unsafe { ffi::CGEventCreateScrollWheelEvent2(std::ptr::null(), ffi::SCROLL_UNIT_LINE, 2, dy, dx, 0) };
    post(ev, "CGEventCreateScrollWheelEvent2")
}

/// Register a display-reconfiguration callback.
///
/// # Safety
/// CoreGraphics hands `user` back to `cb` on its own thread. `user` must stay valid, and be safe for
/// `cb` to dereference, until [`remove_display_callback`] is called with the same pair.
pub unsafe fn add_display_callback(cb: ffi::CGDisplayReconfigurationCallBack, user: *mut c_void) -> Result<(), BackendError> {
    cg_check(
        unsafe { ffi::CGDisplayRegisterReconfigurationCallback(cb, user) },
        "CGDisplayRegisterReconfigurationCallback",
    )
}

/// # Safety
/// `cb` and `user` must be a pair previously registered with [`add_display_callback`]. After this
/// returns `Ok`, CoreGraphics no longer calls `cb` with `user`, so the caller may free `user`.
pub unsafe fn remove_display_callback(cb: ffi::CGDisplayReconfigurationCallBack, user: *mut c_void) -> Result<(), BackendError> {
    cg_check(
        unsafe { ffi::CGDisplayRemoveReconfigurationCallback(cb, user) },
        "CGDisplayRemoveReconfigurationCallback",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hand pushing against the right edge of a 2560-wide display leaves the cursor at 2559.6 or so
    /// (measured). Rounded, that is 2560: off the display, so the edge never counted and the switch
    /// fired only when the cursor happened to sit below 2559.5. Every edge, both signs of origin.
    #[test]
    fn cursor_location_maps_to_the_pixel_that_holds_it() {
        assert_eq!(pixel_at(2559.6, 1439.7), Point { x: 2559, y: 1439 }, "right and bottom edges");
        assert_eq!(pixel_at(0.4, 0.0), Point { x: 0, y: 0 }, "left and top edges");
        assert_eq!(pixel_at(-0.4, -1439.6), Point { x: -1, y: -1440 }, "a display left of or above the main one");
    }

    /// Read-only table lookup; proves the binding links. Type 40 is the ANSI keyboard measured here.
    #[test]
    fn keyboard_layout_lookup_reads_ansi_for_type_40() {
        assert_eq!(unsafe { ffi::KBGetLayoutType(40) }, 0x414E_5349, "'ANSI'");
        assert_ne!(unsafe { ffi::KBGetLayoutType(40) }, ffi::kKeyboardISO);
    }

    /// Lookup only, nothing is called: a macOS that drops one of these makes the grab visible again, and
    /// this is where that shows up first.
    #[test]
    fn background_cursor_symbols_resolve_on_this_macos() {
        for s in [c"_CGSDefaultConnection", c"CGSSetConnectionProperty", c"CGSetLocalEventsSuppressionInterval"] {
            sym(s).unwrap_or_else(|e| panic!("{e}"));
        }
    }

    /// Read-only real OS call; needs no permission. Proves the FFI links and
    /// the display list is non-empty on a machine with a display attached.
    #[test]
    fn active_displays_are_reported_and_nonempty() {
        let d = active_displays().expect("CGGetActiveDisplayList failed");
        assert!(!d.is_empty(), "no active displays reported");
        assert!(d.iter().all(|r| r.w > 0 && r.h > 0));
    }

    #[test]
    fn main_display_center_lies_inside_some_display() {
        let c = main_display_center();
        let d = active_displays().unwrap();
        assert!(d.iter().any(|r| c.x >= r.x && c.x < r.x + r.w && c.y >= r.y && c.y < r.y + r.h));
    }
}

#[link(name = "AppKit", kind = "framework")]
extern "C" {}

#[link(name = "objc")]
extern "C" {
    fn objc_getClass(name: *const std::ffi::c_char) -> *mut c_void;
    fn sel_registerName(name: *const std::ffi::c_char) -> *mut c_void;
    fn objc_msgSend();
    fn objc_autoreleasePoolPush() -> *mut c_void;
    fn objc_autoreleasePoolPop(pool: *mut c_void);
}

/// `[[NSPasteboard generalPasteboard] changeCount]`. Read-only; bumps on any
/// clipboard write by any process.
pub fn pasteboard_change_count() -> Result<i64, BackendError> {
    type SendObj = extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void;
    type SendInt = extern "C" fn(*mut c_void, *mut c_void) -> isize;
    unsafe {
        let cls = objc_getClass(c"NSPasteboard".as_ptr());
        if cls.is_null() {
            return Err(BackendError::Os { code: -1, api: "objc_getClass(NSPasteboard)" });
        }
        let send_obj: SendObj = std::mem::transmute::<*const (), SendObj>(objc_msgSend as *const ());
        let pb = send_obj(cls, sel_registerName(c"generalPasteboard".as_ptr()));
        if pb.is_null() {
            return Err(BackendError::Os { code: -1, api: "generalPasteboard" });
        }
        let send_int: SendInt = std::mem::transmute::<*const (), SendInt>(objc_msgSend as *const ());
        Ok(send_int(pb, sel_registerName(c"changeCount".as_ptr())) as i64)
    }
}

const UTF8_TYPE: &std::ffi::CStr = c"public.utf8-plain-text";

struct Pool(*mut c_void);
impl Pool {
    fn new() -> Pool {
        // SAFETY: balanced with the pop in Drop; no arguments.
        Pool(unsafe { objc_autoreleasePoolPush() })
    }
}
impl Drop for Pool {
    fn drop(&mut self) {
        // SAFETY: pops the pool pushed in `new`.
        unsafe { objc_autoreleasePoolPop(self.0) }
    }
}

/// The general pasteboard object. Caller holds a `Pool` so autoreleased objects are freed.
unsafe fn general_pasteboard() -> Result<*mut c_void, String> {
    type SendObj = extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void;
    let cls = objc_getClass(c"NSPasteboard".as_ptr());
    if cls.is_null() {
        return Err("objc_getClass(NSPasteboard) failed".into());
    }
    let send: SendObj = std::mem::transmute::<*const (), SendObj>(objc_msgSend as *const ());
    let pb = send(cls, sel_registerName(c"generalPasteboard".as_ptr()));
    if pb.is_null() {
        return Err("generalPasteboard returned nil".into());
    }
    Ok(pb)
}

/// `[NSString stringWithUTF8String:]`, autoreleased.
unsafe fn ns_string(s: &std::ffi::CStr) -> Result<*mut c_void, String> {
    type SendPtr = extern "C" fn(*mut c_void, *mut c_void, *const std::ffi::c_char) -> *mut c_void;
    let cls = objc_getClass(c"NSString".as_ptr());
    let send: SendPtr = std::mem::transmute::<*const (), SendPtr>(objc_msgSend as *const ());
    let o = send(cls, sel_registerName(c"stringWithUTF8String:".as_ptr()), s.as_ptr());
    if o.is_null() {
        Err("NSString stringWithUTF8String: returned nil (invalid UTF-8)".into())
    } else {
        Ok(o)
    }
}

/// Current pasteboard text, or `None` when it holds no string.
pub fn pasteboard_read_text() -> Result<Option<String>, String> {
    type SendObj1 = extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> *mut c_void;
    type SendCStr = extern "C" fn(*mut c_void, *mut c_void) -> *const std::ffi::c_char;
    let _pool = Pool::new();
    unsafe {
        let pb = general_pasteboard()?;
        let ty = ns_string(UTF8_TYPE)?;
        let send1: SendObj1 = std::mem::transmute::<*const (), SendObj1>(objc_msgSend as *const ());
        let ns = send1(pb, sel_registerName(c"stringForType:".as_ptr()), ty);
        if ns.is_null() {
            return Ok(None);
        }
        let sc: SendCStr = std::mem::transmute::<*const (), SendCStr>(objc_msgSend as *const ());
        let p = sc(ns, sel_registerName(c"UTF8String".as_ptr()));
        if p.is_null() {
            return Err("UTF8String returned NULL".into());
        }
        Ok(Some(std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()))
    }
}

/// Replaces the pasteboard contents with `s`. A refused write is an error.
pub fn pasteboard_write_text(s: &str) -> Result<(), String> {
    type SendInt = extern "C" fn(*mut c_void, *mut c_void) -> isize;
    type SendSetStr = extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void) -> i8;
    let c = std::ffi::CString::new(s).map_err(|_| "text contains NUL".to_string())?;
    let _pool = Pool::new();
    unsafe {
        let pb = general_pasteboard()?;
        let val = ns_string(&c)?;
        let ty = ns_string(UTF8_TYPE)?;
        let clear: SendInt = std::mem::transmute::<*const (), SendInt>(objc_msgSend as *const ());
        clear(pb, sel_registerName(c"clearContents".as_ptr()));
        let set: SendSetStr = std::mem::transmute::<*const (), SendSetStr>(objc_msgSend as *const ());
        if set(pb, sel_registerName(c"setString:forType:".as_ptr()), val, ty) == 0 {
            return Err("setString:forType: returned NO".into());
        }
        Ok(())
    }
}

/// `[NSPasteboard pasteboardWithName:NSPasteboardNameDrag]`; its value is "Apple CFPasteboard drag"
/// (printed from AppKit). Every process can read it; a new drag anywhere bumps its changeCount, and the
/// last drag's contents stay on it after the drag ends (measured).
unsafe fn drag_pasteboard() -> Result<*mut c_void, String> {
    type SendObj1 = extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> *mut c_void;
    let cls = objc_getClass(c"NSPasteboard".as_ptr());
    if cls.is_null() {
        return Err("objc_getClass(NSPasteboard) failed".into());
    }
    let name = ns_string(c"Apple CFPasteboard drag")?;
    let send: SendObj1 = std::mem::transmute::<*const (), SendObj1>(objc_msgSend as *const ());
    let pb = send(cls, sel_registerName(c"pasteboardWithName:".as_ptr()), name);
    if pb.is_null() {
        return Err("pasteboardWithName: returned nil".into());
    }
    Ok(pb)
}

pub fn drag_change_count() -> Result<u64, String> {
    type SendInt = extern "C" fn(*mut c_void, *mut c_void) -> isize;
    let _pool = Pool::new();
    unsafe {
        let pb = drag_pasteboard()?;
        let send: SendInt = std::mem::transmute::<*const (), SendInt>(objc_msgSend as *const ());
        let n = send(pb, sel_registerName(c"changeCount".as_ptr()));
        u64::try_from(n).map_err(|_| format!("negative drag changeCount {n}"))
    }
}

/// File paths on the drag pasteboard. Each item's `public.file-url` goes through `filePathURL`, which
/// also resolves the file-reference form (`file:///.file/id=…`) to a path.
pub fn drag_files() -> Result<Vec<std::path::PathBuf>, String> {
    type SendObj = extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void;
    type SendObj1 = extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> *mut c_void;
    type SendIdx = extern "C" fn(*mut c_void, *mut c_void, usize) -> *mut c_void;
    type SendCount = extern "C" fn(*mut c_void, *mut c_void) -> usize;
    type SendCStr = extern "C" fn(*mut c_void, *mut c_void) -> *const std::ffi::c_char;
    let _pool = Pool::new();
    unsafe {
        let pb = drag_pasteboard()?;
        let obj: SendObj = std::mem::transmute::<*const (), SendObj>(objc_msgSend as *const ());
        let obj1: SendObj1 = std::mem::transmute::<*const (), SendObj1>(objc_msgSend as *const ());
        let idx: SendIdx = std::mem::transmute::<*const (), SendIdx>(objc_msgSend as *const ());
        let count: SendCount = std::mem::transmute::<*const (), SendCount>(objc_msgSend as *const ());
        let cstr: SendCStr = std::mem::transmute::<*const (), SendCStr>(objc_msgSend as *const ());
        let items = obj(pb, sel_registerName(c"pasteboardItems".as_ptr()));
        if items.is_null() {
            return Ok(Vec::new());
        }
        let ty = ns_string(c"public.file-url")?;
        let url_cls = objc_getClass(c"NSURL".as_ptr());
        let mut out = Vec::new();
        for i in 0..count(items, sel_registerName(c"count".as_ptr())) {
            let item = idx(items, sel_registerName(c"objectAtIndex:".as_ptr()), i);
            let s = obj1(item, sel_registerName(c"stringForType:".as_ptr()), ty);
            if s.is_null() {
                continue; // not a file: text or an image dragged along
            }
            let url = obj1(url_cls, sel_registerName(c"URLWithString:".as_ptr()), s);
            let file = if url.is_null() { url } else { obj(url, sel_registerName(c"filePathURL".as_ptr())) };
            let path = if file.is_null() { file } else { obj(file, sel_registerName(c"path".as_ptr())) };
            if path.is_null() {
                return Err(format!("drag item {i} has a file URL that does not resolve to a path"));
            }
            let p = cstr(path, sel_registerName(c"UTF8String".as_ptr()));
            if p.is_null() {
                return Err("UTF8String returned NULL".into());
            }
            out.push(std::path::PathBuf::from(std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()));
        }
        Ok(out)
    }
}

/// Bytes an unprivileged process may still write on the volume holding `p`.
pub fn free_bytes(p: &std::path::Path) -> Result<u64, String> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).map_err(|_| "path contains NUL".to_string())?;
    // SAFETY: `c` is a valid C string and `st` a correctly sized out-parameter.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return Err(format!("statvfs {}: {}", p.display(), std::io::Error::last_os_error()));
    }
    Ok(u64::from(st.f_bavail).saturating_mul(st.f_frsize as u64))
}

/// Pid of the active app, from any thread.
pub fn frontmost_pid() -> Option<i32> {
    type SendObj = extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void;
    type SendI32 = extern "C" fn(*mut c_void, *mut c_void) -> i32;
    let _pool = Pool::new();
    // SAFETY: plain message sends; every nil is checked before the next send.
    unsafe {
        let cls = objc_getClass(c"NSWorkspace".as_ptr());
        if cls.is_null() {
            return None;
        }
        let obj: SendObj = std::mem::transmute::<*const (), SendObj>(objc_msgSend as *const ());
        let ws = obj(cls, sel_registerName(c"sharedWorkspace".as_ptr()));
        let app = if ws.is_null() { ws } else { obj(ws, sel_registerName(c"frontmostApplication".as_ptr())) };
        if app.is_null() {
            return None;
        }
        let pid: SendI32 = std::mem::transmute::<*const (), SendI32>(objc_msgSend as *const ());
        Some(pid(app, sel_registerName(c"processIdentifier".as_ptr())))
    }
}

/// End a drag in progress in the active app without dropping it: Escape, posted straight to that app's
/// event queue. Routed through the HID stream instead, Escape goes to whichever app is active when it is
/// delivered, and the grab makes our own cursor-hiding window active right after this call: measured, a
/// Finder drag then stays alive, while Escape to Finder's pid ends it. Returns the pid it went to, or
/// `None` when no active app was found and it went through the HID stream.
pub fn cancel_drag_in_front() -> Result<Option<i32>, String> {
    const ESCAPE: u16 = 53;
    let pid = frontmost_pid();
    for down in [true, false] {
        // SAFETY: a null source is allowed; the event is released below.
        let ev = unsafe { ffi::CGEventCreateKeyboardEvent(std::ptr::null(), ESCAPE, down) };
        if ev.is_null() {
            return Err("CGEventCreateKeyboardEvent returned NULL".into());
        }
        mark(ev);
        // SAFETY: `ev` is a valid event we own.
        unsafe {
            match pid {
                Some(p) => ffi::CGEventPostToPid(p, ev),
                None => ffi::CGEventPost(ffi::TAP_LOCATION_HID, ev),
            }
            ffi::CFRelease(ev as *const c_void);
        }
    }
    Ok(pid)
}

/// This Mac's drags for `flowseam_net::files::DragHub`.
pub struct MacDragSource;

impl flowseam_net::files::DragSource for MacDragSource {
    fn generation(&self) -> Result<u64, String> {
        drag_change_count()
    }
    fn files(&self) -> Result<Vec<std::path::PathBuf>, String> {
        drag_files()
    }
}

/// The real clipboard for `flowseam_core::clipboard::ClipboardOs`.
pub struct MacClipboard;

impl flowseam_core::clipboard::ClipboardOs for MacClipboard {
    fn change_count(&self) -> Result<u64, String> {
        let n = pasteboard_change_count().map_err(|e| format!("{e:?}"))?;
        u64::try_from(n).map_err(|_| format!("negative changeCount {n}"))
    }
    fn read_text(&self) -> Result<Option<String>, String> {
        pasteboard_read_text()
    }
    fn write_text(&self, s: &str) -> Result<(), String> {
        pasteboard_write_text(s)
    }
}

#[cfg(test)]
mod pasteboard_tests {
    use super::*;

    /// Touches the real clipboard: run only with explicit consent. Restores the original text.
    #[test]
    #[ignore = "writes the real system clipboard; run only with owner consent"]
    fn real_pasteboard_text_round_trip() {
        let original = pasteboard_read_text().expect("read original");
        let probe = "flowseam clip probe \u{4e2d}\u{6587} \u{1f600}";
        pasteboard_write_text(probe).expect("write probe");
        let got = pasteboard_read_text();
        if let Some(o) = &original {
            pasteboard_write_text(o).expect("restore original");
        }
        assert_eq!(got.expect("read back"), Some(probe.to_string()));
    }

    /// Off the main thread, as the event tap calls it: an active app is found and it is a live process.
    #[test]
    fn frontmost_pid_names_a_live_process_from_a_background_thread() {
        let pid = std::thread::spawn(frontmost_pid).join().unwrap().expect("an active app");
        assert!(pid > 0 && unsafe { libc::kill(pid, 0) } == 0, "pid {pid} is not alive");
    }

    /// Known answer: the volume of the temp dir has some space, and a missing path is an error, not 0.
    #[test]
    fn free_bytes_measures_a_real_volume() {
        let n = free_bytes(&std::env::temp_dir()).expect("statvfs");
        assert!(n > 1 << 20, "{n}");
        assert!(free_bytes(std::path::Path::new("/no/such/flowseam/path")).is_err());
    }

    /// Reads only; the drag pasteboard holds whatever was dragged last on this Mac (maybe nothing).
    #[test]
    fn drag_pasteboard_is_readable() {
        drag_change_count().expect("drag changeCount");
        for p in drag_files().expect("drag files") {
            assert!(p.is_absolute(), "{p:?}");
        }
    }

    #[test]
    fn pasteboard_change_count_is_readable() {
        let a = pasteboard_change_count().expect("changeCount read failed");
        assert!(a >= 0);
    }
}
