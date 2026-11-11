#![cfg_attr(windows, windows_subsystem = "windows")]
mod about;
mod autostart;
mod gui;
mod i18n;
// The monitor switch is set up from the controller, a Mac; on Windows only its tests use it.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod kvm_ui;
mod menubar;
mod model;
mod single;
mod ui_prefs;
mod update_check;
mod worker;

use std::path::PathBuf;

#[cfg(target_os = "macos")]
use flowseam_core::config::Config;

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).expect("HOME/USERPROFILE 未設定"))
}
pub fn config_dir() -> PathBuf {
    home().join(".config/flowseam")
}

/// State written under the project's former name, and where it lives now.
#[cfg(target_os = "macos")]
const LEGACY_DIRS: [(&str, &str); 2] =
    [(".config/kmshare", ".config/flowseam"), ("Library/Application Support/kmshare", "Library/Application Support/flowseam")];

/// Moves each legacy directory to its new place, once: identity, pairings and config come along, so
/// nothing has to be paired again. Returns one line per directory moved or left alone.
///
/// `Err` stops the start: an older build still holding the cursor grab (its lock would move from under
/// it), or a move that failed. Starting anyway would create a fresh identity under the new name, and
/// every pairing would break.
#[cfg(target_os = "macos")]
fn migrate_legacy(home: &std::path::Path, alive: impl Fn(u32) -> bool) -> Result<Vec<String>, String> {
    let old_lock = home.join(LEGACY_DIRS[1].0).join("grab.lock");
    if let Ok(text) = std::fs::read_to_string(&old_lock) {
        if let flowseam_mac::state::RecoveryPlan::LiveOwner { pid } = flowseam_mac::state::plan_recovery(Some(&text), alive) {
            return Err(format!("an older build (pid {pid}) is still running and holds the cursor grab; quit it, then start again"));
        }
    }
    let mut out = Vec::new();
    for (old, new) in LEGACY_DIRS {
        let (o, n) = (home.join(old), home.join(new));
        if !o.is_dir() {
            continue;
        }
        if n.exists() {
            out.push(format!("both {} and {} exist; using the second and leaving the first as is", o.display(), n.display()));
            continue;
        }
        n.parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::rename(&o, &n))
            .map_err(|e| format!("could not move {} to {}: {e}", o.display(), n.display()))?;
        out.push(format!("moved {} to {}", o.display(), n.display()));
    }
    Ok(out)
}
pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}
pub fn identity_dir() -> PathBuf {
    config_dir().join("identity")
}

/// One sharing process per user, however many windows or login items start one: two would each tap the
/// same input and fight over the same peers. The worker holds this lock for its whole life; the OS drops
/// it when the process ends, however it ends.
pub fn worker_lock_path() -> PathBuf {
    config_dir().join("worker.lock")
}

/// `Ok(Some(file))`: taken, and held while `file` is open. `Ok(None)`: another process holds it.
pub fn take_worker_lock(path: &std::path::Path) -> std::io::Result<Option<std::fs::File>> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e),
    }
}

/// Exit code of a worker that did not start because another one is sharing.
pub const EXIT_ALREADY_SHARING: i32 = 3;

fn main() {
    #[cfg(target_os = "macos")]
    match migrate_legacy(&home(), flowseam_mac::backend::pid_alive) {
        Ok(lines) => lines.iter().for_each(|l| eprintln!("flowseam: {l}")),
        Err(e) => {
            eprintln!("flowseam: {e}");
            std::process::exit(1);
        }
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        None | Some("gui") | Some("--hidden") => { if let Err(e) = i18n::load_prefs(&config_dir().join("prefs.toml")) { eprintln!("flowseam: prefs.toml: {e}"); } gui::run(args.iter().any(|a| a == "--hidden")) },
        Some("worker") => cmd_worker(&args[1..]),
        Some("pair") => cmd_pair(),
        #[cfg(target_os = "macos")]
        Some("launch-at-login") => autostart::launch_at_login(),
        Some(other) => {
            eprintln!("未知子命令 {other:?}；可用：gui | worker --config <path> | pair");
            2
        }
    };
    std::process::exit(code);
}

