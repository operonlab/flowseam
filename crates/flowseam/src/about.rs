use std::cmp::Ordering;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

use eframe::egui::{self, Color32, Layout, RichText};

use crate::gui::{bold, link, primary, secondary, sheet, sheet_item, Icon, BLUE, GREEN, ORANGE, PINK, PURPLE, TEAL};

static WHATS_NEW_REQUESTED: AtomicBool = AtomicBool::new(false);
static UPDATE_CHECK_REQUESTED: AtomicBool = AtomicBool::new(false);
const CHANGELOG_URL: &str = "https://github.com/operonlab/flowseam/blob/main/CHANGELOG.md";
const RELEASES_URL: &str = "https://github.com/operonlab/flowseam/releases";

pub fn version() -> &'static str { env!("CARGO_PKG_VERSION") }
pub fn show_whats_new() { WHATS_NEW_REQUESTED.store(true, AtomicOrdering::Release); }
pub fn show_update_check() { UPDATE_CHECK_REQUESTED.store(true, AtomicOrdering::Release); }

struct Note { icon: Icon, tint: Color32, title: &'static str, body: &'static str }
struct Release { version: &'static str, items: &'static [Note] }
const NOTES: &[Release] = &[
    Release { version: "0.1.3", items: &[
        Note { icon: Icon::Arrows, tint: BLUE, title: "經過一台 Mac 控制下一台", body: "一台 Mac 操作另一台 Mac 時，把游標推到那台 Mac 通往下一台電腦的邊緣，就能接著操作下一台，例如經過 Mac mini 操作 Windows PC。只有中間那台 Mac 需要這一版。" },
        Note { icon: Icon::Sliders, tint: GREEN, title: "碰哪台就由哪台接手", body: "另一台電腦正在操作這台 Mac 時，動一下這台 Mac 的滑鼠或按一個鍵，就改由這台 Mac 操作。" },
        Note { icon: Icon::Grid, tint: TEAL, title: "多螢幕 Mac 的修正", body: "另一台電腦操作接了多個螢幕的 Mac 時，游標現在能移到每一個螢幕。" },
        Note { icon: Icon::Display, tint: PURPLE, title: "Windows 版可直接下載", body: "Releases 頁面提供編譯好的 Windows 版，不必再從原始碼建置。" },
    ] },
    Release { version: "0.1.2", items: &[
        Note { icon: Icon::Display, tint: PURPLE, title: "在 Windows 上也能直接放進資料夾", body: "從 Mac 拖到 Windows 的檔案，可以直接放進資料夾、桌面或 App，不再一律存到「下載」。" },
        Note { icon: Icon::Arrows, tint: BLUE, title: "從 Windows 拖檔案回 Mac", body: "由 Mac 操作 Windows 時，在檔案總管把檔案拖到通往 Mac 的邊緣，就能放到 Mac 上想放的地方。Windows 上的檔案留在原處。" },
        Note { icon: Icon::Grid, tint: TEAL, title: "多螢幕 Mac 的修正", body: "Mac 接了多個螢幕時，從另一台拖回來的檔案會放在你放開的地方，不再改存到「下載」。" },
    ] },
    Release { version: "0.1.1", items: &[
        Note { icon: Icon::Display, tint: PURPLE, title: "在 Mac 上直接放進資料夾", body: "把檔案拖到 Mac 上，可以直接放進資料夾、桌面或 App。拖到 Windows 的檔案仍會存到「下載」。" },
        Note { icon: Icon::Grid, tint: ORANGE, title: "選單列與系統匣圖示", body: "Mac 的選單列和 Windows 的系統匣會顯示共用是否開啟、連著哪幾台。關掉視窗後 Flowseam 仍會繼續執行。" },
        Note { icon: Icon::Link, tint: PURPLE, title: "Mac 也能顯示配對碼", body: "兩台 Mac 現在可以直接在 App 裡配對。" },
        Note { icon: Icon::Sliders, tint: ORANGE, title: "音量等媒體鍵", body: "切到另一台電腦時，音量和播放鍵也會送到對方。" },
        Note { icon: Icon::Arrows, tint: BLUE, title: "滑鼠速度修正", body: "控制另一台電腦時，滑鼠快速移動不再掉步，速度和在本機一樣。" },
        Note { icon: Icon::Link, tint: GREEN, title: "自動挑最快的連線", body: "兩台 Mac 之間有多條連線時，優先使用 Thunderbolt 或有線網路。" },
        Note { icon: Icon::Grid, tint: TEAL, title: "設定視窗顯示每台是否連線中", body: "每台已配對電腦的連線狀態現在看得見。" },
        Note { icon: Icon::Sliders, tint: TEAL, title: "四種語言", body: "介面支援 English、繁體中文、日本語、한국어，預設跟隨系統語言，可在「進階 › 語言」更改。" },
        Note { icon: Icon::Display, tint: PINK, title: "新圖示", body: "換上新的水獺圖示。" },
    ] },
    Release { version: "0.1.0", items: &[Note { icon: Icon::Arrows, tint: BLUE, title: "第一版", body: "開始在電腦之間共用鍵盤與滑鼠。" }] },
];

