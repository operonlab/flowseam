//! Worker child-process lifecycle. The GUI never taps input itself: it spawns its own
//! executable as `worker --config <path>` and judges how it ended from the stdout pipe.

use std::io::{Cursor, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use flowseam_mac::supervisor::{await_worker, supervise, WorkerEnd};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerStatus {
    Stopped,
    Running,
    Died(String),
}

/// Pipe contents + exit code → final status. Delegates the CLEAN judgement to the supervisor's
/// own function so the GUI and the supervisor cannot disagree on what "clean" means.
#[cfg(test)]
pub fn classify_end(pipe: &[u8], exit_code: Option<i32>) -> WorkerStatus {
    classify_end_restored(pipe, exit_code, None)
}

/// `restore` is the supervisor's restore outcome after a death: `Some(Err)` means the cursor may still be hidden.
fn classify_end_restored(pipe: &[u8], exit_code: Option<i32>, restore: Option<Result<(), String>>) -> WorkerStatus {
    let note = match restore {
        Some(Ok(())) => "；已還原游標".to_string(),
        Some(Err(e)) => format!("；游標還原失敗（可能仍被隱藏）：{e}"),
        None => String::new(),
    };
    match await_worker(Cursor::new(pipe)) {
        WorkerEnd::Clean => WorkerStatus::Stopped,
        WorkerEnd::Died => {
            let exit = exit_code.map_or("exit 未知（被 signal 終止或查不到）".to_string(), |c| format!("exit {c}"));
            WorkerStatus::Died(format!("stdout EOF 但沒有 CLEAN（{exit}）{note}"))
        }
    }
}

/// What the reaper thread hands back: stdout bytes, exit code, supervisor restore outcome.
type WorkerDone = (Vec<u8>, Option<i32>, Option<Result<(), String>>);

pub struct WorkerHandle {
    pid: u32,
    stdin: Option<std::process::ChildStdin>,
    done: Receiver<WorkerDone>,
    pub stop_requested: bool,
}

impl WorkerHandle {
    pub fn spawn(config: &Path) -> std::io::Result<Self> {
        let exe = std::env::current_exe()?;
        let mut child: Child = Command::new(exe)
            .arg("worker")
            .arg("--config")
            .arg(config)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let pid = child.id();
        let stdin = child.stdin.take();
        let mut out = child.stdout.take().expect("stdout 已設為 piped");
        let (tx, done) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = out.read_to_end(&mut buf);
            let code = child.wait().ok().and_then(|s| s.code());
            // Supervisor duty: an unclean end restores the cursor (the worker may have been SIGKILLed mid-hide).
            let restore = match flowseam_mac::MacBackend::default_lock_path() {
                None => Some(Err("HOME 未設定，找不到 grab.lock".to_string())),
                Some(lock) => {
                    let mut os = flowseam_mac::worker::real_cursor_os();
                    match supervise(Cursor::new(&buf), &mut *os, &lock) {
                        Ok(WorkerEnd::Died) => Some(Ok(())),
                        Ok(WorkerEnd::Clean) => None,
                        Err(e) => Some(Err(format!("{e:?}"))),
                    }
                }
            };
            let _ = tx.send((buf, code, restore));
        });
        Ok(WorkerHandle { pid, stdin, done, stop_requested: false })
    }

    /// `None` while the child is still running.
    pub fn poll(&self) -> Option<WorkerStatus> {
        self.done.try_recv().ok().map(|(buf, code, restore)| classify_end_restored(&buf, code, restore))
    }

    /// Graceful first (close stdin); the GUI tick then calls `escalate` for SIGTERM, then SIGKILL. A forced kill carries no CLEAN,
    /// so it is reported as died — which is true, and makes the supervisor restore the cursor.
    pub fn stop(&mut self) {
        self.stop_requested = true;
        self.stdin = None;
    }

    /// `since` is when `stop` was requested.
    pub fn escalate(&self, since: Instant) {
        let waited = since.elapsed();
        if waited > Duration::from_secs(3) {
            let sig = if waited > Duration::from_secs(6) { "-KILL" } else { "-TERM" };
            let _ = Command::new("kill").arg(sig).arg(self.pid.to_string()).status();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_marker_is_stopped() {
        assert_eq!(classify_end(b"CLEAN", Some(0)), WorkerStatus::Stopped);
    }

    #[test]
    fn empty_pipe_is_died_even_with_exit_zero() {
        match classify_end(b"", Some(0)) {
            WorkerStatus::Died(m) => assert!(m.contains("CLEAN") && m.contains("exit 0"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn partial_or_garbage_marker_is_died() {
        for p in [&b"CLEA"[..], b"CLEANX", b"xCLEAN", b"hello"] {
            assert!(matches!(classify_end(p, None), WorkerStatus::Died(_)), "{p:?}");
        }
    }

    #[test]
    fn failed_restore_is_surfaced_not_swallowed() {
        match classify_end_restored(b"", Some(9), Some(Err("boom".into()))) {
            WorkerStatus::Died(m) => assert!(m.contains("還原失敗") && m.contains("boom"), "{m}"),
            other => panic!("{other:?}"),
        }
        match classify_end_restored(b"", Some(9), Some(Ok(()))) {
            WorkerStatus::Died(m) => assert!(m.contains("已還原游標"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn missing_exit_code_is_called_unknown_not_zero() {
        match classify_end(b"", None) {
            WorkerStatus::Died(m) => assert!(m.contains("未知"), "{m}"),
            other => panic!("{other:?}"),
        }
    }
}
