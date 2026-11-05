//! Pure state for the macOS backend: cursor visibility transitions, the
//! timeout circuit breaker (design §7), and the grab lock file (design §5).
//! No OS calls; every decision here is unit-testable without permissions.

use std::collections::VecDeque;

/// OS action implied by a cursor state transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorAction {
    Hide,
    Show,
}

/// `Hidden` means cursor hidden AND disassociated from the mouse (frozen).
/// `Unknown` means a failed OS call left the real state unverifiable; it is
/// treated as needing both transitions so the next call retries.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum CursorState {
    #[default]
    Visible,
    Hidden,
    Unknown,
}

impl CursorState {
    pub fn is_hidden(&self) -> bool {
        matches!(self, CursorState::Hidden)
    }

    /// The OS action needed to reach `visible`, or `None` if already there.
    /// Call [`CursorState::apply`] only after the OS action has succeeded, so
    /// a failed call leaves the state saying what the OS actually is.
    pub fn next(&self, visible: bool) -> Option<CursorAction> {
        match (self, visible) {
            (CursorState::Visible, false) | (CursorState::Unknown, false) => Some(CursorAction::Hide),
            (CursorState::Hidden, true) | (CursorState::Unknown, true) => Some(CursorAction::Show),
            _ => None,
        }
    }

    pub fn apply(&mut self, action: CursorAction) {
        *self = match action {
            CursorAction::Hide => CursorState::Hidden,
            CursorAction::Show => CursorState::Visible,
        };
    }

    pub fn mark_unknown(&mut self) {
        *self = CursorState::Unknown;
    }
}

pub const BREAKER_WINDOW_MS: u64 = 10_000;
/// More than this many timeouts inside the window tears the tap down.
pub const BREAKER_MAX: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapVerdict {
    /// Re-enable the tap in place; keep state.
    Reenable,
    /// Tear the tap down and report a fault. No automatic retry.
    Tear,
}

/// Circuit breaker over `TapDisabledByTimeout` events.
#[derive(Debug, Default)]
pub struct TimeoutBreaker {
    stamps: VecDeque<u64>,
}

impl TimeoutBreaker {
    pub fn record(&mut self, now_ms: u64) -> TapVerdict {
        self.stamps.push_back(now_ms);
        while let Some(&front) = self.stamps.front() {
            if now_ms.saturating_sub(front) > BREAKER_WINDOW_MS {
                self.stamps.pop_front();
            } else {
                break;
            }
        }
        if self.stamps.len() > BREAKER_MAX {
            TapVerdict::Tear
        } else {
            TapVerdict::Reenable
        }
    }
}

/// Contents of `grab.lock`, written before the cursor is hidden and removed
/// after it is restored. A leftover file means a previous run died mid-grab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrabLock {
    pub pid: u32,
    pub since_unix: u64,
}

/// Written into a file that outlives the process, so it keeps the format's original name: a lock left
/// by a crashed earlier build must still parse, or its hidden cursor is never recovered.
const LOCK_HEADER: &str = "kmmac-grab v1";

pub fn render_lock(lock: &GrabLock) -> String {
    format!("{LOCK_HEADER}\npid={}\nsince={}\n", lock.pid, lock.since_unix)
}

pub fn parse_lock(text: &str) -> Result<GrabLock, String> {
    let mut lines = text.lines();
    match lines.next() {
        Some(LOCK_HEADER) => {}
        other => return Err(format!("bad header: {other:?}")),
    }
    let mut pid = None;
    let mut since = None;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (k, v) = line.split_once('=').ok_or_else(|| format!("bad line: {line:?}"))?;
        match k {
            "pid" => pid = Some(v.parse::<u32>().map_err(|e| format!("pid: {e}"))?),
            "since" => since = Some(v.parse::<u64>().map_err(|e| format!("since: {e}"))?),
            _ => return Err(format!("unknown key: {k:?}")),
        }
    }
    match (pid, since) {
        (Some(pid), Some(since_unix)) => Ok(GrabLock { pid, since_unix }),
        _ => Err("missing pid or since".into()),
    }
}

