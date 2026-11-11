//! Active CGEventTap on a dedicated thread (design §4–§5, §7).
//!
//! The tap callback must return in microseconds (design §4), so it only
//! translates the event and forwards it to the sink. Thread lifetime is kept
//! by a signaled run-loop source, so stop before the next wait is retained.

use crate::ffi::{self, CGEventRef};
use crate::gesture::{Fields as GestureFields, GestureReader};
use crate::grab::GrabGuard;
use crate::inject_mark;
use crate::keymap::{aux_key, by_position, keycode_to_hid, modifier_down, KC_CAPS_LOCK, KC_FN, KC_GRAVE, KC_ISO_SECTION};
use crate::state::{TapVerdict, TimeoutBreaker};
use crate::switch_gate::{SwitchEnv, TapConfig};
use crate::sys;
use flowseam_core::backend::{BackendError, CaptureFault, CaptureSink, CapturedEvent, Disposition};
use flowseam_core::emergency::is_emergency;
use flowseam_core::modifiers::KeyTracker;
use flowseam_core::proto::Point;
use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::Instant;

const EVENT_MASK: u64 = mask(ffi::EV_KEY_DOWN)
    | mask(ffi::EV_KEY_UP)
    | mask(ffi::EV_FLAGS_CHANGED)
    | mask(ffi::EV_LEFT_DOWN)
    | mask(ffi::EV_LEFT_UP)
    | mask(ffi::EV_RIGHT_DOWN)
    | mask(ffi::EV_RIGHT_UP)
    | mask(ffi::EV_OTHER_DOWN)
    | mask(ffi::EV_OTHER_UP)
    | mask(ffi::EV_MOUSE_MOVED)
    | mask(ffi::EV_LEFT_DRAGGED)
    | mask(ffi::EV_RIGHT_DRAGGED)
    | mask(ffi::EV_OTHER_DRAGGED)
    | mask(ffi::EV_SCROLL_WHEEL)
    | mask(ffi::EV_SYSTEM_DEFINED)
    // Every gesture type, so while the peer is driven none of them also acts on this Mac.
    | mask(ffi::EV_ROTATE)
    | mask(ffi::EV_BEGIN_GESTURE)
    | mask(ffi::EV_END_GESTURE)
    | mask(ffi::EV_GESTURE)
    | mask(ffi::EV_DOCK_CONTROL)
    | mask(ffi::EV_SWIPE)
    | mask(ffi::EV_SMART_MAGNIFY);

const fn mask(ty: u32) -> u64 {
    1u64 << ty
}

/// Everything the tap thread touches per event. Lives on the tap thread only, so no lock.
pub struct TapState {
    pub sink: Box<dyn CaptureSink>,
    pub guard: GrabGuard,
    pub keys: KeyTracker,
    pub env: Box<dyn SwitchEnv>,
}

/// Production clock and cursor.
struct SysSwitchEnv {
    started: Instant,
}

impl SwitchEnv for SysSwitchEnv {
    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }
    fn cursor(&self) -> Option<Point> {
        sys::cursor_position().ok()
    }
}

/// Process one callback's captured events on the tap thread (design §7, emergency chord).
///
/// The chord is checked here, before anything reaches the sink, so it works with the network down.
/// On the chord: restore the cursor first, report `Emergency`, swallow the Esc, and halt.
/// Esc-up is not the chord, so it passes through as ordinary input.
pub fn handle(state: &mut TapState, events: Vec<CapturedEvent>) -> Disposition {
    // Local input passes through and nothing reaches the sink once halted. The flag lives in the
    // shared guard so the backend's grab and hide paths see it too (M1).
    if state.guard.is_halted() {
        return Disposition::Pass;
    }
    let mut swallow = false;
    for ev in events {
        if let CapturedEvent::Key { hid, down } = ev {
            if is_emergency(state.keys.mods(), hid, down) {
                // Cursor first: a sink that panics, or waits on a lock a stuck link thread holds, must not
                // stand between the user and a visible cursor. Halted first so nothing can hide it again.
                state.guard.halt();
                if let Err(e) = state.guard.restore() {
                    eprintln!("flowseam: restore on the emergency chord failed: {e:?}; cursor state is unknown");
                }
                // Then the sink hands any remote session back and hears why.
                state.sink.on_emergency();
                state.sink.on_tap_fault(CaptureFault::Emergency);
                return Disposition::Swallow;
            }
            let _ = state.keys.set_key(hid, down);
        } else {
            sample_cursor(state);
        }
        if trace() && state.guard.is_grabbed() {
            if let CapturedEvent::Key { hid, down } = ev {
                eprintln!("flowseam trace: key hid={hid:#04x} down={down}");
            }
        }
        // The sink (the forwarder) alone decides: Local never swallows, so nothing is taken before a switch.
        if state.sink.on_event(ev) == Disposition::Swallow {
            swallow = true;
        }
    }
    // While the peer is driven nothing reaches local apps, including events with no mapping (a key the
    // table cannot express produces no captured event at all): they would type into the local window.
    if swallow || state.guard.is_grabbed() { Disposition::Swallow } else { Disposition::Pass }
}

/// `FLOWSEAM_TRACE_INPUT=1` logs every forwarded key and motion.
fn trace() -> bool {
    static TRACE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *TRACE.get_or_init(|| std::env::var_os("FLOWSEAM_TRACE_INPUT").is_some())
}

/// `FLOWSEAM_TRACE_GESTURES=1`, or a file `~/.config/flowseam/trace-gestures` when the worker starts, logs
/// every gesture event's fields and what they read as. Unlike `FLOWSEAM_TRACE_INPUT` it logs no keys. The
/// file is there because a launchd agent's environment cannot be changed from an ssh login.
fn trace_gestures() -> bool {
    static TRACE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *TRACE.get_or_init(|| {
        std::env::var_os("FLOWSEAM_TRACE_GESTURES").is_some()
            || std::env::var_os("HOME")
                .is_some_and(|h| std::path::Path::new(&h).join(".config/flowseam/trace-gestures").exists())
    })
}