fn compare_versions(a: &str, b: &str) -> Ordering {
    let left: Vec<u64> = a.split('.').filter_map(|x| x.parse().ok()).collect();
    let right: Vec<u64> = b.split('.').filter_map(|x| x.parse().ok()).collect();
    for i in 0..left.len().max(right.len()) {
        match left.get(i).copied().unwrap_or(0).cmp(&right.get(i).copied().unwrap_or(0)) {
            Ordering::Equal => {}, other => return other,
        }
    }
    Ordering::Equal
}

fn versions_to_show(current: &str, last_seen: Option<&str>, used_before: bool) -> Vec<&'static Release> {
    match last_seen {
        None => if used_before { NOTES.iter().filter(|r| r.version == current).collect() } else { Vec::new() },
        Some(seen) if compare_versions(seen, current) == Ordering::Less => NOTES.iter().filter(|r| compare_versions(seen, r.version) == Ordering::Less && compare_versions(r.version, current) != Ordering::Greater).collect(),
        Some(_) => Vec::new(),
    }
}

fn seen_from(text: &str) -> Option<&str> {
    let mut in_about = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') { in_about = trimmed == "[about]"; }
        else if in_about {
            if let Some(value) = trimmed.strip_prefix("whats_new_seen").and_then(|s| s.trim_start().strip_prefix('=')) {
                return value.split('#').next().map(|s| s.trim().trim_matches('"'));
            }
        }
    }
    None
}

fn updated_prefs(text: &str, current: &str) -> String {
    if seen_from(text).is_some_and(|seen| compare_versions(seen, current) != Ordering::Less) { return text.to_owned(); }
    let lines: Vec<&str> = text.lines().collect();
    let about = lines.iter().position(|line| line.trim() == "[about]");
    let mut out = String::new();
    let mut inserted = false;
    for (i, line) in lines.iter().enumerate() {
        if about.is_some_and(|start| i > start) && line.trim_start().starts_with('[') && !inserted {
            out.push_str(&format!("whats_new_seen = \"{current}\"\n"));
            inserted = true;
        }
        if about.is_some_and(|start| i > start) && !inserted && line.trim_start().starts_with("whats_new_seen") && line.trim_start()["whats_new_seen".len()..].trim_start().starts_with('=') {
            out.push_str(&format!("whats_new_seen = \"{current}\"\n"));
            inserted = true;
            continue;
        }
        out.push_str(line); out.push('\n');
    }
    if !inserted {
        if about.is_none() { if !out.is_empty() { out.push('\n'); } out.push_str("[about]\n"); }
        out.push_str(&format!("whats_new_seen = \"{current}\"\n"));
    }
    out
}

fn prefs_path() -> std::path::PathBuf { crate::config_dir().join("prefs.toml") }
fn record_seen(path: &Path, current: &str) -> std::io::Result<()> {
    let text = match fs::read_to_string(path) { Ok(text) => text, Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(), Err(e) => return Err(e) };
    let new_text = updated_prefs(&text, current);
    if new_text == text { return Ok(()); }
    if let Some(parent) = path.parent() { fs::create_dir_all(parent)?; }
    #[cfg(windows)]
    return fs::write(path, new_text);
    #[cfg(not(windows))]
    {
    let tmp = path.with_extension("toml.about-tmp");
    fs::write(&tmp, new_text)?;
    fs::rename(&tmp, path)
    }
}

