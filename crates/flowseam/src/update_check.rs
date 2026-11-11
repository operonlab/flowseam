use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Result { Newer(String), UpToDate, Failed(String) }

pub fn now() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |time| time.as_secs()) }

/// A check time ahead of the clock (stored while the clock was wrong) counts as never checked, or it would
/// hold off every check until the clock caught up.
pub fn due(now: u64, checked_at: u64, failed_at: Option<u64>, running: bool) -> bool {
    !running && (checked_at == 0 || checked_at > now || now - checked_at >= 86_400) && failed_at.is_none_or(|at| at > now || now - at >= 3_600)
}

pub fn parse_release(bytes: &[u8], current: &str) -> Result {
    let json: serde_json::Value = match serde_json::from_slice(bytes) { Ok(json) => json, Err(e) => return Result::Failed(format!("bad JSON: {e}")) };
    let Some(tag) = json.get("tag_name").and_then(|tag| tag.as_str()) else { return Result::Failed("missing tag_name".into()) };
    let version = tag.strip_prefix('v').unwrap_or(tag);
    if version.is_empty() || !version.split('.').all(|part| part.parse::<u64>().is_ok()) { return Result::Failed(format!("invalid tag_name: {tag}")); }
    if crate::about::compare_versions(version, current).is_gt() { Result::Newer(version.into()) } else { Result::UpToDate }
}

/// The newer version to point out in the menu and the settings hint: none for a version the user skipped, until
/// one newer than it appears. A reminder for a version already declined every day is noise. The update sheet
/// still names it.
pub fn shown_newer<'a>(result: Option<&'a Result>, skipped: &str) -> Option<&'a str> {
    match result {
        Some(Result::Newer(v)) if skipped.is_empty() || crate::about::compare_versions(v, skipped).is_gt() => Some(v.as_str()),
        _ => None,
    }
}

fn run_command(mut cmd: Command, limit: Duration) -> std::result::Result<Vec<u8>, String> {
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::null()).spawn().map_err(|e| format!("curl: {e}"))?;
    let mut stdout = child.stdout.take().expect("piped curl stdout");
    // Drain concurrently: a full pipe must not prevent curl from exiting while we wait.
    let reader = std::thread::spawn(move || { let mut bytes = Vec::new(); stdout.read_to_end(&mut bytes).map(|_| bytes) });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if started.elapsed() < limit => std::thread::sleep(Duration::from_millis(50)),
            Ok(None) => { let _ = child.kill(); let _ = child.wait(); break Err("curl did not finish in 20 s".to_owned()); }
            Err(e) => { let _ = child.kill(); let _ = child.wait(); break Err(format!("curl wait: {e}")); }
        }
    };
    let bytes = reader.join().map_err(|_| "curl stdout reader stopped".to_owned())?.map_err(|e| format!("curl stdout: {e}"))?;
    let status = status?;
    if status.success() { Ok(bytes) } else { Err(format!("curl exited with {status}")) }
}

fn fetch() -> Result {
    #[cfg(target_os = "macos")]
    let curl = std::path::PathBuf::from("/usr/bin/curl");
    #[cfg(windows)]
    let curl = std::env::var_os("SystemRoot").map(|root| std::path::PathBuf::from(root).join("System32/curl.exe")).filter(|path| path.is_file()).unwrap_or_else(|| "curl.exe".into());
    let mut cmd = Command::new(curl);
    cmd.args(["-fsSL", "--max-time", "10", "-H", "Accept: application/vnd.github+json", "-H", &format!("User-Agent: Flowseam/{}", crate::about::version()), "https://api.github.com/repos/operonlab/flowseam/releases/latest"]);
    #[cfg(windows)]
    { use std::os::windows::process::CommandExt; cmd.creation_flags(0x0800_0000); }
    match run_command(cmd, Duration::from_secs(20)) { Ok(bytes) => parse_release(&bytes, crate::about::version()), Err(why) => Result::Failed(why) }
}

