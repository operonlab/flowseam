use eframe::egui::{self, ViewportCommand};
#[cfg(target_os = "macos")]
use muda::accelerator::{Accelerator, Code, Modifiers};
use muda::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
#[cfg(target_os = "macos")]
use muda::Submenu;
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use std::sync::{mpsc, OnceLock};
#[cfg(target_os = "macos")]
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(target_os = "macos")]
static REOPEN: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "macos")]
static CORRECTION_LOGGED: AtomicBool = AtomicBool::new(false);
static REPAINT_CONTEXT: OnceLock<egui::Context> = OnceLock::new();

pub fn install_repaint_context(ctx: &egui::Context) { let _ = REPAINT_CONTEXT.set(ctx.clone()); }

fn wake_gui() { if let Some(ctx) = REPAINT_CONTEXT.get() { ctx.request_repaint(); } }

#[cfg(target_os = "macos")]
mod reopen_event {
    use std::ffi::c_void;
    use std::sync::atomic::AtomicBool;
    use std::sync::Once;
    use super::{wake_gui, Ordering, REOPEN};

    const CORE_EVENT_CLASS: u32 = 0x6165_7674; // 'aevt'
    const REOPEN_APPLICATION: u32 = 0x7261_7070; // 'rapp'
    type AeHandler = extern "C" fn(*const c_void, *mut c_void, *mut c_void) -> i16;
    #[link(name = "CoreServices", kind = "framework")]
    extern "C" { fn AEInstallEventHandler(class: u32, id: u32, handler: AeHandler, refcon: *mut c_void, is_sys_handler: u8) -> i16; }

    // The two-process bundle probe delivered reopen to this window process, not its worker.
    extern "C" fn on_reopen(_event: *const c_void, _reply: *mut c_void, _refcon: *mut c_void) -> i16 { REOPEN.store(true, Ordering::Release); wake_gui(); 0 }

    /// Whether reopening the app can bring the window back. When it cannot, the menu bar icon stays on whatever
    /// the preference says, or hiding both icons would leave no way back to Settings.
    pub fn install() -> bool {
        static ONCE: Once = Once::new();
        static INSTALLED: AtomicBool = AtomicBool::new(false);
        ONCE.call_once(|| {
            // SAFETY: CoreServices retains the plain function pointer; the handler only stores to an atomic.
            let err = unsafe { AEInstallEventHandler(CORE_EVENT_CLASS, REOPEN_APPLICATION, on_reopen, std::ptr::null_mut(), 0) };
            if err == 0 { INSTALLED.store(true, Ordering::Release); } else { eprintln!("could not install reopen Apple event handler (OSErr {err}); the menu bar icon stays on"); }
        });
        INSTALLED.load(Ordering::Acquire)
    }

    #[cfg(test)]
    mod tests { use super::*; #[test] fn reopen_handler_sets_flag_and_returns_success() { REOPEN.store(false, Ordering::Release); assert_eq!(on_reopen(std::ptr::null(), std::ptr::null_mut(), std::ptr::null_mut()), 0); assert!(REOPEN.swap(false, Ordering::AcqRel)); } }
}

#[cfg(target_os = "macos")]
pub fn take_reopen() -> bool { REOPEN.swap(false, Ordering::AcqRel) }

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn wants_regular(window_visible: bool, dock_icon: bool) -> bool { window_visible && dock_icon }

/// `SwitchTo(i)` is the switch button of the i-th peer passed to the last `refresh`, `Pick(i, j)` ticks or
/// unticks that peer's j-th monitor for it; `Home` hands control back to the peer driving this machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action { Toggle, Settings, Hide, WhatsNew, Updates, NewerUpdates, Quit, SwitchTo(usize), Pick(usize, usize), Home }

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row { pub label: String, pub enabled: bool, pub checked: bool, pub action: Option<Action> }

/// A peer's switch button in the tray menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    /// Hands over the keyboard and mouse.
    Input,
    /// Switches the ticked monitors first, then the keyboard and mouse.
    InputAndDisplay,
    /// A monitor switch or return is under way: shown, not clickable.
    Busy,
}

