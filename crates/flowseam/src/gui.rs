use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Color32, CornerRadius, FontFamily, FontId, Layout, Margin, RichText, Sense, Stroke, StrokeKind};
use flowseam_core::config::{Config, Edge, KeyMap};

use crate::model::{chord_symbols, edge_label, fingerprint_short, keys_label, PeerRow, UiState, EDGES, KEY_MAPS};
use crate::worker::{WorkerHandle, WorkerStatus};

// Apple system colors, dark-mode variants where they differ (systemGreen / Orange / Red / Purple / Gray).
const GREEN: Color32 = Color32::from_rgb(48, 209, 88);
const ORANGE: Color32 = Color32::from_rgb(255, 159, 10);
const RED: Color32 = Color32::from_rgb(255, 69, 58);
const PURPLE: Color32 = Color32::from_rgb(191, 90, 242);
const GRAY: Color32 = Color32::from_rgb(142, 142, 147);

const SIDEBAR_W: f32 = 230.0;
/// Room for the traffic lights, which sit over the sidebar now that the title bar is transparent.
const TITLEBAR_H: f32 = 52.0;

/// Colors for one appearance, after macOS 27 System Settings: a floating sidebar, grouped rounded cards
/// on a quiet window background, hairline separators.
#[derive(Clone, Copy)]
struct Theme {
    dark: bool,
    window: Color32,
    sidebar: Color32,
    card: Color32,
    line: Color32,
    text: Color32,
    text2: Color32,
    accent: Color32,
    field: Color32,
    knob_off: Color32,
}

impl Theme {
    fn of(dark: bool) -> Theme {
        if dark {
            Theme {
                dark,
                window: Color32::from_rgb(30, 30, 32),
                sidebar: Color32::from_rgb(43, 43, 46),
                card: Color32::from_rgb(44, 44, 46),
                line: Color32::from_white_alpha(22),
                text: Color32::from_rgb(245, 245, 247),
                text2: Color32::from_rgb(152, 152, 157),
                accent: Color32::from_rgb(10, 132, 255),
                field: Color32::from_rgb(58, 58, 60),
                knob_off: Color32::from_rgb(72, 72, 74),
            }
        } else {
            Theme {
                dark,
                window: Color32::from_rgb(242, 242, 245),
                sidebar: Color32::from_rgb(232, 232, 236),
                card: Color32::WHITE,
                line: Color32::from_black_alpha(20),
                text: Color32::from_rgb(29, 29, 31),
                text2: Color32::from_rgb(110, 110, 115),
                accent: Color32::from_rgb(0, 122, 255),
                field: Color32::from_rgb(236, 236, 240),
                knob_off: Color32::from_rgb(220, 220, 224),
            }
        }
    }
}

pub fn run() -> i32 {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([860.0, 620.0])
            .with_min_inner_size([760.0, 520.0])
            .with_title("Flowseam")
            .with_fullsize_content_view(true)
            .with_titlebar_shown(false)
            .with_title_shown(false),
        ..Default::default()
    };
    let started = eframe::run_native(
        "Flowseam",
        opts,
        Box::new(|cc| {
            setup(&cc.egui_ctx);
            Ok(Box::new(App::new()))
        }),
    );
    match started {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("GUI 啟動失敗：{e}");
            1
        }
    }
}

const BOLD: &str = "bold";

/// SF Pro for Latin text and PingFang TC for Chinese, as macOS draws them; a bold family for titles.
/// A font that cannot be read is reported and left out (the system font fallback still covers the text).
fn setup(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let pingfang = find_pingfang();
    let faces: [(&str, Option<std::path::PathBuf>, u32, FontFamily); 4] = [
        ("sf", Some("/System/Library/Fonts/SFNS.ttf".into()), 0, FontFamily::Proportional),
        ("pingfang", pingfang.clone(), 2, FontFamily::Proportional),
        ("hn-bold", Some("/System/Library/Fonts/HelveticaNeue.ttc".into()), 1, FontFamily::Name(BOLD.into())),
        ("pingfang-sb", pingfang, 10, FontFamily::Name(BOLD.into())),
    ];
    let mut order: Vec<(FontFamily, String)> = Vec::new();
    for (name, path, index, family) in faces {
        let Some(path) = path else {
            eprintln!("flowseam: PingFang not found; Chinese text uses the system fallback font");
            continue;
        };
        match std::fs::read(&path) {
            Ok(bytes) => {
                let mut data = egui::FontData::from_owned(bytes);
                data.index = index;
                fonts.font_data.insert(name.into(), Arc::new(data));
                order.push((family, name.into()));
            }
            Err(e) => eprintln!("flowseam: font {} unreadable ({e}); using the fallback", path.display()),
        }
    }
    let base = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let mut prop: Vec<String> = order.iter().filter(|(f, _)| *f == FontFamily::Proportional).map(|(_, n)| n.clone()).collect();
    let mut bold: Vec<String> = order.iter().filter(|(f, _)| *f != FontFamily::Proportional).map(|(_, n)| n.clone()).collect();
    bold.extend(prop.iter().cloned());
    prop.extend(base.iter().cloned());
    bold.extend(base);
    fonts.families.insert(FontFamily::Proportional, prop);
    fonts.families.insert(FontFamily::Name(BOLD.into()), bold);
    ctx.set_fonts(fonts);
    // For screenshots of each appearance: FLOWSEAM_THEME=light|dark pins it; otherwise it follows the system.
    match std::env::var("FLOWSEAM_THEME").as_deref() {
        Ok("light") => ctx.set_theme(egui::ThemePreference::Light),
        Ok("dark") => ctx.set_theme(egui::ThemePreference::Dark),
        _ => {}
    }
    for dark in [false, true] {
        let th = Theme::of(dark);
        let mut style = egui::Style { visuals: if dark { egui::Visuals::dark() } else { egui::Visuals::light() }, ..Default::default() };
        let v = &mut style.visuals;
        v.panel_fill = th.window;
        v.window_fill = th.card;
        v.extreme_bg_color = th.field;
        v.faint_bg_color = th.field;
        v.override_text_color = Some(th.text);
        v.hyperlink_color = th.accent;
        v.selection.bg_fill = th.accent.gamma_multiply(0.45);
        v.selection.stroke = Stroke::new(1.0, th.accent);
        v.window_corner_radius = CornerRadius::same(12);
        v.menu_corner_radius = CornerRadius::same(10);
        v.window_stroke = Stroke::new(1.0, th.line);
        for w in [&mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
            w.corner_radius = CornerRadius::same(7);
            w.bg_fill = th.field;
            w.weak_bg_fill = th.field;
            w.bg_stroke = Stroke::NONE;
            w.fg_stroke = Stroke::new(1.0, th.text);
        }
        v.widgets.hovered.weak_bg_fill = th.field.gamma_multiply(if dark { 1.25 } else { 0.95 });
        v.widgets.hovered.bg_fill = v.widgets.hovered.weak_bg_fill;
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, th.line);
        v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, th.text);
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(12.0, 5.0);
        style.spacing.interact_size.y = 26.0;
        style.spacing.slider_width = 200.0;
        style.text_styles = [
            (egui::TextStyle::Body, FontId::proportional(13.0)),
            (egui::TextStyle::Button, FontId::proportional(13.0)),
            (egui::TextStyle::Small, FontId::proportional(11.0)),
            (egui::TextStyle::Monospace, FontId::monospace(12.0)),
            (egui::TextStyle::Heading, FontId::new(22.0, FontFamily::Name(BOLD.into()))),
        ]
        .into();
        ctx.set_style_of(if dark { egui::Theme::Dark } else { egui::Theme::Light }, style);
    }
}

