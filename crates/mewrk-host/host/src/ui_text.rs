//! The language the host words text for people in.
//!
//! Errors the renderer shows verbatim, native dialogs, menu and tray labels and
//! approval cards are all host text, and all of it follows the app language the
//! renderer resolved and mirrored into the document
//! (`GlobalSettings::resolved_app_language`). Until a document has been read —
//! the single-instance dialog, a document that cannot be loaded — it follows the
//! system language, resolved the way the renderer resolves `auto`.
//!
//! The language is process-wide rather than threaded through every call: a
//! message is worded where it is made, often far from anything that holds the
//! document, and a switch takes effect for the next message. Text meant for the
//! model is not host UI text; it follows the prompt profile instead.
//!
//! Write a message as `ui_text!("中文 {name}", "English {name}")`, or with
//! [`pick`] when nothing is formatted.

use std::sync::atomic::{AtomicU8, Ordering};

use crate::model::ResolvedLanguage;

const UNSET: u8 = 0;
const CHINESE: u8 = 1;
const ENGLISH: u8 = 2;

static LANGUAGE: AtomicU8 = AtomicU8::new(UNSET);

/// Words the host's messages in `language` from now on. Git's messages, which
/// `git-core` words for itself, follow along.
pub(crate) fn set(language: ResolvedLanguage) {
    LANGUAGE.store(
        match language {
            ResolvedLanguage::ZhCn => CHINESE,
            ResolvedLanguage::EnUs => ENGLISH,
        },
        Ordering::Relaxed,
    );
    git_core::set_english(language == ResolvedLanguage::EnUs);
}

/// The language host messages are worded in right now.
pub(crate) fn current() -> ResolvedLanguage {
    #[cfg(test)]
    if let Some(language) = TEST_LANGUAGE.with(std::cell::Cell::get) {
        return language;
    }
    match LANGUAGE.load(Ordering::Relaxed) {
        CHINESE => ResolvedLanguage::ZhCn,
        ENGLISH => ResolvedLanguage::EnUs,
        // Tests word messages in Chinese unless they ask otherwise, whatever
        // the machine running them is set to.
        _ if cfg!(test) => ResolvedLanguage::ZhCn,
        _ => system_language(),
    }
}

#[cfg(test)]
thread_local! {
    static TEST_LANGUAGE: std::cell::Cell<Option<ResolvedLanguage>> =
        const { std::cell::Cell::new(None) };
}

/// Runs `body` with this thread's host messages worded in `language`. Each
/// test runs on a thread of its own, so this never leaks into another test the
/// way [`set`] would.
#[cfg(test)]
pub(crate) fn with_language<R>(language: ResolvedLanguage, body: impl FnOnce() -> R) -> R {
    let previous = TEST_LANGUAGE.with(|cell| cell.replace(Some(language)));
    let result = body();
    TEST_LANGUAGE.with(|cell| cell.set(previous));
    result
}

/// Whether host messages are worded in English right now.
pub(crate) fn english() -> bool {
    current() == ResolvedLanguage::EnUs
}

/// `zh` or `en`, whichever [`current`] picks.
pub(crate) fn pick<T>(zh: T, en: T) -> T {
    if english() {
        en
    } else {
        zh
    }
}

/// `zh` or `en` for an explicit language, for text worded for a language other
/// than the current one (a card raised for a run, a test of both wordings).
pub(crate) fn pick_for<T>(language: ResolvedLanguage, zh: T, en: T) -> T {
    match language {
        ResolvedLanguage::ZhCn => zh,
        ResolvedLanguage::EnUs => en,
    }
}

/// Formats the Chinese or the English message, whichever [`current`] picks.
/// Both are format strings over the same arguments.
macro_rules! ui_text {
    ($zh:literal, $en:literal $(,)?) => {
        if $crate::ui_text::english() {
            ::std::format!($en)
        } else {
            ::std::format!($zh)
        }
    };
    ($zh:literal, $en:literal, $($argument:tt)+) => {
        if $crate::ui_text::english() {
            ::std::format!($en, $($argument)+)
        } else {
            ::std::format!($zh, $($argument)+)
        }
    };
}
pub(crate) use ui_text;