/// While grabbed, replace each motion's delta with what the guard forwards (see
/// `GrabGuard::grabbed_motion`).
pub fn correct_motion(guard: &GrabGuard, events: &mut [CapturedEvent], loc: impl Fn() -> (f64, f64)) {
    for ev in events.iter_mut() {
        if let CapturedEvent::Motion { dx, dy } = ev {
            if let Some((x, y)) = guard.grabbed_motion((*dx, *dy)) {
                if trace() {
                    let l = loc();
                    eprintln!("flowseam trace: loc={:.2},{:.2} delta={},{} sent={x},{y}", l.0, l.1, *dx, *dy);
                }
                (*dx, *dy) = (x, y);
            }
        }
    }
}

fn sample_cursor(state: &mut TapState) {
    if let Some(pos) = state.env.cursor() {
        state.sink.on_cursor(pos);
    }
}

struct TapCtx {
    state: TapState,
    port: ffi::CFMachPortRef,
    breaker: TimeoutBreaker,
    started: Instant,
    stop: Arc<AtomicBool>,
    gestures: GestureReader,
}

pub struct TapHandle {
    thread: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    stop_source: usize,
    run_loop: usize,
}

extern "C" fn stop_source_perform(_info: *mut c_void) {}

fn new_stop_source() -> ffi::CFRunLoopSourceRef {
    let mut context = ffi::CFRunLoopSourceContext {
        version: 0,
        info: ptr::null_mut(),
        retain: None,
        release: None,
        copy_description: None,
        equal: None,
        hash: None,
        schedule: None,
        cancel: None,
        perform: Some(stop_source_perform),
    };
    unsafe { ffi::CFRunLoopSourceCreate(ptr::null(), 0, &mut context) }
}

fn wake_stop_source(source: usize, run_loop: *mut c_void) {
    // A source keeps its signal across the check-to-wait race.
    unsafe {
        ffi::CFRunLoopSourceSignal(source as ffi::CFRunLoopSourceRef);
        ffi::CFRunLoopWakeUp(run_loop);
    }
}

/// Install the tap on its own thread. Returns only after the tap exists, so a
/// failed creation surfaces here instead of later.
pub fn start(sink: Box<dyn CaptureSink>, guard: GrabGuard, cfg: TapConfig) -> Result<TapHandle, BackendError> {
    // A fresh tap re-arms: the emergency halt only lasts until the next start.
    guard.clear_halt();
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let thread_stop = stop.clone();
    let handle = thread::Builder::new()
        .name("flowseam-mac-tap".into())
        .spawn(move || run_tap(sink, guard, cfg, tx, thread_stop))
        .map_err(|_| BackendError::TapCreateFailed)?;
    match rx.recv() {
        Ok(Ok((source, run_loop))) => Ok(TapHandle { thread: Some(handle), stop, stop_source: source, run_loop }),
        Ok(Err(e)) => {
            let _ = handle.join();
            Err(e)
        }
        Err(_) => {
            let _ = handle.join();
            Err(BackendError::TapCreateFailed)
        }
    }
}

impl TapHandle {
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if self.stop_source != 0 {
            wake_stop_source(self.stop_source, self.run_loop as *mut c_void);
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if self.stop_source != 0 {
            unsafe { ffi::CFRelease(self.stop_source as ffi::CFTypeRef) };
            self.stop_source = 0;
        }
    }
}

fn run_tap(
    sink: Box<dyn CaptureSink>,
    guard: GrabGuard,
    _cfg: TapConfig,
    ready: mpsc::Sender<Result<(usize, usize), BackendError>>,
    stop: Arc<AtomicBool>,
) {
    let ctx = Box::into_raw(Box::new(TapCtx {
        state: TapState {
            sink,
            guard,
            keys: KeyTracker::new(),
            env: Box::new(SysSwitchEnv { started: Instant::now() }),
        },
        port: ptr::null_mut(),
        breaker: TimeoutBreaker::default(),
        started: Instant::now(),
        stop: stop.clone(),
        gestures: GestureReader::new(macos_major()),
    }));
    let port = unsafe {
        ffi::CGEventTapCreate(
            ffi::TAP_LOCATION_HID,
            ffi::TAP_PLACE_HEAD_INSERT,
            ffi::TAP_OPTION_DEFAULT,
            EVENT_MASK,
            tap_callback,
            ctx as *mut c_void,
        )
    };
    if port.is_null() {
        let _ = ready.send(Err(BackendError::TapCreateFailed));
        drop(unsafe { Box::from_raw(ctx) });
        return;
    }
    unsafe {
        (*ctx).port = port;
        let run_loop = ffi::CFRunLoopGetCurrent();
        let source = ffi::CFMachPortCreateRunLoopSource(ptr::null(), port, 0);
        ffi::CFRunLoopAddSource(run_loop, source, ffi::kCFRunLoopCommonModes);
        ffi::CFRelease(source as ffi::CFTypeRef);
    }
    let stop_source = new_stop_source();
    if stop_source.is_null() {
        let _ = ready.send(Err(BackendError::TapCreateFailed));
        unsafe {
            ffi::CGEventTapEnable(port, false);
            ffi::CFMachPortInvalidate(port);
            ffi::CFRelease(port as ffi::CFTypeRef);
            drop(Box::from_raw(ctx));
        }
        return;
    }
    let run_loop = unsafe { ffi::CFRunLoopGetCurrent() };
    unsafe { ffi::CFRunLoopAddSource(run_loop, stop_source, ffi::kCFRunLoopDefaultMode) };
    if ready.send(Ok((stop_source as usize, run_loop as usize))).is_err() {
        unsafe { ffi::CFRelease(stop_source as ffi::CFTypeRef) };
    }
    while !stop.load(Ordering::SeqCst) {
        unsafe { ffi::CFRunLoopRunInMode(ffi::kCFRunLoopDefaultMode, 10.0, true) };
    }
    unsafe {
        ffi::CFRunLoopRemoveSource(run_loop, stop_source, ffi::kCFRunLoopDefaultMode);
        ffi::CGEventTapEnable(port, false);
        ffi::CFMachPortInvalidate(port);
        ffi::CFRelease(port as ffi::CFTypeRef);
        drop(Box::from_raw(ctx));
    }
}