/// PingFang ships as a downloadable font asset whose folder name is a hash.
fn find_pingfang() -> Option<std::path::PathBuf> {
    let root = std::path::Path::new("/System/Library/AssetsV2");
    for family in std::fs::read_dir(root).ok()?.flatten() {
        if !family.file_name().to_string_lossy().starts_with("com_apple_MobileAsset_Font") {
            continue;
        }
        for asset in std::fs::read_dir(family.path()).ok()?.flatten() {
            let p = asset.path().join("AssetData/PingFang.ttc");
            if p.exists() {
                return Some(p);
            }
        }
    }
    None
}

fn bold(text: impl Into<String>, size: f32) -> RichText {
    RichText::new(text).family(FontFamily::Name(BOLD.into())).size(size)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Overview,
    Edges,
    Switch,
    Pairing,
    Advanced,
}

/// Sidebar: page, label, icon tile color, icon.
const PAGES: [(Page, &str, Color32, Icon); 5] = [
    (Page::Overview, "總覽", Color32::from_rgb(10, 132, 255), Icon::Grid),
    (Page::Edges, "裝置與邊界", PURPLE, Icon::Display),
    (Page::Switch, "切換方式", ORANGE, Icon::Arrows),
    (Page::Pairing, "配對", GREEN, Icon::Link),
    (Page::Advanced, "進階", GRAY, Icon::Sliders),
];

