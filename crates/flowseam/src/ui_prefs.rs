use std::fs::{self, OpenOptions};
use std::path::Path;

/// `skipped_version`: the newer version the user chose to skip; empty when none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiPrefs { pub dock_icon: bool, pub menu_bar_icon: bool, pub check_updates: bool, pub update_checked_at: u64, pub skipped_version: String }
impl Default for UiPrefs { fn default() -> Self { Self { dock_icon: true, menu_bar_icon: true, check_updates: false, update_checked_at: 0, skipped_version: String::new() } } }

pub fn path() -> std::path::PathBuf { crate::config_dir().join("prefs.toml") }

fn parse(text: &str) -> UiPrefs {
    let mut prefs = UiPrefs::default();
    let mut ui = false;
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.starts_with('[') { ui = line == "[ui]"; continue; }
        if !ui { continue; }
        let Some((key, value)) = line.split_once('=') else { continue };
        match key.trim() {
            "dock_icon" => prefs.dock_icon = value.trim().parse().unwrap_or(true),
            "menu_bar_icon" => prefs.menu_bar_icon = value.trim().parse().unwrap_or(true),
            "check_updates" => prefs.check_updates = value.trim().parse().unwrap_or(false),
            "update_checked_at" => prefs.update_checked_at = value.trim().parse().unwrap_or(0),
            "skipped_version" => prefs.skipped_version = value.trim().trim_matches(|c| c == '"' || c == '\'').to_owned(),
            _ => {}
        }
    }
    prefs
}

pub fn load() -> UiPrefs { std::fs::read_to_string(path()).map(|text| parse(&text)).unwrap_or_default() }

fn updated(text: &str, prefs: &UiPrefs) -> String {
    let mut out = String::new();
    let mut ui = false;
    let mut found = false;
    let mut keys = [false; 5];
    let values = [("dock_icon", prefs.dock_icon.to_string()), ("menu_bar_icon", prefs.menu_bar_icon.to_string()), ("check_updates", prefs.check_updates.to_string()), ("update_checked_at", prefs.update_checked_at.to_string()), ("skipped_version", format!("\"{}\"", prefs.skipped_version))];
    let finish = |out: &mut String, keys: &mut [bool; 5]| { for (i, (key, value)) in values.iter().enumerate() { if !keys[i] { out.push_str(&format!("{key} = {value}\n")); keys[i] = true; } } };
    for line in text.lines() {
        let header = line.split('#').next().unwrap_or("").trim();
        if header.starts_with('[') {
            if ui { finish(&mut out, &mut keys); }
            ui = header == "[ui]";
            found |= ui;
        }
        if ui && !header.starts_with('[') {
            if let Some((key, _)) = line.split_once('=') {
                if let Some(i) = values.iter().position(|(name, _)| *name == key.trim()) {
                    if !keys[i] { out.push_str(&format!("{} = {}\n", values[i].0, values[i].1)); keys[i] = true; }
                    continue;
                }
            }
        }
        out.push_str(line); out.push('\n');
    }
    if !found { if !out.is_empty() { out.push('\n'); } out.push_str("[ui]\n"); }
    if ui || !found { finish(&mut out, &mut keys); }
    out
}

pub fn save(path: &Path, prefs: &UiPrefs) -> std::io::Result<()> {
    write_shared(path, |text| updated(text, prefs))
}