extern "C" fn tap_callback(_proxy: *mut c_void, ty: u32, ev: CGEventRef, user: *mut c_void) -> CGEventRef {
    // SAFETY: `user` is the TapCtx leaked in `run_tap`, alive until the run loop exits.
    let ctx = unsafe { &mut *(user as *mut TapCtx) };
    match ty {
        ffi::EV_TAP_DISABLED_TIMEOUT => {
            match ctx.breaker.record(ctx.started.elapsed().as_millis() as u64) {
                TapVerdict::Reenable => unsafe { ffi::CGEventTapEnable(ctx.port, true) },
                TapVerdict::Tear => {
                    on_fault(&ctx.state.guard, &mut *ctx.state.sink, CaptureFault::TapDisabledByTimeout);
                    ctx.stop.store(true, Ordering::SeqCst);
                }
            }
            return ev;
        }
        ffi::EV_TAP_DISABLED_USER_INPUT => {
            // Secure Input or a revoked permission: the tap cannot be re-enabled.
            on_fault(&ctx.state.guard, &mut *ctx.state.sink, CaptureFault::TapDisabledByUserInput);
            ctx.stop.store(true, Ordering::SeqCst);
            return ev;
        }
        _ => {}
    }
    // Our own injected input is neither captured nor swallowed.
    if inject_mark::is_ours(unsafe { ffi::CGEventGetIntegerValueField(ev, inject_mark::FIELD) }) {
        return ev;
    }
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        let mut captured = if is_gesture(ty) {
            // Read while the pointer is home too, so a swipe that started here ends as one gesture.
            let fields = unsafe { gesture_fields(ty, ev) };
            let read = ctx.gestures.feed(&fields);
            if trace_gestures() {
                eprintln!("flowseam trace: gesture event {fields:?} -> {read:?}");
            }
            read.map(CapturedEvent::Gesture).into_iter().collect()
        } else {
            unsafe { translate(ty, ev) }
        };
        if ty == ffi::EV_SYSTEM_DEFINED && captured.is_empty() {
            // Brightness, mouse-button bookkeeping and the like: not forwarded, so they stay local.
            return false;
        }
        correct_motion(&ctx.state.guard, &mut captured, || {
            let p = unsafe { ffi::CGEventGetLocation(ev) };
            (p.x, p.y)
        });
        handle(&mut ctx.state, captured) == Disposition::Swallow
    }));
    match outcome {
        Ok(true) => ptr::null_mut(),
        Ok(false) => ev,
        Err(_) => {
            // A panic must not unwind across the FFI boundary. Pass the event
            // through so local input keeps working; the panic is still shown.
            eprintln!("flowseam: capture sink panicked; event passed through");
            ev
        }
    }
}

fn is_gesture(ty: u32) -> bool {
    matches!(
        ty,
        ffi::EV_ROTATE | ffi::EV_BEGIN_GESTURE | ffi::EV_END_GESTURE | ffi::EV_GESTURE | ffi::EV_DOCK_CONTROL
            | ffi::EV_SWIPE | ffi::EV_SMART_MAGNIFY
    )
}

unsafe fn gesture_fields(ty: u32, ev: CGEventRef) -> GestureFields {
    let int = |f: u32| ffi::CGEventGetIntegerValueField(ev, f);
    let dbl = |f: u32| ffi::CGEventGetDoubleValueField(ev, f);
    GestureFields {
        ty,
        hid_type: int(ffi::F_GESTURE_HID_TYPE),
        phase: int(ffi::F_GESTURE_PHASE),
        motion: int(ffi::F_GESTURE_SWIPE_MOTION),
        progress: dbl(ffi::F_GESTURE_SWIPE_PROGRESS),
        velocity: dbl(ffi::F_GESTURE_SWIPE_VELOCITY_X),
        zoom: dbl(ffi::F_GESTURE_ZOOM),
    }
}

/// The running macOS major version, 0 when it cannot be read (then horizontal swipes read as before 26).
fn macos_major() -> u32 {
    let mut buf = [0u8; 32];
    let mut len = buf.len();
    let name = c"kern.osproductversion";
    // SAFETY: buf and len describe a writable buffer; the name is NUL-terminated.
    let r = unsafe { libc::sysctlbyname(name.as_ptr(), buf.as_mut_ptr().cast(), &mut len, ptr::null_mut(), 0) };
    let major = if r == 0 { parse_major(&buf[..len]) } else { None };
    major.unwrap_or_else(|| {
        eprintln!("flowseam: macOS version unreadable; horizontal swipes may switch spaces the wrong way");
        0
    })
}

fn parse_major(version: &[u8]) -> Option<u32> {
    let s = std::str::from_utf8(version).ok()?.trim_end_matches('\0');
    s.split('.').next()?.parse().ok()
}

