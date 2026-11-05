mod gui;
mod model;
mod worker;

use std::path::PathBuf;

use flowseam_core::config::Config;

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").expect("HOME 未設定"))
}
pub fn config_dir() -> PathBuf {
    home().join(".config/flowseam")
}

/// State written under the project's former name, and where it lives now.
const LEGACY_DIRS: [(&str, &str); 2] =
    [(".config/kmshare", ".config/flowseam"), ("Library/Application Support/kmshare", "Library/Application Support/flowseam")];

/// Moves each legacy directory to its new place, once: identity, pairings and config come along, so
/// nothing has to be paired again. Returns one line per directory moved or left alone.
///
/// `Err` stops the start: an older build still holding the cursor grab (its lock would move from under
/// it), or a move that failed. Starting anyway would create a fresh identity under the new name, and
/// every pairing would break.
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

fn main() {
    match migrate_legacy(&home(), flowseam_mac::backend::pid_alive) {
        Ok(lines) => lines.iter().for_each(|l| eprintln!("flowseam: {l}")),
        Err(e) => {
            eprintln!("flowseam: {e}");
            std::process::exit(1);
        }
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        None | Some("gui") => gui::run(),
        Some("worker") => cmd_worker(&args[1..]),
        Some("pair") => cmd_pair(),
        Some(other) => {
            eprintln!("未知子命令 {other:?}；可用：gui | worker --config <path> | pair");
            2
        }
    };
    std::process::exit(code);
}

fn cmd_worker(args: &[String]) -> i32 {
    let path = match args {
        [flag, p] if flag == "--config" => PathBuf::from(p),
        _ => {
            eprintln!("用法：flowseam worker --config <path>");
            return 2;
        }
    };
    let cfg = match Config::load(&path).and_then(|c| c.validate_for_run().map(|_| c)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("worker: {e}");
            return 1;
        }
    };
    match flowseam_mac::worker::run(&cfg) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("worker: {e:?}");
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

#[cfg(test)]
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
