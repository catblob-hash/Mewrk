//! `Mewrk Helper`: the executable a bundled Mewrk launches Chromium's renderer, GPU and utility
//! processes from on macOS. It is copied into the `Mewrk Helper*.app` bundles next to the
//! framework (scripts/stage-macos-cef.mjs); development builds use the application executable
//! instead. Nothing else of Mewrk runs here.

#[cfg(target_os = "macos")]
fn main() {
    std::process::exit(mewrk_lib::cef_helper_main());
}

#[cfg(not(target_os = "macos"))]
fn main() {}