/// Map one CG event to zero or more flowseam_core captured events.
///
/// Unmapped keycodes produce nothing and are logged, never guessed.
/// CapsLock arrives as one toggle per press, so it is emitted as a down/up
/// pair: flowseam_core toggles CapsLock on a down edge only.
unsafe fn translate(ty: u32, ev: CGEventRef) -> Vec<CapturedEvent> {
    let field = |f: u32| ffi::CGEventGetIntegerValueField(ev, f);
    match ty {
        ffi::EV_KEY_DOWN | ffi::EV_KEY_UP => {
            let kc = field(ffi::F_KEYBOARD_KEYCODE) as u16;
            let iso = matches!(kc, KC_ISO_SECTION | KC_GRAVE)
                && ffi::KBGetLayoutType(field(ffi::F_KEYBOARD_TYPE) as i16) == ffi::kKeyboardISO;
            match keycode_to_hid(kc).map(|hid| by_position(kc, hid, iso)) {
                Some(hid) => vec![CapturedEvent::Key { hid, down: ty == ffi::EV_KEY_DOWN }],
                None => {
                    eprintln!("flowseam: unmapped keycode {kc:#x}, not forwarded");
                    vec![]
                }
            }
        }
        ffi::EV_FLAGS_CHANGED => {
            let kc = field(ffi::F_KEYBOARD_KEYCODE) as u16;
            if kc == KC_CAPS_LOCK {
                let hid = keycode_to_hid(kc).unwrap_or(0x39);
                return vec![CapturedEvent::Key { hid, down: true }, CapturedEvent::Key { hid, down: false }];
            }
            if kc == KC_FN {
                // fn / Globe only changes what the Mac keyboard itself sends (F-keys, arrows to
                // Home/End); those arrive as their own keys. Nothing to forward.
                return vec![];
            }
            match (modifier_down(kc, ffi::CGEventGetFlags(ev)), keycode_to_hid(kc)) {
                (Some(down), Some(hid)) => vec![CapturedEvent::Key { hid, down }],
                _ => {
                    eprintln!("flowseam: unmapped modifier keycode {kc:#x}, not forwarded");
                    vec![]
                }
            }
        }
        ffi::EV_LEFT_DOWN | ffi::EV_LEFT_UP | ffi::EV_RIGHT_DOWN | ffi::EV_RIGHT_UP | ffi::EV_OTHER_DOWN
        | ffi::EV_OTHER_UP => {
            let button = field(ffi::F_MOUSE_BUTTON_NUMBER) as u8;
            let down = matches!(ty, ffi::EV_LEFT_DOWN | ffi::EV_RIGHT_DOWN | ffi::EV_OTHER_DOWN);
            vec![CapturedEvent::Button { button, down }]
        }
        ffi::EV_MOUSE_MOVED | ffi::EV_LEFT_DRAGGED | ffi::EV_RIGHT_DRAGGED | ffi::EV_OTHER_DRAGGED => {
            vec![CapturedEvent::Motion {
                dx: field(ffi::F_MOUSE_DELTA_X) as i32,
                dy: field(ffi::F_MOUSE_DELTA_Y) as i32,
            }]
        }
        ffi::EV_SYSTEM_DEFINED => {
            let (st, d1) = unsafe { sys::system_defined(ev) };
            match aux_key(st, d1) {
                Some((hid, down)) => vec![CapturedEvent::Key { hid, down }],
                None => vec![],
            }
        }
        ffi::EV_SCROLL_WHEEL => vec![CapturedEvent::Wheel {
            dx: field(ffi::F_SCROLL_DELTA_AXIS2) as i32,
            dy: field(ffi::F_SCROLL_DELTA_AXIS1) as i32,
        }],
        _ => vec![],
    }
}

/// Fault path for the breaker and user-input disable. Restores the cursor before the
/// sink hears about the fault, so the worker never sees "faulted" while the mouse is still frozen.
/// A failed restore is logged and recorded as `Unknown` by the guard; the fault is still reported.
pub fn on_fault(guard: &GrabGuard, sink: &mut dyn CaptureSink, f: CaptureFault) {
    if let Err(e) = guard.restore() {
        eprintln!("flowseam: restore after tap fault failed: {e:?}; cursor state is unknown");
    }
    sink.on_tap_fault(f);
}

