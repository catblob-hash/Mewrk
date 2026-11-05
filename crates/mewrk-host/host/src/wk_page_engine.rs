//! The page engine the host's `feature = "cef"` paths reach, under the name they reach it by.
//!
//! Upstream's `cef_host` module embeds Chromium; its files are kept beside this one and not built. The page lives in
//! the shim (`tauri::webview`, a `WKWebView`), which offers the same handle, so this is only the names.

pub(crate) mod page {
    use tauri::{webview, Position, Size};

    pub(crate) use tauri::webview::{DialogDecision, PageDialog, Webview as CefWebview, WebviewBuilder};
    pub(crate) type CefPageBuilder = WebviewBuilder<tauri::Wry>;

    /// The page with this label, if it is alive. Every page is registered process-wide, so no handle is needed.
    pub(crate) fn get(label: &str) -> Option<CefWebview> {
        webview::live_page(label)
    }

    pub(crate) fn add_child(window: &tauri::Window, builder: CefPageBuilder, position: impl Into<Position>, size: impl Into<Size>) -> Result<CefWebview, String> {
        window.add_child(builder, position, size)
    }
}

/// Nothing outlives the pages: WebKit's processes belong to the system, and end with the app.
pub(crate) fn shutdown() {}

pub(crate) fn begin_exit() {}