/// What startup should do about an existing lock file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryPlan {
    /// No lock file: nothing to undo.
    Clean,
    /// Owner pid is dead: restore the cursor, then delete the lock.
    RestoreAndClear { pid: u32 },
    /// Owner pid is alive: another instance holds the grab. Refuse to start.
    LiveOwner { pid: u32 },
    /// Unreadable lock: restore the cursor (harmless) but keep the file and fail loud.
    Corrupt(String),
}

pub fn plan_recovery(lock_text: Option<&str>, pid_alive: impl Fn(u32) -> bool) -> RecoveryPlan {
    let Some(text) = lock_text else {
        return RecoveryPlan::Clean;
    };
    match parse_lock(text) {
        Err(e) => RecoveryPlan::Corrupt(e),
        Ok(lock) if pid_alive(lock.pid) => RecoveryPlan::LiveOwner { pid: lock.pid },
        Ok(lock) => RecoveryPlan::RestoreAndClear { pid: lock.pid },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_transitions_are_idempotent() {
        let mut c = CursorState::default();
        assert_eq!(c.next(true), None, "already visible");
        let a = c.next(false).unwrap();
        assert_eq!(a, CursorAction::Hide);
        assert_eq!(c.next(false), Some(CursorAction::Hide), "not applied yet, still needs hide");
        c.apply(a);
        assert_eq!(c.next(false), None, "second hide is a no-op");
        assert!(c.is_hidden());
        c.apply(c.next(true).unwrap());
        assert!(!c.is_hidden());
        assert_eq!(c.next(true), None);
    }

    #[test]
    fn breaker_tolerates_three_and_tears_on_fourth_in_window() {
        let mut b = TimeoutBreaker::default();
        assert_eq!(b.record(0), TapVerdict::Reenable);
        assert_eq!(b.record(100), TapVerdict::Reenable);
        assert_eq!(b.record(200), TapVerdict::Reenable);
        assert_eq!(b.record(300), TapVerdict::Tear, "4th inside 10 s");
    }

    #[test]
    fn breaker_forgets_timeouts_outside_window() {
        let mut b = TimeoutBreaker::default();
        for t in [0, 1000, 2000] {
            b.record(t);
        }
        // 0 falls out once now > 10_000; 1000 falls out at 11_001.
        assert_eq!(b.record(10_500), TapVerdict::Reenable, "[1000,2000,10500]: 3 in window");
        assert_eq!(b.record(11_100), TapVerdict::Reenable, "1000 aged out: [2000,10500,11100]");
        assert_eq!(b.record(11_200), TapVerdict::Tear, "[2000,10500,11100,11200]: 4 in window");
    }

    #[test]
    fn breaker_boundary_exactly_window_old_still_counts() {
        let mut b = TimeoutBreaker::default();
        b.record(0);
        b.record(1);
        b.record(2);
        // 0 is exactly BREAKER_WINDOW_MS old: not strictly greater, so it stays.
        assert_eq!(b.record(BREAKER_WINDOW_MS), TapVerdict::Tear);
    }

    #[test]
    fn lock_roundtrip() {
        let l = GrabLock { pid: 4242, since_unix: 1_700_000_000 };
        assert_eq!(parse_lock(&render_lock(&l)), Ok(l));
    }

    #[test]
    fn lock_rejects_garbage_and_missing_fields() {
        assert!(parse_lock("").is_err());
        assert!(parse_lock("other header\npid=1\nsince=2\n").is_err());
        assert!(parse_lock("kmmac-grab v1\npid=1\n").is_err(), "since missing");
        assert!(parse_lock("kmmac-grab v1\npid=x\nsince=2\n").is_err());
        assert!(parse_lock("kmmac-grab v1\npid=1\nsince=2\nextra=3\n").is_err());
    }

    #[test]
    fn recovery_plans_cover_every_lock_state() {
        assert_eq!(plan_recovery(None, |_| true), RecoveryPlan::Clean);
        let text = render_lock(&GrabLock { pid: 7, since_unix: 1 });
        assert_eq!(plan_recovery(Some(&text), |_| false), RecoveryPlan::RestoreAndClear { pid: 7 });
        assert_eq!(plan_recovery(Some(&text), |p| p == 7), RecoveryPlan::LiveOwner { pid: 7 });
        assert!(matches!(plan_recovery(Some("junk"), |_| false), RecoveryPlan::Corrupt(_)));
    }
}
