//! A repeating run-loop timer in the common modes: it keeps firing while AppKit runs its own event-tracking
//! loop on the thread (a drag session, a menu), when the worker's main loop does not turn.

use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::Duration;

use crate::ffi;

pub struct Heartbeat {
    timer: *mut c_void,
    callback: *mut Box<dyn Fn() + 'static>,
}

extern "C" fn fire(_timer: *mut c_void, info: *mut c_void) {
    // SAFETY: the context points to the boxed callback owned by Heartbeat until timer invalidation.
    let callback = unsafe { &*(info as *const Box<dyn Fn() + 'static>) };
    let _ = catch_unwind(AssertUnwindSafe(callback));
}

/// The timer fires in any common mode, including AppKit's event-tracking mode for drags and menus.
/// If the thread stops running its run loop altogether, the timer stops too, so the watchdog catches a real hang.
pub fn keep_beating(run_loop: *mut c_void, every: Duration, f: Box<dyn Fn() + 'static>) -> Heartbeat {
    let callback = Box::into_raw(Box::new(f));
    let mut context = ffi::CFRunLoopTimerContext {
        version: 0,
        info: callback.cast(),
        retain: None,
        release: None,
        copy_description: None,
    };
    // SAFETY: CoreFoundation copies the context, and Heartbeat keeps its info alive until invalidation.
    let timer = unsafe { ffi::CFRunLoopTimerCreate(std::ptr::null(), ffi::CFAbsoluteTimeGetCurrent() + every.as_secs_f64(), every.as_secs_f64(), 0, 0, fire, &mut context) };
    assert!(!timer.is_null(), "CFRunLoopTimerCreate failed");
    // SAFETY: the timer and run loop are valid CoreFoundation objects on the calling thread.
    unsafe { ffi::CFRunLoopAddTimer(run_loop, timer, ffi::kCFRunLoopCommonModes) };
    Heartbeat { timer, callback }
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
        let heartbeat = keep_beating(unsafe { ffi::CFRunLoopGetCurrent() }, Duration::from_millis(20), Box::new(move || { seen.fetch_add(1, Ordering::SeqCst); }));
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
        let heartbeat = keep_beating(unsafe { ffi::CFRunLoopGetCurrent() }, Duration::from_millis(20), Box::new(move || { seen.fetch_add(1, Ordering::SeqCst); }));
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
}
