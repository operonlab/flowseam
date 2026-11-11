//! One window process per user. A second one (the login agent and an older login item both starting Flowseam,
//! or the exe opened again on Windows) asks the first to show its window and exits, instead of adding a second
//! menu bar icon next to a worker that would refuse to share.
use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Held for the life of the first window process; the lock goes with the process, so it is never stale.
pub struct Guard { _file: File }

pub enum Start { First(Guard), Second }

pub fn claim(dir: &Path) -> std::io::Result<Start> {
    std::fs::create_dir_all(dir)?;
    let file = OpenOptions::new().create(true).write(true).truncate(false).open(dir.join("gui.lock"))?;
    match file.try_lock() {
        Ok(()) => Ok(Start::First(Guard { _file: file })),
        Err(TryLockError::WouldBlock) => Ok(Start::Second),
        Err(TryLockError::Error(e)) => Err(e),
    }
}

fn request_path(dir: &Path) -> PathBuf { dir.join("show-request") }

fn now_secs() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |t| t.as_secs()) }

/// A request older than this is a leftover (its sender gave up, or the first process ended before looking) and
/// must not open the window at the next login.
const FRESH_FOR: u64 = 10;

/// Second process: ask the running one for its window.
pub fn ask_to_show(dir: &Path) -> std::io::Result<()> { std::fs::write(request_path(dir), now_secs().to_string()) }

/// First process, once a second: whether a second one asked for the window recently. A stale request is
/// removed without effect.
pub fn show_requested(dir: &Path) -> bool { take_request(dir, now_secs()) }

fn take_request(dir: &Path, now: u64) -> bool {
    let path = request_path(dir);
    let Ok(text) = std::fs::read_to_string(&path) else { return false };
    let _ = std::fs::remove_file(&path);
    text.trim().parse::<u64>().is_ok_and(|at| at <= now && now - at <= FRESH_FOR)
}

/// Second process opened by hand: ask for the window and wait briefly. Returns a guard when the first process
/// went away meanwhile (it was quitting), so this one carries on as the first instead of leaving nothing open.
pub fn hand_over(dir: &Path, wait: Duration) -> std::io::Result<Option<Guard>> {
    ask_to_show(dir)?;
    let end = Instant::now() + wait;
    while Instant::now() < end {
        if !request_path(dir).exists() { return Ok(None); }
        if let Start::First(guard) = claim(dir)? { let _ = std::fs::remove_file(request_path(dir)); return Ok(Some(guard)); }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> PathBuf { std::env::temp_dir().join(format!("flowseam-single-{tag}-{}", std::process::id())) }

    #[test]
    fn a_second_claim_waits_for_the_first_to_go() {
        let d = dir("claim");
        let first = claim(&d).unwrap();
        assert!(matches!(first, Start::First(_)));
        assert!(matches!(claim(&d).unwrap(), Start::Second), "a second window process while the first runs");
        drop(first);
        assert!(matches!(claim(&d).unwrap(), Start::First(_)), "the lock goes with the first");
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn a_show_request_is_seen_once_and_only_while_fresh() {
        let d = dir("show");
        std::fs::create_dir_all(&d).unwrap();
        assert!(!show_requested(&d));
        ask_to_show(&d).unwrap();
        assert!(show_requested(&d));
        assert!(!show_requested(&d), "taken by the first look");
        std::fs::write(request_path(&d), "1000").unwrap();
        assert!(!take_request(&d, 1000 + FRESH_FOR + 1), "a leftover from an earlier session does not open the window");
        assert!(!request_path(&d).exists(), "and is cleared");
        std::fs::write(request_path(&d), "garbage").unwrap();
        assert!(!take_request(&d, 1000));
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn a_hand_over_to_a_quitting_first_carries_on_as_first() {
        let d = dir("handover");
        let first = claim(&d).unwrap();
        let quitter = std::thread::spawn(move || { std::thread::sleep(Duration::from_millis(200)); drop(first); });
        let guard = hand_over(&d, Duration::from_secs(2)).unwrap();
        quitter.join().unwrap();
        assert!(guard.is_some(), "the first went away without taking the request: this one becomes the first");
        assert!(!request_path(&d).exists());
        drop(guard);
        let first = claim(&d).unwrap();
        let taker_dir = d.clone();
        let taker = std::thread::spawn(move || { std::thread::sleep(Duration::from_millis(150)); assert!(show_requested(&taker_dir)); });
        assert!(hand_over(&d, Duration::from_secs(2)).unwrap().is_none(), "a running first took the request");
        taker.join().unwrap();
        drop(first);
        std::fs::remove_dir_all(d).unwrap();
    }
}
