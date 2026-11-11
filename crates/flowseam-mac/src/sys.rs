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

/// Subtype and data1 of a system-defined event, from its CGEvent fields. Not through NSEvent: on the tap
/// thread, `eventWithCGEvent:` of a Caps Lock event runs HIToolbox's input-source switching, which asserts
/// the main queue (SIGILL, MBP worker, 2026-11-10 01:01).
///
/// # Safety
/// `ev` must be a live CGEvent.
pub unsafe fn system_defined(ev: ffi::CGEventRef) -> (i16, i64) {
    // SAFETY: reading integer fields of a live event.
    unsafe {
        (ffi::CGEventGetIntegerValueField(ev, ffi::F_SYSTEM_SUBTYPE) as i16, ffi::CGEventGetIntegerValueField(ev, ffi::F_SYSTEM_DATA1))
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

/// Every NSPasteboard call in this file holds this. Two threads on one pasteboard at once crash in AppKit:
/// two readers of a private pasteboard died with SIGSEGV in 3 runs of 3, and lived with a lock (a Swift probe,
/// 2026-11-10). Each link's clipboard pump reads the general pasteboard on a thread of its own, and the worker
/// crashed in `pasteboard_read_text`'s pool pop (mini, 2026-11-09 16:59).
// ponytail: covers this file's callers only; AppKit's own main-thread writes (a native drag starting) are
// outside it. Locking `beginDraggingSession` too risks waiting on our own file promises; do it if a crash says so.
static PASTEBOARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn pasteboard_lock() -> std::sync::MutexGuard<'static, ()> {
    PASTEBOARD.lock().unwrap_or_else(|e| e.into_inner())
}

/// The lock and an autorelease pool, in one: fields drop in declaration order, so the pool drains (the pop
/// that crashed) before the lock is let go, and every user gets the same order.
struct PasteboardAccess {
    _pool: Pool,
    _lock: std::sync::MutexGuard<'static, ()>,
}

fn pasteboard_access() -> PasteboardAccess {
    let lock = pasteboard_lock();
    PasteboardAccess { _pool: Pool::new(), _lock: lock }
}

/// `[[NSPasteboard generalPasteboard] changeCount]`. Read-only; bumps on any
/// clipboard write by any process.
pub fn pasteboard_change_count() -> Result<i64, BackendError> {
    type SendObj = extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void;
    type SendInt = extern "C" fn(*mut c_void, *mut c_void) -> isize;
    let _pb = pasteboard_access();
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
    let _pb = pasteboard_access();
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
    let _pb = pasteboard_access();
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
    let _pb = pasteboard_access();
    unsafe {
        let pb = drag_pasteboard()?;
        let send: SendInt = std::mem::transmute::<*const (), SendInt>(objc_msgSend as *const ());
        let n = send(pb, sel_registerName(c"changeCount".as_ptr()));
        u64::try_from(n).map_err(|_| format!("negative drag changeCount {n}"))
    }
}

/// File paths on the drag pasteboard, through `readObjectsForClasses:options:`, which hands back its own array.
/// Walking `pasteboardItems` by index raised "index 1 beyond bounds for empty array" when a drag rewrote the
/// pasteboard mid-loop, and the exception aborted the worker (crash report 2026-11-05 20:17, a link thread).
/// `filePathURL` also resolves the file-reference form (`file:///.file/id=…`) to a path.
pub fn drag_files() -> Result<Vec<std::path::PathBuf>, String> {
    let _pb = pasteboard_access();
    // SAFETY: a static NSString exported by AppKit.
    let name = unsafe { objc2_app_kit::NSPasteboardNameDrag };
    // Bound, so it is released before `_pb` lets the lock go.
    let pb = objc2_app_kit::NSPasteboard::pasteboardWithName(name);
    files_on(&pb)
}

/// What `files_on` answers for a file reference URL whose file is gone (dragged, then deleted).
const UNRESOLVED: &str = "a dragged file URL does not resolve to a path";

/// The file paths on `pb`; other URLs and data are left out.
fn files_on(pb: &objc2_app_kit::NSPasteboard) -> Result<Vec<std::path::PathBuf>, String> {
    use objc2::runtime::AnyObject;
    use objc2::ClassType;
    use objc2_app_kit::NSPasteboardURLReadingFileURLsOnlyKey;
    use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSURL};
    objc2::rc::autoreleasepool(|_| {
        // SAFETY: a static NSString exported by AppKit.
        let key = unsafe { NSPasteboardURLReadingFileURLsOnlyKey };
        let classes = NSArray::from_slice(&[NSURL::class()]);
        let yes = NSNumber::new_bool(true);
        let options = NSDictionary::from_slices(&[key], &[&*yes as &AnyObject]);
        // SAFETY: NSURL is a pasteboard-readable class; the option takes an NSNumber.
        let Some(objects) = (unsafe { pb.readObjectsForClasses_options(&classes, Some(&options)) }) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for object in objects.iter() {
            let Some(url) = object.downcast_ref::<NSURL>() else { continue };
            let path = url.filePathURL().and_then(|u| u.path()).ok_or(UNRESOLVED)?;
            out.push(std::path::PathBuf::from(path.to_string()));
        }
        Ok(out)
    })
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

/// Post a media key (an NX key type: volume, mute, play, next, previous) as the system-defined event macOS acts
/// on, marked as ours. Down and up are separate events, like a real keyboard's.
pub fn post_media_key(nx: i64, down: bool) -> Result<(), BackendError> {
    use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSEventType};
    let state: i64 = if down { 0x0A } else { 0x0B };
    objc2::rc::autoreleasepool(|_| {
        let ev = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
            NSEventType::SystemDefined,
            objc2_foundation::NSPoint::new(0.0, 0.0),
            NSEventModifierFlags((state as usize) << 8),
            0.0,
            0,
            None,
            8, // NX_SUBTYPE_AUX_CONTROL_BUTTONS
            ((nx << 16) | (state << 8)) as isize,
            -1,
        )
        .ok_or(BackendError::Os { code: -1, api: "NSEvent otherEventWithType" })?;
        // SAFETY: `CGEvent` returns the event's own CGEventRef, valid while `ev` lives; it is not ours to release.
        // A plain send: objc2's checked msg_send wants the opaque `__CGEvent` struct type spelled out.
        let cg: *mut c_void = unsafe {
            type SendObj = extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void;
            let send: SendObj = std::mem::transmute::<*const (), SendObj>(objc_msgSend as *const ());
            send(objc2::rc::Retained::as_ptr(&ev) as *mut c_void, sel_registerName(c"CGEvent".as_ptr()))
        };
        if cg.is_null() {
            return Err(BackendError::Os { code: -1, api: "NSEvent CGEvent" });
        }
        mark(cg as ffi::CGEventRef);
        // SAFETY: a valid event, posted while `ev` keeps it alive.
        unsafe { ffi::CGEventPost(ffi::TAP_LOCATION_HID, cg as ffi::CGEventRef) };
        Ok(())
    })
}