/// The page to open on: FLOWSEAM_PAGE=edges|switch|pairing|advanced, for screenshots; the overview otherwise.
fn start_page() -> Page {
    match std::env::var("FLOWSEAM_PAGE").as_deref() {
        Ok("edges") => Page::Edges,
        Ok("switch") => Page::Switch,
        Ok("pairing") => Page::Pairing,
        Ok("advanced") => Page::Advanced,
        _ => Page::Overview,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Icon {
    Grid,
    Display,
    Arrows,
    Link,
    Sliders,
}

struct Pairing {
    peer: usize,
    code: String,
    /// The row and address a running pairing was started for; the result is applied only there.
    target: Option<(usize, String)>,
    rx: Option<Receiver<Result<flowseam_net::identity::Fingerprint, String>>>,
}

struct App {
    page: Page,
    ui: UiState,
    /// The configuration as last loaded or saved, to tell whether there are unsaved changes.
    /// `None` when the file could not be read: nothing on disk matches the screen, so save stays enabled.
    saved: Option<Config>,
    notice: String,
    errors: Vec<String>,
    worker: Option<WorkerHandle>,
    stop_at: Option<Instant>,
    status: WorkerStatus,
    perms: Vec<String>,
    pairing: Pairing,
}

fn check_permissions() -> Vec<String> {
    use flowseam_core::backend::Preflight;
    let mut out = Vec::new();
    #[cfg(target_os = "macos")]
    for (what, p) in [("擷取", flowseam_mac::sys::capture_preflight()), ("注入", flowseam_mac::sys::emit_preflight())] {
        match p {
            Preflight::Ok => {}
            Preflight::Missing(m) => out.push(format!("{what}缺權限：{m:?}")),
            Preflight::Unknown(why) => out.push(format!("{what}權限查不到：{why}")),
        }
    }
    #[cfg(not(target_os = "macos"))]
    out.push("權限檢查只支援 macOS（查不到）".to_string());
    out
}

impl App {
    fn new() -> Self {
        let (cfg, saved, notice) = match Config::load(&crate::config_path()) {
            Ok(c) => (c.clone(), Some(c), String::new()),
            Err(e) => (Config::default(), None, format!("設定檔讀取失敗，改用預設（未覆寫檔案）：{e}")),
        };
        App {
            page: start_page(),
            ui: UiState::from_config(&cfg),
            saved,
            notice,
            errors: Vec::new(),
            worker: None,
            stop_at: None,
            status: WorkerStatus::Stopped,
            perms: check_permissions(),
            pairing: Pairing { peer: 0, code: String::new(), target: None, rx: None },
        }
    }

    fn dirty(&self) -> bool {
        self.saved.as_ref() != Some(&self.ui.to_config())
    }

    fn save(&mut self) -> bool {
        self.errors = self.ui.problems();
        if !self.errors.is_empty() {
            self.notice = "尚未儲存，請先修正下方標示的問題".into();
            return false;
        }
        let cfg = self.ui.to_config();
        match cfg.save(&crate::config_path()) {
            Ok(()) => {
                self.saved = Some(cfg);
                self.notice = "已儲存".into();
                true
            }
            Err(e) => {
                self.errors = vec![e.to_string()];
                self.notice = "儲存失敗".into();
                false
            }
        }
    }

    fn tick(&mut self) {
        if let Some(w) = &self.worker {
            if let Some(end) = w.poll() {
                self.status = end;
                self.worker = None;
                self.stop_at = None;
            } else if let Some(t) = self.stop_at {
                w.escalate(t);
            }
        }
        if let Some(rx) = &self.pairing.rx {
            if let Ok(res) = rx.try_recv() {
                self.pairing.rx = None;
                let target = self.pairing.target.take();
                self.notice = match (res, target) {
                    (Ok(fp), Some((idx, addr))) => match self.ui.apply_pairing(idx, &addr, &fp) {
                        Ok(()) => "配對完成，請按「儲存」".into(),
                        Err(e) => e,
                    },
                    (Ok(_), None) => "配對完成，但找不到它是為哪台裝置開始的，請重新配對".into(),
                    (Err(e), _) => format!("配對失敗：{e}"),
                };
            }
        }
    }

    fn start_pairing(&mut self) {
        let Some(row) = self.ui.peers.get(self.pairing.peer) else {
            self.notice = "請先到「裝置與邊界」新增一台裝置".into();
            return;
        };
        let code = self.pairing.code.trim().to_string();
        if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
            self.notice = "配對碼是 6 位數字".into();
            return;
        }
        let addr = row.addr.trim().to_string();
        let (tx, rx) = mpsc::channel();
        self.pairing.target = Some((self.pairing.peer, addr.clone()));
        self.pairing.rx = Some(rx);
        self.notice = "配對中…".into();
        std::thread::spawn(move || {
            let res = crate::pair_as_client(&addr, &code);
            let _ = tx.send(res);
        });
    }

    fn start_worker(&mut self) {
        // worker 讀的是磁碟上的檔案，所以執行中的設定一定等於已儲存的設定。
        if !self.save() {
            return;
        }
        let unpaired = self.ui.to_config().unpaired().join("、");
        if !unpaired.is_empty() {
            self.errors = vec![format!("尚未配對：{unpaired}（先到配對頁完成，或移除該裝置）")];
            self.notice = "無法啟動：有裝置尚未配對".into();
            return;
        }
        match WorkerHandle::spawn(&crate::config_path()) {
            Ok(w) => {
                self.worker = Some(w);
                self.status = WorkerStatus::Running;
            }
            Err(e) => self.status = WorkerStatus::Died(format!("無法啟動：{e}")),
        }
    }

    fn stop_worker(&mut self) {
        if let Some(w) = &mut self.worker {
            w.stop();
            self.stop_at = Some(Instant::now());
        }
    }

    fn sidebar(&mut self, ui: &mut egui::Ui, th: Theme) {
        // The floating sidebar: an inset rounded panel the traffic lights sit on.
        let panel = ui.max_rect().shrink(6.0);
        ui.painter().rect(panel, CornerRadius::same(16), th.sidebar, Stroke::new(1.0, th.line), StrokeKind::Inside);
        let inner = panel.shrink2(egui::vec2(10.0, 0.0));
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(inner).layout(Layout::top_down(Align::Min)));
        let ui = &mut child;
        ui.add_space(TITLEBAR_H - 8.0);
        ui.label(bold("Flowseam", 15.0));
        ui.label(RichText::new("鍵盤與滑鼠共用").size(11.5).color(th.text2));
        ui.add_space(12.0);
        for (page, label, tint, icon) in PAGES {
            if sidebar_item(ui, th, self.page == page, label, tint, icon).clicked() {
                self.page = page;
            }
        }
        // The sharing switch stays in reach on every page, at the foot of the sidebar.
        let (dot, state, _) = self.state_text();
        let foot = egui::Rect::from_min_max(egui::pos2(inner.left(), inner.bottom() - 46.0), egui::pos2(inner.right(), inner.bottom() - 8.0));
        let mut foot_ui = ui.new_child(egui::UiBuilder::new().max_rect(foot).layout(Layout::left_to_right(Align::Center)));
        let (d, _) = foot_ui.allocate_exact_size(egui::vec2(10.0, 10.0), Sense::hover());
        foot_ui.painter().circle_filled(d.center(), 4.0, dot);
        foot_ui.label(RichText::new(state).size(12.5));
        foot_ui.with_layout(Layout::right_to_left(Align::Center), |ui| self.sharing_switch(ui, th));
    }

    /// Status dot color, one word, and a sentence for where sharing stands.
    fn state_text(&self) -> (Color32, &'static str, String) {
        match &self.status {
            _ if self.stop_at.is_some() => (ORANGE, "停止中…", "正在把控制權還給這台電腦。".to_string()),
            _ if self.worker.is_some() => (GREEN, "共用中", "滑鼠移到邊界停留，或按快捷鍵，即可切換到另一台。".to_string()),
            WorkerStatus::Died(why) => (RED, "已中斷", why.clone()),
            _ => (GRAY, "未共用", "打開開關，這台電腦就會參與共用。".to_string()),
        }
    }

    fn sharing_switch(&mut self, ui: &mut egui::Ui, th: Theme) {
        let stopping = self.stop_at.is_some();
        let on = self.worker.is_some() && !stopping;
        if ui.add_enabled_ui(!stopping, |ui| toggle(ui, th, on)).inner.clicked() {
            if on {
                self.stop_worker();
            } else {
                self.start_worker();
            }
        }
    }

    /// The main switch, as a System Settings row: what it does, where it stands, a toggle.
    fn hero(&mut self, ui: &mut egui::Ui, th: Theme) {
        let (dot, state, detail) = self.state_text();
        card(ui, th, |ui| {
            row_frame(ui, |ui| {
                let (r, _) = ui.allocate_exact_size(egui::vec2(40.0, 40.0), Sense::hover());
                ui.painter().rect_filled(r, CornerRadius::same(10), th.accent);
                paint_icon(ui.painter(), r.shrink(9.0), Icon::Arrows, Color32::WHITE);
                ui.add_space(4.0);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(bold("共用鍵盤與滑鼠", 14.0));
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 5.0;
                        let (d, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), Sense::hover());
                        ui.painter().circle_filled(d.center(), 4.0, dot);
                        ui.label(RichText::new(state).size(12.0).color(th.text2));
                        ui.label(RichText::new("·").size(12.0).color(th.text2));
                        ui.label(RichText::new(detail).size(12.0).color(th.text2));
                    });
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| self.sharing_switch(ui, th));
            });
        });
        if !self.perms.is_empty() {
            ui.add_space(10.0);
            egui::Frame::new()
                .fill(ORANGE.gamma_multiply(if th.dark { 0.16 } else { 0.12 }))
                .stroke(Stroke::new(1.0, ORANGE.gamma_multiply(0.5)))
                .corner_radius(CornerRadius::same(12))
                .inner_margin(Margin::symmetric(14, 12))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(bold("需要系統權限", 13.0).color(ORANGE));
                    for p in &self.perms {
                        ui.label(RichText::new(p).size(12.0));
                    }
                    ui.label(RichText::new("到「系統設定 › 隱私權與安全性」允許 Flowseam，完成後按「重新檢查」。").size(12.0).color(th.text2));
                    ui.horizontal(|ui| {
                        if secondary(ui, th, "打開系統設定").clicked() {
                            let _ = std::process::Command::new("open")
                                .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
                                .spawn();
                        }
                        if secondary(ui, th, "重新檢查").clicked() {
                            self.perms = check_permissions();
                        }
                    });
                });
        }
    }

    /// Like System Settings › Displays › Arrange: this Mac in the middle, each peer on its side.
    fn arrangement(&self, ui: &mut egui::Ui, th: Theme) {
        let (area, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 176.0), Sense::hover());
        let p = ui.painter();
        let main = egui::Rect::from_center_size(area.center() - egui::vec2(0.0, 8.0), egui::vec2(168.0, 104.0));
        screen(p, main, th, true);
        p.text(main.center_bottom() + egui::vec2(0.0, 14.0), egui::Align2::CENTER_CENTER, "這台 Mac", FontId::new(12.0, FontFamily::Name(BOLD.into())), th.text);
        for peer in &self.ui.peers {
            let size = egui::vec2(116.0, 72.0);
            let gap = 22.0;
            let c = match peer.edge {
                Edge::Left => main.left_center() - egui::vec2(gap + size.x / 2.0, 0.0),
                Edge::Right => main.right_center() + egui::vec2(gap + size.x / 2.0, 0.0),
                Edge::Top => main.center_top() - egui::vec2(0.0, gap + size.y / 2.0 - 6.0),
                Edge::Bottom => main.center_bottom() + egui::vec2(0.0, gap + size.y / 2.0 + 10.0),
            };
            let r = egui::Rect::from_center_size(c, size);
            screen(p, r, th, false);
            let name = if peer.name.trim().is_empty() { "（未命名）" } else { peer.name.trim() };
            let paired = !peer.fingerprint.is_empty();
            p.text(r.center(), egui::Align2::CENTER_CENTER, name, FontId::new(12.0, FontFamily::Name(BOLD.into())), Color32::WHITE);
            let label_at = if peer.edge == Edge::Top { r.center_top() - egui::vec2(0.0, 10.0) } else { r.center_bottom() + egui::vec2(0.0, 12.0) };
            p.circle_filled(label_at - egui::vec2(30.0, 0.0), 3.5, if paired { GREEN } else { ORANGE });
            p.text(label_at - egui::vec2(22.0, 0.0), egui::Align2::LEFT_CENTER, if paired { "已配對" } else { "未配對" }, FontId::proportional(11.0), th.text2);
        }
    }

    fn page_overview(&mut self, ui: &mut egui::Ui, th: Theme) {
        card(ui, th, |ui| {
            egui::Frame::new().inner_margin(Margin::symmetric(14, 6)).show(ui, |ui| self.arrangement(ui, th));
        });
        section(ui, th, "裝置");
        card(ui, th, |ui| {
            if self.ui.peers.is_empty() {
                row(ui, th, true, "尚未加入任何裝置", Some("到「裝置與邊界」新增。"), |_| {});
            }
            for (i, p) in self.ui.peers.iter().enumerate() {
                let name = if p.name.trim().is_empty() { "（未命名）" } else { p.name.trim() };
                let sub = format!("在{}側 · {}", edge_label(p.edge), keys_short(p.keys));
                row(ui, th, i == 0, name, Some(&sub), |ui| {
                    if p.fingerprint.is_empty() {
                        badge(ui, "未配對", ORANGE);
                    } else {
                        badge(ui, "已配對", GREEN);
                    }
                });
            }
        });
        section(ui, th, "切換方式");
        card(ui, th, |ui| {
            row(ui, th, true, "停留在邊界", Some("游標停住這麼久就切過去"), |ui| {
                ui.label(RichText::new(format!("{} 毫秒", self.ui.dwell_ms)).color(th.text2));
            });
            row(ui, th, false, "快捷鍵", Some("隨時切換到下一台"), |ui| {
                keycap(ui, th, &chord_symbols(&self.ui.chord));
            });
        });
    }

    fn page_edges(&mut self, ui: &mut egui::Ui, th: Theme) {
        let mut remove = None;
        let pairing_busy = self.pairing.rx.is_some();
        for (i, p) in self.ui.peers.iter_mut().enumerate() {
            if i > 0 {
                ui.add_space(14.0);
            }
            card(ui, th, |ui| {
                row_frame(ui, |ui| {
                    let (r, _) = ui.allocate_exact_size(egui::vec2(30.0, 30.0), Sense::hover());
                    ui.painter().rect_filled(r, CornerRadius::same(8), PURPLE);
                    paint_icon(ui.painter(), r.shrink(7.0), Icon::Display, Color32::WHITE);
                    ui.add(
                        egui::TextEdit::singleline(&mut p.name)
                            .hint_text("裝置名稱")
                            .font(FontId::new(14.0, FontFamily::Name(BOLD.into())))
                            .frame(egui::Frame::NONE)
                            .desired_width(220.0),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if p.fingerprint.is_empty() {
                            badge(ui, "未配對", ORANGE);
                        } else {
                            badge(ui, &format!("已配對 · {}", fingerprint_short(&p.fingerprint)), GREEN);
                        }
                    });
                });
                row(ui, th, false, "位址", Some("另一台的 IP 與連接埠"), |ui| {
                    ui.add(egui::TextEdit::singleline(&mut p.addr).hint_text("192.168.1.20:24800").desired_width(210.0));
                });
                row(ui, th, false, "位置", Some("它在這台螢幕的哪一側"), |ui| {
                    segmented(ui, th, ("edge", i), &mut p.edge, &EDGES.map(|e| (e, edge_label(e))));
                });
                row(ui, th, false, "按鍵對應", Some("Mac 鍵盤的按鍵到對方怎麼解讀"), |ui| {
                    egui::ComboBox::from_id_salt(("keys", i))
                        .selected_text(keys_label(p.keys))
                        .width(250.0)
                        .show_ui(ui, |ui| {
                            for k in KEY_MAPS {
                                ui.selectable_value(&mut p.keys, k, keys_label(k));
                            }
                        });
                });
                row(ui, th, false, "", None, |ui| {
                    let hint = if pairing_busy { "配對中，完成後才能移除" } else { "" };
                    if ui.add_enabled(!pairing_busy, egui::Button::new(RichText::new("移除這台裝置").color(RED)).frame(false)).on_disabled_hover_text(hint).clicked() {
                        remove = Some(i);
                    }
                });
            });
        }
        if let Some(i) = remove {
            self.ui.peers.remove(i);
            self.pairing.peer = 0;
        }
        ui.add_space(14.0);
        ui.add_enabled_ui(self.ui.peers.len() < 4, |ui| {
            if secondary(ui, th, "＋ 新增裝置").clicked() {
                let free = EDGES.into_iter().find(|e| self.ui.peers.iter().all(|p| p.edge != *e)).unwrap_or(Edge::Right);
                self.ui.peers.push(PeerRow { name: String::new(), addr: String::new(), edge: free, fingerprint: String::new(), keys: KeyMap::default() });
            }
        });
    }

    fn page_switch(&mut self, ui: &mut egui::Ui, th: Theme) {
        section(ui, th, "停留切換");
        card(ui, th, |ui| {
            row(ui, th, true, "停留時間", Some("游標停在邊界多久才切過去"), |ui| {
                ui.add(egui::Slider::new(&mut self.ui.dwell_ms, 50..=5000).suffix(" 毫秒").logarithmic(true));
            });
            row(ui, th, false, "重新計時距離", Some("離開邊界多遠才重新計時"), |ui| {
                ui.add(egui::Slider::new(&mut self.ui.arm_dist, 1..=64).suffix(" px"));
            });
            row(ui, th, false, "邊界寬度", Some("貼邊多少像素內算在邊界上"), |ui| {
                ui.add(egui::Slider::new(&mut self.ui.boundary_px, 1..=32).suffix(" px"));
            });
        });
        section(ui, th, "快捷鍵切換");
        card(ui, th, |ui| {
            row(ui, th, true, "組合鍵", Some("格式如 ctrl+alt+cmd+s"), |ui| {
                keycap(ui, th, &chord_symbols(&self.ui.chord));
                ui.add(egui::TextEdit::singleline(&mut self.ui.chord).desired_width(150.0));
            });
            if let Err(e) = crate::model::check_chord(&self.ui.chord) {
                row(ui, th, false, "", None, |ui| {
                    ui.label(RichText::new(e).size(12.0).color(RED));
                });
            }
        });
        ui.add_space(14.0);
        if secondary(ui, th, "恢復預設").clicked() {
            let d = flowseam_core::config::Switch::default();
            self.ui.dwell_ms = d.dwell_ms;
            self.ui.arm_dist = d.arm_dist;
            self.ui.boundary_px = d.boundary_px;
            self.ui.chord = d.chord;
        }
    }

    fn page_pairing(&mut self, ui: &mut egui::Ui, th: Theme) {
        card(ui, th, |ui| {
            for (i, step) in ["在另一台電腦開啟配對，畫面會顯示 6 位數字。", "選擇對應的裝置，輸入數字後按「配對」。", "配對一次即可，之後自動信任。"].iter().enumerate() {
                row_frame_sep(ui, th, i > 0, |ui| {
                    let (r, _) = ui.allocate_exact_size(egui::vec2(22.0, 22.0), Sense::hover());
                    ui.painter().circle_filled(r.center(), 11.0, th.accent);
                    ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, (i + 1).to_string(), FontId::new(12.0, FontFamily::Name(BOLD.into())), Color32::WHITE);
                    ui.label(*step);
                });
            }
        });
        section(ui, th, "配對");
        if self.ui.peers.is_empty() {
            card(ui, th, |ui| {
                row(ui, th, true, "還沒有可配對的裝置", Some("請先到「裝置與邊界」新增裝置。"), |_| {});
            });
            return;
        }
        let names = self.ui.pairing_labels();
        let busy = self.pairing.rx.is_some();
        let mut go = false;
        card(ui, th, |ui| {
            row(ui, th, true, "裝置", None, |ui| {
                ui.add_enabled_ui(!busy, |ui| {
                    egui::ComboBox::from_id_salt("pairpeer")
                        .selected_text(names.get(self.pairing.peer).cloned().unwrap_or_default())
                        .width(260.0)
                        .show_ui(ui, |ui| {
                            for (i, n) in names.iter().enumerate() {
                                ui.selectable_value(&mut self.pairing.peer, i, n);
                            }
                        });
                });
            });
            row(ui, th, false, "配對碼", Some("另一台畫面上的 6 位數字"), |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.pairing.code)
                        .hint_text("000000")
                        .font(FontId::monospace(16.0))
                        .char_limit(6)
                        .align(egui::Align2::CENTER_CENTER)
                        .desired_width(110.0),
                );
            });
            row(ui, th, false, "", None, |ui| {
                if ui.add_enabled_ui(!busy, |ui| primary(ui, th, if busy { "配對中…" } else { "配對" })).inner.clicked() {
                    go = true;
                }
            });
        });
        if go {
            self.start_pairing();
        }
    }

    fn page_advanced(&mut self, ui: &mut egui::Ui, th: Theme) {
        card(ui, th, |ui| {
            row(ui, th, true, "接收位址", Some("這台電腦監聽的位址，大多數情況保持預設"), |ui| {
                ui.add(egui::TextEdit::singleline(&mut self.ui.listen).desired_width(180.0));
            });
            let path = crate::config_path();
            row(ui, th, false, "設定檔", Some(&path.display().to_string()), |ui| {
                if secondary(ui, th, "在 Finder 中顯示").clicked() {
                    let _ = std::process::Command::new("open").arg("-R").arg(&path).spawn();
                }
            });
        });
    }

    fn footer(&mut self, ui: &mut egui::Ui, th: Theme) {
        let dirty = self.dirty();
        ui.horizontal(|ui| {
            ui.set_min_height(30.0);
            let (text, color) = if !self.errors.is_empty() {
                (self.notice.clone(), RED)
            } else if self.saved.is_none() && !self.notice.is_empty() {
                (self.notice.clone(), ORANGE)
            } else if dirty {
                ("有尚未儲存的變更".to_string(), th.text2)
            } else if self.notice.is_empty() {
                ("所有變更都已儲存".to_string(), th.text2)
            } else {
                (self.notice.clone(), th.text2)
            };
            ui.label(RichText::new(text).size(12.0).color(color));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add_enabled_ui(dirty, |ui| primary(ui, th, "儲存")).inner.clicked() {
                    self.save();
                }
            });
        });
        for e in &self.errors {
            ui.label(RichText::new(e).size(12.0).color(RED));
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.tick();
        ctx.request_repaint_after(Duration::from_millis(200));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let th = Theme::of(ui.ctx().theme() == egui::Theme::Dark);
        egui::Panel::left("nav")
            .exact_size(SIDEBAR_W)
            .resizable(false)
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(th.window))
            .show(ui, |ui| self.sidebar(ui, th));

        egui::Panel::bottom("footer")
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(th.window).inner_margin(Margin { left: 24, right: 24, top: 8, bottom: 12 }).stroke(Stroke::NONE))
            .show(ui, |ui| self.footer(ui, th));

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(th.window).inner_margin(Margin { left: 24, right: 24, top: 0, bottom: 0 }))
            .show(ui, |ui| {
                ui.add_space(TITLEBAR_H - 30.0);
                let title = PAGES.iter().find(|p| p.0 == self.page).map_or("", |p| p.1);
                ui.label(bold(title, 22.0));
                ui.add_space(10.0);
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    match self.page {
                        Page::Overview => {
                            self.hero(ui, th);
                            ui.add_space(14.0);
                            self.page_overview(ui, th)
                        }
                        Page::Edges => self.page_edges(ui, th),
                        Page::Switch => self.page_switch(ui, th),
                        Page::Pairing => self.page_pairing(ui, th),
                        Page::Advanced => self.page_advanced(ui, th),
                    }
                    ui.add_space(12.0);
                });
            });

        // The title bar is transparent: dragging the band at the top of the window moves it.
        let ctx = ui.ctx().clone();
        let band = egui::Rect::from_min_size(ctx.content_rect().min + egui::vec2(80.0, 0.0), egui::vec2(ctx.content_rect().width() - 80.0, 28.0));
        if ui.interact(band, ui.scope_id().with("titlebar-drag"), Sense::drag()).drag_started() {
            ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
    }
}

