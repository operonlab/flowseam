//! One sharing process per account: a second one would tap the same input and fight over the same peers.

use std::process::{Command, Output};

fn worker(home: &std::path::Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_flowseam"))
        .args(["worker", "--config"])
        .arg(home.join("broken.toml"))
        .env("HOME", home)
        .env("USERPROFILE", home)
        .output()
        .unwrap()
}

#[test]
fn a_second_worker_does_not_start_while_one_is_sharing() {
    let home = std::env::temp_dir().join(format!("flowseam-one-worker-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let lock = home.join(".config/flowseam/worker.lock");
    std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
    // A config that cannot be read, so no run of this test ever gets as far as tapping real input.
    std::fs::write(home.join("broken.toml"), "peers = [").unwrap();

    // Control: nobody is sharing, so it gets as far as the config and stops there.
    #[cfg(target_os = "macos")]
    {
        let free = worker(&home);
        assert_eq!(free.status.code(), Some(1), "{}", String::from_utf8_lossy(&free.stderr));
    }

    let held = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&lock).unwrap();
    held.try_lock().unwrap();
    let busy = worker(&home);
    assert_eq!(busy.status.code(), Some(3), "{}", String::from_utf8_lossy(&busy.stderr));
    // A clean end: the GUI's supervisor must not restore a cursor the running worker owns.
    #[cfg(target_os = "macos")]
    assert_eq!(busy.stdout, b"CLEAN\n");

    drop(held);
    let _ = std::fs::remove_dir_all(&home);
}
