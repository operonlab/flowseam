use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Color32, CornerRadius, FontFamily, FontId, Layout, Margin, RichText, Sense, Stroke, StrokeKind};
use flowseam_core::config::{Config, Edge, KeyMap};
use flowseam_core::proto::Kind;
use flowseam_core::status::{self, LinkStatus};

use crate::model::{chord_symbols, edge_label, fingerprint_short, keys_label, role, PeerRow, UiState, EDGES, KEY_MAPS};
use crate::worker::{WorkerHandle, WorkerStatus};

// Apple system colors, dark-mode variants where they differ (systemGreen / Orange / Red / Purple / Gray).
pub(crate) const GREEN: Color32 = Color32::from_rgb(48, 209, 88);
pub(crate) const ORANGE: Color32 = Color32::from_rgb(255, 159, 10);
pub(crate) const RED: Color32 = Color32::from_rgb(255, 69, 58);
pub(crate) const PURPLE: Color32 = Color32::from_rgb(191, 90, 242);
const GRAY: Color32 = Color32::from_rgb(142, 142, 147);
pub(crate) const BLUE: Color32 = Color32::from_rgb(10, 132, 255);
pub(crate) const TEAL: Color32 = Color32::from_rgb(48, 176, 199);
pub(crate) const PINK: Color32 = Color32::from_rgb(255, 55, 95);

pub(crate) fn liveness(worker_running: bool, status: Option<&LinkStatus>, now_ms: u64, peer: &str) -> (String, Color32) {
    if !worker_running { return (crate::i18n::tr("共用未開").into(), GRAY); }
    let Some(status) = status.filter(|s| now_ms.saturating_sub(s.written_ms) <= 3000 && s.written_ms <= now_ms) else { return (crate::i18n::tr("狀態未知").into(), GRAY) };
    match status.peers.iter().find(|p| p.name == peer) {
        Some(p) if p.up => (match p.kind { Some(Kind::Wired) => crate::i18n::tr("連線中 · 有線"), Some(Kind::WiFi) => crate::i18n::tr("連線中 · Wi‑Fi"), _ => crate::i18n::tr("連線中") }.into(), GREEN),
        Some(p) => (p.reason.as_ref().map_or_else(|| crate::i18n::tr("未連線").into(), |r| crate::i18n::trf("未連線 · {}", &[&crate::i18n::tr_runtime(r)])), ORANGE),
        None => (crate::i18n::tr("未連線").into(), ORANGE),
    }
}

/// This computer's screen in the Overview arrangement, and a peer's tile beside it.
const ARR_MAIN: egui::Vec2 = egui::vec2(168.0, 104.0);
const ARR_TILE: egui::Vec2 = egui::vec2(116.0, 72.0);
const ARR_GAP: f32 = 22.0;

/// Room above and below this computer's screen. A peer above or below needs its tile and its status line inside
/// the area; with only left and right peers the area keeps its old 176 px.
fn arrangement_margins(top: bool, bottom: bool) -> (f32, f32) {
    (if top { 108.0 } else { 28.0 }, if bottom { 124.0 } else { 44.0 })
}

fn arrangement_main(area: egui::Rect, above: f32) -> egui::Rect {
    egui::Rect::from_min_size(egui::pos2(area.center().x - ARR_MAIN.x / 2.0, area.top() + above), ARR_MAIN)
}

fn peer_tile(main: egui::Rect, edge: Edge) -> egui::Rect {
    let c = match edge {
        Edge::Left => main.left_center() - egui::vec2(ARR_GAP + ARR_TILE.x / 2.0, 0.0),
        Edge::Right => main.right_center() + egui::vec2(ARR_GAP + ARR_TILE.x / 2.0, 0.0),
        Edge::Top => main.center_top() - egui::vec2(0.0, ARR_GAP + ARR_TILE.y / 2.0 - 6.0),
        Edge::Bottom => main.center_bottom() + egui::vec2(0.0, ARR_GAP + ARR_TILE.y / 2.0 + 10.0),
    };
    egui::Rect::from_center_size(c, ARR_TILE)
}

#[cfg(test)]
mod arrangement_tests {
    use super::*;

    /// A peer placed above this computer drew its tile over the card above the arrangement, on both platforms.
    #[test]
    fn every_tile_and_its_status_line_stay_inside_the_area() {
        for (top, bottom) in [(false, false), (true, false), (false, true), (true, true)] {
            let (above, below) = arrangement_margins(top, bottom);
            let area = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(600.0, above + ARR_MAIN.y + below));
            let main = arrangement_main(area, above);
            let mut edges = vec![Edge::Left, Edge::Right];
            if top {
                edges.push(Edge::Top);
            }
            if bottom {
                edges.push(Edge::Bottom);
            }
            for edge in edges {
                let tile = peer_tile(main, edge);
                // The status line is 11 pt text centred 10 px above a top tile and 12 px below the others.
                let (lo, hi) = if edge == Edge::Top { (tile.top() - 10.0 - 7.0, tile.bottom()) } else { (tile.top(), tile.bottom() + 12.0 + 7.0) };
                let span = egui::Rect::from_min_max(egui::pos2(tile.left(), lo), egui::pos2(tile.right(), hi));
                assert!(area.contains_rect(span), "{edge:?} (top {top}, bottom {bottom}): {span:?} is outside {area:?}");
            }
        }
        let (above, below) = arrangement_margins(false, false);
        assert_eq!(above + ARR_MAIN.y + below, 176.0, "with only left and right peers the Overview looks as before");
    }
}

#[cfg(test)]
mod liveness_tests {
    use super::*;
    use flowseam_core::status::PeerStatus;

    fn status(up: bool, kind: Option<Kind>, reason: Option<&str>) -> LinkStatus {
        LinkStatus { written_ms: 10_000, peers: vec![PeerStatus { name: "Mac".into(), up, kind, remote: None, since_ms: 9_000, reason: reason.map(str::to_string) }] }
    }

    #[test]
    fn connected_kinds() {
        for (kind, text) in [(Some(Kind::Wired), crate::i18n::tr("連線中 · 有線")), (Some(Kind::WiFi), crate::i18n::tr("連線中 · Wi‑Fi")), (None, crate::i18n::tr("連線中")), (Some(Kind::Other), crate::i18n::tr("連線中"))] {
            assert_eq!(liveness(true, Some(&status(true, kind, None)), 10_000, "Mac"), (text.into(), GREEN));
        }
    }

    #[test]
    fn down_and_missing_peer() {
        assert_eq!(liveness(true, Some(&status(false, None, Some("連線中斷"))), 10_000, "Mac"), (crate::i18n::trf("未連線 · {}", &[&"連線中斷"]), ORANGE));
        assert_eq!(liveness(true, Some(&status(false, None, None)), 10_000, "Mac"), (crate::i18n::tr("未連線").into(), ORANGE));
        assert_eq!(liveness(true, Some(&status(true, Some(Kind::Wired), None)), 10_000, "Other"), (crate::i18n::tr("未連線").into(), ORANGE));
    }

    #[test]
    fn unavailable_status() {
        let fresh = status(true, Some(Kind::Wired), None);
        assert_eq!(liveness(true, Some(&fresh), 13_000, "Mac"), (crate::i18n::tr("連線中 · 有線").into(), GREEN));
        assert_eq!(liveness(true, Some(&fresh), 13_001, "Mac"), (crate::i18n::tr("狀態未知").into(), GRAY));
        assert_eq!(liveness(true, None, 10_000, "Mac"), (crate::i18n::tr("狀態未知").into(), GRAY));
        assert_eq!(liveness(false, Some(&fresh), 10_000, "Mac"), (crate::i18n::tr("共用未開").into(), GRAY));
    }