// ---- widgets -------------------------------------------------------------------------------------

/// A grouped card: rounded, hairline-bordered, filling the width.
fn card<R>(ui: &mut egui::Ui, th: Theme, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::new()
        .fill(th.card)
        .stroke(Stroke::new(1.0, th.line))
        .corner_radius(CornerRadius::same(12))
        .shadow(egui::Shadow { offset: [0, 1], blur: 3, spread: 0, color: Color32::from_black_alpha(if th.dark { 40 } else { 10 }) })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            add(ui)
        })
        .inner
}

/// A small gray heading above a card.
fn section(ui: &mut egui::Ui, th: Theme, title: &str) {
    ui.add_space(18.0);
    ui.horizontal(|ui| {
        ui.add_space(6.0);
        ui.label(bold(title, 12.5).color(th.text2));
    });
    ui.add_space(6.0);
}

/// A card row's padding, without a separator.
fn row_frame(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new().inner_margin(Margin::symmetric(14, 10)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 6.0;
        ui.horizontal(|ui| {
            ui.set_min_height(28.0);
            add(ui);
        });
    });
}

/// A card row with a hairline above it unless it is the first.
fn row_frame_sep(ui: &mut egui::Ui, th: Theme, separator: bool, add: impl FnOnce(&mut egui::Ui)) {
    if separator {
        let r = ui.available_rect_before_wrap();
        let y = r.top();
        ui.painter().hline((r.left() + 14.0)..=(r.right() - 14.0), y, Stroke::new(1.0, th.line));
    }
    row_frame(ui, add);
}

