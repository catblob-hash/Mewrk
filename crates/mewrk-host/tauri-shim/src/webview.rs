//! The page engine's surface. On macOS it is the system's `WKWebView` (`wk`), which answers the host's DevTools calls
//! through an adapter. On Windows it is WebView2 (`webview_win`), whose controllers the host drives itself. Elsewhere no
//! engine is attached and every call says so.

#[cfg(target_os = "macos")]
#[path = "wk/mod.rs"]
mod engine;

#[cfg(windows)]
#[path = "webview_win.rs"]
mod engine;

#[cfg(not(any(target_os = "macos", windows)))]
#[path = "webview_stub.rs"]
mod engine;

pub use engine::*;
