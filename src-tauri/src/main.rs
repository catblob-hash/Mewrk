#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // `ssh` runs this executable to ask the user something (`SSH_ASKPASS`): answer and leave
    // before anything of the application loads.
    if let Some(code) = mewrk_lib::askpass_main() {
        std::process::exit(code);
    }
    // A development build has no helper bundle, so Chromium relaunches this executable for its
    // renderer, GPU and utility processes; they must never reach the application.
    #[cfg(target_os = "macos")]
    if let Some(code) = mewrk_lib::cef_subprocess_main() {
        std::process::exit(code);
    }
    #[cfg(target_os = "macos")]
    mewrk_lib::cef_preload_framework();
    mewrk_lib::run();
}