#[cfg(test)]
impl TapHandle {
    /// A handle with no real tap behind it. Lets tests reach the grab path without creating an OS tap.
    pub fn dummy() -> Self {
        TapHandle { thread: Some(thread::spawn(|| {})), stop: Arc::new(AtomicBool::new(false)), stop_source: 0, run_loop: 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grab::mock::{Log, MockOs};
    use flowseam_core::backend::{Permission, Preflight};
    use std::sync::Mutex;

    #[test]
    fn macos_major_version_parses_and_reads() {
        assert_eq!(parse_major(b"26.7.1\0"), Some(26));
        assert_eq!(parse_major(b"27.0"), Some(27));
        assert_eq!(parse_major(b""), None);
        assert_eq!(parse_major(b"x.1"), None);
        assert!(macos_major() >= 11, "the running system reads as {}", macos_major());
    }

    #[test]
    fn every_gesture_type_is_tapped() {
        for ty in [ffi::EV_ROTATE, ffi::EV_BEGIN_GESTURE, ffi::EV_END_GESTURE, ffi::EV_GESTURE, ffi::EV_DOCK_CONTROL, ffi::EV_SWIPE, ffi::EV_SMART_MAGNIFY] {
            assert!(is_gesture(ty) && EVENT_MASK & mask(ty) != 0, "type {ty}");
        }
        assert!(!is_gesture(ffi::EV_SCROLL_WHEEL));
    }

    #[test]
    fn stop_source_wakes_long_run_loop_wait() {
        for delay in [0, 20] {
            let (tx, rx) = mpsc::channel();
            let thread = thread::spawn(move || {
                let run_loop = unsafe { ffi::CFRunLoopGetCurrent() };
                let source = new_stop_source();
                assert!(!source.is_null());
                unsafe { ffi::CFRunLoopAddSource(run_loop, source, ffi::kCFRunLoopDefaultMode) };
                tx.send((source as usize, run_loop as usize)).unwrap();
                unsafe { ffi::CFRunLoopRunInMode(ffi::kCFRunLoopDefaultMode, 10.0, true) };
                unsafe { ffi::CFRunLoopRemoveSource(run_loop, source, ffi::kCFRunLoopDefaultMode) };
            });
            let (source, run_loop) = rx.recv().unwrap();
            thread::sleep(std::time::Duration::from_millis(delay));
            let start = Instant::now();
            wake_stop_source(source, run_loop as *mut c_void);
            thread.join().unwrap();
            assert!(start.elapsed().as_millis() < 100);
            unsafe { ffi::CFRelease(source as ffi::CFTypeRef) };
        }
    }

    /// Records each fault and whether the cursor had already been shown when the fault arrived.
    struct RecSink {
        log: Log,
        faults: Arc<Mutex<Vec<(CaptureFault, bool)>>>,
    }

    impl CaptureSink for RecSink {
        fn on_event(&mut self, _ev: CapturedEvent) -> Disposition {
            Disposition::Pass
        }
        fn on_tap_fault(&mut self, f: CaptureFault) {
            let shown = self.log.calls().iter().any(|c| c == "show");
            self.faults.lock().unwrap().push((f, shown));
        }
    }

    fn temp_lock(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("flowseam-mac-tap-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("grab.lock")
    }

    type FaultLog = Arc<Mutex<Vec<(CaptureFault, bool)>>>;

    fn grabbed_guard(name: &str, fail_show: bool) -> (GrabGuard, Log, FaultLog) {
        let log = Log::default();
        let os = MockOs { log: log.clone(), preflight: Preflight::Missing(Permission::Accessibility), secure: Ok(Some(false)), fail_show };
        let guard = GrabGuard::new(Box::new(os), temp_lock(name));
        guard.grab().unwrap();
        (guard, log, Arc::new(Mutex::new(vec![])))
    }

    /// A2: a user-input disable must put the cursor back before the sink hears about it.
    #[test]
    fn user_input_fault_restores_cursor_before_reporting() {
        let (guard, log, faults) = grabbed_guard("a2-user", false);
        let mut sink = RecSink { log: log.clone(), faults: faults.clone() };
        on_fault(&guard, &mut sink, CaptureFault::TapDisabledByUserInput);
        assert!(log.calls().iter().any(|c| c == "show"), "cursor must be shown: {:?}", log.calls());
        assert!(log.calls().iter().any(|c| c == "assoc:true"), "mouse must be reassociated");
        assert!(!guard.is_grabbed());
        assert_eq!(faults.lock().unwrap().as_slice(), &[(CaptureFault::TapDisabledByUserInput, true)]);
    }

    /// A2: breaker teardown is the same fault path.
    #[test]
    fn breaker_teardown_restores_cursor_before_reporting() {
        let (guard, log, faults) = grabbed_guard("a2-timeout", false);
        let mut sink = RecSink { log: log.clone(), faults: faults.clone() };
        on_fault(&guard, &mut sink, CaptureFault::TapDisabledByTimeout);
        assert!(!guard.is_grabbed());
        assert_eq!(faults.lock().unwrap().as_slice(), &[(CaptureFault::TapDisabledByTimeout, true)]);
    }

    /// A2: a failed restore is recorded as unknown, not as restored, and the fault is still reported.
    #[test]
    fn failed_restore_is_recorded_as_unknown_and_still_reported() {
        let (guard, log, faults) = grabbed_guard("a2-fail", true);
        let mut sink = RecSink { log: log.clone(), faults: faults.clone() };
        on_fault(&guard, &mut sink, CaptureFault::TapDisabledByUserInput);
        assert_eq!(guard.cursor(), crate::state::CursorState::Unknown, "failed show must not read as visible");
        assert_eq!(faults.lock().unwrap().len(), 1, "fault must still reach the sink");
    }
}

#[cfg(test)]
mod emergency_tests {
    use super::*;
    use crate::grab::mock::{Log, MockOs};
    use flowseam_core::backend::{Permission, Preflight};
    use flowseam_core::emergency::HID_ESCAPE;
    use std::sync::Mutex;

    const LCTRL: u16 = 0xE0;
    const LALT: u16 = 0xE2;
    const LGUI: u16 = 0xE3;
    const KEY_A: u16 = 0x04;

    #[derive(Clone, Default)]
    struct Rec {
        events: Arc<Mutex<Vec<CapturedEvent>>>,
        faults: Arc<Mutex<Vec<CaptureFault>>>,
    }

    impl CaptureSink for Rec {
        fn on_event(&mut self, ev: CapturedEvent) -> Disposition {
            self.events.lock().unwrap().push(ev);
            Disposition::Pass
        }
        fn on_tap_fault(&mut self, f: CaptureFault) {
            self.faults.lock().unwrap().push(f);
        }
    }

    struct Rig {
        state: TapState,
        log: Log,
        rec: Rec,
    }

    fn key(hid: u16, down: bool) -> CapturedEvent {
        CapturedEvent::Key { hid, down }
    }

    /// A grabbed guard over a recording mock. The grab itself logs "hide"; only "show" means a restore.
    fn rig(name: &str) -> Rig {
        let dir = std::env::temp_dir().join(format!("flowseam-mac-emerg-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let log = Log::default();
        let os = MockOs { log: log.clone(), preflight: Preflight::Missing(Permission::Accessibility), secure: Ok(Some(false)), fail_show: false };
        let guard = GrabGuard::new(Box::new(os), dir.join("grab.lock"));
        guard.grab().unwrap();
        let rec = Rec::default();
        let state = TapState {
            sink: Box::new(rec.clone()),
            guard,
            keys: KeyTracker::new(),
            env: Box::new(ScriptEnv::idle()),
        };
        Rig { state, log, rec }
    }

    fn assert_no_fire(r: &Rig) {
        assert_eq!(r.log.count("show"), 0, "no restore expected: {:?}", r.log.calls());
        assert!(r.rec.faults.lock().unwrap().is_empty(), "no emergency fault expected");
        assert!(r.state.guard.is_grabbed(), "grab must still hold");
    }

    /// The chord is the user's way out, so it must not depend on the sink: a forwarder that panics while
    /// handing the session back still leaves the cursor visible, reassociated and the grab released.
    #[test]
    fn chord_restores_the_cursor_even_when_the_sink_panics() {
        struct Panics;
        impl CaptureSink for Panics {
            fn on_event(&mut self, _ev: CapturedEvent) -> Disposition {
                Disposition::Pass
            }
            fn on_tap_fault(&mut self, _f: CaptureFault) {}
            fn on_emergency(&mut self) {
                panic!("forwarder blew up");
            }
        }
        let mut r = rig("sink-panics");
        r.state.sink = Box::new(Panics);
        let chord = vec![key(LCTRL, true), key(LALT, true), key(LGUI, true), key(HID_ESCAPE, true)];
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle(&mut r.state, chord)));
        assert!(caught.is_err(), "the sink's panic still propagates to the callback's catch_unwind");
        assert_eq!(r.log.count("show"), 1, "cursor must be shown before the sink runs: {:?}", r.log.calls());
        assert!(r.log.calls().iter().any(|c| c == "assoc:true"));
        assert!(!r.state.guard.is_grabbed());
        assert!(r.state.guard.is_halted(), "halted, so nothing can hide it again");
    }

    /// A1: the full chord, in one batch, restores the cursor, reports Emergency, and swallows the Esc.
    #[test]
    fn full_chord_restores_cursor_and_reports_emergency() {
        let mut r = rig("full");
        let d = handle(
            &mut r.state,
            vec![key(LCTRL, true), key(LALT, true), key(LGUI, true), key(HID_ESCAPE, true)],
        );
        assert_eq!(d, Disposition::Swallow, "the chord's Esc must not reach local apps");
        assert_eq!(r.log.count("show"), 1, "cursor restored once: {:?}", r.log.calls());
        assert!(r.log.calls().iter().any(|c| c == "assoc:true"));
        assert!(!r.state.guard.is_grabbed());
        assert_eq!(r.rec.faults.lock().unwrap().as_slice(), &[CaptureFault::Emergency]);
        let evs = r.rec.events.lock().unwrap();
        assert!(!evs.contains(&key(HID_ESCAPE, true)), "Esc must not be forwarded to the sink");
        assert!(evs.contains(&key(LGUI, true)), "modifiers before the chord are ordinary input");
    }

    /// A1: the modifiers may be pressed across separate tap callbacks.
    #[test]
    fn chord_split_across_callbacks_still_fires() {
        let mut r = rig("split");
        handle(&mut r.state, vec![key(LCTRL, true)]);
        handle(&mut r.state, vec![key(LALT, true)]);
        handle(&mut r.state, vec![key(LGUI, true)]);
        handle(&mut r.state, vec![key(HID_ESCAPE, true)]);
        assert_eq!(r.log.count("show"), 1, "split chord must fire: {:?}", r.log.calls());
    }

    /// A1: after firing, the tap is halted: local keys pass through and the sink hears nothing more.
    #[test]
    fn after_emergency_local_input_passes_and_sink_is_quiet() {
        let mut r = rig("halt");
        handle(&mut r.state, vec![key(LCTRL, true), key(LALT, true), key(LGUI, true), key(HID_ESCAPE, true)]);
        let before = r.rec.events.lock().unwrap().len();
        let d = handle(&mut r.state, vec![key(KEY_A, true)]);
        assert_eq!(d, Disposition::Pass, "local key must pass through after emergency");
        assert_eq!(r.rec.events.lock().unwrap().len(), before, "sink must not receive events while halted");
        assert_eq!(r.log.count("show"), 1, "no second restore");
    }

    /// A1 negative: missing Cmd does not fire; Esc goes through as ordinary input.
    #[test]
    fn missing_one_modifier_does_not_fire() {
        let mut r = rig("neg-gui");
        let d = handle(&mut r.state, vec![key(LCTRL, true), key(LALT, true), key(HID_ESCAPE, true)]);
        assert_eq!(d, Disposition::Swallow, "grabbed: ordinary input belongs to the peer, not local apps");
        assert!(r.rec.events.lock().unwrap().contains(&key(HID_ESCAPE, true)));
        assert_no_fire(&r);
    }

    /// A1 negative: a modifier released before Esc disarms the chord.
    #[test]
    fn released_modifier_before_escape_does_not_fire() {
        let mut r = rig("neg-release");
        handle(&mut r.state, vec![key(LCTRL, true), key(LALT, true), key(LGUI, true)]);
        handle(&mut r.state, vec![key(LCTRL, false)]);
        handle(&mut r.state, vec![key(HID_ESCAPE, true)]);
        assert_no_fire(&r);
    }

    /// A1 negative: Esc-up alone never fires.
    #[test]
    fn escape_up_does_not_fire() {
        let mut r = rig("neg-up");
        handle(&mut r.state, vec![key(LCTRL, true), key(LALT, true), key(LGUI, true)]);
        handle(&mut r.state, vec![key(HID_ESCAPE, false)]);
        assert_no_fire(&r);
    }
}

#[cfg(test)]
pub struct ScriptEnv {
    now: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pos: std::sync::Arc<std::sync::Mutex<Option<Point>>>,
}

#[cfg(test)]
impl ScriptEnv {
    /// No cursor sample, so the sink sees no motion position.
    pub fn idle() -> Self {
        Self { now: Default::default(), pos: Default::default() }
    }
}

#[cfg(test)]
impl SwitchEnv for ScriptEnv {
    fn now_ms(&self) -> u64 {
        self.now.load(Ordering::SeqCst)
    }
    fn cursor(&self) -> Option<Point> {
        *self.pos.lock().unwrap()
    }
}

#[cfg(test)]
mod fwd_tests {
    use super::*;
    use crate::grab::mock::{Log, MockOs};
    use crate::sink::EmitSink;
    use crate::wire::ForwardSink;
    use flowseam_core::backend::{EmitOutcome, InputEmitter, Permission, Preflight};
    use flowseam_core::forward::{FwdConfig, Forwarder, Mode, Outbox, PeerSpec};
    use flowseam_core::proto::{Edge, ModifierSet, Msg, Rect};
    use flowseam_core::switch::SwitchPolicy;
    use std::sync::Mutex;

    const LCTRL: u16 = 0xE0;
    const LALT: u16 = 0xE2;
    const LGUI: u16 = 0xE3;
    const HID_S: u16 = 0x16;
    const KEY_A: u16 = 0x04;

    struct NullEm;
    impl InputEmitter for NullEm {
        fn preflight(&self) -> Preflight { Preflight::Ok }
        fn move_rel(&mut self, _: i32, _: i32) -> Result<EmitOutcome, flowseam_core::backend::BackendError> { Ok(EmitOutcome::Delivered) }
        fn move_abs(&mut self, _: Point) -> Result<EmitOutcome, flowseam_core::backend::BackendError> { Ok(EmitOutcome::Delivered) }
        fn button(&mut self, _: u8, _: bool) -> Result<EmitOutcome, flowseam_core::backend::BackendError> { Ok(EmitOutcome::Delivered) }
        fn wheel(&mut self, _: i32, _: i32) -> Result<EmitOutcome, flowseam_core::backend::BackendError> { Ok(EmitOutcome::Delivered) }
        fn key(&mut self, _: u16, _: bool, _: bool) -> Result<EmitOutcome, flowseam_core::backend::BackendError> { Ok(EmitOutcome::Delivered) }
        fn set_modifiers(&mut self, _: ModifierSet) -> Result<EmitOutcome, flowseam_core::backend::BackendError> { Ok(EmitOutcome::Delivered) }
        fn release_all(&mut self) -> Result<(), flowseam_core::backend::BackendError> { Ok(()) }
        fn set_cursor_visible(&mut self, _: bool) -> Result<(), flowseam_core::backend::BackendError> { Ok(()) }
    }

    #[derive(Clone, Default)]
    struct Mail(Arc<Mutex<Vec<Msg>>>);
    impl Outbox for Mail {
        fn send(&mut self, _peer: &str, msg: Msg) {
            self.0.lock().unwrap().push(msg);
        }
    }

    struct Rig {
        state: TapState,
        log: Log,
        mail: Mail,
        fwd: crate::wire::Shared<NullEm, Mail>,
        pos: Arc<Mutex<Option<Point>>>,
        now: Arc<std::sync::atomic::AtomicU64>,
    }

    struct Quiet;
    impl CaptureSink for Quiet {
        fn on_event(&mut self, _: CapturedEvent) -> Disposition { Disposition::Pass }
        fn on_tap_fault(&mut self, _: CaptureFault) {}
    }

    /// One peer on the right edge, link up. `secure` makes the grab fail.
    fn rig(name: &str, secure: bool) -> Rig {
        let dir = std::env::temp_dir().join(format!("flowseam-mac-fwd-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let log = Log::default();
        let os = MockOs { log: log.clone(), preflight: Preflight::Missing(Permission::Accessibility), secure: Ok(Some(secure)), fail_show: false };
        let guard = GrabGuard::new(Box::new(os), dir.join("grab.lock"));
        let mail = Mail::default();
        let screen = Rect { x: 0, y: 0, w: 1000, h: 800 };
        let cfg = FwdConfig { peers: vec![PeerSpec { name: "p".into(), edge: Edge::Right, keys: flowseam_core::config::KeyMap::None, gestures: Default::default() }], policy: SwitchPolicy::default(), boundary_px: 2 };
        let mut f = Forwarder::new(cfg, vec![screen], EmitSink::new(NullEm, guard.clone()), mail.clone());
        f.link_up("p");
        let fwd = Arc::new(Mutex::new(f));
        let env = ScriptEnv::idle();
        let (now, pos) = (env.now.clone(), env.pos.clone());
        let clock_now = now.clone();
        let sink = ForwardSink::new(
            fwd.clone(),
            Arc::new(move || clock_now.load(Ordering::SeqCst)),
            Box::new(move || Some(vec![screen])),
            Box::new(Quiet),
        );
        let state = TapState { sink: Box::new(sink), guard, keys: KeyTracker::new(), env: Box::new(env) };
        Rig { state, log, mail, fwd, pos, now }
    }

    fn at(r: &Rig, t: u64, p: Point) {
        r.now.store(t, Ordering::SeqCst);
        *r.pos.lock().unwrap() = Some(p);
    }
    fn key(hid: u16, down: bool) -> CapturedEvent { CapturedEvent::Key { hid, down } }
    fn motion(r: &mut Rig) -> Disposition { handle(&mut r.state, vec![CapturedEvent::Motion { dx: 1, dy: 0 }]) }
    fn press_mods(r: &mut Rig) { handle(&mut r.state, vec![key(LCTRL, true), key(LALT, true), key(LGUI, true)]); }
    fn mode_is_local(r: &Rig) -> bool { matches!(r.fwd.lock().unwrap().mode(), Mode::Local) }

    /// Before a switch the tap takes nothing, however long the cursor sits mid-screen.
    #[test]
    fn nothing_is_swallowed_or_grabbed_before_the_switch() {
        let mut r = rig("pre", false);
        for t in 0..20u64 {
            at(&r, t * 100, Point { x: 500, y: 400 });
            assert_eq!(motion(&mut r), Disposition::Pass);
        }
        assert_eq!(handle(&mut r.state, vec![key(KEY_A, true)]), Disposition::Pass);
        assert_eq!(r.log.count("hide"), 0, "{:?}", r.log.calls());
        assert!(mode_is_local(&r) && !r.state.guard.is_grabbed());
    }

    /// Edge dwell: touching the strip is not enough; after the dwell the forwarder grabs and swallows.
    #[test]
    fn edge_dwell_switches_only_after_threshold() {
        let mut r = rig("dwell", false);
        at(&r, 0, Point { x: 999, y: 400 });
        assert_eq!(motion(&mut r), Disposition::Pass);
        assert!(mode_is_local(&r));
        at(&r, 300, Point { x: 999, y: 400 });
        motion(&mut r);
        assert!(!mode_is_local(&r), "dwell must switch");
        assert!(r.state.guard.is_grabbed());
        assert_eq!(motion(&mut r), Disposition::Swallow, "RemoteOut swallows local input");
        assert!(matches!(r.mail.0.lock().unwrap()[0], Msg::Enter { .. }));
    }

    /// After a switch the OS puts the grab warp's jump (edge to center) into the next event's delta.
    /// Pushed to the peer, it drove the peer's cursor to its exit edge and control came straight back.
    /// The real path (correct, then handle) must forward the hand's motion and never the jump.
    #[test]
    fn the_grab_warp_jump_never_reaches_the_peer() {
        let mut r = rig("jump", false);
        at(&r, 0, Point { x: 999, y: 400 });
        motion(&mut r);
        at(&r, 300, Point { x: 999, y: 400 });
        motion(&mut r);
        assert!(!mode_is_local(&r), "must have switched");
        let before = r.mail.0.lock().unwrap().len();
        for (dx, dy) in [(-998, -86), (4, -1), (1, 0)] {
            let mut evs = vec![CapturedEvent::Motion { dx, dy }];
            correct_motion(&r.state.guard, &mut evs, || (0.0, 0.0));
            assert_eq!(handle(&mut r.state, evs), Disposition::Swallow);
        }
        let sent: Vec<(i32, i32)> = r.mail.0.lock().unwrap()[before..].iter()
            .filter_map(|m| if let Msg::MotionRel { dx, dy, .. } = m { Some((*dx, *dy)) } else { None })
            .collect();
        assert_eq!(sent, vec![(0, 0), (4, -1), (1, 0)]);
    }

    /// A key the table cannot express produces no captured event. Measured: keypad 0 and + pressed
    /// while Windows was driven typed "0+" into the local terminal. Driven, nothing may pass; local,
    /// the same event is the user's own and must.
    #[test]
    fn unmapped_input_never_leaks_locally_while_the_peer_is_driven() {
        let mut r = rig("leak", false);
        assert_eq!(handle(&mut r.state, vec![]), Disposition::Pass, "local: unmapped input is the user's own");
        at(&r, 0, Point { x: 999, y: 400 });
        motion(&mut r);
        at(&r, 300, Point { x: 999, y: 400 });
        motion(&mut r);
        assert!(!mode_is_local(&r), "must have switched");
        assert_eq!(handle(&mut r.state, vec![]), Disposition::Swallow);
    }

    /// The switch chord key is swallowed on keydown and keyup never reaches the app as ordinary input.
    #[test]
    fn chord_key_is_swallowed_and_grabs() {
        let mut r = rig("chord", false);
        press_mods(&mut r);
        assert_eq!(handle(&mut r.state, vec![key(HID_S, true)]), Disposition::Swallow);
        assert!(r.state.guard.is_grabbed());
        assert!(!mode_is_local(&r));
    }

    /// A refused grab (Secure Input) returns to Local, tells the peer, and lets the key through.
    #[test]
    fn failed_grab_aborts_and_lets_the_key_through() {
        let mut r = rig("abort", true);
        press_mods(&mut r);
        let d = handle(&mut r.state, vec![key(HID_S, true)]);
        assert_eq!(d, Disposition::Pass, "key of a switch that did not happen is ordinary input");
        assert!(mode_is_local(&r));
        assert!(!r.state.guard.is_grabbed());
        let sent = r.mail.0.lock().unwrap().clone();
        assert!(matches!(sent.last(), Some(Msg::Leave { .. })), "peer must hear the switch was undone: {sent:?}");
        // Retry works: another chord press tries again.
        handle(&mut r.state, vec![key(HID_S, false)]);
        handle(&mut r.state, vec![key(HID_S, true)]);
        assert!(mode_is_local(&r));
    }

    /// Emergency chord: Local stays Local, halted, cursor restored, Esc swallowed.
    #[test]
    fn emergency_from_remote_out_returns_local_and_halts() {
        let mut r = rig("emerg", false);
        press_mods(&mut r);
        handle(&mut r.state, vec![key(HID_S, true)]);
        assert!(!mode_is_local(&r));
        let d = handle(&mut r.state, vec![key(flowseam_core::emergency::HID_ESCAPE, true)]);
        assert_eq!(d, Disposition::Swallow);
        assert!(mode_is_local(&r) && !r.state.guard.is_grabbed() && r.state.guard.is_halted());
    }
}
