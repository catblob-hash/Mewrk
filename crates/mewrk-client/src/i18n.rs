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
        apple_languages_first().is_some_and(|l| l.to_ascii_lowercase().starts_with("zh"))
    })
}

fn apple_languages_first() -> Option<String> {
    let out = std::process::Command::new("defaults").args(["read", "-g", "AppleLanguages"]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().nth(1).map(|l| l.trim().trim_matches(|c| c == '"' || c == ',').to_owned())
}
