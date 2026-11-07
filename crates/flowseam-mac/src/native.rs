//! A file drag arriving from a peer, carried on as a system drag on this Mac (docs/native-drop.md).
//!
//! The hub calls [`MacNativeDrop::begin`] when a drag's files are announced. The rest runs on the main thread
//! through [`crate::front::on_main`]: a small transparent window under the cursor, a posted left press that lands
//! on it, and a dragging session of file promises started from that press. The peer's injected moves carry it and
//! its injected release drops it. Finder, or any target that takes promised files, then asks for each file where it
//! was dropped, and the promise waits there for bytes still on the wire.
//!
//! Spike evidence (2026-11-05, on the driven Mac with the mouse disassociated): 10 drops, 1 cancel by Escape, 1
//! drop whose bytes came 6 s late; the first drop of the first run failed for a reason never found, which the
//! Downloads fallback covers.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use block2::DynBlock;
use flowseam_net::files::{NativeDrop, NativeSession, Staged};
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSColor, NSDragOperation, NSDraggingContext, NSDraggingItem, NSDraggingSession,
    NSDraggingSource, NSEvent, NSFilePromiseProvider, NSFilePromiseProviderDelegate, NSRunningApplication, NSScreen,
    NSView, NSWindow, NSWindowStyleMask, NSWorkspace,
};
use objc2_foundation::{NSArray, NSError, NSOperationQueue, NSPoint, NSRect, NSSize, NSString, NSURL};

use crate::{ffi, sys};

/// The press this module made is down. The emitter posts moves as drags while it is; moves posted as plain
/// moves would leave the system drag behind.
static LEFT_HELD: AtomicBool = AtomicBool::new(false);

/// Whether the left press made for a native drag is down.
pub fn left_held() -> bool {
    LEFT_HELD.load(Ordering::SeqCst)
}

/// The peer's release was posted: it is the native drag's drop, so the press is no longer ours to let go.
pub fn released() {
    LEFT_HELD.store(false, Ordering::SeqCst);
}

/// How long the posted press may take to land on the window and start the drag before it counts as failed.
const START_WITHIN: Duration = Duration::from_millis(1500);
/// How long a just-ordered window takes to be on screen and take a press. 150 ms after ordering front; the spike
/// that worked waited 300 ms, and a press in the same turn missed the window every time.
const PRESS_AFTER: Duration = Duration::from_millis(150);
/// Promise delegates are kept alive (the provider holds them weakly) until their file is written, or this long.
const PROMISE_LIFETIME: Duration = Duration::from_secs(3600);
const KEY_ESCAPE: u16 = 53;
/// How many times the window may follow the cursor to another screen before pressing.
const MAX_MOVES: u32 = 3;
/// NSPopUpMenuWindowLevel: above ordinary windows, so the posted press lands on ours.
const LEVEL_POPUP: isize = 101;

type Ended = Box<dyn FnOnce(bool) + Send>;

struct Shared {
    drag: u32,
    staged: Arc<Staged>,
    cancelled: AtomicBool,
    started: AtomicBool,
    first_move: AtomicBool,
    /// Times the window followed the cursor to another screen before the press (bounded by `MAX_MOVES`).
    moves: AtomicU32,
    ended: Mutex<Option<Ended>>,
    /// The app that was frontmost before the drag window took the front; it gets it back.
    front_before: Mutex<Option<i32>>,
}

impl Shared {
    /// Report the end once; later calls do nothing.
    fn end(&self, took: bool) {
        let ended = self.ended.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(f) = ended {
            f(took);
        }
    }
}

/// [`NativeDrop`] for macOS.
pub struct MacNativeDrop;

impl NativeDrop for MacNativeDrop {
    fn begin(&self, drag: u32, staged: Arc<Staged>, ended: Ended) -> Result<Box<dyn NativeSession>, String> {
        let shared = Arc::new(Shared {
            drag,
            staged,
            cancelled: AtomicBool::new(false),
            started: AtomicBool::new(false),
            first_move: AtomicBool::new(false),
            moves: AtomicU32::new(0),
            ended: Mutex::new(Some(ended)),
            front_before: Mutex::new(None),
        });
        flowseam_core::log::line(&format!("flowseam native drop: drag {drag} arrived; starting a system drag"));
        let s = shared.clone();
        crate::front::on_main(Instant::now(), Box::new(move || start(s)));
        Ok(Box::new(Session(shared)))
    }
}