// The Windows block returns early so the Mac code after it stays a plain tail; on Windows that return ends the body.
#[cfg_attr(windows, allow(clippy::needless_return))]
fn cmd_worker(args: &[String]) -> i32 {
    let path = match args {
        [flag, p] if flag == "--config" => PathBuf::from(p),
        _ => {
            flowseam_core::log::line("用法：flowseam worker --config <path>");
            return 2;
        }
    };
    let _sharing = match take_worker_lock(&worker_lock_path()) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            flowseam_core::log::line("worker: 另一個 Flowseam 已經在分享，這個不啟動");
            // Nothing was hidden or grabbed: a clean end, so the supervisor leaves alone the cursor the
            // running worker owns.
            #[cfg(target_os = "macos")]
            {
                use std::io::Write;
                let mut out = std::io::stdout();
                let _ = out.write_all(flowseam_mac::supervisor::CLEAN).and_then(|()| out.write_all(b"\n")).and_then(|()| out.flush());
            }
            return EXIT_ALREADY_SHARING;
        }
        Err(e) => {
            flowseam_core::log::line(&format!("worker: 無法鎖定 {}：{e}", worker_lock_path().display()));
            return 1;
        }
    };
    #[cfg(target_os = "macos")]
    let cfg = match Config::load(&path).and_then(|c| c.validate_for_run().map(|_| c)) {
        Ok(c) => c,
        Err(e) => {
            flowseam_core::log::line(&format!("worker: {e}"));
            return 1;
        }
    };
    #[cfg(target_os = "windows")]
    {
        use std::io::BufRead;
        // Each stdin line is a command from the window (`home` is its "back" button); EOF is the stop request.
        std::thread::spawn(|| {
            let mut stdin = std::io::stdin().lock();
            let mut buf = Vec::new();
            while matches!(stdin.read_until(b'\n', &mut buf), Ok(n) if n > 0) {
                if String::from_utf8_lossy(&buf).trim() == "home" {
                    flowseam_win::app::ask_home();
                }
                buf.clear();
            }
            std::process::exit(0);
        });
        let (clipboard, injector, displays) = flowseam_win::real_host_parts();
        let host = flowseam_win::app::Host { clipboard: &*clipboard, injector: &*injector, displays: &*displays };
        return match flowseam_win::app::run_serve(&path, &host) {
            Ok(()) => 0,
            Err(e) => {
                flowseam_core::log::line(&format!("worker: {e}"));
                1
            }
        };
    }
    #[cfg(target_os = "macos")]
    match flowseam_mac::worker::run(&cfg, &path) {
        Ok(()) => 0,
        Err(e) => {
            flowseam_core::log::line(&format!("worker: {e:?}"));
            1
        }
    }
}

fn cmd_pair() -> i32 {
    let ask = |q: &str| -> String {
        eprint!("{q}");
        let mut s = String::new();
        let _ = std::io::stdin().read_line(&mut s);
        s.trim().to_string()
    };
    let addr = ask("對方 addr（host:port）：");
    let code = ask("6 位配對碼：");
    if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
        eprintln!("配對碼須為 6 位數字");
        return 2;
    }
    match pair_as_client(&addr, &code) {
        Ok(fp) => {
            println!("{}", flowseam_net::identity::to_hex(&fp));
            0
        }
        Err(e) => {
            eprintln!("配對失敗：{e}");
            1
        }
    }
}

/// Parse `host:port` then run the real pairing handshake. Returns the peer's fingerprint.
pub fn pair_as_client(addr: &str, code: &str) -> Result<flowseam_net::identity::Fingerprint, String> {
    let sa: std::net::SocketAddr = addr.trim().parse().map_err(|e| format!("addr {addr:?} 不是 host:port（{e}）"))?;
    flowseam_net::pair::pair_as_client(sa, code, &identity_dir()).map_err(|e| format!("{e:?}"))
}

/// What a cancelled pairing server returns, so the window can say "cancelled" rather than "failed".
#[cfg(any(target_os = "macos", test))]
pub const PAIR_CANCELLED: &str = "pairing cancelled";