    #[test]
    fn tile_label_drops_the_reason_only() {
        let down = crate::i18n::trf("未連線 · {}", &[&"對方沒有回應（程式可能卡住）"]);
        assert_eq!(tile_label(&down), crate::i18n::tr("未連線"));
        assert_eq!(tile_label(crate::i18n::tr("連線中 · Wi‑Fi")), crate::i18n::tr("連線中 · Wi‑Fi"));
        assert_eq!(tile_label(crate::i18n::tr("狀態未知")), crate::i18n::tr("狀態未知"));
    }
}

fn liveness_color(color: Color32, th: Theme) -> Color32 { if color == GRAY { th.text2 } else { color } }

/// A liveness label short enough for the arrangement tile: a link that is down drops its reason.
fn tile_label(text: &str) -> &str { if text.starts_with(crate::i18n::tr("未連線")) { crate::i18n::tr("未連線") } else { text } }

const SIDEBAR_W: f32 = 230.0;
/// Room for the traffic lights, which sit over the sidebar now that the title bar is transparent.
const TITLEBAR_H: f32 = 52.0;

/// Colors for one appearance, after macOS 27 System Settings: a floating sidebar, grouped rounded cards
/// on a quiet window background, hairline separators.
#[derive(Clone, Copy)]
pub(crate) struct Theme {
    dark: bool,
    window: Color32,
    sidebar: Color32,
    pub(crate) card: Color32,
    pub(crate) line: Color32,
    pub(crate) text: Color32,
    pub(crate) text2: Color32,
    pub(crate) accent: Color32,
    pub(crate) field: Color32,
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

pub fn run(hidden: bool) -> i32 {
    let viewport = egui::ViewportBuilder::default()
        .with_inner_size([860.0, 620.0])
        .with_min_inner_size([760.0, 520.0])
        .with_title("Flowseam")
        .with_visible(!hidden);
    // macOS: keep the bundle's icon, which the system rounds to its own shape (an empty icon tells eframe to
    // leave it alone instead of drawing its default). Windows has no bundle icon, so the window carries ours.
    #[cfg(target_os = "macos")]
    let viewport = viewport.with_icon(std::sync::Arc::new(egui::IconData::default()));
    #[cfg(not(target_os = "macos"))]
    let viewport = viewport.with_icon(std::sync::Arc::new(
        eframe::icon_data::from_png_bytes(include_bytes!("../../../assets/icon/icon-512.png")).expect("the bundled icon is a PNG"),
    ));
    #[cfg(target_os = "macos")]
    let viewport = viewport
            .with_fullsize_content_view(true)
            .with_titlebar_shown(false)
            .with_title_shown(false);
    #[allow(unused_mut)]
    let mut opts = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    #[cfg(windows)]
    {
        opts.renderer = eframe::Renderer::Wgpu;
        if let eframe::egui_wgpu::WgpuSetup::CreateNew(setup) = &mut opts.wgpu_options.wgpu_setup {
            setup.power_preference = eframe::wgpu::PowerPreference::LowPower;
            setup.instance_descriptor.backends = eframe::wgpu::Backends::DX12;
            // The integrated GPU when there is one, so the window never holds a context on a discrete card (a
            // flowseam.exe stuck in an RTX 3090's driver could not be terminated); any GPU that can draw it otherwise.
            setup.native_adapter_selector = Some(Arc::new(|adapters, surface| {
                let fits = |adapter: &&eframe::wgpu::Adapter| surface.is_none_or(|s| adapter.is_surface_supported(s));
                adapters.iter().filter(fits).find(|adapter| adapter.get_info().device_type == eframe::wgpu::DeviceType::IntegratedGpu)
                    .or_else(|| adapters.iter().find(fits))
                    .cloned().ok_or_else(|| "no DX12 adapter can draw this window".to_string())
            }));
        }
    }
    #[cfg(not(windows))]
    let opts = opts;
    let started = eframe::run_native(
        "Flowseam",
        opts,
        Box::new(|cc| {
            setup(&cc.egui_ctx);
            Ok(Box::new(App::new(crate::menubar::MenuBar::new(hidden))))
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
fn font_candidates() -> Vec<(&'static str, Option<std::path::PathBuf>, u32, FontFamily)> {
    #[cfg(target_os = "macos")]
    {
        let pingfang = find_pingfang();
        vec![
            (
                "sf",
                Some("/System/Library/Fonts/SFNS.ttf".into()),
                0,
                FontFamily::Proportional,
            ),
            ("pingfang", pingfang.clone(), 2, FontFamily::Proportional),
            (
                "hn-bold",
                Some("/System/Library/Fonts/HelveticaNeue.ttc".into()),
                1,
                FontFamily::Name(BOLD.into()),
            ),
            ("pingfang-sb", pingfang, 10, FontFamily::Name(BOLD.into())),
        ]
    }
    #[cfg(windows)]
    {
        let dir = std::env::var_os("WINDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| r"C:\Windows".into())
            .join("Fonts");
        let bold = if dir.join("seguisb.ttf").is_file() {
            "seguisb.ttf"
        } else {
            "segoeuib.ttf"
        };
        vec![
            (
                "segoe",
                Some(dir.join("segoeui.ttf")),
                0,
                FontFamily::Proportional,
            ),
            (
                "msjh",
                Some(dir.join("msjh.ttc")),
                0,
                FontFamily::Proportional,
            ),
            (
                "segoe-bold",
                Some(dir.join(bold)),
                0,
                FontFamily::Name(BOLD.into()),
            ),
            (
                "msjh-bold",
                Some(dir.join("msjhbd.ttc")),
                0,
                FontFamily::Name(BOLD.into()),
            ),
        ]
    }
}

fn setup(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let faces = font_candidates();
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
#[cfg(target_os = "macos")]
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

pub(crate) fn bold(text: impl Into<String>, size: f32) -> RichText {
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
pub(crate) enum Icon {
    Grid,
    Display,
    Arrows,
    Link,
    Sliders,
    Download,
}

struct Pairing {
    peer: usize,
    code: String,
    /// The row and address a running pairing was started for; the result is applied only there.
    target: Option<(usize, String)>,
    rx: Option<Receiver<Result<flowseam_net::identity::Fingerprint, String>>>,
    server_rx: Option<Receiver<PairEvent>>,
    /// Raised to stop the pairing server this Mac runs (its accept blocks until someone connects).
    #[cfg(target_os = "macos")]
    cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
}

enum PairEvent {
    Code(String),
    #[cfg(target_os = "macos")]
    Paired(Result<flowseam_net::identity::Fingerprint, String>),
    #[cfg(windows)]
    Done(Result<(), String>),
}

struct App {
    about: crate::about::About,
    menubar: crate::menubar::MenuBar,
    quitting: bool,
    page: Page,
    autostart: Option<crate::autostart::State>,
    autostart_error: Option<String>,
    ui: UiState,
    /// The configuration as last loaded or saved, to tell whether there are unsaved changes.
    /// `None` when the file could not be read: nothing on disk matches the screen, so save stays enabled.
    saved: Option<Config>,
    notice: String,
    errors: Vec<String>,
    worker: Option<WorkerHandle>,
    stop_at: Option<Instant>,
    /// Settings were saved while sharing: the worker reads them only when it starts, so it is being stopped
    /// to start again with them.
    apply_on_stop: bool,
    status: WorkerStatus,
    restart_history: Vec<Instant>,
    restart_at: Option<Instant>,
    restart_delay: Duration,
    sharing_intended: bool,
    auto_restarted: bool,
    watchdog_killed: bool,
    link_status: Option<LinkStatus>,
    status_at: Instant,
    perms: Vec<String>,
    /// When `perms` was last read: grants change in System Settings while the window is open.
    perms_at: Instant,
    pairing: Pairing,
}

fn check_permissions() -> Vec<String> {
    #[cfg(target_os = "macos")]
    use flowseam_core::backend::Preflight;
    #[allow(unused_mut)]
    let mut out = Vec::new();
    #[cfg(target_os = "macos")]
    for (what, p) in [(crate::i18n::tr("擷取"), flowseam_mac::sys::capture_preflight()), (crate::i18n::tr("注入"), flowseam_mac::sys::emit_preflight())] {
        match p {
            Preflight::Ok => {}
            Preflight::Missing(m) => out.push(crate::i18n::trf("{}缺權限：{}", &[&what, &format!("{m:?}")])),
            Preflight::Unknown(why) => out.push(crate::i18n::trf("{}權限查不到：{}", &[&what, &why])),
        }
    }

    out
}

impl App {
    fn new(menubar: crate::menubar::MenuBar) -> Self {
        let (cfg, saved, notice) = match Config::load(&crate::config_path()) {
            Ok(c) => (c.clone(), Some(c), String::new()),
            Err(e) => (Config::default(), None, crate::i18n::trf("設定檔讀取失敗，改用預設（未覆寫檔案）：{}", &[&e])),
        };
        let mut app = App {
            about: crate::about::About::new(saved.as_ref().is_some_and(|c| !c.peers.is_empty())),
            menubar,
            quitting: false,
            page: start_page(),
            autostart: None,
            autostart_error: None,
            ui: UiState::from_config(&cfg),
            saved,
            notice,
            errors: Vec::new(),
            worker: None,
            stop_at: None,
            apply_on_stop: false,
            status: WorkerStatus::Stopped,
            restart_history: Vec::new(),
            restart_at: None,
            restart_delay: Duration::ZERO,
            sharing_intended: false,
            auto_restarted: false,
            watchdog_killed: false,
            link_status: None,
            status_at: Instant::now() - Duration::from_secs(1),
            perms: check_permissions(),
            perms_at: Instant::now(),
            pairing: Pairing {
                peer: 0,
                code: String::new(),
                target: None,
                rx: None,
                server_rx: None,
                #[cfg(target_os = "macos")]
                cancel: None,
            },
        };
        // Back to sharing if it was on, unless this is a preview window opened on one page (screenshots).
        if app.saved.is_some() && std::env::var_os("FLOWSEAM_PAGE").is_none() && crate::model::sharing_remembered(&crate::config_dir()) {
            app.start_worker();
        }
        app
    }

    fn dirty(&self) -> bool {
        self.saved.as_ref() != Some(&self.ui.to_config())
    }

    fn save(&mut self) -> bool {
        self.errors = self.ui.problems();
        if !self.errors.is_empty() {
            self.notice = crate::i18n::tr("尚未儲存，請先修正下方標示的問題").into();
            return false;
        }
        let cfg = self.ui.to_config();
        match cfg.save(&crate::config_path()) {
            Ok(()) => {
                self.saved = Some(cfg);
                self.notice = crate::i18n::tr("已儲存").into();
                true
            }
            Err(e) => {
                self.errors = vec![e.to_string()];
                self.notice = crate::i18n::tr("儲存失敗").into();
                false
            }
        }
    }

    fn tick(&mut self) {
        if self.status_at.elapsed() >= Duration::from_secs(1) {
            self.link_status = status::read(&status::status_path(&crate::config_path())).ok();
            self.status_at = Instant::now();
            if let Some(w) = &self.worker {
                if self.stop_at.is_none() && !self.watchdog_killed {
                    if let Some(age) = crate::worker::stalled(w.started, w.started_ms, Instant::now(), self.link_status.as_ref().map(|s| s.written_ms), status::now_ms()) {
                        crate::worker::log_event(&format!("worker stalled: status {age} s old"));
                        w.kill_stalled();
                        self.watchdog_killed = true;
                    }
                }
            }
        }
        if self.perms_at.elapsed() >= Duration::from_secs(2) {
            self.perms = check_permissions();
            self.perms_at = Instant::now();
        }
        if let Some(w) = &self.worker {
            if let Some(end) = w.poll() {
                let died = matches!(end, WorkerStatus::Died(_));
                // A quit Apple event can reach the worker rather than this process; quit as the menu's Quit does.
                if end == WorkerStatus::QuitRequested {
                    self.quitting = true;
                    self.apply_on_stop = false;
                    self.restart_at = None;
                }
                self.status = end;
                self.worker = None;
                self.stop_at = None;
                if std::mem::take(&mut self.apply_on_stop) {
                    self.start_worker();
                    if self.worker.is_some() {
                        self.notice = crate::i18n::tr("已套用新設定").into();
                    }
                } else if died && self.sharing_intended {
                    let delay = crate::worker::next_restart(&self.restart_history, Instant::now()).unwrap_or(Duration::from_secs(1));
                    self.restart_delay = delay;
                    self.restart_at = Some(Instant::now() + delay);
                    crate::worker::log_event(&format!("worker died; restarting in {} s", delay.as_secs()));
                }
            } else if let Some(t) = self.stop_at {
                w.escalate(t);
            }
        }
        if self.restart_at.is_some_and(|at| Instant::now() >= at) {
            self.restart_at = None;
            self.start_worker();
            if self.worker.is_some() {
                self.auto_restarted = true;
                self.notice = crate::i18n::tr("已自動重新啟動").into();
                crate::worker::log_event("worker automatically restarted");
            } else if self.sharing_intended {
                self.restart_delay = Duration::from_secs(30);
                self.restart_at = Some(Instant::now() + self.restart_delay);
            }
        }
        if let Some(rx) = &self.pairing.server_rx {
            if let Ok(event) = rx.try_recv() {
                match event {
                    PairEvent::Code(code) => {
                        self.pairing.code = code;
                        self.notice = crate::i18n::tr("等待另一台電腦輸入配對碼…").into();
                    }
                    #[cfg(target_os = "macos")]
                    PairEvent::Paired(result) => {
                        self.pairing.server_rx = None;
                        self.pairing.cancel = None;
                        self.pairing.code.clear();
                        let target = self.pairing.target.take();
                        self.notice = match (result, target) {
                            (Err(e), _) if e == crate::PAIR_CANCELLED => crate::i18n::tr("已取消配對").into(),
                            (Ok(fp), Some((idx, addr))) => match self.ui.apply_pairing(idx, &addr, &fp) {
                                Ok(true) => crate::i18n::tr("配對完成，請按「儲存」").into(),
                                Ok(false) => crate::i18n::tr("配對完成：指紋沒有變").into(),
                                Err(e) => e,
                            },
                            (Ok(_), None) => crate::i18n::tr("配對完成，但找不到它是為哪台裝置開始的，請重新配對").into(),
                            (Err(e), _) => crate::i18n::trf("配對失敗：{}", &[&e]),
                        };
                    }
                    #[cfg(windows)]
                    PairEvent::Done(result) => {
                        self.pairing.server_rx = None;
                        self.pairing.code.clear();
                        self.notice = match result {
                            Ok(()) => crate::i18n::tr("配對完成").into(),
                            Err(e) => crate::i18n::trf("配對失敗：{}", &[&e]),
                        };
                        if let Ok(cfg) = Config::load(&crate::config_path()) {
                            self.ui = UiState::from_config(&cfg);
                            self.saved = Some(cfg);
                        }
                    }
                }
            }
        }
        if let Some(rx) = &self.pairing.rx {
            if let Ok(res) = rx.try_recv() {
                self.pairing.rx = None;
                let target = self.pairing.target.take();
                self.notice = match (res, target) {
                    (Ok(fp), Some((idx, addr))) => match self.ui.apply_pairing(idx, &addr, &fp) {
                        Ok(true) => crate::i18n::tr("配對完成，請按「儲存」").into(),
                        Ok(false) => crate::i18n::tr("配對完成：指紋沒有變").into(),
                        Err(e) => e,
                    },
                    (Ok(_), None) => crate::i18n::tr("配對完成，但找不到它是為哪台裝置開始的，請重新配對").into(),
                    (Err(e), _) => crate::i18n::trf("配對失敗：{}", &[&e]),
                };
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn start_pairing(&mut self) {
        if self.pairing_running() {
            return;
        }
        let Some(row) = self.ui.peers.get(self.pairing.peer) else {
            self.notice = crate::i18n::tr("請先到「裝置與邊界」新增一台裝置").into();
            return;
        };
        let code = self.pairing.code.trim().to_string();
        if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
            self.notice = crate::i18n::tr("配對碼是 6 位數字").into();
            return;
        }
        let addr = row.addr.trim().to_string();
        let (tx, rx) = mpsc::channel();
        self.pairing.target = Some((self.pairing.peer, addr.clone()));
        self.pairing.rx = Some(rx);
        self.notice = crate::i18n::tr("配對中…").into();
        std::thread::spawn(move || {
            let res = crate::pair_as_client(&addr, &code);
            let _ = tx.send(res);
        });
    }

    #[cfg(target_os = "macos")]
    fn start_pair_server(&mut self) {
        if self.worker.is_some() || self.pairing_running() {
            return;
        }
        let Some(row) = self.ui.peers.get(self.pairing.peer) else {
            self.notice = crate::i18n::tr("請先到「裝置與邊界」新增一台裝置").into();
            return;
        };
        self.pairing.target = Some((self.pairing.peer, row.addr.trim().to_string()));
        let (tx, rx) = mpsc::channel();
        self.pairing.server_rx = Some(rx);
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.pairing.cancel = Some(cancel.clone());
        self.pairing.code.clear();
        self.notice = crate::i18n::tr("正在準備配對…").into();
        std::thread::spawn(move || {
            let code_tx = tx.clone();
            let result = crate::pair_serve(&cancel, move |code| {
                let _ = code_tx.send(PairEvent::Code(code.into()));
            });
            let _ = tx.send(PairEvent::Paired(result));
        });
    }

    #[cfg(windows)]
    fn start_pair_server(&mut self) {
        if self.worker.is_some() || self.pairing.server_rx.is_some() {
            return;
        }
        if !self.save() {
            return;
        }
        let Some(row) = self.ui.peers.get(self.pairing.peer) else {
            self.notice = crate::i18n::tr("請先到「裝置與邊界」新增要配對的 Mac").into();
            return;
        };
        let (peer, addr, config) = (row.name.clone(), row.addr.clone(), crate::config_path());
        let (tx, rx) = mpsc::channel();
        self.pairing.server_rx = Some(rx);
        self.pairing.code.clear();
        self.notice = crate::i18n::tr("正在準備配對…").into();
        std::thread::spawn(move || {
            let code_tx = tx.clone();
            let result = flowseam_win::app::pair_serve_with(
                &config,
                &peer,
                &addr,
                move |code| {
                    let _ = code_tx.send(PairEvent::Code(code.into()));
                },
                |_| {},
            );
            let _ = tx.send(PairEvent::Done(result));
        });
    }

    /// A pairing is under way on either side.
    fn pairing_running(&self) -> bool {
        self.pairing.rx.is_some() || self.pairing.server_rx.is_some()
    }

    fn start_worker(&mut self) {
        // worker 讀的是磁碟上的檔案，所以執行中的設定一定等於已儲存的設定。
        if self.pairing_running() {
            self.notice = crate::i18n::tr("請先完成配對").into();
            return;
        }
        if !self.save() {
            return;
        }
        // Without the grants the worker can neither see nor send input; it would run blind.
        self.perms = check_permissions();
        self.perms_at = Instant::now();
        if !self.perms.is_empty() {
            self.errors = self.perms.clone();
            self.notice = crate::i18n::tr("無法啟動：缺少權限，請到「系統設定 › 隱私權與安全性」打開 Flowseam").into();
            return;
        }
        let unpaired = self.ui.to_config().unpaired().join("、");
        if !unpaired.is_empty() {
            self.errors = vec![crate::i18n::trf("尚未配對：{}（先到配對頁完成，或移除該裝置）", &[&unpaired])];
            self.notice = crate::i18n::tr("無法啟動：有裝置尚未配對").into();
            return;
        }
        // Another window, or a copy started at login, may be sharing already; its worker holds the lock.
        match crate::take_worker_lock(&crate::worker_lock_path()) {
            Ok(Some(free)) => drop(free), // the worker takes it for real
            Ok(None) => {
                self.notice = crate::i18n::tr("這台已經有另一個 Flowseam 在分享（可能是另一個視窗）").into();
                return;
            }
            Err(e) => {
                self.status = WorkerStatus::Died(crate::i18n::trf("無法啟動：{}", &[&e]));
                return;
            }
        }
        match WorkerHandle::spawn(&crate::config_path()) {
            Ok(w) => {
                if self.restart_history.last().is_some_and(|last| Instant::now().duration_since(*last) >= Duration::from_secs(60)) { self.restart_history.clear(); }
                self.restart_history.push(w.started);
                self.worker = Some(w);
                self.status = WorkerStatus::Running;
                self.sharing_intended = true;
                self.watchdog_killed = false;
            }
            Err(e) => self.status = WorkerStatus::Died(crate::i18n::trf("無法啟動：{}", &[&e])),
        }
    }

    fn stop_worker(&mut self) {
        if !self.apply_on_stop {
            self.sharing_intended = false;
            self.restart_at = None;
            self.restart_history.clear();
            self.auto_restarted = false;
            if self.worker.is_none() { self.status = WorkerStatus::Stopped; }
        }
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
        ui.add_space(if cfg!(target_os = "macos") {
            TITLEBAR_H - 8.0
        } else {
            12.0
        });
        ui.label(bold("Flowseam", 15.0));
        ui.label(RichText::new(crate::i18n::tr("鍵盤與滑鼠共用")).size(11.5).color(th.text2));
        ui.add_space(12.0);
        for (page, label, tint, icon) in PAGES {
            if sidebar_item(ui, th, self.page == page, crate::i18n::tr(label), tint, icon).clicked() {
                self.page = page;
                if page == Page::Advanced { self.autostart = None; self.autostart_error = None; }
            }
        }
        // The sharing switch stays in reach on every page, at the foot of the sidebar.
        let (dot, state, _) = self.state_text();
        let foot = egui::Rect::from_min_max(egui::pos2(inner.left(), inner.bottom() - 62.0), egui::pos2(inner.right(), inner.bottom() - 24.0));
        let mut foot_ui = ui.new_child(egui::UiBuilder::new().max_rect(foot).layout(Layout::left_to_right(Align::Center)));
        let (d, _) = foot_ui.allocate_exact_size(egui::vec2(10.0, 10.0), Sense::hover());
        foot_ui.painter().circle_filled(d.center(), 4.0, dot);
        foot_ui.label(RichText::new(state).size(12.5));
        foot_ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            self.sharing_switch(ui, th)
        });
        ui.painter().text(egui::pos2(inner.left() + 1.0, inner.bottom() - 12.0), egui::Align2::LEFT_CENTER, crate::i18n::trf("版本 {}", &[&crate::about::version()]), FontId::proportional(10.5), th.text2);
    }

    /// Status dot color, one word, and a sentence for where sharing stands.
    fn state_text(&self) -> (Color32, &'static str, String) {
        if let Some(at) = self.restart_at {
            let secs = at.saturating_duration_since(Instant::now()).as_secs().max(1);
            return (ORANGE, "已中斷", crate::i18n::trf("worker 已停止，{} 秒後自動重新啟動", &[&secs.to_string()]));
        }
        match &self.status {
            _ if self.stop_at.is_some() => {
                (ORANGE, crate::i18n::tr("停止中…"), crate::i18n::tr("正在把控制權還給這台電腦。").to_string())
            }
            _ if self.worker.is_some() => (
                GREEN,
                crate::i18n::tr("共用中"),
                if self.auto_restarted {
                    crate::i18n::tr("已自動重新啟動").into()
                } else if role().shows_chord() {
                    crate::i18n::tr("滑鼠移到邊界停留，或按快捷鍵，即可切換到另一台。").to_string()
                } else {
                    crate::i18n::tr("Mac 的游標移過來時，就由 Mac 操作這台電腦。").to_string()
                },
            ),
            WorkerStatus::Died(why) => (RED, crate::i18n::tr("已中斷"), why.clone()),
            _ => (GRAY, crate::i18n::tr("未共用"), crate::i18n::tr("打開開關，這台電腦就會參與共用。").to_string()),
        }
    }

    fn sharing_switch(&mut self, ui: &mut egui::Ui, th: Theme) {
        let stopping = self.stop_at.is_some();
        let pairing = self.pairing_running();
        let on = (self.worker.is_some() || self.restart_at.is_some()) && !stopping;
        if ui
            .add_enabled_ui(!stopping && !pairing, |ui| toggle(ui, th, on))
            .inner
            .clicked()
        {
            if on {
                self.stop_worker();
                if let Err(e) = crate::model::remember_sharing(&crate::config_dir(), false) {
                    self.notice = crate::i18n::trf("無法記下共用狀態：{}", &[&e]);
                }
            } else {
                self.start_worker();
                if self.worker.is_some() {
                    if let Err(e) = crate::model::remember_sharing(&crate::config_dir(), true) {
                        self.notice = crate::i18n::trf("無法記下共用狀態：{}", &[&e]);
                    }
                }
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
                    ui.label(bold(crate::i18n::tr("共用鍵盤與滑鼠"), 14.0));
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 5.0;
                        let (d, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), Sense::hover());
                        ui.painter().circle_filled(d.center(), 4.0, dot);
                        ui.label(RichText::new(state).size(12.0).color(th.text2));
                        ui.label(RichText::new("·").size(12.0).color(th.text2));
                        ui.label(RichText::new(detail).size(12.0).color(th.text2));
                    });
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    self.sharing_switch(ui, th)
                });
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
                    ui.label(bold(crate::i18n::tr("需要系統權限"), 13.0).color(ORANGE));
                    for p in &self.perms {
                        ui.label(RichText::new(p).size(12.0));
                    }
                    ui.label(RichText::new(crate::i18n::tr("到「系統設定 › 隱私權與安全性」允許 Flowseam，完成後按「重新檢查」。")).size(12.0).color(th.text2));
                    ui.horizontal(|ui| {
                        if secondary(ui, th, crate::i18n::tr("打開系統設定")).clicked() {
                            let _ = std::process::Command::new("open")
                                .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
                                .spawn();
                        }
                        if secondary(ui, th, crate::i18n::tr("重新檢查")).clicked() {
                            self.perms = check_permissions();
                        }
                    });
                });
        }
    }

    /// Like System Settings › Displays › Arrange: this Mac in the middle, each peer on its side.
    fn arrangement(&self, ui: &mut egui::Ui, th: Theme) {
        let has = |edge| self.ui.peers.iter().any(|p| p.edge == edge);
        let (above, below) = arrangement_margins(has(Edge::Top), has(Edge::Bottom));
        let (area, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), above + ARR_MAIN.y + below), Sense::hover());
        let p = ui.painter();
        let main = arrangement_main(area, above);
        screen(p, main, th, true);
        p.text(
            main.center_bottom() + egui::vec2(0.0, 14.0),
            egui::Align2::CENTER_CENTER,
            if cfg!(windows) { crate::i18n::tr("這台 Windows") } else { crate::i18n::tr("這台 Mac") },
            FontId::new(12.0, FontFamily::Name(BOLD.into())),
            th.text,
        );
        for peer in &self.ui.peers {
            let r = peer_tile(main, peer.edge);
            screen(p, r, th, false);
            let name = if peer.name.trim().is_empty() { crate::i18n::tr("（未命名）") } else { peer.name.trim() };
            let paired = !peer.fingerprint.is_empty();
            p.text(r.center(), egui::Align2::CENTER_CENTER, name, FontId::new(12.0, FontFamily::Name(BOLD.into())), Color32::WHITE);
            let label_at = if peer.edge == Edge::Top { r.center_top() - egui::vec2(0.0, 10.0) } else { r.center_bottom() + egui::vec2(0.0, 12.0) };
            // Under the tile there is room for a few words only: the reason a link is down is in the peer list.
            let (text, color) = if paired {
                let (text, color) = liveness(self.worker.is_some(), self.link_status.as_ref(), status::now_ms(), &peer.name);
                (tile_label(&text).to_string(), liveness_color(color, th))
            } else {
                (crate::i18n::tr("未配對").to_string(), ORANGE)
            };
            let galley = p.layout_no_wrap(text, FontId::proportional(11.0), th.text2);
            let left = label_at.x - (galley.size().x + 11.0) / 2.0;
            p.circle_filled(egui::pos2(left + 3.5, label_at.y), 3.5, color);
            p.galley(egui::pos2(left + 11.0, label_at.y - galley.size().y / 2.0), galley, th.text2);
        }
    }