struct Session(Arc<Shared>);

impl NativeSession for Session {
    fn cancel(&self) {
        if self.0.cancelled.swap(true, Ordering::SeqCst) {
            return;
        }
        flowseam_core::log::line(&format!("flowseam native drop: drag {} cancelled; Escape and a release follow", self.0.drag));
        // The source refuses every operation from now on, so even a release that wins the race drops nothing.
        // Escape ends the drag; the press we made is let go so no button stays down here.
        let _ = sys::post_key_to_pid(KEY_ESCAPE, std::process::id() as i32);
        if LEFT_HELD.swap(false, Ordering::SeqCst) {
            if let Ok(at) = sys::cursor_position() {
                let _ = sys::post_mouse(ffi::EV_LEFT_UP, at, 0);
            }
        }
        let s = self.0.clone();
        // Also covers a drag that never started (the press never landed): its window goes.
        crate::front::on_main(Instant::now(), Box::new(move || finish(&s)));
    }
}

thread_local! {
    /// Main thread: the drag windows, by drag id, until their drag ends.
    static WINDOWS: RefCell<Vec<(u32, Retained<NSWindow>)>> = const { RefCell::new(Vec::new()) };
}

/// Keeps each promise delegate alive until its file is written (or `PROMISE_LIFETIME` passes).
static PROMISES: Mutex<Vec<(Instant, Retained<Promise>)>> = Mutex::new(Vec::new());

fn keep_promise(p: Retained<Promise>) {
    let mut v = PROMISES.lock().unwrap_or_else(|e| e.into_inner());
    v.retain(|(at, _)| at.elapsed() < PROMISE_LIFETIME);
    v.push((Instant::now(), p));
}

fn forget_promise(p: &Promise) {
    PROMISES.lock().unwrap_or_else(|e| e.into_inner()).retain(|(_, q)| !std::ptr::eq(&**q, p));
}

/// Main thread: cover the cursor's screen with a transparent window, and press on it once it is on screen; the
/// press starts the drag in `mouseDown:`. A press posted in the same turn as the window is ordered front lands on
/// whatever was there before (seen on the first install: every drop fell back to Downloads). Covering the screen
/// keeps the press on the window however far the cursor moves meanwhile; it is ordered out as the drag starts.
fn start(shared: Arc<Shared>) {
    let Some(mtm) = MainThreadMarker::new() else {
        shared.end(false);
        return;
    };
    if shared.cancelled.load(Ordering::SeqCst) {
        shared.end(false);
        return;
    }
    let at = sys::cursor_position();
    let (Ok(at), Some(frame)) = (at.clone(), screen_frame_at(mtm, at.ok())) else {
        flowseam_core::log::line("flowseam native drop: no cursor or screen; the drop falls back to Downloads");
        shared.end(false);
        return;
    };
    // SAFETY: main thread; a borderless window with no special requirements.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    // SAFETY: we keep our own reference and close it ourselves.
    unsafe { window.setReleasedWhenClosed(false) };
    window.setOpaque(false);
    window.setBackgroundColor(Some(&NSColor::colorWithWhite_alpha(0.0, 0.01)));
    window.setLevel(LEVEL_POPUP);
    let view = DragView::new(mtm, shared.clone(), frame.size);
    window.setContentView(Some(&view));
    window.orderFrontRegardless();
    *shared.front_before.lock().unwrap_or_else(|e| e.into_inner()) = sys::frontmost_pid();
    // SAFETY: main thread; plain message send on the shared application.
    let _: () = unsafe { msg_send![&*NSApplication::sharedApplication(mtm), activateIgnoringOtherApps: true] };
    WINDOWS.with(|w| w.borrow_mut().push((shared.drag, window)));
    flowseam_core::log::line(&format!("flowseam native drop: drag {} window over the screen at cursor ({}, {})", shared.drag, at.x, at.y));
    let s = shared.clone();
    crate::front::on_main(Instant::now() + PRESS_AFTER, Box::new(move || press(s)));
}

/// Index of the frame (AppKit coordinates: x, y, width, height) that contains `p`.
fn screen_index(p: (f64, f64), frames: &[(f64, f64, f64, f64)]) -> Option<usize> {
    frames.iter().position(|&(x, y, w, h)| p.0 >= x && p.0 < x + w && p.1 >= y && p.1 < y + h)
}

