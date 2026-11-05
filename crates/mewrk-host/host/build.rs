//! The facts the host reads from its build: the target triple, and on
//! macOS that pages have an engine with a DevTools-shaped handle, which `browser.rs` knows as `feature = "cef"`.

use std::env;

fn main() {
    let target = env::var("TARGET").expect("TARGET is set by Cargo for build scripts");
    println!("cargo:rustc-env=MEWRK_TARGET_TRIPLE={target}");

    // Upstream's `cef` feature turns on every page path in `browser.rs` behind one handle: Chromium's with the `chromium`
    // feature, the shim's WKWebView page without it. Either way it is simply on for the platform it exists on, and the
    // host's gated code stays what upstream wrote.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-cfg=feature=\"cef\"");
    }
    // The distribution cef-dll-sys fetched and compiled this build against, which a development run loads the framework
    // from. A bundle carries its own copy in Contents/Frameworks.
    if let Ok(directory) = env::var("DEP_CEF_DLL_WRAPPER_CEF_DIR") {
        println!("cargo:rustc-env=MEWRK_CEF_DIR={directory}");
    }
}
