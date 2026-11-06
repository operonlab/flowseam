//! Worker child-process lifecycle. The GUI never taps input itself: it spawns its own
//! executable as `worker --config <path>` and judges how it ended from the stdout pipe.

#[cfg(target_os = "macos")]
use std::io::{Cursor, Read};
use std::path::Path;
use std::fs::{File, OpenOptions};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
use flowseam_mac::supervisor::{await_worker, supervise, WorkerEnd};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerStatus {
    Stopped,
    Running,
    Died(String),
    /// The worker ended cleanly because the system asked the app to quit; the window quits too.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    QuitRequested,
}

/// Two windows can both find the lock free and both start a worker; the one that loses says so this way.
const ALREADY_SHARING: &str = "沒有啟動：這台已經有另一個 Flowseam 在分享（可能是另一個視窗）";

/// Pipe contents + exit code → final status. Delegates the CLEAN judgement to the supervisor's
/// own function so the GUI and the supervisor cannot disagree on what "clean" means.
#[cfg(all(test, target_os = "macos"))]
pub fn classify_end(pipe: &[u8], exit_code: Option<i32>) -> WorkerStatus {
    classify_end_restored(pipe, exit_code, None)
}

/// `restore` is the supervisor's restore outcome after a death: `Some(Err)` means the cursor may still be hidden.
#[cfg(target_os = "macos")]
fn classify_end_restored(
    pipe: &[u8],
    exit_code: Option<i32>,
    restore: Option<Result<(), String>>,
) -> WorkerStatus {
    let note = match restore {
        Some(Ok(())) => "；已還原游標".to_string(),
        Some(Err(e)) => format!("；游標還原失敗（可能仍被隱藏）：{e}"),
        None => String::new(),
    };
    match await_worker(Cursor::new(pipe)) {
        // A clean end (it touched nothing), but it never started: not a stop the user asked for.
        WorkerEnd::Clean if exit_code == Some(crate::EXIT_ALREADY_SHARING) => WorkerStatus::Died(ALREADY_SHARING.into()),
        WorkerEnd::Clean => WorkerStatus::Stopped,
        WorkerEnd::Quit => WorkerStatus::QuitRequested,
        WorkerEnd::Died => {
            let exit = exit_code.map_or("exit 未知（被 signal 終止或查不到）".to_string(), |c| format!("exit {c}"));
            WorkerStatus::Died(format!("stdout EOF 但沒有 CLEAN（{exit}）{note}"))
        }
    }
}

/// What the reaper thread hands back: stdout bytes, exit code, supervisor restore outcome.
#[cfg(target_os = "macos")]
type WorkerDone = (Vec<u8>, Option<i32>, Option<Result<(), String>>);
#[cfg(windows)]
type WorkerDone = Option<i32>;

pub struct WorkerHandle {
    pid: u32,
    stdin: Option<std::process::ChildStdin>,
    done: Receiver<WorkerDone>,
    pub stop_requested: bool,
    pub started: Instant,
    /// `started` on the wall clock, to tell this worker's status writes from an older one's.
    pub started_ms: u64,
}

/// Where the worker's messages go: a GUI-started worker has no terminal, and without this they were lost.
/// The previous run's log is kept beside it as `worker.log.1`.
pub fn log_path() -> Option<std::path::PathBuf> {
    #[cfg(target_os = "macos")]
    let dir = std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join("Library/Logs/Flowseam"));
    #[cfg(windows)]
    let dir = std::env::var_os("LOCALAPPDATA").map(|d| std::path::PathBuf::from(d).join("Flowseam"));
    dir.map(|d| d.join("worker.log"))
}

fn child_log(path: &Path) -> std::io::Result<File> {
    if let Some(dir) = path.parent() { std::fs::create_dir_all(dir)?; }
    if path.exists() { std::fs::rename(path, path.with_extension("log.1"))?; }
    OpenOptions::new().create(true).append(true).open(path)
}

pub fn log_event(message: &str) {
    use std::io::Write;
    if let Some(path) = log_path() {
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
            let ms = flowseam_core::status::now_ms();
            let _ = writeln!(file, "{}", flowseam_core::log::format_at(ms, message));
        }
    }
}

/// Returns the delay for a death, or None when sharing was deliberately stopped.
pub fn next_restart(history: &[Instant], now: Instant) -> Option<Duration> {
    let latest = *history.last()?;
    let deaths = if now.saturating_duration_since(latest) >= Duration::from_secs(60) { 0 } else { history.len().saturating_sub(1) };
    Some(Duration::from_secs([1, 2, 5, 10, 30][deaths.min(4)]))
}

/// How stale this worker's status is, in seconds, when it is stalled. Only a status this worker has written
/// counts: a worker with no peers writes none, and a file left by the previous worker says nothing about this
/// one, so either is "cannot tell", never "stalled".
pub fn stalled(started: Instant, started_ms: u64, now: Instant, written_ms: Option<u64>, now_ms: u64) -> Option<u64> {
    if now.saturating_duration_since(started) < Duration::from_secs(15) { return None; }
    let ms = written_ms.filter(|&ms| ms >= started_ms && ms <= now_ms)?;
    let age = (now_ms - ms) / 1000;
    (age > 10).then_some(age)
}