/// Keep the code valid across connections that do not pair until one peer pairs, or until `cancel` is raised.
/// `accept` blocks, so a cancel also connects to the listener (`pair_serve_cancel`); that attempt fails and the
/// flag is seen. The flag is also read before each accept, since a cancel raised before the port was bound had
/// nothing to wake. A pairing that succeeds as the flag goes up still counts.
#[cfg(any(target_os = "macos", test))]
pub fn pair_serve_on(
    listener: &std::net::TcpListener,
    identity: &std::path::Path,
    cancel: &std::sync::atomic::AtomicBool,
    on_code: impl FnOnce(&str),
) -> Result<flowseam_net::identity::Fingerprint, String> {
    let code = flowseam_net::pair::generate_code();
    on_code(&code);
    let guard = flowseam_net::pair::PairGuard::default();
    let stop = || cancel.load(std::sync::atomic::Ordering::SeqCst);
    match flowseam_net::pair::serve_code(listener, &code, identity, &guard, stop, flowseam_core::log::line) {
        Ok(Some(fp)) => Ok(fp),
        Ok(None) => Err(PAIR_CANCELLED.into()),
        Err(e) => Err(e.to_string()),
    }
}

/// Show a code on this Mac and wait for the other computer to type it. Binds the sharing port, so sharing
/// must be off.
#[cfg(target_os = "macos")]
pub fn pair_serve(cancel: &std::sync::atomic::AtomicBool, on_code: impl FnOnce(&str)) -> Result<flowseam_net::identity::Fingerprint, String> {
    let cfg = Config::load(&config_path()).map_err(|e| e.to_string())?;
    let listener = std::net::TcpListener::bind(&cfg.listen).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AddrInUse {
            format!("bind {}: {e}; 請先關閉共用（配對會用到同一個連接埠）", cfg.listen)
        } else {
            format!("bind {}: {e}", cfg.listen)
        }
    })?;
    pair_serve_on(&listener, &identity_dir(), cancel, on_code)
}

/// End a `pair_serve` that is waiting: raise the flag, then wake its blocked accept with a connection that
/// closes at once.
#[cfg(target_os = "macos")]
pub fn pair_serve_cancel(cancel: &std::sync::atomic::AtomicBool) {
    cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    let Ok(cfg) = Config::load(&config_path()) else { return };
    let Ok(mut addr) = cfg.listen.parse::<std::net::SocketAddr>() else { return };
    if addr.ip().is_unspecified() {
        addr.set_ip(if addr.is_ipv4() { std::net::Ipv4Addr::LOCALHOST.into() } else { std::net::Ipv6Addr::LOCALHOST.into() });
    }
    let _ = std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(500));
}

#[cfg(test)]
mod pair_tests {
    use super::*;
    use flowseam_net::identity::Identity;
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc};
    use std::time::Duration;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("flowseam-pair-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn server_retries_wrong_code_then_returns_client_fingerprint() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let base = temp("retry");
        let (dir_a, dir_b) = (base.join("a"), base.join("b"));
        let (code_tx, code_rx) = mpsc::channel();
        let server = std::thread::spawn(move || pair_serve_on(&listener, &dir_a, &AtomicBool::new(false), |code| code_tx.send(code.to_string()).unwrap()));
        let code = code_rx.recv().unwrap();
        let wrong = if code == "000000" { "999999" } else { "000000" };
        assert!(flowseam_net::pair::pair_as_client(addr, wrong, &dir_b).is_err());
        let fp_client = flowseam_net::pair::pair_as_client(addr, &code, &dir_b).unwrap();
        assert_eq!(server.join().unwrap().unwrap(), Identity::load_or_create(&dir_b).unwrap().fingerprint());
        assert_eq!(fp_client, Identity::load_or_create(&base.join("a")).unwrap().fingerprint());
        std::fs::remove_dir_all(base).unwrap();
    }

    /// The window's Cancel: the flag plus one connection that closes at once ends a server blocked in accept.
    /// The server may see the flag before it accepts and close its listener, so the test holds a clone open
    /// for the wake-up connection to land on.
    #[test]
    fn a_cancel_ends_a_server_that_is_waiting() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let _held = listener.try_clone().unwrap();
        let dir = temp("cancel");
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let (c, d) = (cancel.clone(), dir.clone());
        std::thread::spawn(move || {
            let _ = tx.send(pair_serve_on(&listener, &d, &c, |_| {}));
        });
        cancel.store(true, Ordering::SeqCst);
        drop(TcpStream::connect(addr).unwrap());
        let result = rx.recv_timeout(Duration::from_secs(10)).expect("the server kept waiting after a cancel");
        assert_eq!(result, Err(PAIR_CANCELLED.to_string()));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Cancel pressed before the port was bound: the wake-up connection was refused, so none ever arrives.
    #[test]
    fn a_cancel_raised_before_serving_ends_it_without_a_wake_up() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let dir = temp("early-cancel");
        let (tx, rx) = mpsc::channel();
        let d = dir.clone();
        std::thread::spawn(move || {
            let _ = tx.send(pair_serve_on(&listener, &d, &AtomicBool::new(true), |_| {}));
        });
        let result = rx.recv_timeout(Duration::from_secs(5)).expect("the server waited for a connection after an early cancel");
        assert_eq!(result, Err(PAIR_CANCELLED.to_string()));
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(all(test, target_os = "macos"))]
mod migrate_tests {
    use super::*;