/// The System Settings row: title (and a gray subtitle) on the left, the control on the right.
fn row(ui: &mut egui::Ui, th: Theme, first: bool, title: &str, subtitle: Option<&str>, control: impl FnOnce(&mut egui::Ui)) {
    row_frame_sep(ui, th, !first, |ui| {
        if !title.is_empty() || subtitle.is_some() {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                if !title.is_empty() {
                    ui.label(RichText::new(title).size(13.0));
                }
                if let Some(s) = subtitle {
                    ui.label(RichText::new(s).size(11.5).color(th.text2));
                }
            });
        }
        ui.with_layout(Layout::right_to_left(Align::Center), control);
    });
}

fn sidebar_item(ui: &mut egui::Ui, th: Theme, selected: bool, label: &str, tint: Color32, icon: Icon) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 32.0), Sense::click());
    let p = ui.painter();
    if selected {
        p.rect_filled(rect, CornerRadius::same(8), th.accent);
    } else if resp.hovered() {
        p.rect_filled(rect, CornerRadius::same(8), if th.dark { Color32::from_white_alpha(14) } else { Color32::from_black_alpha(12) });
    }
    let tile = egui::Rect::from_min_size(rect.left_center() + egui::vec2(6.0, -11.0), egui::vec2(22.0, 22.0));
    p.rect_filled(tile, CornerRadius::same(6), tint);
    paint_icon(p, tile.shrink(5.0), icon, Color32::WHITE);
    let color = if selected { Color32::WHITE } else { th.text };
    p.text(tile.right_center() + egui::vec2(9.0, 0.0), egui::Align2::LEFT_CENTER, label, FontId::proportional(13.0), color);
    resp
}