impl WorkerHandle {
    pub fn spawn(config: &Path) -> std::io::Result<Self> {
        let exe = std::env::current_exe()?;
        let mut command = Command::new(exe);
        let stderr = log_path().map(|path| child_log(&path)).transpose()?;
        command
            .arg("worker")
            .arg("--config")
            .arg(config)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr.map_or_else(Stdio::inherit, Stdio::from));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let mut child: Child = command.spawn()?;
        let pid = child.id();
        let stdin = child.stdin.take();
        let mut out = child.stdout.take().expect("stdout 已設為 piped");
        let (tx, done) = mpsc::channel();
        #[cfg(target_os = "macos")]
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
                        Ok(WorkerEnd::Clean | WorkerEnd::Quit) => None,
                        Err(e) => Some(Err(format!("{e:?}"))),
                    }
                }
            };
            let _ = tx.send((buf, code, restore));
        });
        #[cfg(windows)]
        std::thread::spawn(move || {
            let _ = std::io::copy(&mut out, &mut std::io::sink());
            let _ = tx.send(child.wait().ok().and_then(|s| s.code()));
        });
        Ok(WorkerHandle {
            pid,
            stdin,
            done,
            stop_requested: false,
            started: Instant::now(),
            started_ms: flowseam_core::status::now_ms(),
        })
    }

    /// `None` while the child is still running.
    pub fn poll(&self) -> Option<WorkerStatus> {
        #[cfg(target_os = "macos")]
        {
            self.done
                .try_recv()
                .ok()
                .map(|(buf, code, restore)| classify_end_restored(&buf, code, restore))
        }
        #[cfg(windows)]
        {
            self.done.try_recv().ok().map(|code| {
                if code == Some(crate::EXIT_ALREADY_SHARING) {
                    WorkerStatus::Died(ALREADY_SHARING.into())
                } else if self.stop_requested {
                    WorkerStatus::Stopped
                } else {
                    WorkerStatus::Died(
                        code.map_or("exit 未知".to_string(), |c| format!("exit {c}")),
                    )
                }
            })
        }
    }

    /// Graceful first (close stdin); the GUI tick then calls `escalate` for SIGTERM, then SIGKILL. A forced kill carries no CLEAN,
    /// so it is reported as died — which is true, and makes the supervisor restore the cursor.
    pub fn stop(&mut self) {
        self.stop_requested = true;
        self.stdin = None;
    }

    pub fn kill_stalled(&self) {
        #[cfg(windows)]
        unsafe {
            use windows_sys::Win32::Foundation::CloseHandle;
            use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
            let process = OpenProcess(PROCESS_TERMINATE, 0, self.pid);
            if !process.is_null() {
                let _ = TerminateProcess(process, 1);
                let _ = CloseHandle(process);
            }
        }
        #[cfg(target_os = "macos")]
        let _ = Command::new("kill").arg("-KILL").arg(self.pid.to_string()).status();
    }

    /// `since` is when `stop` was requested.
    pub fn escalate(&self, since: Instant) {
        let waited = since.elapsed();
        #[cfg(windows)]
        if waited > Duration::from_secs(3) {
            let _ = Command::new("taskkill")
                .args(["/PID", &self.pid.to_string(), "/F"])
                .status();
        }
        #[cfg(target_os = "macos")]
        {
            if waited > Duration::from_secs(3) {
                let sig = if waited > Duration::from_secs(6) {
                    "-KILL"
                } else {
                    "-TERM"
                };
                let _ = Command::new("kill")
                    .arg(sig)
                    .arg(self.pid.to_string())
                    .status();
            }
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn clean_marker_is_stopped() {
        assert_eq!(classify_end(b"CLEAN", Some(0)), WorkerStatus::Stopped);
        assert_eq!(classify_end(b"QUIT\n", Some(0)), WorkerStatus::QuitRequested);
    }

    /// The worker that lost the race to another window ends clean but did not run: the window says why.
    #[test]
    fn a_worker_that_found_another_sharing_is_not_a_stop() {
        match classify_end(b"CLEAN\n", Some(crate::EXIT_ALREADY_SHARING)) {
            WorkerStatus::Died(m) => assert!(m.contains("另一個 Flowseam"), "{m}"),
            other => panic!("{other:?}"),
        }
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

    #[test]
    fn restart_backoff_resets_after_a_minute() {
        let t = Instant::now();
        assert_eq!(next_restart(&[], t), None);
        for (n, seconds) in [1, 2, 5, 10, 30, 30].into_iter().enumerate() {
            assert_eq!(next_restart(&vec![t; n + 1], t), Some(Duration::from_secs(seconds)));
        }
        assert_eq!(next_restart(&[t, t, t], t + Duration::from_secs(60)), Some(Duration::from_secs(1)));
    }

    #[test]
    fn watchdog_has_start_grace_and_ten_second_limit() {
        let t = Instant::now();
        assert_eq!(stalled(t, 5_000, t + Duration::from_secs(14), Some(5_000), 30_000), None, "grace");
        assert_eq!(stalled(t, 5_000, t + Duration::from_secs(15), Some(20_000), 30_000), None);
        assert_eq!(stalled(t, 5_000, t + Duration::from_secs(15), Some(19_000), 30_000), Some(11));
        assert_eq!(stalled(t, 5_000, t + Duration::from_secs(60), None, 65_000), None, "no status written: cannot tell");
        assert_eq!(stalled(t, 5_000, t + Duration::from_secs(60), Some(4_000), 65_000), None, "a previous worker's file");
    }

    #[test]
    fn child_stderr_is_written_to_file() {
        let path = std::env::temp_dir().join(format!("flowseam-child-log-{}-{}", std::process::id(), flowseam_core::status::now_ms()));
        let file = child_log(&path).unwrap();
        let status = Command::new("sh").args(["-c", "echo x >&2"]).stderr(Stdio::from(file)).status().unwrap();
        assert!(status.success());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x\n");
        std::fs::remove_file(path).unwrap();
    }
}
