//! A repeating run-loop timer in the common modes: it keeps firing while AppKit runs its own event-tracking
//! loop on the thread (a drag session, a menu), when the worker's main loop does not turn.

use std::ffi::c_void;
use std::cell::Cell;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::ffi;

pub struct Heartbeat {
    timer: *mut c_void,
    run_loop: *mut c_void,
    callback: *mut Callback,
}

struct Callback {
    f: Box<dyn Fn() -> Duration>,
    interval: Cell<Duration>,
    /// Counts pokes, so `fire` can tell one landed while the callback ran.
    pokes: Arc<AtomicU64>,
}

/// Retained references let the tap thread advance a sleeping main-thread timer.
pub struct Poker {
    timer: *mut c_void,
    run_loop: *mut c_void,
    pokes: Arc<AtomicU64>,
}

// SAFETY: Core Foundation's timer date and run-loop wake operations are thread-safe.
unsafe impl Send for Poker {}
unsafe impl Sync for Poker {}

impl Poker {
    pub fn poke(&self) {
        self.pokes.fetch_add(1, Ordering::SeqCst);
        // SAFETY: both references are retained until this handle is dropped.
        unsafe {
            ffi::CFRunLoopTimerSetNextFireDate(self.timer, ffi::CFAbsoluteTimeGetCurrent());
            ffi::CFRunLoopWakeUp(self.run_loop);
        }
    }
}

impl Drop for Poker {
    fn drop(&mut self) {
        // SAFETY: this handle owns one reference to each object.
        unsafe {
            ffi::CFRelease(self.timer);
            ffi::CFRelease(self.run_loop);
        }
    }
}

impl Heartbeat {
    pub fn poker(&self) -> Poker {
        // SAFETY: Core Foundation retains both live objects for the handle.
        unsafe {
            Poker {
                timer: ffi::CFRetain(self.timer) as *mut c_void,
                run_loop: ffi::CFRetain(self.run_loop) as *mut c_void,
                pokes: (*self.callback).pokes.clone(),
            }
        }
    }
}

extern "C" fn fire(timer: *mut c_void, info: *mut c_void) {
    // SAFETY: the context points to the boxed callback owned by Heartbeat until timer invalidation.
    let callback = unsafe { &*(info as *const Callback) };
    let pokes = callback.pokes.load(Ordering::SeqCst);
    let interval = catch_unwind(AssertUnwindSafe(|| (callback.f)())).unwrap_or_else(|_| callback.interval.get());
    callback.interval.set(interval);
    // SAFETY: the timer is live for the duration of its callback.
    unsafe { ffi::CFRunLoopTimerSetNextFireDate(timer, ffi::CFAbsoluteTimeGetCurrent() + interval.as_secs_f64()) };
    // A poke made while the callback ran had its date overwritten just above: honour it.
    if callback.pokes.load(Ordering::SeqCst) != pokes {
        // SAFETY: as above.
        unsafe { ffi::CFRunLoopTimerSetNextFireDate(timer, ffi::CFAbsoluteTimeGetCurrent()) };
    }
}

/// The timer fires in any common mode, including AppKit's event-tracking mode for drags and menus.
/// If the thread stops running its run loop altogether, the timer stops too, so the watchdog catches a real hang.
pub fn keep_beating(run_loop: *mut c_void, first: Duration, f: Box<dyn Fn() -> Duration>) -> Heartbeat {
    let callback = Box::into_raw(Box::new(Callback { f, interval: Cell::new(first), pokes: Arc::new(AtomicU64::new(0)) }));
    let mut context = ffi::CFRunLoopTimerContext {
        version: 0,
        info: callback.cast(),
        retain: None,
        release: None,
        copy_description: None,
    };
    // SAFETY: CoreFoundation copies the context, and Heartbeat keeps its info alive until invalidation.
    let timer = unsafe { ffi::CFRunLoopTimerCreate(std::ptr::null(), ffi::CFAbsoluteTimeGetCurrent() + first.as_secs_f64(), first.as_secs_f64(), 0, 0, fire, &mut context) };
    assert!(!timer.is_null(), "CFRunLoopTimerCreate failed");
    // SAFETY: the timer and run loop are valid CoreFoundation objects on the calling thread.
    unsafe { ffi::CFRunLoopAddTimer(run_loop, timer, ffi::kCFRunLoopCommonModes) };
    Heartbeat { timer, run_loop, callback }
}