/// The macOS switch: a capsule track, a white knob that slides, accent when on.
fn toggle(ui: &mut egui::Ui, th: Theme, on: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(40.0, 24.0), Sense::click());
    let t = ui.ctx().animate_bool_with_time(resp.id, on, 0.15);
    let track = if ui.is_enabled() { lerp_color(th.knob_off, GREEN, t) } else { th.knob_off };
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(12), track);
    let x = egui::lerp((rect.left() + 12.0)..=(rect.right() - 12.0), t);
    p.circle_filled(egui::pos2(x, rect.center().y + 0.5), 10.5, Color32::from_black_alpha(40));
    p.circle_filled(egui::pos2(x, rect.center().y), 10.0, Color32::WHITE);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A segmented control: a sunken capsule with the chosen segment raised.
fn segmented<T: PartialEq + Copy>(ui: &mut egui::Ui, th: Theme, id: impl std::hash::Hash + std::fmt::Debug, value: &mut T, options: &[(T, &str)]) {
    let seg_w = 46.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(seg_w * options.len() as f32 + 4.0, 28.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(8), th.field);
    for (i, (v, label)) in options.iter().enumerate() {
        let r = egui::Rect::from_min_size(rect.min + egui::vec2(2.0 + seg_w * i as f32, 2.0), egui::vec2(seg_w, 24.0));
        let resp = ui.interact(r, ui.scope_id().with(&id).with(i), Sense::click());
        if resp.clicked() {
            *value = *v;
        }
        let selected = *value == *v;
        if selected {
            let raised = if th.dark { Color32::from_rgb(99, 99, 102) } else { Color32::WHITE };
            ui.painter().rect_filled(r.translate(egui::vec2(0.0, 0.5)), CornerRadius::same(6), Color32::from_black_alpha(30));
            ui.painter().rect_filled(r, CornerRadius::same(6), raised);
        }
        let font = if selected { FontId::new(12.5, FontFamily::Name(BOLD.into())) } else { FontId::proportional(12.5) };
        ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, *label, font, th.text);
    }
}