/// Post a key press and release straight to one process, marked as ours.
pub fn post_key_to_pid(keycode: u16, pid: i32) -> Result<(), String> {
    for down in [true, false] {
        // SAFETY: a null source is allowed; the event is released below.
        let ev = unsafe { ffi::CGEventCreateKeyboardEvent(std::ptr::null(), keycode, down) };
        if ev.is_null() {
            return Err("CGEventCreateKeyboardEvent returned NULL".into());
        }
        mark(ev);
        // SAFETY: `ev` is a valid event we own.
        unsafe {
            ffi::CGEventPostToPid(pid, ev);
            ffi::CFRelease(ev as *const c_void);
        }
    }
    Ok(())
}

/// End a drag in progress here without dropping it: Escape, posted straight to the event queue of the app
/// that owns the drag. Routed through the HID stream instead, Escape goes to whichever app is active when it
/// is delivered, and the grab makes our own cursor-hiding window active right after this call: measured, a
/// Finder drag then stays alive, while Escape to Finder's pid ends it. The owner is the app whose window was
/// under the press, not the active app: Finder drags an icon out of a background window without becoming
/// active, and the active app then gets the Escape instead (a text view treats it as "complete"). Falls back
/// to the active app, then to the HID stream. Returns the pid it went to and which of the two it was, or
/// `None` when it went through the HID stream.
pub fn cancel_drag(pressed_at: Option<Point>) -> Result<Option<(i32, &'static str)>, String> {
    const ESCAPE: u16 = 53;
    let target = pressed_at
        .and_then(window_owner_at)
        .map(|pid| (pid, "the window it was pressed in"))
        .or_else(|| frontmost_pid().map(|pid| (pid, "the active app")));
    for down in [true, false] {
        // SAFETY: a null source is allowed; the event is released below.
        let ev = unsafe { ffi::CGEventCreateKeyboardEvent(std::ptr::null(), ESCAPE, down) };
        if ev.is_null() {
            return Err("CGEventCreateKeyboardEvent returned NULL".into());
        }
        mark(ev);
        // SAFETY: `ev` is a valid event we own.
        unsafe {
            match target {
                Some((pid, _)) => ffi::CGEventPostToPid(pid, ev),
                None => ffi::CGEventPost(ffi::TAP_LOCATION_HID, ev),
            }
            ffi::CFRelease(ev as *const c_void);
        }
    }
    Ok(target)
}