pub struct Checker { rx: Option<Receiver<Result>>, pub result: Option<Result>, failed_at: Option<u64> }
impl Checker {
    pub fn new() -> Self { Self { rx: None, result: None, failed_at: None } }
    pub fn clear(&mut self) { self.result = None; }
    pub fn tick(&mut self, prefs: &mut crate::ui_prefs::UiPrefs) {
        if !prefs.check_updates { return; }
        if let Some(rx) = &self.rx {
            match rx.try_recv() {
                Ok(result) => {
                    if let Result::Failed(why) = &result { self.failed_at = Some(now()); eprintln!("automatic update check failed: {why}"); }
                    else { prefs.update_checked_at = now(); if let Err(e) = crate::ui_prefs::save(&crate::ui_prefs::path(), prefs) { eprintln!("save update check time failed: {e}"); } }
                    self.result = Some(result); self.rx = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => { self.failed_at = Some(now()); self.result = Some(Result::Failed("check thread stopped".into())); self.rx = None; eprintln!("automatic update check failed: check thread stopped"); }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if due(now(), prefs.update_checked_at, self.failed_at, self.rx.is_some()) {
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || { let _ = tx.send(fetch()); });
            self.rx = Some(rx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn due_respects_success_failure_and_running() { assert!(due(100, 0, None, false)); assert!(!due(100, 0, None, true)); assert!(!due(86_399, 1, None, false)); assert!(due(86_401, 1, None, false)); assert!(!due(4_000, 0, Some(500), false)); assert!(due(4_100, 0, Some(500), false)); }
    #[test]
    fn a_check_time_ahead_of_the_clock_does_not_block_checks() { assert!(due(1_000, 2_000_000_000, None, false)); assert!(due(1_000, 0, Some(2_000_000_000), false)); }
    #[test]
    fn release_tag_has_three_outcomes() { assert_eq!(parse_release(br#"{"tag_name":"v0.1.4"}"#, "0.1.3"), Result::Newer("0.1.4".into())); assert_eq!(parse_release(br#"{"tag_name":"0.1.3"}"#, "0.1.3"), Result::UpToDate); assert!(matches!(parse_release(br#"{"tag_name":"18446744073709551616"}"#, "0.1.3"), Result::Failed(_))); assert!(matches!(parse_release(br#"{}"#, "0.1.3"), Result::Failed(_))); assert!(matches!(parse_release(b"oops", "0.1.3"), Result::Failed(_))); }
    #[test]
    fn a_skipped_version_is_not_pointed_out_until_a_newer_one() {
        let newer = |v: &str| Some(Result::Newer(v.into()));
        assert_eq!(shown_newer(newer("0.1.4").as_ref(), ""), Some("0.1.4"));
        assert_eq!(shown_newer(newer("0.1.4").as_ref(), "0.1.4"), None, "skipped");
        assert_eq!(shown_newer(newer("0.1.5").as_ref(), "0.1.4"), Some("0.1.5"), "a newer one appears");
        assert_eq!(shown_newer(Some(&Result::UpToDate), ""), None);
        assert_eq!(shown_newer(Some(&Result::Failed("x".into())), ""), None);
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn hanging_command_is_killed_at_deadline() { let mut cmd = Command::new("/bin/sleep"); cmd.arg("2"); let started = Instant::now(); assert_eq!(run_command(cmd, Duration::from_millis(50)), Err("curl did not finish in 20 s".into())); assert!(started.elapsed() < Duration::from_secs(1)); }
    #[test]
    fn disabling_clears_hint_but_keeps_running_check_and_backoff() { let mut checker = Checker::new(); let (_tx, rx) = mpsc::channel(); checker.rx = Some(rx); checker.failed_at = Some(10); checker.result = Some(Result::Failed("error".into())); checker.clear(); assert!(checker.result.is_none()); assert!(checker.rx.is_some()); assert_eq!(checker.failed_at, Some(10)); }
}
