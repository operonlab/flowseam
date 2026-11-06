use eframe::egui::{self, ViewportCommand};
#[cfg(target_os = "macos")]
use muda::accelerator::{Accelerator, Code, Modifiers};
use muda::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
#[cfg(target_os = "macos")]
use muda::Submenu;
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action { Toggle, Settings, Hide, WhatsNew, Updates, Quit }

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row { pub label: String, pub enabled: bool, pub checked: bool, pub action: Option<Action> }

pub fn model(sharing: bool, can_toggle: bool, peers: &[(String, String)]) -> Vec<Row> {
    let mut rows = vec![Row { label: crate::i18n::tr(if sharing { "共用中" } else { "共用已關閉" }).into(), enabled: false, checked: false, action: None }];
    rows.extend(peers.iter().map(|(name, live)| Row { label: crate::i18n::trf("{}：{}", &[name, live]), enabled: false, checked: false, action: None }));
    rows.push(Row { label: String::new(), enabled: false, checked: false, action: None });
    rows.extend([
        Row { label: crate::i18n::tr("共用鍵盤與滑鼠").into(), enabled: can_toggle, checked: sharing, action: Some(Action::Toggle) },
        Row { label: crate::i18n::tr("設定…").into(), enabled: true, checked: false, action: Some(Action::Settings) },
        Row { label: crate::i18n::tr("Flowseam 的新功能").into(), enabled: true, checked: false, action: Some(Action::WhatsNew) },
        Row { label: crate::i18n::tr("檢查更新…").into(), enabled: true, checked: false, action: Some(Action::Updates) },
        Row { label: String::new(), enabled: false, checked: false, action: None },
        Row { label: crate::i18n::tr("結束 Flowseam").into(), enabled: true, checked: false, action: Some(Action::Quit) },
    ]);
    rows
}

pub fn close_to_hide(close_requested: bool, quitting: bool) -> bool { close_requested && !quitting }

#[cfg(target_os = "macos")]
fn shortcut(code: Code) -> Option<Accelerator> { Some(Accelerator::new(Some(Modifiers::SUPER), code)) }

#[cfg(target_os = "macos")]
fn app_menu() -> Menu {
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
    menu
}

fn tray_menu(rows: &[Row]) -> Menu {
    let menu = Menu::new();
    for row in rows {
        match row.action {
            Some(Action::Toggle) => menu.append(&CheckMenuItem::with_id("toggle", &row.label, row.enabled, row.checked, None)),
            Some(action) => menu.append(&MenuItem::with_id(match action { Action::Settings => "settings", Action::Hide => "hide", Action::WhatsNew => "whats-new", Action::Updates => "updates", Action::Quit => "quit", Action::Toggle => unreachable!() }, &row.label, row.enabled, None)),
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
    previous: Vec<Row>,
}

impl MenuBar {
    pub fn new(hidden: bool) -> Self {
        #[cfg(not(target_os = "macos"))]
        let _ = hidden; // hiding the Dock icon is a Mac idea
        let rows = model(false, true, &[]);
        let tray = TrayIconBuilder::new().with_icon(icon()).with_icon_as_template(cfg!(target_os = "macos")).with_tooltip(crate::i18n::tr("Flowseam")).with_menu(Box::new(tray_menu(&rows))).build().expect("create Flowseam tray icon");
        #[cfg(target_os = "macos")]
        let app_menu = { let menu = app_menu(); menu.init_for_nsapp(); set_activation_policy(hidden); menu };
        Self { _tray: tray, #[cfg(target_os = "macos")] _app_menu: app_menu, previous: rows }
    }

    pub fn refresh(&mut self, sharing: bool, can_toggle: bool, peers: &[(String, String)]) {
        let rows = model(sharing, can_toggle, peers);
        if rows != self.previous { self._tray.set_menu(Some(Box::new(tray_menu(&rows)))); self.previous = rows; }
    }

    pub fn actions(&mut self) -> Vec<Action> {
        let mut out = Vec::new();
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            let action = match event.id.0.as_str() { "toggle" => Action::Toggle, "settings" => Action::Settings, "hide" => Action::Hide, "whats-new" => Action::WhatsNew, "updates" => Action::Updates, "quit" => Action::Quit, _ => continue };
            if action == Action::Toggle { self.previous.clear(); }
            out.push(action);
        }
        out
    }

    pub fn set_hidden(&self, ctx: &egui::Context, hidden: bool) {
        ctx.send_viewport_cmd(ViewportCommand::Visible(!hidden));
        #[cfg(target_os = "macos")]
        set_activation_policy(hidden);
        if !hidden { ctx.send_viewport_cmd(ViewportCommand::Focus); }
    }
}

#[cfg(target_os = "macos")]
fn set_activation_policy(hidden: bool) {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
    let app = NSApplication::sharedApplication(MainThreadMarker::new().expect("menu must run on main thread"));
    app.setActivationPolicy(if hidden { NSApplicationActivationPolicy::Accessory } else { NSApplicationActivationPolicy::Regular });
    if !hidden { app.activate(); }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_model_orders_status_peers_actions_and_separators() {
        let rows = model(true, false, &[("Mac".into(), "連線中 · 有線".into()), ("Win".into(), "未連線".into())]);
        assert_eq!(rows.iter().map(|r| r.label.as_str()).collect::<Vec<_>>(), ["共用中", "Mac：連線中 · 有線", "Win：未連線", "", "共用鍵盤與滑鼠", "設定…", "Flowseam 的新功能", "檢查更新…", "", "結束 Flowseam"]);
        assert!(!rows[0].enabled && !rows[1].enabled && !rows[2].enabled);
        assert!(!rows[4].enabled && rows[4].checked);
        assert!(rows[5].enabled && rows[7].enabled && rows[9].enabled);
        assert_eq!(model(false, true, &[])[0].label, "共用已關閉");
    }

    #[test]
    fn close_hides_only_when_not_quitting() {
        assert!(close_to_hide(true, false));
        assert!(!close_to_hide(false, false));
        assert!(!close_to_hide(true, true));
    }
}
