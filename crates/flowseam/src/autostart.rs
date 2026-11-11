#[derive(Debug, PartialEq, Eq)]
pub enum State { On, Off, #[cfg_attr(not(target_os = "macos"), allow(dead_code))] NeedsApproval, Unknown(String) }

#[cfg(any(target_os = "macos", test))]
fn mac_status(code: isize) -> State {
    match code {
        0 => State::Off,
        1 => State::On,
        2 => State::NeedsApproval,
        // notFound: never registered. Measured on the installed, signed app before any register call. It is off,
        // and the toggle must stay usable: register() reports its own error if it really cannot.
        3 => State::Off,
        _ => State::Unknown(crate::i18n::trf("未知的登入項目狀態：{}", &[&code])),
    }
}

#[cfg(any(target_os = "macos", test))]
/// Run at every start until it converges: the old main-app item goes only once the agent is on, so there is
/// never a start with neither. An agent waiting for approval keeps the old item; an unregister that failed is
/// tried again at the next start.
fn migrate_if_needed(old: State, new: State, register: impl FnOnce() -> Result<State, String>, unregister: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
    if old != State::On { return Ok(()); }
    match new {
        State::On => unregister(),
        State::NeedsApproval | State::Unknown(_) => Ok(()),
        State::Off => match register()? {
            State::On => unregister(),
            State::NeedsApproval => Err(crate::i18n::tr("需要在『系統設定 › 一般 › 登入項目』允許").into()),
            State::Unknown(why) => Err(why),
            State::Off => Err(crate::i18n::tr("無法變更登入項目").into()),
        },
    }
}

#[cfg(any(windows, test))]
fn windows_run_value(exe: &std::path::Path) -> Result<String, String> {
    let path = exe.to_str().ok_or_else(|| crate::i18n::tr("執行檔路徑不是有效的文字").to_string())?;
    if path.contains('"') { return Err(crate::i18n::tr("執行檔路徑含有無法引用的引號").into()); }
    Ok(format!("\"{path}\" --hidden"))
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{mac_status, migrate_if_needed, State};
    use objc2::{class, msg_send};
    use objc2::runtime::AnyObject;

    #[link(name = "ServiceManagement", kind = "framework")]
    extern "C" {}

    fn bundled() -> bool {
        std::env::current_exe().ok().is_some_and(|p| p.to_string_lossy().contains("/Flowseam.app/Contents/MacOS/"))
    }

    unsafe fn service(agent: bool) -> Result<*mut AnyObject, String> {
        let service: *mut AnyObject = if agent {
            let name: *mut AnyObject = msg_send![class!(NSString), stringWithUTF8String: c"com.operonlab.flowseam.login.plist".as_ptr()];
            msg_send![class!(SMAppService), agentServiceWithPlistName: name]
        } else {
            msg_send![class!(SMAppService), mainAppService]
        };
        if service.is_null() { Err(crate::i18n::tr("無法取得 Flowseam 登入項目").into()) } else { Ok(service) }
    }

    unsafe fn change(service: *mut AnyObject, on: bool) -> Result<(), String> {
        let mut error: *mut AnyObject = std::ptr::null_mut();
        let ok: bool = if on { msg_send![service, registerAndReturnError: &mut error] } else { msg_send![service, unregisterAndReturnError: &mut error] };
        if ok { Ok(()) } else if error.is_null() { Err(crate::i18n::tr("無法變更登入項目").into()) } else {
            let code: isize = msg_send![error, code];
            Err(crate::i18n::trf("無法變更登入項目（錯誤代碼：{}）", &[&code]))
        }
    }

    pub fn state() -> State {
        if !bundled() { return State::Unknown(crate::i18n::tr("未從 Flowseam.app 啟動").into()); }
        unsafe {
            let service = match service(true) { Ok(service) => service, Err(why) => return State::Unknown(why) };
            let code: isize = msg_send![service, status];
            mac_status(code)
        }
    }

    pub fn set(on: bool) -> Result<(), String> {
        if !bundled() { return Err(crate::i18n::tr("未從 Flowseam.app 啟動").into()); }
        unsafe {
            change(service(true)?, on)
        }
    }

    pub fn migrate_legacy_login() -> Result<(), String> {
        if !bundled() { return Ok(()); }
        unsafe {
            let old = service(false)?;
            let new = service(true)?;
            let old_state: isize = msg_send![old, status];
            let new_state: isize = msg_send![new, status];
            migrate_if_needed(mac_status(old_state), mac_status(new_state), || {
                change(new, true)?;
                let code: isize = msg_send![new, status];
                Ok(mac_status(code))
            }, || change(old, false))
        }
    }

    pub fn open_settings() -> Result<(), String> {
        std::process::Command::new("open").arg("x-apple.systempreferences:com.apple.LoginItems-Settings.extension").spawn().map(|_| ()).map_err(|e| crate::i18n::trf("無法開啟登入項目設定：{}", &[&e]))
    }
}

#[cfg(windows)]
mod platform {
    use super::{windows_run_value, State};
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ};