/// The frame of the screen the cursor is on (the main screen when it is on none), in AppKit coordinates.
fn screen_frame_at(mtm: MainThreadMarker, at: Option<flowseam_core::proto::Point>) -> Option<NSRect> {
    let screens = NSScreen::screens(mtm);
    let main = screens.firstObject()?.frame();
    let frames: Vec<NSRect> = screens.iter().map(|s| s.frame()).collect();
    let tuples: Vec<_> = frames.iter().map(|f| (f.origin.x, f.origin.y, f.size.width, f.size.height)).collect();
    // Quartz measures from the top of the main screen, AppKit from its bottom.
    let point = at.map(|at| (at.x as f64, main.size.height - at.y as f64));
    Some(point.and_then(|p| screen_index(p, &tuples)).map_or(main, |i| frames[i]))
}

/// Main thread, once the window is on screen: press where the cursor is now, and give up if no drag starts.
fn press(shared: Arc<Shared>) {
    if shared.cancelled.load(Ordering::SeqCst) {
        finish(&shared);
        shared.end(false);
        return;
    }
    // A drag carried back from the machine this Mac was driving arrives before the cursor is put back: the window
    // went up over the screen of the parked cursor, which need not be the screen the cursor comes home on. Move it
    // there and press after it has settled, or the press misses it and the drop falls back to Downloads. A cursor
    // still crossing screens meanwhile is followed again, a few times at most.
    if let (Some(mtm), true) = (MainThreadMarker::new(), shared.moves.load(Ordering::SeqCst) < MAX_MOVES) {
        let to = screen_frame_at(mtm, sys::cursor_position().ok());
        let moved = WINDOWS.with(|w| {
            w.borrow().iter().find(|(d, _)| *d == shared.drag).is_some_and(|(_, win)| match to {
                Some(to) if win.frame() != to => {
                    win.setFrame_display(to, true);
                    true
                }
                _ => false,
            })
        });
        if moved {
            shared.moves.fetch_add(1, Ordering::SeqCst);
            flowseam_core::log::line(&format!("flowseam native drop: drag {} window moved to the cursor's screen", shared.drag));
            let s = shared.clone();
            crate::front::on_main(Instant::now() + PRESS_AFTER, Box::new(move || press(s)));
            return;
        }
    }
    match sys::cursor_position().and_then(|at| sys::post_mouse(ffi::EV_LEFT_DOWN, at, 0).map(|()| at)) {
        Ok(at) => {
            LEFT_HELD.store(true, Ordering::SeqCst);
            flowseam_core::log::line(&format!("flowseam native drop: drag {} pressed at ({}, {})", shared.drag, at.x, at.y));
        }
        Err(e) => flowseam_core::log::line(&format!("flowseam native drop: could not press: {e:?}")),
    }
    let s = shared.clone();
    crate::front::on_main(
        Instant::now() + START_WITHIN,
        Box::new(move || {
            if !s.started.load(Ordering::SeqCst) {
                flowseam_core::log::line(&format!("flowseam native drop: drag {} did not start; the drop falls back to Downloads", s.drag));
                if LEFT_HELD.swap(false, Ordering::SeqCst) {
                    if let Ok(at) = sys::cursor_position() {
                        let _ = sys::post_mouse(ffi::EV_LEFT_UP, at, 0);
                    }
                }
                finish(&s);
                s.end(false);
            }
        }),
    );
}

/// Main thread: close this drag's window and give the front back to the app that had it.
fn finish(shared: &Shared) {
    let window = WINDOWS.with(|w| {
        let mut w = w.borrow_mut();
        w.iter().position(|(d, _)| *d == shared.drag).map(|i| w.remove(i).1)
    });
    let Some(window) = window else { return };
    window.orderOut(None);
    window.close();
    let me = std::process::id() as i32;
    if let Some(pid) = shared.front_before.lock().unwrap_or_else(|e| e.into_inner()).take().filter(|p| *p != me) {
        if let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid) {
            #[allow(deprecated)]
            app.activateWithOptions(objc2_app_kit::NSApplicationActivationOptions::empty());
        }
    }
}

struct ViewIvars {
    shared: Arc<Shared>,
}

