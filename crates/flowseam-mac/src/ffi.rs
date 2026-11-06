//! Raw CoreGraphics / CoreFoundation / ApplicationServices / Carbon bindings.
//! Only `sys.rs` and `tap.rs` call these. core-graphics 0.19 has no event-tap
//! wrapper, so the tap is bound directly here.

#![allow(non_upper_case_globals)]

use std::ffi::c_void;

pub type CGError = i32;
pub type CGDirectDisplayID = u32;
pub type CGEventRef = *mut c_void;
pub type CGEventSourceRef = *const c_void;
pub type CFMachPortRef = *mut c_void;
pub type CFRunLoopSourceRef = *mut c_void;
pub type CFTypeRef = *const c_void;
#[repr(C)]
pub struct CFRunLoopTimerContext {
    pub version: isize,
    pub info: *mut c_void,
    pub retain: Option<extern "C" fn(*const c_void) -> *const c_void>,
    pub release: Option<extern "C" fn(*const c_void)>,
    pub copy_description: Option<extern "C" fn(*const c_void) -> *const c_void>,
}
pub type CGEventTapCallBack = extern "C" fn(*mut c_void, u32, CGEventRef, *mut c_void) -> CGEventRef;
pub type CGDisplayReconfigurationCallBack = extern "C" fn(CGDirectDisplayID, u32, *mut c_void);

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CGPoint {
    pub x: f64,
    pub y: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CGSize {
    pub width: f64,
    pub height: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CGRect {
    pub origin: CGPoint,
    pub size: CGSize,
}

// Event types (CGEventType).
pub const EV_LEFT_DOWN: u32 = 1;
pub const EV_LEFT_UP: u32 = 2;
pub const EV_RIGHT_DOWN: u32 = 3;
pub const EV_RIGHT_UP: u32 = 4;
pub const EV_MOUSE_MOVED: u32 = 5;
pub const EV_LEFT_DRAGGED: u32 = 6;
pub const EV_RIGHT_DRAGGED: u32 = 7;
pub const EV_KEY_DOWN: u32 = 10;
pub const EV_KEY_UP: u32 = 11;
pub const EV_FLAGS_CHANGED: u32 = 12;
/// NX_SYSDEFINED: media and brightness keys, among others.
pub const EV_SYSTEM_DEFINED: u32 = 14;
pub const EV_SCROLL_WHEEL: u32 = 22;
pub const EV_OTHER_DOWN: u32 = 25;
pub const EV_OTHER_UP: u32 = 26;
pub const EV_OTHER_DRAGGED: u32 = 27;
/// Delivered to the tap callback out of band, never masked.
pub const EV_TAP_DISABLED_TIMEOUT: u32 = 0xFFFF_FFFE;
pub const EV_TAP_DISABLED_USER_INPUT: u32 = 0xFFFF_FFFF;

// Event fields (CGEventField).
pub const F_MOUSE_BUTTON_NUMBER: u32 = 3;
pub const F_MOUSE_DELTA_X: u32 = 4;
pub const F_MOUSE_DELTA_Y: u32 = 5;
pub const F_KEYBOARD_AUTOREPEAT: u32 = 8;
pub const F_KEYBOARD_KEYCODE: u32 = 9;
pub const F_KEYBOARD_TYPE: u32 = 10;
pub const F_SCROLL_DELTA_AXIS1: u32 = 11;
pub const F_SCROLL_DELTA_AXIS2: u32 = 12;

// Tap placement and options.
pub const TAP_LOCATION_HID: u32 = 0;
pub const TAP_PLACE_HEAD_INSERT: u32 = 0;
pub const TAP_OPTION_DEFAULT: u32 = 0; // active filter: may swallow events
pub const SCROLL_UNIT_LINE: u32 = 1;
pub const CG_DISPLAY_BEGIN_CONFIG: u32 = 1;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    pub fn AXIsProcessTrusted() -> bool;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    pub fn CGPreflightListenEventAccess() -> bool;
    pub fn CGPreflightPostEventAccess() -> bool;
    pub fn CGMainDisplayID() -> CGDirectDisplayID;
    pub fn CGGetActiveDisplayList(max: u32, displays: *mut CGDirectDisplayID, count: *mut u32) -> CGError;
    pub fn CGDisplayBounds(display: CGDirectDisplayID) -> CGRect;
    pub fn CGWindowListCopyWindowInfo(option: u32, relative_to_window: u32) -> CFTypeRef;
    pub static kCGWindowOwnerPID: CFTypeRef;
    pub fn CGDisplayHideCursor(display: CGDirectDisplayID) -> CGError;
    pub fn CGDisplayShowCursor(display: CGDirectDisplayID) -> CGError;
    pub fn CGAssociateMouseAndMouseCursorPosition(connected: bool) -> CGError;
    pub fn CGWarpMouseCursorPosition(p: CGPoint) -> CGError;
    pub fn CGDisplayRegisterReconfigurationCallback(cb: CGDisplayReconfigurationCallBack, user: *mut c_void) -> CGError;
    pub fn CGDisplayRemoveReconfigurationCallback(cb: CGDisplayReconfigurationCallBack, user: *mut c_void) -> CGError;
    pub fn CGEventCreate(source: CGEventSourceRef) -> CGEventRef;
    pub fn CGEventGetLocation(event: CGEventRef) -> CGPoint;
    pub fn CGEventSetType(event: CGEventRef, ty: u32);
    pub fn CGEventGetFlags(event: CGEventRef) -> u64;
    pub fn CGEventSetFlags(event: CGEventRef, flags: u64);
    pub fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
    pub fn CGEventSetIntegerValueField(event: CGEventRef, field: u32, value: i64);
    pub fn CGEventCreateMouseEvent(source: CGEventSourceRef, ty: u32, pos: CGPoint, button: u32) -> CGEventRef;
    pub fn CGEventCreateKeyboardEvent(source: CGEventSourceRef, keycode: u16, down: bool) -> CGEventRef;
    pub fn CGEventCreateScrollWheelEvent2(
        source: CGEventSourceRef,
        units: u32,
        wheel_count: u32,
        wheel1: i32,
        wheel2: i32,
        wheel3: i32,
    ) -> CGEventRef;
    pub fn CGEventPost(tap: u32, event: CGEventRef);
    pub fn CGEventPostToPid(pid: i32, event: CGEventRef);
    pub fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: CGEventTapCallBack,
        user_info: *mut c_void,
    ) -> CFMachPortRef;
    pub fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    pub fn CFRelease(cf: CFTypeRef);
    pub fn CFArrayGetCount(array: CFTypeRef) -> isize;
    pub fn CFArrayGetValueAtIndex(array: CFTypeRef, index: isize) -> CFTypeRef;
    pub fn CFDictionaryGetValue(dict: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
    pub fn CFNumberGetValue(number: CFTypeRef, the_type: i32, value: *mut c_void) -> bool;
    pub fn CFMachPortCreateRunLoopSource(alloc: *const c_void, port: CFMachPortRef, order: isize) -> CFRunLoopSourceRef;
    pub fn CFMachPortInvalidate(port: CFMachPortRef);
    pub fn CFRunLoopGetCurrent() -> *mut c_void;
    pub fn CFRunLoopAddSource(rl: *mut c_void, source: CFRunLoopSourceRef, mode: CFTypeRef);
    pub fn CFRunLoopTimerCreate(allocator: *const c_void, fire_date: f64, interval: f64, flags: usize, order: isize, callout: extern "C" fn(*mut c_void, *mut c_void), context: *mut CFRunLoopTimerContext) -> *mut c_void;
    pub fn CFRunLoopAddTimer(rl: *mut c_void, timer: *mut c_void, mode: CFTypeRef);
    pub fn CFRunLoopTimerInvalidate(timer: *mut c_void);
    pub fn CFAbsoluteTimeGetCurrent() -> f64;
    #[cfg(test)]
    pub fn CFRunLoopAddCommonMode(rl: *mut c_void, mode: CFTypeRef);
    pub fn CFRunLoopRunInMode(mode: CFTypeRef, seconds: f64, return_after_source: bool) -> i32;
    pub static kCFRunLoopDefaultMode: CFTypeRef;
    pub static kCFRunLoopCommonModes: CFTypeRef;
    pub fn CFStringCreateWithCString(alloc: *const c_void, cstr: *const std::ffi::c_char, encoding: u32) -> CFTypeRef;
    pub static kCFBooleanTrue: CFTypeRef;
}

pub const kCFStringEncodingUTF8: u32 = 0x0800_0100;

// libSystem: looked up at run time for private or deprecated symbols, so a macOS that drops one reports
// it instead of failing to link or crashing.
extern "C" {
    pub fn dlsym(handle: *mut c_void, symbol: *const std::ffi::c_char) -> *mut c_void;
}
pub const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;

#[link(name = "Carbon", kind = "framework")]
extern "C" {
    pub fn IsSecureEventInputEnabled() -> bool;
    /// ANSI, ISO or JIS for a keyboard type (`kCGKeyboardEventKeyboardType`).
    pub fn KBGetLayoutType(keyboard_type: i16) -> u32;
    /// Keyboard type of the keyboard used last.
    pub fn LMGetKbdType() -> u8;
}
/// `kKeyboardISO`: 'ISO '.
pub const kKeyboardISO: u32 = 0x4953_4F20;

/// `AEEventHandlerProcPtr`: (event, reply, refcon) -> OSErr.
pub type AeHandler = extern "C" fn(*const c_void, *mut c_void, *mut c_void) -> i16;
#[link(name = "CoreServices", kind = "framework")]
extern "C" {
    /// Replaces the process's handler for one Apple event, including one AppKit installed.
    pub fn AEInstallEventHandler(class: u32, id: u32, handler: AeHandler, refcon: *mut c_void, is_sys_handler: u8) -> i16;
}
/// `kCoreEventClass`: 'aevt'.
pub const kCoreEventClass: u32 = 0x6165_7674;
/// `kAEQuitApplication`: 'quit'.
pub const kAEQuitApplication: u32 = 0x7175_6974;

extern "C" {
    /// libc `kill(pid, 0)`: existence probe, sends no signal.
    pub fn kill(pid: i32, sig: i32) -> i32;
    /// libc `signal(sig, handler)`. Handler is a function pointer passed as `usize`; returns the previous one.
    pub fn signal(sig: i32, handler: usize) -> usize;
}

// Objective-C runtime, for the one thing CoreGraphics does not expose: the subtype and data1 of a
// system-defined event (media keys). NSEvent lives in AppKit.
#[link(name = "AppKit", kind = "framework")]
extern "C" {}
#[link(name = "objc")]
extern "C" {
    pub fn objc_getClass(name: *const std::ffi::c_char) -> *mut c_void;
    pub fn sel_registerName(name: *const std::ffi::c_char) -> *mut c_void;
    /// Untyped: cast to the signature of the method being sent before calling.
    pub fn objc_msgSend();
    pub fn objc_autoreleasePoolPush() -> *mut c_void;
    pub fn objc_autoreleasePoolPop(pool: *mut c_void);
}
