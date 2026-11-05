//! The renderer's `t(zh, en, params)`: every string carries both languages where it is used.

use std::sync::atomic::{AtomicBool, Ordering};

static ENGLISH: AtomicBool = AtomicBool::new(false);

pub fn set_english(on: bool) {
    ENGLISH.store(on, Ordering::Relaxed);
}

pub fn is_english() -> bool {
    ENGLISH.load(Ordering::Relaxed)
}

pub fn t(zh: &'static str, en: &'static str) -> &'static str {
    if is_english() { en } else { zh }
}

/// The same with `{name}` parameters.
pub fn tf(zh: &str, en: &str, params: &[(&str, &dyn std::fmt::Display)]) -> String {
    let mut out = if is_english() { en.to_owned() } else { zh.to_owned() };
    for (name, value) in params {
        out = out.replace(&format!("{{{name}}}"), &value.to_string());
    }
    out
}

/// What the system language resolves to, as the renderer's `resolveApplicationLanguage` decides it.
pub fn system_prefers_chinese() -> bool {
    // Asked every frame, and the answer can take starting `defaults`, so it is worked out once. The system's language
    // only changes under a running app when someone means to log out for it.
    static PREFERS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *PREFERS.get_or_init(|| {
        let locale = std::env::var("MEWRK_LANG").or_else(|_| std::env::var("LANG")).unwrap_or_default();
        let primary = locale.split(['-', '_', '.']).next().unwrap_or("").to_ascii_lowercase();
        if !primary.is_empty() && primary != "c" {
            return matches!(primary.as_str(), "zh" | "cmn" | "yue" | "wuu" | "hak" | "nan" | "gan" | "hsn" | "cdo" | "cjy" | "cpx" | "czh" | "czo" | "lzh" | "ltc" | "mnp" | "och");
        }
        #[cfg(windows)]
        return windows_ui_language_is_chinese();
        #[cfg(not(windows))]
        apple_languages_first().is_some_and(|l| l.to_ascii_lowercase().starts_with("zh"))
    })
}

/// The user's locale, `zh-CN` and the like.
#[cfg(windows)]
pub fn windows_locale_name() -> String {
    unsafe extern "system" {
        fn GetUserDefaultLocaleName(name: *mut u16, length: i32) -> i32;
    }
    let mut name = [0u16; 85];
    // SAFETY: the buffer's length is what's declared, and a return above 1 counts what was written, with its terminating NUL.
    let written = unsafe { GetUserDefaultLocaleName(name.as_mut_ptr(), name.len() as i32) };
    if written > 1 { String::from_utf16_lossy(&name[..written as usize - 1]) } else { String::new() }
}

#[cfg(windows)]
fn windows_ui_language_is_chinese() -> bool {
    unsafe extern "system" {
        fn GetUserDefaultUILanguage() -> u16;
    }
    // SAFETY: takes nothing and returns a LANGID, whose low ten bits are the primary language; 0x04 is LANG_CHINESE.
    unsafe { GetUserDefaultUILanguage() & 0x3ff == 0x04 }
}

#[cfg(not(windows))]
fn apple_languages_first() -> Option<String> {
    let out = std::process::Command::new("defaults").args(["read", "-g", "AppleLanguages"]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().nth(1).map(|l| l.trim().trim_matches(|c| c == '"' || c == ',').to_owned())
}