/// Owner of the frontmost window a mouse-down at this CoreGraphics point would hit.
pub fn window_owner_at(p: Point) -> Option<i32> {
    type WindowAt = unsafe extern "C" fn(*mut c_void, *mut c_void, CGPoint, isize) -> isize;
    // SAFETY: both CoreGraphics display queries take no owned pointers.
    let bounds = unsafe { ffi::CGDisplayBounds(ffi::CGMainDisplayID()) };
    let cocoa = CGPoint { x: p.x as f64, y: bounds.size.height - p.y as f64 };
    let _pool = Pool::new();
    // SAFETY: the class method accepts an NSPoint (same layout as CGPoint) and NSInteger;
    // the window list is owned here and released after reading its borrowed values.
    unsafe {
        let class = ffi::objc_getClass(c"NSWindow".as_ptr());
        if class.is_null() { return None; }
        let send: WindowAt = std::mem::transmute::<*const (), WindowAt>(ffi::objc_msgSend as *const ());
        let wid = send(class, ffi::sel_registerName(c"windowNumberAtPoint:belowWindowWithWindowNumber:".as_ptr()), cocoa, 0);
        if wid <= 0 { return None; }
        let list = ffi::CGWindowListCopyWindowInfo(1 << 3, wid as u32);
        if list.is_null() { return None; }
        let result = if ffi::CFArrayGetCount(list) > 0 {
            let dict = ffi::CFArrayGetValueAtIndex(list, 0);
            if dict.is_null() { None } else {
                let number = ffi::CFDictionaryGetValue(dict, ffi::kCGWindowOwnerPID);
                let mut pid = 0i32;
                if !number.is_null() && ffi::CFNumberGetValue(number, 3, (&mut pid as *mut i32).cast()) && pid > 0 { Some(pid) } else { None }
            }
        } else { None };
        ffi::CFRelease(list);
        result
    }
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
mod system_defined_tests {
    use super::*;

    /// A system-defined event as NSEvent builds it, and what `system_defined` reads off its CGEvent.
    fn read_back(subtype: i16, data1: i64) -> (i16, i64) {
        use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSEventType};
        objc2::rc::autoreleasepool(|_| {
            let ev = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
                NSEventType::SystemDefined, objc2_foundation::NSPoint::new(0.0, 0.0), NSEventModifierFlags(0), 0.0, 0, None,
                subtype, data1 as isize, -1,
            )
            .expect("NSEvent");
            // SAFETY: as in `post_media_key`: the event's own CGEventRef, alive while `ev` is.
            let cg = unsafe {
                type SendObj = extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void;
                let send: SendObj = std::mem::transmute::<*const (), SendObj>(objc_msgSend as *const ());
                send(objc2::rc::Retained::as_ptr(&ev) as *mut c_void, sel_registerName(c"CGEvent".as_ptr()))
            } as ffi::CGEventRef;
            assert!(!cg.is_null());
            // SAFETY: a live event.
            unsafe { system_defined(cg) }
        })
    }

    /// The fields are undocumented: this fails if a macOS moves them, rather than media keys going quiet.
    #[test]
    fn subtype_and_data1_read_as_nsevent_wrote_them() {
        for (subtype, data1) in [(8, 0x0010_0A00), (8, 0x0011_0B00), (7, 0x0000_0A00), (8, 0x0004_0A00)] {
            assert_eq!(read_back(subtype, data1), (subtype, data1));
        }
        // Read back, a play press is the media key it was, and Caps Lock's aux event (type 4) is none.
        let (st, d1) = read_back(8, 0x0010_0A00);
        assert_eq!(crate::keymap::aux_key(st, d1), Some((crate::keymap::HID_PLAY_PAUSE, true)));
        let (st, d1) = read_back(8, 0x0004_0A00);
        assert_eq!(crate::keymap::aux_key(st, d1), None);
    }
}