    fn page_overview(&mut self, ui: &mut egui::Ui, th: Theme) {
        card(ui, th, |ui| {
            egui::Frame::new().inner_margin(Margin::symmetric(14, 6)).show(ui, |ui| self.arrangement(ui, th));
        });
        section(ui, th, crate::i18n::tr("裝置"));
        card(ui, th, |ui| {
            if self.ui.peers.is_empty() {
                row(ui, th, true, crate::i18n::tr("尚未加入任何裝置"), Some(crate::i18n::tr("到「裝置與邊界」新增。")), |_| {});
            }
            for (i, p) in self.ui.peers.iter().enumerate() {
                let name = if p.name.trim().is_empty() { crate::i18n::tr("（未命名）") } else { p.name.trim() };
                let sub = if role().shows_key_mapping() {
                    crate::i18n::trf("在{}側 · {}", &[&edge_label(p.edge), &keys_short(p.keys)])
                } else {
                    crate::i18n::trf("在{}側", &[&edge_label(p.edge)])
                };
                row(ui, th, i == 0, name, Some(&sub), |ui| {
                    if p.fingerprint.is_empty() {
                        badge(ui, crate::i18n::tr("未配對"), ORANGE);
                    } else {
                        badge(ui, crate::i18n::tr("已配對"), GREEN);
                        let (text, color) = liveness(self.worker.is_some(), self.link_status.as_ref(), status::now_ms(), &p.name);
                        badge(ui, &text, liveness_color(color, th));
                    }
                });
            }
        });
        section(ui, th, crate::i18n::tr("切換方式"));
        card(ui, th, |ui| {
            row(
                ui,
                th,
                true,
                crate::i18n::tr("停留在邊界"),
                Some(crate::i18n::tr("游標停住這麼久就切過去")),
                |ui| {
                    ui.label(RichText::new(crate::i18n::trf("{} 毫秒", &[&self.ui.dwell_ms])).color(th.text2));
                },
            );
            if role().shows_chord() {
                row(
                    ui,
                    th,
                    false,
                    crate::i18n::tr("快捷鍵"),
                    Some(crate::i18n::tr("隨時切換到下一台")),
                    |ui| {
                        keycap(ui, th, &chord_symbols(&self.ui.chord));
                    },
                );
            }
        });
    }