/// One peer's lines in the tray menu. `monitors`: those that can switch with its button, each ticked or
/// not, under the button. `note`: why its monitors will not switch, under them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerMenu { pub name: String, pub live: String, pub button: Option<Button>, pub monitors: Vec<MenuMonitor>, pub note: Option<String> }

/// A monitor's tick box under its peer's button. `usable`: it can switch now; otherwise `label` says why not
/// and the box cannot be clicked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuMonitor { pub uuid: String, pub label: String, pub ticked: bool, pub usable: bool }

/// Long enough for the gist of a worker's reason; the full text is in the worker log.
const NOTE_CHARS: usize = 80;

/// `driven_by`: the peer driving this machine, which gets a "back" button. `note`: why the last monitor
/// switch did not complete. `newer`: a newer release, offered above Quit.
pub fn model(sharing: bool, can_toggle: bool, peers: &[PeerMenu], driven_by: Option<&str>, note: Option<&str>, newer: Option<&str>) -> Vec<Row> {
    let mut rows = vec![Row { label: crate::i18n::tr(if sharing { "共用中" } else { "共用已關閉" }).into(), enabled: false, checked: false, action: None }];
    for (i, p) in peers.iter().enumerate() {
        rows.push(Row { label: crate::i18n::trf("{}：{}", &[&p.name, &p.live]), enabled: false, checked: false, action: None });
        let button = |label: String| Row { label, enabled: true, checked: false, action: Some(Action::SwitchTo(i)) };
        match p.button {
            Some(Button::Input) => rows.push(button(crate::i18n::trf("切到 {}（鍵鼠）", &[&p.name]))),
            Some(Button::InputAndDisplay) => rows.push(button(crate::i18n::trf("切到 {}（螢幕＋鍵鼠）", &[&p.name]))),
            Some(Button::Busy) => rows.push(Row { label: crate::i18n::tr("螢幕切換中…").into(), enabled: false, checked: false, action: None }),
            None => {}
        }
        // Only beside a button: while a monitor is out or moving, ticking would change nothing now.
        if matches!(p.button, Some(Button::Input | Button::InputAndDisplay)) {
            for (j, m) in p.monitors.iter().enumerate() {
                rows.push(Row { label: m.label.chars().take(NOTE_CHARS).collect(), enabled: m.usable, checked: m.ticked && m.usable, action: Some(Action::Pick(i, j)) });
            }
        }
        if let Some(n) = &p.note {
            rows.push(Row { label: n.chars().take(NOTE_CHARS).collect(), enabled: false, checked: false, action: None });
        }
    }
    if let Some(by) = driven_by {
        rows.push(Row { label: crate::i18n::trf("回到 {}", &[&by]), enabled: true, checked: false, action: Some(Action::Home) });
    }
    if let Some(note) = note {
        let short: String = note.chars().take(NOTE_CHARS).collect();
        rows.push(Row { label: crate::i18n::trf("螢幕切換失敗：{}", &[&short]), enabled: false, checked: false, action: None });
    }
    rows.push(Row { label: String::new(), enabled: false, checked: false, action: None });
    rows.extend([
        Row { label: crate::i18n::tr("共用鍵盤與滑鼠").into(), enabled: can_toggle, checked: sharing, action: Some(Action::Toggle) },
        Row { label: crate::i18n::tr("設定…").into(), enabled: true, checked: false, action: Some(Action::Settings) },
        Row { label: crate::i18n::tr("Flowseam 的新功能").into(), enabled: true, checked: false, action: Some(Action::WhatsNew) },
        Row { label: crate::i18n::tr("檢查更新…").into(), enabled: true, checked: false, action: Some(Action::Updates) },
        Row { label: String::new(), enabled: false, checked: false, action: None },
        Row { label: crate::i18n::tr("結束 Flowseam").into(), enabled: true, checked: false, action: Some(Action::Quit) },
    ]);
    if let Some(version) = newer { rows.insert(rows.len() - 2, Row { label: crate::i18n::trf("有新版 {}", &[&version]), enabled: true, checked: false, action: Some(Action::NewerUpdates) }); }
    rows
}

