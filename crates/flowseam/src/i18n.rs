//! User-visible text, keyed by its Traditional Chinese source.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang { ZhHant, En, Ja, Ko }

impl Lang {
    pub const ALL: [Self; 4] = [Self::ZhHant, Self::En, Self::Ja, Self::Ko];
    pub fn name(self) -> &'static str { match self { Self::ZhHant => "繁體中文", Self::En => "English", Self::Ja => "日本語", Self::Ko => "한국어" } }
    fn code(self) -> &'static str { match self { Self::ZhHant => "zh-Hant", Self::En => "en", Self::Ja => "ja", Self::Ko => "ko" } }
    fn from_code(s: &str) -> Option<Self> { match s { "zh-Hant" => Some(Self::ZhHant), "en" => Some(Self::En), "ja" => Some(Self::Ja), "ko" => Some(Self::Ko), _ => None } }
    fn index(self) -> u8 { match self { Self::ZhHant => 1, Self::En => 2, Self::Ja => 3, Self::Ko => 4 } }
}

static OVERRIDE: AtomicU8 = AtomicU8::new(0);
static SYSTEM: OnceLock<Lang> = OnceLock::new();
type Catalog = HashMap<String, HashMap<String, String>>;
static CATALOG: OnceLock<Catalog> = OnceLock::new();

fn catalog() -> &'static Catalog { CATALOG.get_or_init(|| serde_json::from_str(include_str!("../i18n/catalog.json")).expect("valid i18n catalog")) }
pub fn set_lang(lang: Option<Lang>) { OVERRIDE.store(lang.map_or(0, Lang::index), Ordering::Relaxed); }
pub fn selected() -> Option<Lang> { match OVERRIDE.load(Ordering::Relaxed) { 1 => Some(Lang::ZhHant), 2 => Some(Lang::En), 3 => Some(Lang::Ja), 4 => Some(Lang::Ko), _ => None } }
pub fn current() -> Lang { selected().unwrap_or_else(|| *SYSTEM.get_or_init(system_language)) }

fn from_locales<'a>(locales: impl IntoIterator<Item = &'a str>) -> Lang {
    for locale in locales {
        let normalized = locale.to_ascii_lowercase().replace('_', "-");
        let prefix = normalized.split('-').next().unwrap_or("");
        match prefix { "zh" => return Lang::ZhHant, "en" => return Lang::En, "ja" => return Lang::Ja, "ko" => return Lang::Ko, _ => {} }
    }
    Lang::En
}

#[cfg(target_os = "macos")]
fn system_language() -> Lang {
    use std::ffi::{c_char, c_void};
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFLocaleCopyPreferredLanguages() -> *const c_void;
        fn CFArrayGetCount(array: *const c_void) -> isize;
        fn CFArrayGetValueAtIndex(array: *const c_void, index: isize) -> *const c_void;
        fn CFStringGetCString(string: *const c_void, buffer: *mut c_char, size: isize, encoding: u32) -> u8;
        fn CFRelease(value: *const c_void);
    }
    let mut locales = Vec::new();
    unsafe {
        let array = CFLocaleCopyPreferredLanguages();
        if array.is_null() { return Lang::En; }
        for i in 0..CFArrayGetCount(array) {
            let mut buf = [0_i8; 128];
            if CFStringGetCString(CFArrayGetValueAtIndex(array, i), buf.as_mut_ptr(), buf.len() as isize, 0x08000100) != 0 {
                let bytes = std::ffi::CStr::from_ptr(buf.as_ptr());
                if let Ok(s) = bytes.to_str() { locales.push(s.to_owned()); }
            }
        }
        CFRelease(array);
    }
    from_locales(locales.iter().map(String::as_str))
}

#[cfg(windows)]
fn system_language() -> Lang {
    use windows_sys::Win32::Globalization::GetUserDefaultLocaleName;
    let mut buf = [0_u16; 85];
    let len = unsafe { GetUserDefaultLocaleName(buf.as_mut_ptr(), buf.len() as i32) };
    if len <= 1 { return Lang::En; }
    from_locales([String::from_utf16_lossy(&buf[..len as usize - 1])].iter().map(String::as_str))
}

