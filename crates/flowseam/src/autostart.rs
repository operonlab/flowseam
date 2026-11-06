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

#[cfg(any(windows, test))]
fn windows_run_value(exe: &std::path::Path) -> Result<String, String> {
    let path = exe.to_str().ok_or_else(|| crate::i18n::tr("執行檔路徑不是有效的文字").to_string())?;
    if path.contains('"') { return Err(crate::i18n::tr("執行檔路徑含有無法引用的引號").into()); }
    Ok(format!("\"{path}\" --hidden"))
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{mac_status, State};
    use objc2::{class, msg_send};
    use objc2::runtime::AnyObject;

    #[link(name = "ServiceManagement", kind = "framework")]
    extern "C" {}

    fn bundled() -> bool {
        std::env::current_exe().ok().is_some_and(|p| p.to_string_lossy().contains("/Flowseam.app/Contents/MacOS/"))
    }

    pub fn state() -> State {
        if !bundled() { return State::Unknown(crate::i18n::tr("未從 Flowseam.app 啟動").into()); }
        unsafe {
            let service: *mut AnyObject = msg_send![class!(SMAppService), mainAppService];
            if service.is_null() { return State::Unknown(crate::i18n::tr("無法取得 Flowseam 登入項目").into()); }
            let code: isize = msg_send![service, status];
            mac_status(code)
        }
    }

    pub fn set(on: bool) -> Result<(), String> {
        if !bundled() { return Err(crate::i18n::tr("未從 Flowseam.app 啟動").into()); }
        unsafe {
            let service: *mut AnyObject = msg_send![class!(SMAppService), mainAppService];
            if service.is_null() { return Err(crate::i18n::tr("無法取得 Flowseam 登入項目").into()); }
            let mut error: *mut AnyObject = std::ptr::null_mut();
            let ok: bool = if on { msg_send![service, registerAndReturnError: &mut error] } else { msg_send![service, unregisterAndReturnError: &mut error] };
            if ok { Ok(()) } else if error.is_null() { Err(crate::i18n::tr("無法變更登入項目").into()) } else {
                let code: isize = msg_send![error, code];
                Err(crate::i18n::trf("無法變更登入項目（錯誤代碼：{}）", &[&code]))
            }
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
}