pub fn close_to_hide(close_requested: bool, quitting: bool) -> bool { close_requested && !quitting }

fn parse_indexed(id: &str) -> Option<Action> {
    if let Some(i) = id.strip_prefix("switch:") {
        return i.parse().ok().map(Action::SwitchTo);
    }
    let (i, j) = id.strip_prefix("pick:")?.split_once(':')?;
    Some(Action::Pick(i.parse().ok()?, j.parse().ok()?))
}

#[cfg(target_os = "macos")]
fn shortcut(code: Code) -> Option<Accelerator> { Some(Accelerator::new(Some(Modifiers::SUPER), code)) }

#[cfg(target_os = "macos")]
fn app_menu() -> (Menu, Submenu) {
    let menu = Menu::new();
    let app = Submenu::new(crate::i18n::tr("Flowseam"), true);
    app.append(&PredefinedMenuItem::about(None, None)).expect("About menu item");
    app.append(&MenuItem::with_id("updates", crate::i18n::tr("檢查更新…"), true, None)).expect("updates menu item");
    app.append(&MenuItem::with_id("whats-new", crate::i18n::tr("Flowseam 的新功能"), true, None)).expect("what's new menu item");
    app.append(&PredefinedMenuItem::separator()).expect("separator");
    app.append(&MenuItem::with_id("settings", crate::i18n::tr("設定…"), true, shortcut(Code::Comma))).expect("settings menu item");
    app.append(&MenuItem::with_id("hide", crate::i18n::tr("隱藏 Flowseam"), true, shortcut(Code::KeyH))).expect("hide menu item");
    app.append(&PredefinedMenuItem::separator()).expect("separator");
    app.append(&MenuItem::with_id("quit", crate::i18n::tr("結束 Flowseam"), true, shortcut(Code::KeyQ))).expect("quit menu item");
    menu.append(&app).expect("app menu");
    (menu, app)
}

fn tray_menu(rows: &[Row]) -> Menu {
    let menu = Menu::new();
    for row in rows {
        match row.action {
            Some(Action::Toggle) => menu.append(&CheckMenuItem::with_id("toggle", &row.label, row.enabled, row.checked, None)),
            Some(Action::SwitchTo(i)) => menu.append(&MenuItem::with_id(format!("switch:{i}"), &row.label, row.enabled, None)),
            Some(Action::Pick(i, j)) => menu.append(&CheckMenuItem::with_id(format!("pick:{i}:{j}"), &row.label, row.enabled, row.checked, None)),
            Some(action) => menu.append(&MenuItem::with_id(match action { Action::Settings => "settings", Action::Hide => "hide", Action::WhatsNew => "whats-new", Action::Updates => "updates", Action::NewerUpdates => "newer-updates", Action::Quit => "quit", Action::Home => "home", Action::Toggle | Action::SwitchTo(_) | Action::Pick(..) => unreachable!() }, &row.label, row.enabled, None)),
            None if row.label.is_empty() => menu.append(&PredefinedMenuItem::separator()),
            None => menu.append(&MenuItem::new(&row.label, false, None)),
        }.expect("tray menu item");
    }
    menu
}

fn icon() -> Icon {
    #[cfg(target_os = "macos")]
    let bytes = include_bytes!("../assets/menu-template.png");
    #[cfg(not(target_os = "macos"))]
    let bytes = include_bytes!("../../../assets/icon/icon-512.png");
    let data = eframe::icon_data::from_png_bytes(bytes).expect("bundled tray icon PNG");
    Icon::from_rgba(data.rgba, data.width, data.height).expect("bundled tray icon RGBA")
}