    const KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
    const NAME: &str = "Flowseam";
    fn wide(s: &str) -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() }
    fn key_name() -> Vec<u16> { wide(KEY) }
    fn value_name() -> Vec<u16> { wide(NAME) }

    pub fn state() -> State {
        unsafe {
            let mut key: HKEY = std::ptr::null_mut();
            let code = RegOpenKeyExW(HKEY_CURRENT_USER, key_name().as_ptr(), 0, KEY_QUERY_VALUE, &mut key);
            if code == ERROR_FILE_NOT_FOUND { return State::Off; }
            if code != ERROR_SUCCESS { return State::Unknown(crate::i18n::trf("無法讀取登入項目：{}", &[&code])); }
            let mut kind = 0;
            let mut len = 0;
            let code = RegQueryValueExW(key, value_name().as_ptr(), std::ptr::null(), &mut kind, std::ptr::null_mut(), &mut len);
            if code == ERROR_FILE_NOT_FOUND { RegCloseKey(key); return State::Off; }
            if code != ERROR_SUCCESS { RegCloseKey(key); return State::Unknown(crate::i18n::trf("無法讀取登入項目：{}", &[&code])); }
            let mut bytes = vec![0u8; len as usize];
            let code = RegQueryValueExW(key, value_name().as_ptr(), std::ptr::null(), &mut kind, bytes.as_mut_ptr(), &mut len);
            RegCloseKey(key);
            if code != ERROR_SUCCESS || kind != REG_SZ || len % 2 != 0 { return State::Unknown(crate::i18n::trf("登入項目內容無效：{}", &[&code])); }
            let chars: Vec<u16> = bytes[..len as usize].chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect();
            let actual = String::from_utf16(&chars[..chars.iter().position(|c| *c == 0).unwrap_or(chars.len())]);
            match (actual, std::env::current_exe().ok().and_then(|p| windows_run_value(&p).ok())) {
                (Ok(actual), Some(expected)) if actual == expected => State::On,
                (Ok(_), _) => State::Unknown(crate::i18n::tr("登入項目指向另一個執行檔").into()),
                _ => State::Unknown(crate::i18n::tr("登入項目內容不是有效文字").into()),
            }
        }
    }

    pub fn set(on: bool) -> Result<(), String> {
        let value = if on { Some(wide(&windows_run_value(&std::env::current_exe().map_err(|e| crate::i18n::trf("無法取得執行檔路徑：{}", &[&e]))?)?)) } else { None };
        unsafe {
            let mut key: HKEY = std::ptr::null_mut();
            let code = RegCreateKeyExW(HKEY_CURRENT_USER, key_name().as_ptr(), 0, std::ptr::null(), REG_OPTION_NON_VOLATILE, KEY_SET_VALUE, std::ptr::null(), &mut key, std::ptr::null_mut());
            if code != ERROR_SUCCESS { return Err(crate::i18n::trf("無法開啟登入項目：{}", &[&code])); }
            let code = if let Some(value) = value {
                RegSetValueExW(key, value_name().as_ptr(), 0, REG_SZ, value.as_ptr().cast(), (value.len() * 2) as u32)
            } else { RegDeleteValueW(key, value_name().as_ptr()) };
            RegCloseKey(key);
            if code == ERROR_SUCCESS || (!on && code == ERROR_FILE_NOT_FOUND) { Ok(()) } else { Err(crate::i18n::trf("無法變更登入項目：{}", &[&code])) }
        }
    }
}

pub use platform::{set, state};
#[cfg(target_os = "macos")]
pub use platform::open_settings;
#[cfg(target_os = "macos")]
pub use platform::migrate_legacy_login;

/// The app bundle that holds `exe` (…/Flowseam.app/Contents/MacOS/flowseam), so the launcher opens this copy and
/// not another one Launch Services might pick by bundle id.
#[cfg(any(target_os = "macos", test))]
fn bundle_of(exe: &std::path::Path) -> Option<std::path::PathBuf> {
    let bundle = exe.parent()?.parent()?.parent()?;
    (bundle.extension().is_some_and(|e| e == "app") && exe.parent()?.ends_with("Contents/MacOS")).then(|| bundle.to_owned())
}