#[cfg(test)]
mod pasteboard_tests {
    use super::*;

    /// Each reader waits for the pasteboard lock: held here, none of them finishes until it is let go. Only
    /// reads, so the clipboard is left as it was; `pasteboard_write_text` takes the lock the same way.
    #[test]
    fn every_pasteboard_read_waits_for_the_lock() {
        type Reader = (&'static str, fn() -> bool);
        let readers: [Reader; 4] = [
            ("change count", || pasteboard_change_count().is_ok()),
            ("text", || pasteboard_read_text().is_ok()),
            ("drag change count", || drag_change_count().is_ok()),
            ("drag files", || { let _ = drag_files(); true }),
        ];
        for (name, read) in readers {
            let held = pasteboard_lock();
            let (tx, rx) = std::sync::mpsc::channel();
            let reader = std::thread::spawn(move || tx.send(read()).unwrap());
            assert!(rx.recv_timeout(std::time::Duration::from_millis(300)).is_err(), "{name} read the pasteboard while it was locked");
            drop(held);
            assert!(rx.recv_timeout(std::time::Duration::from_secs(5)).expect("finishes once unlocked"), "{name}");
            reader.join().unwrap();
        }
    }

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

    #[test]
    fn window_owner_at_cursor_is_live_from_a_background_thread() {
        let pid = std::thread::spawn(|| cursor_position().ok().and_then(window_owner_at)).join().unwrap();
        if let Some(pid) = pid {
            // SAFETY: signal zero only probes whether the process exists.
            assert!(unsafe { libc::kill(pid, 0) } == 0, "pid {pid} is not alive");
        }
    }

    #[test]
    fn window_owner_at_far_outside_displays_is_none() {
        assert_eq!(window_owner_at(Point { x: -100000, y: -100000 }), None);
    }

    /// Known answer: the volume of the temp dir has some space, and a missing path is an error, not 0.
    #[test]
    fn free_bytes_measures_a_real_volume() {
        let n = free_bytes(&std::env::temp_dir()).expect("statvfs");
        assert!(n > 1 << 20, "{n}");
        assert!(free_bytes(std::path::Path::new("/no/such/flowseam/path")).is_err());
    }

    /// Known answer on a private pasteboard: the file URLs come back as their paths, in order; a web URL is not a
    /// file and is left out.
    #[test]
    fn files_on_a_pasteboard_are_its_file_urls_as_paths() {
        use objc2::runtime::ProtocolObject;
        use objc2_app_kit::{NSPasteboard, NSPasteboardWriting};
        use objc2_foundation::{NSArray, NSString, NSURL};
        let pb = NSPasteboard::pasteboardWithUniqueName();
        pb.clearContents();
        let a = NSURL::fileURLWithPath(&NSString::from_str("/tmp/flowseam a.txt"));
        let web = NSURL::URLWithString(&NSString::from_str("https://example.com/b")).unwrap();
        let c = NSURL::fileURLWithPath(&NSString::from_str("/tmp/中文 c.pdf"));
        let objects: Vec<&ProtocolObject<dyn NSPasteboardWriting>> = [&a, &web, &c].iter().map(|u| ProtocolObject::from_ref(&***u)).collect();
        assert!(pb.writeObjects(&NSArray::from_slice(&objects)));
        let got = files_on(&pb).expect("read");
        assert_eq!(got, vec![std::path::PathBuf::from("/tmp/flowseam a.txt"), std::path::PathBuf::from("/tmp/中文 c.pdf")]);
    }

    /// Known answer: Finder puts file reference URLs (file:///.file/id=…) on the drag pasteboard, and one whose file
    /// has since been deleted no longer resolves. That is an error, not an empty list: the file cannot be sent.
    #[test]
    fn a_dangling_file_reference_is_an_error() {
        use objc2::runtime::ProtocolObject;
        use objc2_app_kit::{NSPasteboard, NSPasteboardWriting};
        use objc2_foundation::{NSArray, NSString, NSURL};
        let path = std::env::temp_dir().join(format!("flowseam-dangling-{}.txt", std::process::id()));
        std::fs::write(&path, b"x").unwrap();
        let reference = NSURL::fileURLWithPath(&NSString::from_str(path.to_str().unwrap())).fileReferenceURL().expect("a reference to a file that exists");
        let pb = NSPasteboard::pasteboardWithUniqueName();
        pb.clearContents();
        let objects: Vec<&ProtocolObject<dyn NSPasteboardWriting>> = vec![ProtocolObject::from_ref(&*reference)];
        assert!(pb.writeObjects(&NSArray::from_slice(&objects)));
        assert_eq!(files_on(&pb).expect("resolves while the file exists"), vec![path.canonicalize().unwrap()]);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(files_on(&pb), Err(UNRESOLVED.to_string()));
    }

    /// Known answer on the real machine: a posted volume-up raises the output volume, a volume-down restores it.
    /// Changes the volume for a moment, so it runs only when asked for.
    #[test]
    #[ignore = "changes this Mac's output volume briefly"]
    fn a_posted_volume_key_changes_the_volume() {
        let volume = || {
            let out = std::process::Command::new("osascript").args(["-e", "output volume of (get volume settings)"]).output().unwrap();
            String::from_utf8_lossy(&out.stdout).trim().parse::<i32>().expect("a volume")
        };
        let before = volume();
        let (up, down) = if before >= 94 { (1, 0) } else { (0, 1) }; // room to move in the direction tried first
        for (nx, d) in [(up, true), (up, false)] {
            post_media_key(nx, d).unwrap();
        }
        std::thread::sleep(std::time::Duration::from_millis(400));
        let moved = volume();
        for (nx, d) in [(down, true), (down, false)] {
            post_media_key(nx, d).unwrap();
        }
        std::thread::sleep(std::time::Duration::from_millis(400));
        assert_ne!(moved, before, "the volume did not move ({before})");
        assert!((volume() - before).abs() <= 7, "restored near {before}");
    }

    /// Reads only; the drag pasteboard holds whatever was dragged last on this Mac: maybe nothing, maybe a file
    /// deleted since (`a_dangling_file_reference_is_an_error` pins what that reads as).
    #[test]
    fn drag_pasteboard_is_readable() {
        drag_change_count().expect("drag changeCount");
        match drag_files() {
            Ok(paths) => {
                for p in paths {
                    assert!(p.is_absolute(), "{p:?}");
                }
            }
            Err(e) => assert_eq!(e, UNRESOLVED, "drag files"),
        }
    }

    #[test]
    fn pasteboard_change_count_is_readable() {
        let a = pasteboard_change_count().expect("changeCount read failed");
        assert!(a >= 0);
    }
}