fn open_url(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    let status = Command::new("open").arg(url).status()?;
    #[cfg(windows)]
    let status = Command::new("cmd").args(["/c", "start", "", url]).status()?;
    if status.success() { Ok(()) } else { Err(std::io::Error::other(format!("open exited with {status}"))) }
}

#[cfg(target_os = "macos")]
fn homebrew() -> Option<&'static str> {
    let exe = std::env::current_exe().ok()?;
    let app = app_bundle_of(&exe)?;
    ["/opt/homebrew", "/usr/local"].into_iter().find(|prefix| installed_with_homebrew(Path::new(prefix), app))
}

#[cfg(target_os = "macos")]
fn app_bundle_of(exe: &Path) -> Option<&Path> {
    exe.ancestors().nth(3).filter(|bundle| bundle.extension().is_some_and(|ext| ext == "app"))
}

// Flowseam ships as a cask, which links Caskroom/flowseam/<version>/Flowseam.app to the app it installed.
// Only that copy is updated by brew: a copy from the zip running beside it does not count.
#[cfg(target_os = "macos")]
fn installed_with_homebrew(prefix: &Path, app: &Path) -> bool {
    let (Ok(app), Ok(versions)) = (app.canonicalize(), fs::read_dir(prefix.join("Caskroom/flowseam"))) else { return false };
    versions.flatten().any(|v| v.path().join("Flowseam.app").canonicalize().is_ok_and(|linked| linked == app))
}

#[cfg(target_os = "macos")]
fn upgrade_with_homebrew(prefix: &str) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::temp_dir().join("flowseam-update.command");
    fs::write(&path, format!("#!/bin/zsh\n'{prefix}/bin/brew' upgrade --cask operonlab/tap/flowseam\n"))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
    let status = Command::new("open").arg(&path).status()?;
    if status.success() { Ok(()) } else { Err(std::io::Error::other(format!("open exited with {status}"))) }
}

#[derive(Clone, Copy)]
enum Sheet { WhatsNew, UpdateCheck }

pub struct About { sheet: Option<Sheet>, releases: Vec<&'static Release>, pending: Vec<&'static Release>, error: Option<String> }
impl About {
    pub fn new(used_before: bool) -> Self {
        let seen = fs::read_to_string(prefs_path()).ok().and_then(|text| seen_from(&text).map(str::to_owned));
        let mut pending = versions_to_show(version(), seen.as_deref(), used_before);
        // For screenshots of a sheet without changing prefs, as FLOWSEAM_PAGE does for pages.
        match std::env::var("FLOWSEAM_SHEET").as_deref() {
            Ok("whatsnew") => pending = NOTES.iter().collect(),
            Ok("update") => show_update_check(),
            _ => {}
        }
        Self { sheet: None, releases: Vec::new(), pending, error: None }
    }

    pub fn draw(&mut self, ctx: &egui::Context) {
        if self.sheet.is_none() {
            if UPDATE_CHECK_REQUESTED.swap(false, AtomicOrdering::AcqRel) { self.sheet = Some(Sheet::UpdateCheck); }
            else if WHATS_NEW_REQUESTED.swap(false, AtomicOrdering::AcqRel) { self.releases = NOTES.iter().filter(|r| r.version == version()).collect(); self.sheet = Some(Sheet::WhatsNew); }
            else if !self.pending.is_empty() { self.releases = std::mem::take(&mut self.pending); self.sheet = Some(Sheet::WhatsNew); }
        }
        match self.sheet {
            Some(Sheet::WhatsNew) => self.draw_whats_new(ctx),
            Some(Sheet::UpdateCheck) => self.draw_update_check(ctx),
            None => {},
        }
    }