/// Run by the login launch agent (`flowseam launch-at-login`). It opens the app hidden through Launch Services
/// and returns, so the agent's job holds no process: unregistering an agent stops its process, and turning the
/// option off must not quit the running app. It does nothing while Flowseam runs, which is also the case when
/// the agent first runs at registration.
#[cfg(target_os = "macos")]
pub fn launch_at_login() -> i32 {
    let dir = crate::config_dir();
    match crate::single::claim(&dir) {
        Ok(crate::single::Start::Second) => return 0,
        Ok(crate::single::Start::First(guard)) => drop(guard),
        Err(e) => eprintln!("flowseam: could not check for a running Flowseam ({e}); opening it anyway"),
    }
    let Some(bundle) = std::env::current_exe().ok().and_then(|exe| bundle_of(&exe)) else { eprintln!("flowseam: launch-at-login runs only from inside Flowseam.app"); return 1 };
    match std::process::Command::new("/usr/bin/open").arg("-g").arg("-a").arg(&bundle).args(["--args", "--hidden"]).status() {
        Ok(status) if status.success() => 0,
        Ok(status) => { eprintln!("flowseam: open {} exited with {status}", bundle.display()); 1 }
        Err(e) => { eprintln!("flowseam: could not run open: {e}"); 1 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_status_mapping() {
        assert_eq!(mac_status(0), State::Off);
        assert_eq!(mac_status(1), State::On);
        assert_eq!(mac_status(2), State::NeedsApproval);
        assert_eq!(mac_status(3), State::Off, "never registered is off, not unknown");
        assert!(matches!(mac_status(99), State::Unknown(_)));
    }

    #[test]
    fn windows_command_quotes_spaces_and_hides() {
        assert_eq!(windows_run_value(std::path::Path::new(r"C:\Program Files\Flowseam\flowseam.exe")).unwrap(), r#""C:\Program Files\Flowseam\flowseam.exe" --hidden"#);
    }

    #[test]
    fn the_launcher_opens_the_bundle_it_lives_in() {
        assert_eq!(bundle_of(std::path::Path::new("/Applications/Flowseam.app/Contents/MacOS/flowseam")), Some("/Applications/Flowseam.app".into()));
        assert_eq!(bundle_of(std::path::Path::new("/opt/x/target/release/flowseam")), None, "not in a bundle");
        assert_eq!(bundle_of(std::path::Path::new("/a/b/Contents/MacOS/flowseam")), None);
    }

    #[test]
    fn migration_registers_first_and_keeps_old_on_failure() {
        let calls = std::cell::RefCell::new(Vec::new());
        migrate_if_needed(State::On, State::Off, || { calls.borrow_mut().push("register"); Ok(State::On) }, || { calls.borrow_mut().push("unregister"); Ok(()) }).unwrap();
        assert_eq!(*calls.borrow(), ["register", "unregister"]);
        let mut unregistered = false;
        assert!(migrate_if_needed(State::On, State::Off, || Err("register failed".into()), || { unregistered = true; Ok(()) }).is_err());
        assert!(!unregistered);
        assert!(migrate_if_needed(State::On, State::Off, || Ok(State::NeedsApproval), || { unregistered = true; Ok(()) }).is_err());
        assert!(!unregistered);
    }

    #[test]
    fn migration_converges_on_later_starts() {
        let mut unregistered = 0;
        migrate_if_needed(State::On, State::On, || panic!("must not register again"), || { unregistered += 1; Ok(()) }).unwrap();
        assert_eq!(unregistered, 1, "agent already on: the old item still enabled is removed now");
        migrate_if_needed(State::On, State::NeedsApproval, || panic!("must not register again"), || panic!("the old item stays until the agent is approved")).unwrap();
        migrate_if_needed(State::Off, State::On, || panic!("nothing to migrate"), || panic!("nothing to migrate")).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn packaged_login_plist_starts_hidden_in_aqua() {
        let root = std::env::temp_dir().join(format!("flowseam-login-plist-{}-{:?}", std::process::id(), std::thread::current().id()));
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/make-app.sh");
        let output = std::process::Command::new("bash").arg(script).arg("--write-login-plist-for-test").arg(&root).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let plist = root.join("Contents/Library/LaunchAgents/com.operonlab.flowseam.login.plist");
        let output = std::process::Command::new("plutil").args(["-convert", "json", "-o", "-"]).arg(&plist).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["Label"], "com.operonlab.flowseam.login");
        assert_eq!(value["BundleProgram"], "Contents/MacOS/flowseam");
        assert_eq!(value["ProgramArguments"], serde_json::json!(["flowseam", "launch-at-login"]), "a launcher that returns: unregistering the agent then stops no running app");
        assert_eq!(value["RunAtLoad"], true);
        assert_eq!(value["LimitLoadToSessionType"], "Aqua");
        assert_eq!(value["ProcessType"], "Interactive");
        assert_eq!(value["AssociatedBundleIdentifiers"], serde_json::json!(["com.operonlab.flowseam"]));
        assert!(value.get("KeepAlive").is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
}