    fn page_edges(&mut self, ui: &mut egui::Ui, th: Theme) {
        let mut remove = None;
        let pairing_busy = self.pairing_running();
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
                            .hint_text(crate::i18n::tr("裝置名稱"))
                            .font(FontId::new(14.0, FontFamily::Name(BOLD.into())))
                            .frame(egui::Frame::NONE)
                            .desired_width(220.0),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if p.fingerprint.is_empty() {
                            badge(ui, crate::i18n::tr("未配對"), ORANGE);
                        } else {
                            badge(ui, &crate::i18n::trf("已配對 · {}", &[&fingerprint_short(&p.fingerprint)]), GREEN);
                            let (text, color) = liveness(self.worker.is_some(), self.link_status.as_ref(), status::now_ms(), &p.name);
                            badge(ui, &text, liveness_color(color, th));
                        }
                    });
                });
                row(
                    ui,
                    th,
                    false,
                    crate::i18n::tr("位址"),
                    Some(crate::i18n::tr("另一台的 IP 與連接埠")),
                    |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut p.addr)
                                .hint_text("192.168.1.20:24800")
                                .desired_width(210.0),
                        );
                    },
                );
                row(
                    ui,
                    th,
                    false,
                    crate::i18n::tr("位置"),
                    Some(crate::i18n::tr("它在這台螢幕的哪一側")),
                    |ui| {
                        segmented(
                            ui,
                            th,
                            ("edge", i),
                            &mut p.edge,
                            &EDGES.map(|e| (e, edge_label(e))),
                        );
                    },
                );
                if role().shows_key_mapping() {
                    row(
                        ui,
                        th,
                        false,
                        crate::i18n::tr("按鍵對應"),
                        Some(crate::i18n::tr("Mac 鍵盤的按鍵到對方怎麼解讀")),
                        |ui| {
                            egui::ComboBox::from_id_salt(("keys", i))
                                .selected_text(keys_label(p.keys))
                                .width(250.0)
                                .show_ui(ui, |ui| {
                                    for k in KEY_MAPS {
                                        ui.selectable_value(&mut p.keys, k, keys_label(k));
                                    }
                                });
                        },
                    );
                }
                row(ui, th, false, "", None, |ui| {
                    let hint = if pairing_busy { crate::i18n::tr("配對中，完成後才能移除") } else { "" };
                    if ui.add_enabled(!pairing_busy, egui::Button::new(RichText::new(crate::i18n::tr("移除這台裝置")).color(RED)).frame(false)).on_disabled_hover_text(hint).clicked() {
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
            if secondary(ui, th, crate::i18n::tr("＋ 新增裝置")).clicked() {
                let free = EDGES.into_iter().find(|e| self.ui.peers.iter().all(|p| p.edge != *e)).unwrap_or(Edge::Right);
                self.ui.peers.push(PeerRow { name: String::new(), addr: String::new(), edge: free, fingerprint: String::new(), keys: KeyMap::default() });
            }
        });
    }

    fn page_switch(&mut self, ui: &mut egui::Ui, th: Theme) {
        section(ui, th, crate::i18n::tr("停留切換"));
        card(ui, th, |ui| {
            row(ui, th, true, crate::i18n::tr("停留時間"), Some(crate::i18n::tr("游標停在邊界多久才切過去")), |ui| {
                ui.add(egui::Slider::new(&mut self.ui.dwell_ms, 50..=5000).suffix(crate::i18n::tr(" 毫秒")).logarithmic(true));
            });
            row(ui, th, false, crate::i18n::tr("重新計時距離"), Some(crate::i18n::tr("離開邊界多遠才重新計時")), |ui| {
                ui.add(egui::Slider::new(&mut self.ui.arm_dist, 1..=64).suffix(" px"));
            });
            row(ui, th, false, crate::i18n::tr("邊界寬度"), Some(crate::i18n::tr("貼邊多少像素內算在邊界上")), |ui| {
                ui.add(egui::Slider::new(&mut self.ui.boundary_px, 1..=32).suffix(" px"));
            });
        });
        if role().shows_chord() {
            section(ui, th, crate::i18n::tr("快捷鍵切換"));
            card(ui, th, |ui| {
                row(
                    ui,
                    th,
                    true,
                    crate::i18n::tr("組合鍵"),
                    Some(crate::i18n::tr("格式如 ctrl+alt+cmd+s")),
                    |ui| {
                        keycap(ui, th, &chord_symbols(&self.ui.chord));
                        ui.add(egui::TextEdit::singleline(&mut self.ui.chord).desired_width(150.0));
                    },
                );
                if let Err(e) = crate::model::check_chord(&self.ui.chord) {
                    row(ui, th, false, "", None, |ui| {
                        ui.label(RichText::new(e).size(12.0).color(RED));
                    });
                }
            });
        }
        ui.add_space(14.0);
        if secondary(ui, th, crate::i18n::tr("恢復預設")).clicked() {
            let d = flowseam_core::config::Switch::default();
            self.ui.dwell_ms = d.dwell_ms;
            self.ui.arm_dist = d.arm_dist;
            self.ui.boundary_px = d.boundary_px;
            self.ui.chord = d.chord;
        }
    }

    fn page_pairing(&mut self, ui: &mut egui::Ui, th: Theme) {
        card(ui, th, |ui| {
            let steps: &[&str] = if role().enters_pair_code() {
                &[
                    crate::i18n::tr("在另一台電腦開啟配對：Windows 會顯示 6 位數字；兩台 Mac 配對時，請其中一台按「顯示配對碼」。"),
                    crate::i18n::tr("選擇對應的裝置，輸入數字後按「配對」。"),
                    crate::i18n::tr("配對一次即可，之後自動信任。"),
                ]
            } else {
                &[
                    crate::i18n::tr("選擇要配對的 Mac。"),
                    crate::i18n::tr("顯示配對碼，並在 Mac 上輸入。"),
                    crate::i18n::tr("配對一次即可，之後自動信任。"),
                ]
            };
            for (i, step) in steps.iter().enumerate() {
                row_frame_sep(ui, th, i > 0, |ui| {
                    let (r, _) = ui.allocate_exact_size(egui::vec2(22.0, 22.0), Sense::hover());
                    ui.painter().circle_filled(r.center(), 11.0, th.accent);
                    ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, (i + 1).to_string(), FontId::new(12.0, FontFamily::Name(BOLD.into())), Color32::WHITE);
                    // A row is horizontal, where a label grows instead of wrapping and widens the whole page.
                    ui.add(egui::Label::new(*step).wrap());
                });
            }
        });
        section(ui, th, crate::i18n::tr("配對"));
        if self.ui.peers.is_empty() {
            card(ui, th, |ui| {
                row(ui, th, true, crate::i18n::tr("還沒有可配對的裝置"), Some(crate::i18n::tr("請先到「裝置與邊界」新增裝置。")), |_| {});
            });
            return;
        }
        let names = self.ui.pairing_labels();
        let busy = self.pairing_running();
        let mut go = false;
        let (mut show_code, mut cancel_code) = (false, false);
        card(ui, th, |ui| {
            row(ui, th, true, crate::i18n::tr("裝置"), None, |ui| {
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
            if role().enters_pair_code() {
                if self.pairing.server_rx.is_some() && !self.pairing.code.is_empty() {
                    row(ui, th, false, crate::i18n::tr("配對碼"), None, |ui| {
                        ui.label(RichText::new(&self.pairing.code).monospace().size(36.0));
                    });
                } else {
                    row(ui, th, false, crate::i18n::tr("配對碼"), Some(crate::i18n::tr("另一台畫面上的 6 位數字")), |ui| {
                        ui.add_enabled_ui(!busy, |ui| {
                            ui.add(egui::TextEdit::singleline(&mut self.pairing.code)
                                .hint_text("000000")
                                .font(FontId::monospace(16.0))
                                .char_limit(6)
                                .align(egui::Align2::CENTER_CENTER)
                                .desired_width(110.0));
                        });
                    });
                }
                row(ui, th, false, "", None, |ui| {
                    if ui
                        .add_enabled_ui(!busy, |ui| {
                            primary(ui, th, if busy { crate::i18n::tr("配對中…") } else { crate::i18n::tr("配對") })
                        })
                        .inner
                        .clicked()
                    {
                        go = true;
                    }
                });
                row(ui, th, false, "", None, |ui| {
                    if self.pairing.server_rx.is_some() {
                        if secondary(ui, th, crate::i18n::tr("取消")).clicked() {
                            cancel_code = true;
                        }
                    } else {
                        let disabled = self.worker.is_some() || busy;
                        if ui.add_enabled_ui(!disabled, |ui| secondary(ui, th, crate::i18n::tr("顯示配對碼"))).inner.clicked() {
                            show_code = true;
                        }
                    }
                });
                if self.worker.is_some() {
                    row(ui, th, false, "", None, |ui| {
                        ui.add(egui::Label::new(crate::i18n::tr("請先關閉共用（配對會用到同一個連接埠）")).wrap());
                    });
                }
            } else {
                #[cfg(windows)]
                {
                    if !self.pairing.code.is_empty() {
                        row(ui, th, false, crate::i18n::tr("配對碼"), None, |ui| {
                            ui.label(RichText::new(&self.pairing.code).monospace().size(36.0));
                        });
                    }
                    row(ui, th, false, "", None, |ui| {
                        let disabled = self.worker.is_some() || self.pairing.server_rx.is_some();
                        if ui
                            .add_enabled_ui(!disabled, |ui| primary(ui, th, crate::i18n::tr("顯示配對碼")))
                            .inner
                            .clicked()
                        {
                            go = true;
                        }
                    });
                    if self.worker.is_some() {
                        row(ui, th, false, "", None, |ui| {
                            ui.add(egui::Label::new(crate::i18n::tr("請先關閉共用（配對會用到同一個連接埠）")).wrap());
                        });
                    }
                }
            }
        });
        if go {
            #[cfg(target_os = "macos")]
            self.start_pairing();
            #[cfg(windows)]
            self.start_pair_server();
        }
        #[cfg(target_os = "macos")]
        {
            if show_code {
                self.start_pair_server();
            }
            if cancel_code {
                if let Some(c) = &self.pairing.cancel {
                    let c = c.clone();
                    // Waking the accept connects to this machine; off the UI thread, it never stalls a frame.
                    std::thread::spawn(move || crate::pair_serve_cancel(&c));
                }
            }
        }
        // The Mac branch is the only one that draws these buttons.
        #[cfg(not(target_os = "macos"))]
        let _ = (show_code, cancel_code);
    }

    fn page_advanced(&mut self, ui: &mut egui::Ui, th: Theme) {
        let state = self.autostart.get_or_insert_with(crate::autostart::state);
        let on = matches!(state, crate::autostart::State::On | crate::autostart::State::NeedsApproval);
        let unknown = matches!(state, crate::autostart::State::Unknown(_));
        let reason = match state {
            crate::autostart::State::NeedsApproval => Some(crate::i18n::tr("需要在『系統設定 › 一般 › 登入項目』允許").to_string()),
            crate::autostart::State::Unknown(why) => Some(crate::i18n::trf("無法確認登入項目：{}", &[why])),
            _ => None,
        };
        card(ui, th, |ui| {
            row(ui, th, true, crate::i18n::tr("語言"), None, |ui| {
                let selected = crate::i18n::selected();
                native_controls(ui, th, |ui| {
                    egui::ComboBox::from_id_salt("language").width(150.0).selected_text(RichText::new(selected.map_or(crate::i18n::tr("跟隨系統"), crate::i18n::Lang::name)).size(13.0)).show_ui(ui, |ui| {
                        if ui.selectable_label(selected.is_none(), RichText::new(crate::i18n::tr("跟隨系統")).size(13.0)).clicked() { self.change_language(None); }
                        for lang in crate::i18n::Lang::ALL {
                            if ui.selectable_label(selected == Some(lang), RichText::new(lang.name()).size(13.0)).clicked() { self.change_language(Some(lang)); }
                        }
                    });
                });
            });
            let mut clicked = false;
            row(ui, th, false, crate::i18n::tr("登入時自動開啟"), reason.as_deref(), |ui| {
                clicked = ui.add_enabled_ui(!unknown, |ui| toggle(ui, th, on)).inner.clicked();
            });
            if clicked {
                self.autostart_error = crate::autostart::set(!on).err();
                self.autostart = Some(crate::autostart::state());
            }
            if let Some(error) = &self.autostart_error {
                row_frame(ui, |ui| { ui.label(RichText::new(error).size(11.5).color(RED)); });
            }
            #[cfg(target_os = "macos")]
            if matches!(self.autostart, Some(crate::autostart::State::NeedsApproval)) {
                let mut open = false;
                row_frame(ui, |ui| { open = secondary(ui, th, crate::i18n::tr("開啟登入項目設定")).clicked(); });
                if open { self.autostart_error = crate::autostart::open_settings().err(); }
            }
            row(ui, th, false, crate::i18n::tr("接收位址"), Some(crate::i18n::tr("這台電腦監聽的位址，大多數情況保持預設")), |ui| {
                ui.add(egui::TextEdit::singleline(&mut self.ui.listen).desired_width(180.0));
            });
            let path = crate::config_path();
            row(
                ui,
                th,
                false,
                crate::i18n::tr("設定檔"),
                Some(&path.display().to_string()),
                |ui| {
                    if secondary(
                        ui,
                        th,
                        if cfg!(windows) {
                            crate::i18n::tr("在檔案總管中顯示")
                        } else {
                            crate::i18n::tr("在 Finder 中顯示")
                        },
                    )
                    .clicked()
                    {
                        #[cfg(target_os = "macos")]
                        let _ = std::process::Command::new("open")
                            .arg("-R")
                            .arg(&path)
                            .spawn();
                        #[cfg(windows)]
                        let _ = std::process::Command::new("explorer")
                            .arg(format!("/select,{}", path.display()))
                            .spawn();
                    }
                },
            );
        });
    }

    fn change_language(&mut self, lang: Option<crate::i18n::Lang>) {
        crate::i18n::set_lang(lang);
        self.notice.clear();
        self.errors = self.ui.problems();
        self.perms = check_permissions();
        if let Err(e) = crate::i18n::save_prefs(&crate::config_dir().join("prefs.toml"), lang) {
            self.notice = crate::i18n::trf("無法儲存語言偏好：{}", &[&e]);
        }
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
                (crate::i18n::tr("有尚未儲存的變更").to_string(), th.text2)
            } else if self.notice.is_empty() {
                (crate::i18n::tr("所有變更都已儲存").to_string(), th.text2)
            } else {
                (self.notice.clone(), th.text2)
            };
            ui.label(RichText::new(text).size(12.0).color(color));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add_enabled_ui(dirty, |ui| primary(ui, th, crate::i18n::tr("儲存"))).inner.clicked()
                    && self.save()
                    && self.worker.is_some()
                    && self.stop_at.is_none()
                {
                    self.apply_on_stop = true;
                    self.stop_worker();
                    self.notice = crate::i18n::tr("已儲存，正在重新啟動分享以套用…").into();
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
        if crate::menubar::close_to_hide(ctx.input(|i| i.viewport().close_requested()), self.quitting) { ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose); self.menubar.set_hidden(ctx, true); }
        let peers: Vec<_> = self.ui.peers.iter().filter(|p| !p.fingerprint.is_empty()).map(|p| { let (live, _) = liveness(self.worker.is_some(), self.link_status.as_ref(), status::now_ms(), &p.name); (p.name.clone(), live) }).collect();
        self.menubar.refresh(self.worker.is_some() && self.stop_at.is_none(), !self.pairing_running() && self.stop_at.is_none(), &peers);
        for action in self.menubar.actions() { match action { crate::menubar::Action::Toggle => { if self.worker.is_some() && self.stop_at.is_none() { self.stop_worker(); if let Err(e) = crate::model::remember_sharing(&crate::config_dir(), false) { self.notice = crate::i18n::trf("無法記下共用狀態：{}", &[&e]); } } else if self.worker.is_none() { self.start_worker(); if self.worker.is_some() { if let Err(e) = crate::model::remember_sharing(&crate::config_dir(), true) { self.notice = crate::i18n::trf("無法記下共用狀態：{}", &[&e]); } } } }, crate::menubar::Action::Settings => self.menubar.set_hidden(ctx, false), crate::menubar::Action::Hide => self.menubar.set_hidden(ctx, true), crate::menubar::Action::WhatsNew => crate::about::show_whats_new(), crate::menubar::Action::Updates => crate::about::show_update_check(), crate::menubar::Action::Quit => { self.quitting = true; self.apply_on_stop = false; self.stop_worker(); } } }
        if self.quitting && self.worker.is_none() { ctx.send_viewport_cmd(egui::ViewportCommand::Close); }
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
                ui.add_space(if cfg!(target_os = "macos") {
                    TITLEBAR_H - 30.0
                } else {
                    12.0
                });
                let title = PAGES.iter().find(|p| p.0 == self.page).map_or("", |p| p.1);
                ui.label(bold(crate::i18n::tr(title), 22.0));
                ui.add_space(10.0);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
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

        self.about.draw(ui.ctx());
        // The title bar is transparent: dragging the band at the top of the window moves it.
        #[cfg(target_os = "macos")]
        let ctx = ui.ctx().clone();
        #[cfg(target_os = "macos")]
        let band = egui::Rect::from_min_size(
            ctx.content_rect().min + egui::vec2(80.0, 0.0),
            egui::vec2(ctx.content_rect().width() - 80.0, 28.0),
        );
        #[cfg(target_os = "macos")]
        if ui
            .interact(band, ui.scope_id().with("titlebar-drag"), Sense::drag())
            .drag_started()
        {
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
    egui::Frame::new()
        .inner_margin(Margin::symmetric(14, 10))
        .show(ui, |ui| {
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

pub(crate) fn primary(ui: &mut egui::Ui, th: Theme, text: &str) -> egui::Response {
    let fill = if ui.is_enabled() { th.accent } else { th.field };
    let color = if ui.is_enabled() { Color32::WHITE } else { th.text2 };
    ui.add(egui::Button::new(RichText::new(text).color(color)).fill(fill).corner_radius(CornerRadius::same(14)).min_size(egui::vec2(72.0, 28.0)))
}

pub(crate) fn secondary(ui: &mut egui::Ui, th: Theme, text: &str) -> egui::Response {
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
        KeyMap::Position => crate::i18n::tr("依鍵盤位置"),
        KeyMap::Mac => crate::i18n::tr("Mac 快捷鍵"),
        KeyMap::None => crate::i18n::tr("不轉換"),
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
        Icon::Download => {
            let ctr = r.center();
            p.circle_stroke(ctr, w * 0.5, s);
            p.line_segment([ctr - egui::vec2(0.0, w * 0.26), ctr + egui::vec2(0.0, w * 0.22)], s);
            p.line_segment([ctr + egui::vec2(-w * 0.17, w * 0.06), ctr + egui::vec2(0.0, w * 0.23)], s);
            p.line_segment([ctr + egui::vec2(w * 0.17, w * 0.06), ctr + egui::vec2(0.0, w * 0.23)], s);
        }
    }
}

pub(crate) fn theme_of(ctx: &egui::Context) -> Theme {
    Theme::of(ctx.theme() == egui::Theme::Dark)
}

/// A sheet over the dimmed window, as macOS presents one: a raised card with a large icon tile, a title and a
/// line under it, then the content. Returns what `add` returns, and whether Escape or a click outside asked to
/// close it.
pub(crate) fn sheet<R>(ctx: &egui::Context, id: &str, icon: Icon, tint: Color32, title: &str, subtitle: &str, add: impl FnOnce(&mut egui::Ui, Theme) -> R) -> (R, bool) {
    let th = theme_of(ctx);
    let frame = egui::Frame::new()
        .fill(th.card)
        .stroke(Stroke::new(1.0, th.line))
        .corner_radius(CornerRadius::same(20))
        .shadow(egui::Shadow { offset: [0, 18], blur: 48, spread: 0, color: Color32::from_black_alpha(if th.dark { 150 } else { 60 }) })
        .inner_margin(Margin { left: 28, right: 28, top: 26, bottom: 22 });
    let resp = egui::Modal::new(egui::Id::unique(id)).frame(frame).backdrop_color(Color32::from_black_alpha(if th.dark { 120 } else { 70 })).show(ctx, |ui| {
        ui.set_width(400.0);
        ui.spacing_mut().item_spacing.y = 0.0;
        ui.vertical_centered(|ui| {
            let (tile, _) = ui.allocate_exact_size(egui::vec2(52.0, 52.0), Sense::hover());
            ui.painter().rect_filled(tile, CornerRadius::same(13), tint);
            paint_icon(ui.painter(), tile.shrink(13.0), icon, Color32::WHITE);
            ui.add_space(14.0);
            ui.label(bold(title, 18.0).color(th.text));
            if !subtitle.is_empty() {
                ui.add_space(5.0);
                ui.label(RichText::new(subtitle).size(12.5).color(th.text2));
            }
        });
        ui.add_space(20.0);
        add(ui, th)
    });
    let close = resp.should_close();
    (resp.inner, close)
}

/// One line of a sheet's list: a small tinted icon tile, a bold title, the body under it.
pub(crate) fn sheet_item(ui: &mut egui::Ui, th: Theme, icon: Icon, tint: Color32, title: &str, body: &str) {
    ui.horizontal_top(|ui| {
        let (tile, _) = ui.allocate_exact_size(egui::vec2(30.0, 30.0), Sense::hover());
        ui.painter().rect_filled(tile, CornerRadius::same(8), tint);
        paint_icon(ui.painter(), tile.shrink(7.5), icon, Color32::WHITE);
        ui.add_space(12.0);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 3.0;
            ui.label(bold(title, 13.5).color(th.text));
            ui.add(egui::Label::new(RichText::new(body).size(12.5).color(th.text2)).wrap());
        });
    });
    ui.add_space(16.0);
}

/// A text button in the accent colour, for the quieter action of a sheet.
pub(crate) fn link(ui: &mut egui::Ui, th: Theme, text: &str) -> egui::Response {
    ui.add(egui::Label::new(RichText::new(text).size(13.0).color(th.accent)).sense(Sense::click())).on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Runs `add` with egui's own widgets (combo box and its popup) dressed as the window's controls: a field-coloured
/// button, and a popup that is a card with accent selection, as a macOS pop-up menu.
fn native_controls<R>(ui: &mut egui::Ui, th: Theme, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.scope(|ui| {
        let v = &mut ui.style_mut().visuals;
        for w in [&mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
            w.bg_fill = th.field;
            w.weak_bg_fill = th.field;
            w.bg_stroke = Stroke::NONE;
            w.corner_radius = CornerRadius::same(7);
            w.fg_stroke = Stroke::new(1.0, th.text);
            w.expansion = 0.0;
        }
        v.widgets.hovered.weak_bg_fill = lerp_color(th.field, th.text, 0.06);
        v.window_fill = th.card;
        v.window_stroke = Stroke::new(1.0, th.line);
        v.menu_corner_radius = CornerRadius::same(10);
        v.popup_shadow = egui::Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(if th.dark { 110 } else { 40 }) };
        v.selection.bg_fill = th.accent;
        v.selection.stroke = Stroke::new(1.0, Color32::WHITE);
        ui.style_mut().spacing.button_padding = egui::vec2(10.0, 5.0);
        add(ui)
    })
    .inner
}