fn primary(ui: &mut egui::Ui, th: Theme, text: &str) -> egui::Response {
    let fill = if ui.is_enabled() { th.accent } else { th.field };
    let color = if ui.is_enabled() { Color32::WHITE } else { th.text2 };
    ui.add(egui::Button::new(RichText::new(text).color(color)).fill(fill).corner_radius(CornerRadius::same(14)).min_size(egui::vec2(72.0, 28.0)))
}

fn secondary(ui: &mut egui::Ui, th: Theme, text: &str) -> egui::Response {
    ui.add(egui::Button::new(RichText::new(text).color(th.text)).fill(th.field).corner_radius(CornerRadius::same(14)).min_size(egui::vec2(0.0, 28.0)))
}

/// A tinted capsule label: green for paired, orange for not yet.
fn badge(ui: &mut egui::Ui, text: &str, color: Color32) {
    let font = FontId::new(11.0, FontFamily::Name(BOLD.into()));
    let galley = ui.painter().layout_no_wrap(text.to_string(), font, color);
    let (r, _) = ui.allocate_exact_size(egui::vec2(galley.size().x + 16.0, 20.0), Sense::hover());
    ui.painter().rect_filled(r, CornerRadius::same(10), color.gamma_multiply(0.16));
    ui.painter().galley(r.center() - galley.size() / 2.0, galley, color);
}

