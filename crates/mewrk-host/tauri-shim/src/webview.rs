//! The page engine's surface. On macOS it is the system's `WKWebView` (`wk`), which answers the host's
//! DevTools calls through an adapter; elsewhere no engine is attached and every call says so.

#[cfg(target_os = "macos")]
#[path = "wk/mod.rs"]
mod engine;

#[cfg(not(target_os = "macos"))]
#[path = "webview_stub.rs"]
mod engine;

pub use engine::*;