define_class!(
    // SAFETY: NSView has no subclassing requirements; DragView has no Drop.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "FlowseamDragView"]
    #[ivars = ViewIvars]
    struct DragView;

    impl DragView {
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let shared = self.ivars().shared.clone();
            if shared.started.swap(true, Ordering::SeqCst) || shared.cancelled.load(Ordering::SeqCst) {
                return;
            }
            let ws = NSWorkspace::sharedWorkspace();
            // The icons start under the cursor: the view covers the whole screen.
            let at = event.locationInWindow();
            let mut items = Vec::new();
            for (i, f) in shared.staged.files().iter().enumerate() {
                let promise = Promise::new(shared.clone(), i);
                let provider = NSFilePromiseProvider::initWithFileType_delegate(
                    NSFilePromiseProvider::alloc(),
                    &NSString::from_str("public.data"),
                    ProtocolObject::from_ref(&*promise),
                );
                keep_promise(promise);
                let item = NSDraggingItem::initWithPasteboardWriter(NSDraggingItem::alloc(), ProtocolObject::from_ref(&*provider));
                let ext = std::path::Path::new(&f.name).extension().and_then(|e| e.to_str()).unwrap_or("");
                #[allow(deprecated)]
                let icon = ws.iconForFileType(&NSString::from_str(ext));
                let offset = 4.0 * i.min(4) as f64;
                // SAFETY: the contents are an NSImage, as the dragging item expects.
                unsafe {
                    item.setDraggingFrame_contents(NSRect::new(NSPoint::new(at.x - 16.0 + offset, at.y - 16.0 - offset), NSSize::new(32.0, 32.0)), Some(&icon))
                };
                items.push(item);
            }
            let items = NSArray::from_retained_slice(&items);
            flowseam_core::log::line(&format!("flowseam native drop: drag {} pressed buttons: {}", shared.drag, NSEvent::pressedMouseButtons()));
            self.beginDraggingSessionWithItems_event_source(&items, event, ProtocolObject::from_ref(self));
            flowseam_core::log::line(&format!("flowseam native drop: drag {} started with {} file(s)", shared.drag, items.count()));
            // Out of the way: a window under the drop point that takes no drops would refuse it.
            if let Some(w) = self.window() {
                w.orderOut(None);
            }
        }
    }

    unsafe impl NSObjectProtocol for DragView {}

    unsafe impl NSDraggingSource for DragView {
        #[unsafe(method(draggingSession:willBeginAtPoint:))]
        fn will_begin(&self, _session: &NSDraggingSession, at: NSPoint) {
            flowseam_core::log::line(&format!("flowseam native drop: drag {} will begin at ({}, {})", self.ivars().shared.drag, at.x, at.y));
        }

        #[unsafe(method(draggingSession:movedToPoint:))]
        fn moved(&self, _session: &NSDraggingSession, at: NSPoint) {
            let shared = &self.ivars().shared;
            if !shared.first_move.swap(true, Ordering::SeqCst) {
                flowseam_core::log::line(&format!("flowseam native drop: drag {} first move at ({}, {})", shared.drag, at.x, at.y));
            }
        }

        #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
        fn operation_mask(&self, _session: &NSDraggingSession, _context: NSDraggingContext) -> NSDragOperation {
            match self.ivars().shared.cancelled.load(Ordering::SeqCst) {
                true => NSDragOperation::None,
                false => NSDragOperation::Copy,
            }
        }

        #[unsafe(method(draggingSession:endedAtPoint:operation:))]
        fn ended(&self, _session: &NSDraggingSession, at: NSPoint, operation: NSDragOperation) {
            let shared = self.ivars().shared.clone();
            // The release that ended it was posted already (the peer's, or ours on cancel).
            LEFT_HELD.store(false, Ordering::SeqCst);
            let took = operation != NSDragOperation::None && !shared.cancelled.load(Ordering::SeqCst);
            flowseam_core::log::line(&format!("flowseam native drop: drag {} ended at ({}, {}), operation {}, taken: {took}", shared.drag, at.x, at.y, operation.0));
            finish(&shared);
            shared.end(took);
        }
    }
);

impl DragView {
    fn new(mtm: MainThreadMarker, shared: Arc<Shared>, size: NSSize) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ViewIvars { shared });
        // SAFETY: NSView's designated initialiser.
        unsafe { msg_send![super(this), initWithFrame: NSRect::new(NSPoint::new(0.0, 0.0), size)] }
    }
}