pub struct MenuBar {
    _tray: TrayIcon,
    #[cfg(target_os = "macos")]
    _app_menu: Menu,
    #[cfg(target_os = "macos")]
    app_submenu: Submenu,
    #[cfg(target_os = "macos")]
    newer_item: Option<MenuItem>,
    #[cfg(target_os = "macos")]
    current_newer: Option<String>,
    window_visible: std::cell::Cell<bool>,
    #[cfg(target_os = "macos")]
    dock_icon: std::cell::Cell<bool>,
    previous: Vec<Row>,
    events: mpsc::Receiver<MenuEvent>,
}

impl MenuBar {
    pub fn new(hidden: bool, prefs: &crate::ui_prefs::UiPrefs) -> Self {
        let (sender, events) = mpsc::channel();
        MenuEvent::set_event_handler(Some(move |event| { let _ = sender.send(event); wake_gui(); }));
        let rows = model(false, true, &[], None, None, None);
        let tray = TrayIconBuilder::new().with_icon(icon()).with_icon_as_template(cfg!(target_os = "macos")).with_tooltip(crate::i18n::tr("Flowseam")).with_menu(Box::new(tray_menu(&rows))).build().expect("create Flowseam tray icon");
        #[cfg(target_os = "macos")]
        let (app_menu, app_submenu) = { let (menu, submenu) = app_menu(); menu.init_for_nsapp(); reopen_event::install(); set_activation_policy(wants_regular(!hidden, prefs.dock_icon), !hidden); (menu, submenu) };
        #[cfg(target_os = "macos")]
        tray.set_visible(prefs.menu_bar_icon || !reopen_event::install()).expect("set menu bar icon visibility");
        #[cfg(not(target_os = "macos"))]
        let _ = prefs;
        Self { _tray: tray, #[cfg(target_os = "macos")] _app_menu: app_menu, #[cfg(target_os = "macos")] app_submenu, #[cfg(target_os = "macos")] newer_item: None, #[cfg(target_os = "macos")] current_newer: None, window_visible: std::cell::Cell::new(!hidden), #[cfg(target_os = "macos")] dock_icon: std::cell::Cell::new(prefs.dock_icon), previous: rows, events }
    }

    pub fn refresh(&mut self, sharing: bool, can_toggle: bool, peers: &[PeerMenu], driven_by: Option<&str>, note: Option<&str>, newer: Option<&str>) {
        let rows = model(sharing, can_toggle, peers, driven_by, note, newer);
        #[cfg(target_os = "macos")]
        let update_changed = self.current_newer.as_deref() != newer;
        if rows != self.previous { self._tray.set_menu(Some(Box::new(tray_menu(&rows)))); self.previous = rows; }
        #[cfg(target_os = "macos")]
        if update_changed {
            if let Some(item) = self.newer_item.take() { self.app_submenu.remove(&item).expect("remove update item"); }
            if let Some(version) = newer { let item = MenuItem::with_id("newer-updates", crate::i18n::trf("有新版 {}", &[&version]), true, None); self.app_submenu.insert(&item, 2).expect("new version item"); self.newer_item = Some(item); }
            self.current_newer = newer.map(str::to_owned);
        }
    }

    pub fn actions(&mut self) -> Vec<Action> {
        let mut out = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            let action = match event.id.0.as_str() { "toggle" => Action::Toggle, "settings" => Action::Settings, "hide" => Action::Hide, "whats-new" => Action::WhatsNew, "updates" => Action::Updates, "newer-updates" => Action::NewerUpdates, "quit" => Action::Quit, "home" => Action::Home, id => match parse_indexed(id) { Some(a) => a, None => continue } };
            // A check item flips its own mark when clicked; rebuild so the mark is what the window decides.
            if matches!(action, Action::Toggle | Action::Pick(..)) { self.previous.clear(); }
            out.push(action);
        }
        out
    }