    fn home(name: &str) -> PathBuf {
        let h = std::env::temp_dir().join(format!("flowseam-migrate-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&h);
        h
    }
    fn nobody_alive(_: u32) -> bool {
        false
    }

    /// A pairing made under the old name keeps working: the identity directory moves as a whole.
    #[test]
    fn legacy_state_moves_once_with_its_contents() {
        let h = home("moves");
        std::fs::create_dir_all(h.join(".config/kmshare/identity")).unwrap();
        std::fs::write(h.join(".config/kmshare/identity/cert.der"), b"cert").unwrap();
        std::fs::create_dir_all(h.join("Library/Application Support/kmshare")).unwrap();
        assert_eq!(migrate_legacy(&h, nobody_alive).unwrap().len(), 2);
        assert_eq!(std::fs::read(h.join(".config/flowseam/identity/cert.der")).unwrap(), b"cert");
        assert!(h.join("Library/Application Support/flowseam").is_dir());
        assert!(!h.join(".config/kmshare").exists());
        assert!(migrate_legacy(&h, nobody_alive).unwrap().is_empty(), "second run has nothing to do");
    }

    /// State already under the new name wins; the old directory is not merged into it, and that is said.
    #[test]
    fn an_existing_new_directory_is_left_alone_and_reported() {
        let h = home("exists");
        std::fs::create_dir_all(h.join(".config/kmshare")).unwrap();
        std::fs::create_dir_all(h.join(".config/flowseam")).unwrap();
        let lines = migrate_legacy(&h, nobody_alive).unwrap();
        assert!(lines.iter().any(|l| l.starts_with("both ")), "{lines:?}");
        assert!(h.join(".config/kmshare").is_dir());
    }

    /// An older build still running holds its grab lock in the old directory: moving it would pull the
    /// lock from under it. Nothing moves and the start stops.
    #[test]
    fn a_running_older_build_stops_the_move() {
        let h = home("live");
        let dir = h.join("Library/Application Support/kmshare");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("grab.lock"), "kmmac-grab v1\npid=4242\nsince=1\n").unwrap();
        std::fs::create_dir_all(h.join(".config/kmshare/identity")).unwrap();
        let e = migrate_legacy(&h, |pid| pid == 4242).unwrap_err();
        assert!(e.contains("4242"), "{e}");
        assert!(h.join(".config/kmshare/identity").is_dir(), "nothing moved");
        assert!(dir.join("grab.lock").is_file());
    }

    /// A move that fails stops the start, so no fresh identity is created under the new name.
    #[test]
    fn a_failed_move_stops_the_start() {
        use std::os::unix::fs::PermissionsExt;
        let h = home("fails");
        std::fs::create_dir_all(h.join(".config/kmshare/identity")).unwrap();
        let parent = h.join(".config");
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o555)).unwrap(); // no new entries
        let r = migrate_legacy(&h, nobody_alive);
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(r.as_ref().is_err_and(|e| e.contains("could not move")), "{r:?}");
        assert!(h.join(".config/kmshare/identity").is_dir(), "the old directory stays");
        assert!(!h.join(".config/flowseam").exists());
    }
}
