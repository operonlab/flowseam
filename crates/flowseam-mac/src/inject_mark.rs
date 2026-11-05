//! Tag on every event Flowseam posts, so the local tap can tell its own injection from real input.
//! Without it a remote key injected here would be captured as a local key and sent back.
//! The tag lives in the CGEvent "source user data" field.

/// kCGEventSourceUserData.
pub const FIELD: u32 = 42;
/// "kmshare" in ASCII: the project's former name, kept so the value never changes under a running build.
pub const MARK: i64 = 0x006B_6D73_6861_7265;

/// The tap's test for the value it read from [`FIELD`].
pub fn is_ours(user_data: i64) -> bool {
    user_data == MARK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_and_check_are_a_pair_and_zero_is_not_ours() {
        assert!(is_ours(MARK));
        assert!(!is_ours(0), "unmarked events carry 0");
        assert!(!is_ours(MARK ^ 1));
        assert!(!is_ours(-1));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn posted_events_carry_the_mark_the_tap_checks() {
        use crate::ffi;
        // Same construction `sys::post_key` uses; no event is posted.
        let ev = unsafe { ffi::CGEventCreateKeyboardEvent(std::ptr::null(), 0, true) };
        assert!(!ev.is_null());
        assert!(!is_ours(unsafe { ffi::CGEventGetIntegerValueField(ev, FIELD) }), "fresh event is unmarked");
        crate::sys::mark(ev);
        assert!(is_ours(unsafe { ffi::CGEventGetIntegerValueField(ev, FIELD) }));
        unsafe { ffi::CFRelease(ev as *const std::ffi::c_void) };
    }
}