    pub fn set_hidden(&self, ctx: &egui::Context, hidden: bool) {
        self.window_visible.set(!hidden);
        ctx.send_viewport_cmd(ViewportCommand::Visible(!hidden));
        #[cfg(target_os = "macos")]
        { set_activation_policy(wants_regular(!hidden, self.dock_icon.get()), !hidden); }
        if !hidden { ctx.send_viewport_cmd(ViewportCommand::Focus); }
    }
    pub fn is_hidden(&self) -> bool { !self.window_visible.get() }
    #[cfg(target_os = "macos")]
    pub fn set_icons(&mut self, dock: bool, menu: bool) { self.dock_icon.set(dock); self._tray.set_visible(menu || !reopen_event::install()).expect("set menu bar icon visibility"); set_activation_policy(wants_regular(self.window_visible.get(), dock), self.window_visible.get()); }
    #[cfg(target_os = "macos")]
    pub fn check_activation_policy(&self) { check_activation_policy(wants_regular(self.window_visible.get(), self.dock_icon.get())); }
}

#[cfg(target_os = "macos")]
fn set_activation_policy(regular: bool, activate: bool) {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
    let app = NSApplication::sharedApplication(MainThreadMarker::new().expect("menu must run on main thread"));
    app.setActivationPolicy(if regular { NSApplicationActivationPolicy::Regular } else { NSApplicationActivationPolicy::Accessory });
    if activate { app.activate(); }
}

