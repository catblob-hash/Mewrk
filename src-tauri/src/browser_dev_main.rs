fn main() {
    // Chromium relaunches this executable for its subprocesses too, exactly as it does `mewrk`.
    #[cfg(target_os = "macos")]
    if let Some(code) = mewrk_lib::cef_subprocess_main() {
        std::process::exit(code);
    }
    #[cfg(target_os = "macos")]
    mewrk_lib::cef_preload_framework();
    std::process::exit(mewrk_lib::run_browser_dev());
}