pub(crate) fn write_shared(path: &Path, transform: impl FnOnce(&str) -> String) -> std::io::Result<()> {
    if let Some(dir) = path.parent() { std::fs::create_dir_all(dir)?; }
    let lock = OpenOptions::new().create(true).truncate(false).write(true).open(path.with_extension("toml.lock"))?;
    lock.lock()?;
    let text = match fs::read_to_string(path) { Ok(text) => text, Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(), Err(e) => return Err(e) };
    let new_text = transform(&text);
    if new_text == text { return Ok(()); }
    // One writer at a time holds the lock, so one temporary name will do, and a crash leaves no pile of them.
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, &new_text)?;
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        // Windows refuses to replace a file another program holds open without delete sharing (an indexer, an
        // antivirus scan); the lock still keeps other writers out, so write in place rather than fail the save.
        #[cfg(windows)]
        Err(_) => { let written = fs::write(path, &new_text); let _ = fs::remove_file(&tmp); written }
        #[cfg(not(windows))]
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn round_trip_and_preserve_other_sections() {
        let source = "language = \"ja\"\n[ui]\nfuture = 7\ndock_icon = true\n[about]\nwhats_new_seen = \"0.1.2\"\n";
        let prefs = UiPrefs { dock_icon: false, menu_bar_icon: false, check_updates: true, update_checked_at: 123, skipped_version: "0.1.4".into() };
        let out = updated(source, &prefs);
        assert_eq!(parse(&out), prefs);
        assert!(out.contains("skipped_version = \"0.1.4\""));
        for keep in ["language = \"ja\"", "future = 7", "[about]\nwhats_new_seen = \"0.1.2\""] { assert!(out.contains(keep)); }
        assert_eq!(parse(&updated("", &prefs)), prefs);
        assert_eq!(parse(&updated(&out, &UiPrefs { skipped_version: String::new(), ..prefs.clone() })).skipped_version, "", "clearing the skip is written back");
    }
    #[test]
    fn a_single_quoted_skip_reads_as_its_version() { assert_eq!(parse("[ui]\nskipped_version = '0.1.4'\n").skipped_version, "0.1.4"); }
    #[test]
    fn malformed_values_use_defaults() { assert_eq!(parse("[ui]\ndock_icon = nope\nmenu_bar_icon = 1\ncheck_updates = yes\nupdate_checked_at = -3\n"), UiPrefs::default()); }
    #[test]
    fn commented_ui_header_is_updated_in_place() { let prefs = UiPrefs { dock_icon: false, ..UiPrefs::default() }; let out = updated("language = \"ja\"\n[ui] # note\ndock_icon = true\n[about] # note\nwhats_new_seen = \"0.1.2\"\n", &prefs); assert_eq!(out.matches("[ui]").count(), 1); assert!(out.contains("[ui] # note\ndock_icon = false")); assert!(out.contains("[about] # note\nwhats_new_seen")); assert_eq!(parse(&out), prefs); }
    #[test]
    fn save_reads_back_without_rewriting_other_owners() {
        let path = std::env::temp_dir().join(format!("flowseam-ui-prefs-{}-{}.toml", std::process::id(), crate::update_check::now()));
        std::fs::write(&path, "language = \"ko\"\n[about]\nwhats_new_seen = \"0.1.2\"\n[future]\nkey = 3\n").unwrap();
        let prefs = UiPrefs { dock_icon: false, menu_bar_icon: true, check_updates: true, update_checked_at: 42, skipped_version: String::new() };
        save(&path, &prefs).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(parse(&text), prefs);
        for keep in ["language = \"ko\"", "[about]\nwhats_new_seen = \"0.1.2\"", "[future]\nkey = 3"] { assert!(text.contains(keep)); }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn shared_writer_waits_for_lock_then_preserves_both_changes() {
        use std::sync::mpsc;
        use std::time::Duration;
        let root = std::env::temp_dir().join(format!("flowseam-prefs-lock-{}-{:?}", std::process::id(), std::thread::current().id()));
        let path = root.join("prefs.toml");
        let (locked_tx, locked_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let holder_path = path.clone();
        let holder = std::thread::spawn(move || write_shared(&holder_path, |text| {
            locked_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            updated(text, &UiPrefs { dock_icon: false, ..UiPrefs::default() })
        }));
        locked_rx.recv().unwrap();
        let (done_tx, done_rx) = mpsc::channel();
        let saver_path = path.clone();
        let saver = std::thread::spawn(move || { let result = crate::i18n::save_prefs(&saver_path, Some(crate::i18n::Lang::Ja)); done_tx.send(result).unwrap(); });
        assert!(matches!(done_rx.recv_timeout(Duration::from_millis(100)), Err(mpsc::RecvTimeoutError::Timeout)), "save must wait for the held lock");
        release_tx.send(()).unwrap();
        holder.join().unwrap().unwrap();
        done_rx.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
        saver.join().unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("language = \"ja\""));
        assert!(!parse(&text).dock_icon);
        fs::remove_dir_all(root).unwrap();
    }
}
