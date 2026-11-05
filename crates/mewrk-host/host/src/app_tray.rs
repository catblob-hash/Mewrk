//! The tray is the native client's; the host only needs to bring the window back and to know the menu's words.

use std::sync::{Arc, RwLock};

use tauri::AppHandle;

use crate::model::ResolvedLanguage;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TrayLabels {
    pub open_window: &'static str,
    pub quit: &'static str,
}

/// A Mac menu bar extra speaks the Mac's words: the window is the app there,
/// and its quit is the same 退出 as the application menu's.
#[cfg(target_os = "macos")]
pub(crate) fn tray_labels(language: ResolvedLanguage) -> TrayLabels {
    match language {
        ResolvedLanguage::ZhCn => TrayLabels { open_window: "打开 Mewrk", quit: "退出 Mewrk" },
        ResolvedLanguage::EnUs => TrayLabels { open_window: "Open Mewrk", quit: "Quit Mewrk" },
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn tray_labels(language: ResolvedLanguage) -> TrayLabels {
    match language {
        ResolvedLanguage::ZhCn => TrayLabels { open_window: "打开窗口", quit: "关闭 Mewrk" },
        ResolvedLanguage::EnUs => TrayLabels { open_window: "Open window", quit: "Quit Mewrk" },
    }
}

type ShowWindow = Arc<dyn Fn() + Send + Sync>;

static SHOW_WINDOW: RwLock<Option<ShowWindow>> = RwLock::new(None);

pub(crate) fn install_native(show: Option<ShowWindow>) {
    *SHOW_WINDOW.write().unwrap_or_else(|p| p.into_inner()) = show;
}

pub(crate) fn show_main_window(_app: &AppHandle, _main_window_label: &str) {
    if let Some(show) = SHOW_WINDOW.read().unwrap_or_else(|p| p.into_inner()).clone() {
        show();
    }
}

pub(crate) fn apply_language(_app: &AppHandle, _language: ResolvedLanguage) {}