#[cfg(target_os = "macos")]
fn check_activation_policy(regular: bool) {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
    let app = NSApplication::sharedApplication(MainThreadMarker::new().expect("menu must run on main thread"));
    let wanted = if regular { NSApplicationActivationPolicy::Regular } else { NSApplicationActivationPolicy::Accessory };
    let actual = app.activationPolicy();
    if actual != wanted { if !CORRECTION_LOGGED.swap(true, Ordering::AcqRel) { eprintln!("activation policy corrected: actual {actual:?}, wanted {wanted:?}"); } app.setActivationPolicy(wanted); }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(name: &str, live: &str, button: Option<Button>) -> PeerMenu {
        PeerMenu { name: name.into(), live: live.into(), button, monitors: vec![], note: None }
    }

    #[test]
    fn the_monitors_sit_under_the_button_as_tick_boxes_and_one_that_cannot_switch_is_shown_unclickable() {
        let monitors = vec![
            MenuMonitor { uuid: "U".into(), label: "Left".into(), ticked: true, usable: true },
            MenuMonitor { uuid: "V".into(), label: "Right".into(), ticked: false, usable: true },
            MenuMonitor { uuid: "W".into(), label: "Third · why".into(), ticked: true, usable: false },
        ];
        let p = PeerMenu { monitors, ..peer("pc", "連線中", Some(Button::InputAndDisplay)) };
        let rows = model(true, true, &[peer("air", "連線中", None), p], None, None, None);
        let labels: Vec<_> = rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(&labels[3..7], ["切到 pc（螢幕＋鍵鼠）", "Left", "Right", "Third · why"]);
        assert_eq!((rows[4].checked, rows[4].action), (true, Some(Action::Pick(1, 0))));
        assert_eq!((rows[5].checked, rows[5].enabled), (false, true));
        assert_eq!((rows[6].checked, rows[6].enabled), (false, false), "a monitor that cannot switch is not shown ticked");
        let busy = PeerMenu { button: Some(Button::Busy), ..rows_peer() };
        assert!(!model(true, true, &[busy], None, None, None).iter().any(|r| matches!(r.action, Some(Action::Pick(..)))), "no boxes while monitors move");
    }

    fn rows_peer() -> PeerMenu {
        PeerMenu { monitors: vec![MenuMonitor { uuid: "U".into(), label: "Left".into(), ticked: true, usable: true }], ..peer("pc", "連線中", Some(Button::InputAndDisplay)) }
    }

    #[test]
    fn menu_ids_round_trip() {
        assert_eq!(parse_indexed("switch:3"), Some(Action::SwitchTo(3)));
        assert_eq!(parse_indexed("pick:1:2"), Some(Action::Pick(1, 2)));
        assert_eq!(parse_indexed("pick:1"), None);
        assert_eq!(parse_indexed("other"), None);
    }

    #[test]
    fn a_monitor_that_will_not_switch_says_why_under_its_peer() {
        let p = PeerMenu { note: Some("螢幕輸入尚未確認".into()), ..peer("pc", "連線中", Some(Button::Input)) };
        let rows = model(true, true, &[p, peer("air", "連線中", None)], None, None, None);
        let labels: Vec<_> = rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(&labels[1..5], ["pc：連線中", "切到 pc（鍵鼠）", "螢幕輸入尚未確認", "air：連線中"]);
        assert!(!rows[3].enabled);
    }

    #[test]
    fn menu_model_orders_status_peers_actions_and_separators() {
        let rows = model(true, false, &[peer("Mac", "連線中 · 有線", None), peer("Win", "未連線", None)], None, None, None);
        assert_eq!(rows.iter().map(|r| r.label.as_str()).collect::<Vec<_>>(), ["共用中", "Mac：連線中 · 有線", "Win：未連線", "", "共用鍵盤與滑鼠", "設定…", "Flowseam 的新功能", "檢查更新…", "", "結束 Flowseam"]);
        assert!(!rows[0].enabled && !rows[1].enabled && !rows[2].enabled);
        assert!(!rows[4].enabled && rows[4].checked);
        assert!(rows[5].enabled && rows[7].enabled && rows[9].enabled);
        assert_eq!(model(false, true, &[], None, None, None)[0].label, "共用已關閉");
    }

    #[test]
    fn a_switch_button_sits_under_its_peer_and_names_the_peer_by_position() {
        let rows = model(true, true, &[peer("mini", "未連線", None), peer("pc", "連線中", Some(Button::InputAndDisplay)), peer("air", "連線中", Some(Button::Input))], None, None, None);
        let labels: Vec<_> = rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(&labels[..6], ["共用中", "mini：未連線", "pc：連線中", "切到 pc（螢幕＋鍵鼠）", "air：連線中", "切到 air（鍵鼠）"]);
        assert_eq!((rows[3].enabled, rows[3].action), (true, Some(Action::SwitchTo(1))));
        assert_eq!(rows[5].action, Some(Action::SwitchTo(2)));
    }

    #[test]
    fn a_busy_switch_is_shown_but_not_clickable_and_a_failure_is_shown() {
        let long = "x".repeat(200);
        let rows = model(true, true, &[peer("pc", "連線中", Some(Button::Busy))], None, Some(&long), None);
        assert_eq!((rows[2].label.as_str(), rows[2].enabled, rows[2].action), ("螢幕切換中…", false, None));
        assert!(rows[3].label.starts_with("螢幕切換失敗：") && !rows[3].enabled);
        assert_eq!(rows[3].label.chars().count(), "螢幕切換失敗：".chars().count() + NOTE_CHARS);
    }

    #[test]
    fn a_driven_machine_gets_a_back_button() {
        let rows = model(true, true, &[peer("mini", "連線中", None)], Some("mini"), None, None);
        assert_eq!((rows[2].label.as_str(), rows[2].enabled, rows[2].action), ("回到 mini", true, Some(Action::Home)));
        assert!(!model(true, true, &[peer("mini", "連線中", None)], None, None, None).iter().any(|r| r.action == Some(Action::Home)));
    }

    #[test]
    fn dock_policy_needs_visible_window_and_preference() { assert!(wants_regular(true, true)); assert!(!wants_regular(false, true)); assert!(!wants_regular(true, false)); assert!(!wants_regular(false, false)); }
    #[test]
    fn newer_release_adds_update_action() { let rows = model(false, true, &[], None, None, Some("0.1.4")); assert!(rows.iter().any(|row| row.label == "有新版 0.1.4" && row.action == Some(Action::NewerUpdates))); }

    #[test]
    fn close_hides_only_when_not_quitting() {
        assert!(close_to_hide(true, false));
        assert!(!close_to_hide(false, false));
        assert!(!close_to_hide(true, true));
    }
}