impl Drop for Heartbeat {
    fn drop(&mut self) {
        // SAFETY: invalidation removes the timer before its callback storage is freed; Heartbeat owns both references.
        unsafe {
            ffi::CFRunLoopTimerInvalidate(self.timer);
            ffi::CFRelease(self.timer);
            drop(Box::from_raw(self.callback));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn tracking_mode() -> ffi::CFTypeRef {
        // SAFETY: the C string is terminated and the returned CFString is kept alive for this test thread.
        let mode = unsafe { ffi::CFStringCreateWithCString(std::ptr::null(), c"FlowseamHeartbeatTrackingMode".as_ptr(), ffi::kCFStringEncodingUTF8) };
        assert!(!mode.is_null());
        // SAFETY: the current run loop and mode are valid CoreFoundation objects.
        unsafe { ffi::CFRunLoopAddCommonMode(ffi::CFRunLoopGetCurrent(), mode) };
        mode
    }

    #[test]
    fn beats_in_a_tracking_mode() {
        let mode = tracking_mode();
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        // SAFETY: the timer is installed and run on this test thread's current run loop.
        let heartbeat = keep_beating(unsafe { ffi::CFRunLoopGetCurrent() }, Duration::from_millis(20), Box::new(move || { seen.fetch_add(1, Ordering::SeqCst); Duration::from_millis(20) }));
        // SAFETY: mode belongs to this thread's run loop and remains alive through this call.
        // Run a full second: the OS coalesces timers of a background test process to ~100 ms, so 0.3 s only yields 3-4 beats.
        unsafe { ffi::CFRunLoopRunInMode(mode, 1.0, false) };
        assert!(count.load(Ordering::SeqCst) >= 5);
        drop(heartbeat);
        // SAFETY: this test owns the CFString reference.
        unsafe { ffi::CFRelease(mode) };
    }

    #[test]
    fn stops_when_dropped() {
        let mode = tracking_mode();
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        // SAFETY: the timer is installed on this test thread's current run loop.
        let heartbeat = keep_beating(unsafe { ffi::CFRunLoopGetCurrent() }, Duration::from_millis(20), Box::new(move || { seen.fetch_add(1, Ordering::SeqCst); Duration::from_millis(20) }));
        drop(heartbeat);
        let before = count.load(Ordering::SeqCst);
        // SAFETY: mode belongs to this thread's run loop and remains alive through this call.
        unsafe { ffi::CFRunLoopRunInMode(mode, 0.2, false) };
        assert_eq!(count.load(Ordering::SeqCst), before);
        // SAFETY: this test owns the CFString reference.
        unsafe { ffi::CFRelease(mode) };
    }

    #[test]
    fn a_panicking_callback_does_not_unwind() {
        let mode = tracking_mode();
        // SAFETY: the timer is installed and run on this test thread's current run loop.
        let heartbeat = keep_beating(unsafe { ffi::CFRunLoopGetCurrent() }, Duration::from_millis(20), Box::new(|| panic!("expected callback panic")));
        // SAFETY: mode belongs to this thread's run loop and remains alive through this call.
        unsafe { ffi::CFRunLoopRunInMode(mode, 0.1, false) };
        drop(heartbeat);
        // SAFETY: this test owns the CFString reference.
        unsafe { ffi::CFRelease(mode) };
    }

    #[test]
    fn callback_can_slow_its_next_fire() {
        let mode = tracking_mode();
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        let heartbeat = keep_beating(unsafe { ffi::CFRunLoopGetCurrent() }, Duration::from_millis(20), Box::new(move || {
            seen.fetch_add(1, Ordering::SeqCst);
            Duration::from_secs(1)
        }));
        unsafe { ffi::CFRunLoopRunInMode(mode, 0.3, false) };
        assert_eq!(count.load(Ordering::SeqCst), 1);
        drop(heartbeat);
        unsafe { ffi::CFRelease(mode) };
    }

    #[test]
    fn another_thread_can_poke_a_distant_timer() {
        let mode = tracking_mode();
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        let heartbeat = keep_beating(unsafe { ffi::CFRunLoopGetCurrent() }, Duration::from_secs(10), Box::new(move || {
            seen.fetch_add(1, Ordering::SeqCst);
            Duration::from_secs(10)
        }));
        let poker = heartbeat.poker();
        let thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            poker.poke();
        });
        unsafe { ffi::CFRunLoopRunInMode(mode, 0.3, false) };
        thread.join().unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
        drop(heartbeat);
        unsafe { ffi::CFRelease(mode) };
    }

    /// A poke that lands while the callback runs (it decided "slow" a moment before the tap thread made the
    /// clock matter) must not be overwritten by the interval the callback returns.
    #[test]
    fn a_poke_during_the_callback_still_brings_the_next_fire_forward() {
        let mode = tracking_mode();
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        let slot: Arc<std::sync::Mutex<Option<Poker>>> = Arc::new(std::sync::Mutex::new(None));
        let inner = slot.clone();
        let heartbeat = keep_beating(unsafe { ffi::CFRunLoopGetCurrent() }, Duration::from_millis(20), Box::new(move || {
            if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                if let Some(p) = &*inner.lock().unwrap() { p.poke(); }
            }
            Duration::from_secs(10)
        }));
        *slot.lock().unwrap() = Some(heartbeat.poker());
        // SAFETY: mode belongs to this thread's run loop and remains alive through this call.
        unsafe { ffi::CFRunLoopRunInMode(mode, 0.5, false) };
        assert_eq!(count.load(Ordering::SeqCst), 2, "the poke made in the first fire brings a second one");
        slot.lock().unwrap().take();
        drop(heartbeat);
        // SAFETY: this test owns the CFString reference.
        unsafe { ffi::CFRelease(mode) };
    }
}