    fn draw_whats_new(&mut self, ctx: &egui::Context) {
        let releases = self.releases.clone();
        let title = crate::i18n::trf("Flowseam {} 的新功能", &[&version()]);
        let (action, closing) = sheet(ctx, "whats-new", Icon::Arrows, BLUE, &title, crate::i18n::tr("這一版的更新內容"), |ui, th| {
            // Room for the header and the buttons, whatever the window's height.
            let room = (ui.ctx().content_rect().height() - 300.0).clamp(140.0, 380.0);
            egui::ScrollArea::vertical().max_height(room).show(ui, |ui| {
                for (i, release) in releases.iter().enumerate() {
                    if releases.len() > 1 {
                        if i > 0 { ui.add_space(6.0); }
                        ui.label(bold(release.version, 12.0).color(th.text2));
                        ui.add_space(10.0);
                    }
                    for note in release.items {
                        sheet_item(ui, th, note.icon, note.tint, crate::i18n::tr(note.title), crate::i18n::tr(note.body));
                    }
                }
            });
            if let Some(error) = &self.error { ui.label(RichText::new(crate::i18n::trf("無法完成操作：{}", &[error])).size(11.5).color(crate::gui::RED)); ui.add_space(8.0); }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let changelog = link(ui, th, crate::i18n::tr("完整更新紀錄")).clicked();
                let done = ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| primary(ui, th, crate::i18n::tr("繼續")).clicked()).inner;
                if done { Some(true) } else if changelog { Some(false) } else { None }
            }).inner
        });
        match action {
            Some(false) => self.error = open_url(CHANGELOG_URL).err().map(|e| e.to_string()),
            Some(true) => self.close_whats_new(),
            None if closing => self.close_whats_new(),
            None => {}
        }
    }

    /// Closing the sheet, by its button or by Escape, counts as having seen these notes.
    fn close_whats_new(&mut self) {
        match record_seen(&prefs_path(), version()) {
            Ok(()) => { self.sheet = None; self.error = None; }
            Err(e) => self.error = Some(e.to_string()),
        }
    }

    fn draw_update_check(&mut self, ctx: &egui::Context) {
        #[derive(PartialEq)]
        enum Do { Releases, Brew, Copy, Done }
        let subtitle = crate::i18n::trf("目前版本 {}", &[&version()]);
        #[cfg(target_os = "macos")]
        let brew = homebrew();
        #[cfg(not(target_os = "macos"))]
        let brew: Option<&str> = None;
        let error = self.error.clone();
        let (action, closing) = sheet(ctx, "update-check", Icon::Download, GREEN, crate::i18n::tr("檢查更新"), &subtitle, |ui, th| {
            let mut act = None;
            ui.vertical_centered(|ui| {
                ui.add(egui::Label::new(RichText::new(crate::i18n::tr("Flowseam 不會自行連網。請到版本頁查看是否有新版本。")).size(12.5).color(th.text2)).wrap());
            });
            ui.add_space(16.0);
            if cfg!(target_os = "macos") && brew.is_none() {
                ui.label(RichText::new(crate::i18n::tr("可用 Homebrew 安裝：")).size(12.0).color(th.text2));
                ui.add_space(6.0);
                egui::Frame::new().fill(th.field).corner_radius(egui::CornerRadius::same(8)).inner_margin(egui::Margin::symmetric(12, 8)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("brew install --cask operonlab/tap/flowseam").monospace().size(12.0).color(th.text));
                        ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| { if link(ui, th, crate::i18n::tr("拷貝")).clicked() { act = Some(Do::Copy); } });
                    });
                });
                ui.add_space(16.0);
            }
            if let Some(error) = &error { ui.label(RichText::new(crate::i18n::trf("無法完成操作：{}", &[error])).size(11.5).color(crate::gui::RED)); ui.add_space(8.0); }
            ui.horizontal(|ui| {
                if brew.is_some() && secondary(ui, th, crate::i18n::tr("用 Homebrew 更新")).clicked() { act = Some(Do::Brew); }
                ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                    if primary(ui, th, crate::i18n::tr("打開版本頁")).clicked() { act = Some(Do::Releases); }
                    if secondary(ui, th, crate::i18n::tr("完成")).clicked() { act = Some(Do::Done); }
                });
            });
            act
        });
        match action {
            Some(Do::Releases) => self.error = open_url(RELEASES_URL).err().map(|e| e.to_string()),
            #[cfg(target_os = "macos")]
            Some(Do::Brew) => self.error = brew.and_then(|prefix| upgrade_with_homebrew(prefix).err()).map(|e| e.to_string()),
            #[cfg(not(target_os = "macos"))]
            Some(Do::Brew) => {}
            Some(Do::Copy) => ctx.copy_text("brew install --cask operonlab/tap/flowseam".to_string()),
            Some(Do::Done) => { self.sheet = None; self.error = None; }
            None if closing => { self.sheet = None; self.error = None; }
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dotted_numeric_versions() { assert_eq!(compare_versions("0.1.10", "0.1.9"), Ordering::Greater); assert_eq!(compare_versions("0.1", "0.1.0"), Ordering::Equal); }
    #[test]
    fn policy_fresh_and_existing() { assert!(versions_to_show("0.1.1", None, false).is_empty()); assert_eq!(versions_to_show("0.1.1", None, true).iter().map(|r| r.version).collect::<Vec<_>>(), ["0.1.1"]); }
    #[test]
    fn policy_since_seen() { assert_eq!(versions_to_show("0.1.1", Some("0.0.9"), true).iter().map(|r| r.version).collect::<Vec<_>>(), ["0.1.1", "0.1.0"]); assert!(versions_to_show("0.1.0", Some("0.1.1"), true).is_empty()); }
    #[test]
    fn prefs_preserve_unknown_and_never_decrease() { let source = "language = \"ja\"\nother = 2\n\n[about]\nwhats_new_seen = \"0.1.1\"\ncustom = true\n\n[future]\nkey = 3\n"; assert_eq!(updated_prefs(source, "0.1.0"), source); let changed = updated_prefs(source, "0.1.2"); assert!(changed.contains("language = \"ja\"")); assert!(changed.contains("custom = true")); assert!(changed.contains("[future]\nkey = 3")); assert_eq!(seen_from(&changed), Some("0.1.2")); }
    #[cfg(target_os = "macos")]
    #[test]
    fn the_app_bundle_is_three_levels_above_the_executable() {
        assert_eq!(app_bundle_of(Path::new("/Applications/Flowseam.app/Contents/MacOS/flowseam")), Some(Path::new("/Applications/Flowseam.app")));
        assert_eq!(app_bundle_of(Path::new("/tmp/flowseam/target/debug/flowseam")), None);
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn only_the_copy_the_cask_installed_counts_as_homebrew() {
        let root = std::env::temp_dir().join(format!("flowseam-brew-test-{}-{:?}", std::process::id(), std::thread::current().id()));
        let (brew, formula) = (root.join("brew"), root.join("formula"));
        let (cask_app, zip_app) = (root.join("Applications/Flowseam.app"), root.join("Downloads/Flowseam.app"));
        fs::create_dir_all(&cask_app).unwrap();
        fs::create_dir_all(&zip_app).unwrap();
        fs::create_dir_all(brew.join("Caskroom/flowseam/.metadata")).unwrap();
        fs::create_dir_all(brew.join("Caskroom/flowseam/0.1.0")).unwrap();
        std::os::unix::fs::symlink(&cask_app, brew.join("Caskroom/flowseam/0.1.0/Flowseam.app")).unwrap();
        fs::create_dir_all(formula.join("Cellar/flowseam/0.1.0")).unwrap();
        assert!(installed_with_homebrew(&brew, &cask_app));
        assert!(!installed_with_homebrew(&brew, &zip_app));
        assert!(!installed_with_homebrew(&formula, &cask_app));
        assert!(!installed_with_homebrew(&root.join("nothing"), &cask_app));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn record_seen_preserves_other_tables() {
        let path = std::env::temp_dir().join(format!("flowseam-about-test-{}-{:?}.toml", std::process::id(), std::thread::current().id()));
        fs::write(&path, "language = \"en\"\n[other]\nvalue = 7\n").unwrap();
        record_seen(&path, "0.1.1").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "language = \"en\"\n[other]\nvalue = 7\n\n[about]\nwhats_new_seen = \"0.1.1\"\n");
        record_seen(&path, "0.1.0").unwrap();
        assert_eq!(seen_from(&fs::read_to_string(&path).unwrap()), Some("0.1.1"));
        fs::remove_file(path).unwrap();
    }
}