/// A key mapping in a few words, for lists.
fn keys_short(k: KeyMap) -> &'static str {
    match k {
        KeyMap::Position => "依鍵盤位置",
        KeyMap::Mac => "Mac 快捷鍵",
        KeyMap::None => "不轉換",
    }
}

/// A shortcut drawn as a keycap.
fn keycap(ui: &mut egui::Ui, th: Theme, text: &str) {
    egui::Frame::new()
        .fill(th.field)
        .stroke(Stroke::new(1.0, th.line))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::symmetric(8, 3))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(13.0).color(th.text));
        });
}

/// A display: a dark bezel around a wallpaper gradient, with a menu bar strip on this Mac.
fn screen(p: &egui::Painter, r: egui::Rect, th: Theme, this_mac: bool) {
    p.rect_filled(r.expand(3.0), CornerRadius::same(10), if th.dark { Color32::from_rgb(12, 12, 14) } else { Color32::from_rgb(40, 40, 44) });
    let (a, b) = if this_mac { (Color32::from_rgb(10, 132, 255), Color32::from_rgb(191, 90, 242)) } else { (Color32::from_rgb(88, 86, 214), Color32::from_rgb(52, 120, 246)) };
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(r.left_top(), a);
    mesh.colored_vertex(r.right_top(), lerp_color(a, b, 0.5));
    mesh.colored_vertex(r.right_bottom(), b);
    mesh.colored_vertex(r.left_bottom(), lerp_color(a, b, 0.5));
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    p.add(mesh);
    if this_mac {
        let bar = egui::Rect::from_min_size(r.min, egui::vec2(r.width(), 7.0));
        p.rect_filled(bar, CornerRadius::ZERO, Color32::from_white_alpha(70));
    }
    p.rect_stroke(r.expand(3.0), CornerRadius::same(10), Stroke::new(1.0, th.line), StrokeKind::Outside);
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_unmultiplied(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()), l(a.a(), b.a()))
}

/// Line icons in the SF Symbols manner, drawn into `r`.
fn paint_icon(p: &egui::Painter, r: egui::Rect, icon: Icon, c: Color32) {
    let s = Stroke::new(1.6, c);
    let w = r.width();
    match icon {
        Icon::Grid => {
            let g = w * 0.12;
            let cell = (w - g) / 2.0;
            for (i, j) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)] {
                let q = egui::Rect::from_min_size(r.min + egui::vec2(i * (cell + g), j * (cell + g)), egui::vec2(cell, cell));
                p.rect_filled(q, CornerRadius::same(2), c);
            }
        }
        Icon::Display => {
            let screen = egui::Rect::from_min_max(r.min, egui::pos2(r.right(), r.top() + r.height() * 0.68));
            p.rect_stroke(screen, CornerRadius::same(2), s, StrokeKind::Inside);
            let base_y = r.bottom() - 0.5;
            p.line_segment([egui::pos2(r.center().x, screen.bottom()), egui::pos2(r.center().x, base_y)], s);
            p.line_segment([egui::pos2(r.center().x - w * 0.22, base_y), egui::pos2(r.center().x + w * 0.22, base_y)], s);
        }
        Icon::Arrows => {
            let y1 = r.top() + r.height() * 0.32;
            let y2 = r.top() + r.height() * 0.68;
            let h = w * 0.22;
            p.line_segment([egui::pos2(r.left(), y1), egui::pos2(r.right(), y1)], s);
            p.line_segment([egui::pos2(r.right() - h, y1 - h), egui::pos2(r.right(), y1)], s);
            p.line_segment([egui::pos2(r.right() - h, y1 + h), egui::pos2(r.right(), y1)], s);
            p.line_segment([egui::pos2(r.right(), y2), egui::pos2(r.left(), y2)], s);
            p.line_segment([egui::pos2(r.left() + h, y2 - h), egui::pos2(r.left(), y2)], s);
            p.line_segment([egui::pos2(r.left() + h, y2 + h), egui::pos2(r.left(), y2)], s);
        }
        Icon::Link => {
            let a = egui::Rect::from_center_size(r.center() - egui::vec2(w * 0.17, -w * 0.17), egui::vec2(w * 0.62, w * 0.36));
            let b = egui::Rect::from_center_size(r.center() + egui::vec2(w * 0.17, -w * 0.17), egui::vec2(w * 0.62, w * 0.36));
            let rot = |rect: egui::Rect| {
                // Draw each link as a rounded capsule tilted 45°: approximated with a polyline.
                let c0 = rect.center();
                let (hw, hh) = (rect.width() / 2.0, rect.height() / 2.0);
                let pts: Vec<egui::Pos2> = (0..=24)
                    .map(|k| {
                        let t = k as f32 / 24.0 * std::f32::consts::TAU;
                        let (x, y) = (t.cos() * hw, t.sin() * hh);
                        let (cs, sn) = (std::f32::consts::FRAC_1_SQRT_2, -std::f32::consts::FRAC_1_SQRT_2);
                        c0 + egui::vec2(x * cs - y * sn, x * sn + y * cs)
                    })
                    .collect();
                p.add(egui::Shape::line(pts, s));
            };
            rot(a);
            rot(b);
        }
        Icon::Sliders => {
            for (k, at) in [(0.2, 0.3), (0.5, 0.7), (0.8, 0.45)] {
                let y = r.top() + r.height() * k;
                p.line_segment([egui::pos2(r.left(), y), egui::pos2(r.right(), y)], s);
                p.circle_filled(egui::pos2(r.left() + w * at, y), 2.6, c);
            }
        }
    }
}