#[cfg(not(any(target_os = "macos", windows)))]
fn system_language() -> Lang { Lang::En }

pub fn tr(key: &'static str) -> &'static str {
    let lang = current();
    if lang == Lang::ZhHant { return key; }
    lookup(key, lang, catalog()).unwrap_or(key)
}

fn lookup<'a>(key: &str, lang: Lang, catalog: &'a Catalog) -> Option<&'a str> {
    catalog.get(key).and_then(|row| row.get(lang.code()).or_else(|| row.get("en"))).map(String::as_str)
}

/// Text that arrives at run time keyed the same way, such as the reasons the worker writes to status.toml.
/// An exact key is translated; `prefix：detail` translates the prefix when the catalog has `prefix：{}`; anything
/// else comes back as it is.
pub fn tr_runtime(text: &str) -> String {
    tr_runtime_in(text, current())
}

fn tr_runtime_in(text: &str, lang: Lang) -> String {
    if lang == Lang::ZhHant { return text.to_string(); }
    if let Some(t) = lookup(text, lang, catalog()) { return t.to_string(); }
    if let Some((prefix, detail)) = text.split_once('：') {
        if let Some(t) = lookup(&format!("{prefix}：{{}}"), lang, catalog()) { return t.replacen("{}", detail, 1); }
    }
    text.to_string()
}

pub fn trf(key: &'static str, args: &[&dyn std::fmt::Display]) -> String {
    let mut out = String::new();
    let mut rest = tr(key);
    for a in args {
        match rest.split_once("{}") {
            Some((head, tail)) => { out.push_str(head); out.push_str(&a.to_string()); rest = tail; }
            None => break,
        }
    }
    out.push_str(rest);
    out
}