struct PromiseIvars {
    shared: Arc<Shared>,
    index: usize,
    queue: Retained<NSOperationQueue>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; Promise has no Drop.
    #[unsafe(super(NSObject))]
    #[name = "FlowseamFilePromise"]
    #[ivars = PromiseIvars]
    struct Promise;

    unsafe impl NSObjectProtocol for Promise {}

    unsafe impl NSFilePromiseProviderDelegate for Promise {
        #[unsafe(method_id(filePromiseProvider:fileNameForType:))]
        fn file_name(&self, _provider: &NSFilePromiseProvider, _file_type: &NSString) -> Retained<NSString> {
            let iv = self.ivars();
            NSString::from_str(&iv.shared.staged.files()[iv.index].name)
        }

        #[unsafe(method_id(operationQueueForFilePromiseProvider:))]
        fn operation_queue(&self, _provider: &NSFilePromiseProvider) -> Retained<NSOperationQueue> {
            self.ivars().queue.clone()
        }

        /// On our own queue, so waiting here for bytes still on the wire holds up nothing else.
        #[unsafe(method(filePromiseProvider:writePromiseToURL:completionHandler:))]
        fn write(&self, _provider: &NSFilePromiseProvider, url: &NSURL, done: &DynBlock<dyn Fn(*mut NSError)>) {
            let iv = self.ivars();
            flowseam_core::log::line(&format!("flowseam native drop: drag {} file {} asked for at {:?}", iv.shared.drag, iv.index, url.path()));
            let result = if iv.shared.cancelled.load(Ordering::SeqCst) {
                Err("拖曳已取消".to_string())
            } else {
                match url.path() {
                    Some(p) => iv.shared.staged.deliver(iv.index, &PathBuf::from(p.to_string())),
                    None => Err("目的地不是檔案位置".to_string()),
                }
            };
            match result {
                Ok(()) => done.call((std::ptr::null_mut(),)),
                Err(e) => {
                    flowseam_core::log::line(&format!("flowseam native drop: file {} of drag {}: {e}", iv.index, iv.shared.drag));
                    let err = NSError::new(1, &NSString::from_str("Flowseam"));
                    done.call((Retained::as_ptr(&err) as *mut NSError,));
                }
            }
            forget_promise(self);
        }
    }
);

// SAFETY: the ivars are Send + Sync (an Arc of Send + Sync state, an index, an operation queue, which is
// thread-safe), and retain/release are thread-safe; the delegate is called on the main thread and on its queue.
unsafe impl Send for Promise {}
// SAFETY: as above; every field is read-only after init or synchronised.
unsafe impl Sync for Promise {}

impl Promise {
    fn new(shared: Arc<Shared>, index: usize) -> Retained<Self> {
        let queue = NSOperationQueue::new();
        let this = Self::alloc().set_ivars(PromiseIvars { shared, index, queue });
        // SAFETY: NSObject's init.
        unsafe { msg_send![super(this), init] }
    }
}

#[cfg(test)]
mod tests {
    use super::screen_index;

    /// The test Mac's layout in AppKit coordinates: a 2560x1440 main screen and a 1920x1080 screen to its right whose
    /// top is 157 px below the main screen's top.
    const SCREENS: [(f64, f64, f64, f64); 2] = [(0.0, 0.0, 2560.0, 1440.0), (2560.0, 203.0, 1920.0, 1080.0)];

    #[test]
    fn the_screen_is_the_one_under_the_cursor_not_the_main_one() {
        assert_eq!(screen_index((1280.0, 720.0), &SCREENS), Some(0), "the parked cursor, main screen");
        assert_eq!(screen_index((4455.0, 430.0), &SCREENS), Some(1), "home at the right edge of the second screen");
        assert_eq!(screen_index((2560.0, 203.0), &SCREENS), Some(1), "a frame includes its own origin");
        assert_eq!(screen_index((2559.9, 300.0), &SCREENS), Some(0));
        assert_eq!(screen_index((4480.0, 430.0), &SCREENS), None, "past the right edge of every screen");
        assert_eq!(screen_index((3000.0, 100.0), &SCREENS), None, "the gap under the second screen");
    }
}