/// The system's language, resolved as the renderer resolves the `auto`
/// preference: any Chinese-family language is Simplified Chinese, everything
/// else English.
pub(crate) fn system_language() -> ResolvedLanguage {
    static SYSTEM: std::sync::OnceLock<ResolvedLanguage> = std::sync::OnceLock::new();
    *SYSTEM.get_or_init(|| resolve_locale(&system_locale().unwrap_or_default()))
}

/// The renderer's `resolveApplicationLanguage` for `auto`.
fn resolve_locale(locale: &str) -> ResolvedLanguage {
    const CHINESE_SUBTAGS: [&str; 17] = [
        "zh", "cmn", "yue", "wuu", "hak", "nan", "gan", "hsn", "cdo", "cjy", "cpx", "czh", "czo",
        "lzh", "ltc", "mnp", "och",
    ];
    let primary = locale
        .trim()
        .replace('_', "-")
        .split('-')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if CHINESE_SUBTAGS.contains(&primary.as_str()) {
        ResolvedLanguage::ZhCn
    } else {
        ResolvedLanguage::EnUs
    }
}

/// The first preferred language, which is what WebKit reports as
/// `navigator.language`.
#[cfg(target_os = "macos")]
fn system_locale() -> Option<String> {
    let languages = objc2_foundation::NSLocale::preferredLanguages();
    languages.firstObject().map(|language| language.to_string())
}

/// The user's UI language, which is what WebView2 reports as
/// `navigator.language`.
#[cfg(windows)]
fn system_locale() -> Option<String> {
    use windows_sys::Win32::Globalization::{GetUserDefaultUILanguage, LCIDToLocaleName};
    const LOCALE_NAME_MAX_LENGTH: usize = 85;
    let mut name = [0u16; LOCALE_NAME_MAX_LENGTH];
    // SAFETY: the buffer is LOCALE_NAME_MAX_LENGTH wide characters, as declared.
    let written = unsafe {
        LCIDToLocaleName(
            u32::from(GetUserDefaultUILanguage()),
            name.as_mut_ptr(),
            LOCALE_NAME_MAX_LENGTH as i32,
            0,
        )
    };
    (written > 1).then(|| String::from_utf16_lossy(&name[..written as usize - 1]))
}

#[cfg(not(any(target_os = "macos", windows)))]
fn system_locale() -> Option<String> {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chinese_family_locales_resolve_to_chinese_and_everything_else_to_english() {
        for locale in ["zh-Hans-CN", "zh_CN", "zh-Hant-TW", "yue-HK", "zh"] {
            assert_eq!(resolve_locale(locale), ResolvedLanguage::ZhCn, "{locale}");
        }
        for locale in ["en-US", "ja-JP", "fr", "", "C"] {
            assert_eq!(resolve_locale(locale), ResolvedLanguage::EnUs, "{locale}");
        }
    }

    #[test]
    fn a_message_follows_the_language_it_is_worded_in() {
        let name = "x";
        assert_eq!(ui_text!("缺少 {name}", "Missing {name}"), "缺少 x");
        with_language(ResolvedLanguage::EnUs, || {
            assert_eq!(ui_text!("缺少 {name}", "Missing {name}"), "Missing x");
            assert_eq!(ui_text!("缺少 {}", "Missing {}", 3), "Missing 3");
            assert_eq!(pick("中", "en"), "en");
        });
    }

    #[test]
    fn a_message_is_worded_in_the_explicit_language() {
        assert_eq!(pick_for(ResolvedLanguage::EnUs, "中", "en"), "en");
        assert_eq!(pick_for(ResolvedLanguage::ZhCn, "中", "en"), "中");
    }
}