fn parse_pref(text: &str) -> Option<Option<Lang>> {
    let value = text.lines().find_map(|line| line.trim().strip_prefix("language = "))?.trim().trim_matches('"');
    if value == "system" { Some(None) } else { Lang::from_code(value).map(Some) }
}
pub fn load_prefs(path: &Path) -> std::io::Result<()> {
    match std::fs::read_to_string(path) {
        Ok(text) => { if let Some(lang) = parse_pref(&text) { set_lang(lang); } Ok(()) }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}
/// `text` with its top-level `language` set to `lang`, every other line kept: prefs.toml is shared (the about
/// sheet keeps its seen version there), so rewriting the whole file would lose what others stored.
fn with_language(text: &str, lang: Option<Lang>) -> String {
    let line = format!("language = \"{}\"", lang.map_or("system", Lang::code));
    let mut out = Vec::new();
    let mut placed = false;
    let mut top = true;
    for l in text.lines() {
        if l.trim_start().starts_with('[') {
            if top && !placed { out.push(line.clone()); placed = true; }
            top = false;
        }
        if top && l.trim_start().starts_with("language") && l.trim_start()["language".len()..].trim_start().starts_with('=') {
            if !placed { out.push(line.clone()); placed = true; }
            continue;
        }
        out.push(l.to_string());
    }
    if !placed { out.insert(0, line); }
    out.join("\n") + "\n"
}
pub fn save_prefs(path: &Path, lang: Option<Lang>) -> std::io::Result<()> {
    crate::ui_prefs::write_shared(path, |text| with_language(text, lang))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn locale_mapping() {
        assert_eq!(from_locales(["fr-FR", "zh-HK", "en-US"]), Lang::ZhHant);
        assert_eq!(from_locales(["zh-CN"]), Lang::ZhHant);
        assert_eq!(from_locales(["fr", "ja-JP"]), Lang::Ja);
        assert_eq!(from_locales(["ko-KR"]), Lang::Ko);
        assert_eq!(from_locales(["de-DE"]), Lang::En);
    }
    #[test]
    fn fallback_order() {
        let catalog = HashMap::from([("key".to_owned(), HashMap::from([("en".to_owned(), "English".to_owned()), ("ja".to_owned(), "日本語".to_owned())]))]);
        assert_eq!(lookup("key", Lang::Ja, &catalog), Some("日本語"));
        assert_eq!(lookup("key", Lang::Ko, &catalog), Some("English"));
        assert_eq!(lookup("absent", Lang::Ko, &catalog), None);
    }
    #[test]
    fn prefs_round_trip() {
        let path = std::env::temp_dir().join(format!("flowseam-i18n-{}.toml", std::process::id()));
        for lang in [None, Some(Lang::ZhHant), Some(Lang::En), Some(Lang::Ja), Some(Lang::Ko)] {
            save_prefs(&path, lang).unwrap();
            assert_eq!(parse_pref(&std::fs::read_to_string(&path).unwrap()), Some(lang));
        }
        std::fs::remove_file(path).unwrap();
        set_lang(None);
    }
    #[test]
    fn runtime_reasons_translate_whole_or_by_prefix() {
        // Not through set_lang: other tests read the process-wide language in parallel.
        assert_eq!(tr_runtime_in("連線逾時", Lang::En), "Connection timed out");
        assert_eq!(tr_runtime_in("對方拒絕：busy", Lang::En), "Refused by the other computer: busy");
        assert_eq!(tr_runtime_in("沒有這句", Lang::En), "沒有這句");
        assert_eq!(tr_runtime_in("連線逾時", Lang::ZhHant), "連線逾時");
    }
    #[test]
    fn saving_the_language_keeps_what_others_stored() {
        let text = "language = \"ja\"\n\n[about]\nwhats_new_seen = \"0.1.1\"\n";
        let out = with_language(text, Some(Lang::Ko));
        assert_eq!(out, "language = \"ko\"\n\n[about]\nwhats_new_seen = \"0.1.1\"\n");
        assert_eq!(with_language("[about]\nwhats_new_seen = \"0.1.1\"\n", None), "language = \"system\"\n[about]\nwhats_new_seen = \"0.1.1\"\n");
        assert_eq!(parse_pref(&out), Some(Some(Lang::Ko)));
    }
    #[test]
    fn catalog_matches_sources() {
        let catalog = catalog();
        for (key, row) in catalog {
            for lang in ["en", "ja", "ko"] {
                let translation = row.get(lang).unwrap_or_else(|| panic!("{key:?} missing {lang}"));
                assert_eq!(key.matches("{}").count(), translation.matches("{}").count(), "{key:?} {lang}");
            }
        }
        let sources = [include_str!("gui.rs"), include_str!("model.rs"), include_str!("about.rs"), include_str!("autostart.rs"), include_str!("menubar.rs"), include_str!("kvm_ui.rs")].join("\n");
        let re = regex::Regex::new(r#"\b(?:tr|trf)\("((?:[^"\\]|\\.)*)""#).unwrap();
        let mut used: std::collections::HashSet<_> = re.captures_iter(&sources).map(|c| c[1].to_owned()).collect();
        let page_re = regex::Regex::new(r#"\(Page::\w+, "([^"]+)""#).unwrap();
        used.extend(page_re.captures_iter(include_str!("gui.rs")).map(|c| c[1].to_owned()));
        // Release notes are drawn through tr(note.title) / tr(note.body), not literals at the call.
        // The worker's reasons for a link being down reach the window through status.toml (tr_runtime).
        let worker = include_str!("../../flowseam-mac/src/links.rs");
        let describe = &worker[worker.find("fn describe").unwrap()..worker.find("fn mark_down").unwrap()];
        let reason_re = regex::Regex::new(r#"=> (?:format!\()?"([^"{]+)(\{[a-z:?]+\})?"#).unwrap();
        used.extend(reason_re.captures_iter(describe).map(|c| if c.get(2).is_some() { format!("{}{{}}", &c[1]) } else { c[1].to_owned() }));
        used.insert("連線中斷".to_owned());
        let note_re = regex::Regex::new(r#"\b(?:title|body): "([^"]+)""#).unwrap();
        used.extend(note_re.captures_iter(include_str!("about.rs")).map(|c| c[1].to_owned()));
        for key in &used { assert!(catalog.contains_key(key), "missing catalog key: {key}"); }
        let unused: Vec<_> = catalog.keys().filter(|key| !used.contains(key.as_str())).collect();
        assert!(unused.is_empty(), "unused catalog keys: {unused:?}");
    }
}
