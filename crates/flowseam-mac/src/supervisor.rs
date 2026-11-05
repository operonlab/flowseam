//! Supervisor side of the worker pair (design §5). The supervisor holds no
//! Accessibility dependency: it only restores the cursor when the worker dies
//! without finishing cleanly. A SIGKILLed worker cannot run its own restore,
//! so this is the only thing that can undo a hide it left behind.
//!
//! Pipe protocol: the worker writes [`CLEAN`] right before a normal exit, after
//! its own restore. EOF without that marker means the worker died (crash,
//! SIGKILL, OOM) and the cursor may still be hidden.

use crate::grab::{remove_lock, CursorOs};
use flowseam_core::backend::BackendError;
use std::io::Read;
use std::path::Path;

pub const CLEAN: &[u8] = b"CLEAN";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerEnd {
    /// The worker wrote the clean marker and restored its own cursor.
    Clean,
    /// EOF without the marker, or the pipe read failed. Treated as a death.
    Died,
}

/// Block until the worker's end of the pipe closes, then classify how it ended.
pub fn await_worker(mut pipe: impl Read) -> WorkerEnd {
    let mut buf = Vec::new();
    match pipe.read_to_end(&mut buf) {
        // The worker writes `CLEAN\n`; one trailing newline is part of the marker.
        Ok(_) if buf.strip_suffix(b"\n").unwrap_or(&buf) == CLEAN => WorkerEnd::Clean,
        _ => WorkerEnd::Died,
    }
}

/// Wait for the worker, then restore the cursor if it died. Returns how the worker ended.
pub fn supervise(pipe: impl Read, os: &mut dyn CursorOs, lock_path: &Path) -> Result<WorkerEnd, BackendError> {
    let end = await_worker(pipe);
    if end == WorkerEnd::Died {
        restore_after_death(os, lock_path)?;
    }
    Ok(end)
}

/// The restore owed after an unclean death. Show and reassociate are both attempted even if the
/// first fails. The lock is removed only when both succeed, so a failed restore leaves it for retry.
fn restore_after_death(os: &mut dyn CursorOs, lock_path: &Path) -> Result<(), BackendError> {
    let shown = os.show_cursor();
    let assoc = os.set_association(true);
    shown.and(assoc)?;
    remove_lock(lock_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grab::mock::{Log, MockOs};
    use flowseam_core::backend::{Permission, Preflight};
    use std::fs;
    use std::io::Write;

    fn os(fail_show: bool) -> (MockOs, Log) {
        let log = Log::default();
        let os = MockOs {
            log: log.clone(),
            preflight: Preflight::Missing(Permission::Accessibility),
            secure: Ok(Some(false)),
            fail_show,
        };
        (os, log)
    }

    /// A lock file as the worker left it: the cursor is hidden and the lock records the grab.
    fn lock_for(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("flowseam-mac-sup-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("grab.lock");
        fs::write(&path, "pid=1\n").unwrap();
        path
    }

    /// M-B: a worker that dies without the clean marker (the SIGKILL shape) gets the cursor restored
    /// and the lock removed. The worker end of the pipe is dropped, not killed.
    #[test]
    fn eof_without_clean_marker_restores_cursor_and_lock() {
        let (mut mock, log) = os(false);
        let lock = lock_for("died");
        let (r, w) = std::io::pipe().unwrap();
        drop(w);
        let end = supervise(r, &mut mock, &lock).unwrap();
        assert_eq!(end, WorkerEnd::Died);
        assert_eq!(log.count("show"), 1, "cursor must be shown: {:?}", log.calls());
        assert_eq!(log.count("assoc:true"), 1, "mouse must be reassociated: {:?}", log.calls());
        assert!(!lock.exists(), "lock must be removed after restore");
    }

    /// M-B: a worker that finished cleanly already restored itself, so the supervisor adds no restore.
    #[test]
    fn clean_marker_does_not_restore_again() {
        let (mut mock, log) = os(false);
        let lock = lock_for("clean");
        let (r, mut w) = std::io::pipe().unwrap();
        w.write_all(CLEAN).unwrap();
        drop(w);
        let end = supervise(r, &mut mock, &lock).unwrap();
        assert_eq!(end, WorkerEnd::Clean);
        assert!(log.calls().is_empty(), "no OS call expected on clean exit: {:?}", log.calls());
    }

    /// A failed restore keeps the lock file so startup recovery can retry, and reports the error.
    #[test]
    fn failed_restore_keeps_lock_for_recovery() {
        let (mut mock, _log) = os(true);
        let lock = lock_for("fail");
        let (r, w) = std::io::pipe().unwrap();
        drop(w);
        assert!(supervise(r, &mut mock, &lock).is_err());
        assert!(lock.exists(), "lock must survive a failed restore");
    }

    /// A pipe that errors mid-read is treated as a death, so the cursor still gets restored.
    #[test]
    fn read_error_counts_as_death() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("pipe broke"))
            }
        }
        assert_eq!(await_worker(Broken), WorkerEnd::Died);
    }

    /// Only the exact marker is a clean end: anything else, even one containing it, is a death.
    #[test]
    fn non_clean_payload_counts_as_death_and_restores() {
        for payload in [&b"OOPS"[..], &b"CLEAN!!"[..], &b"CLEA"[..]] {
            let (mut mock, log) = os(false);
            let lock = lock_for("garbage");
            let (r, mut w) = std::io::pipe().unwrap();
            w.write_all(payload).unwrap();
            drop(w);
            let end = supervise(r, &mut mock, &lock).unwrap();
            assert_eq!(end, WorkerEnd::Died, "payload {payload:?}");
            assert_eq!(log.count("show"), 1, "payload {payload:?}: {:?}", log.calls());
        }
    }

    /// The worker writes `CLEAN\n`; that is the same clean end, but two newlines are not.
    #[test]
    fn clean_marker_with_one_trailing_newline_is_clean() {
        for (payload, want) in [(&b"CLEAN\n"[..], WorkerEnd::Clean), (&b"CLEAN\n\n"[..], WorkerEnd::Died)] {
            let (r, mut w) = std::io::pipe().unwrap();
            w.write_all(payload).unwrap();
            drop(w);
            assert_eq!(await_worker(r), want, "payload {payload:?}");
        }
    }
}
