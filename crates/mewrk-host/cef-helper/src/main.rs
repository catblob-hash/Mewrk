//! `Mewrk Helper`: the executable a bundled Mewrk launches Chromium's renderer, GPU and utility processes from on macOS,
//! copied into the `Mewrk Helper*.app` bundles next to the framework (`scripts/bundle-macos.sh`). A development build uses
//! the application's own executable instead. Nothing else of Mewrk runs here, which is why it is a program of its own:
//! a helper can't start the application, and five copies of it cost next to nothing.

#[cfg(target_os = "macos")]
fn main() {
    std::process::exit(helper::run());
}

#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
mod helper {
    use std::{ffi::CString, os::unix::ffi::OsStrExt, path::PathBuf};

    use cef::*;

    const FRAMEWORK_DIRECTORY: &str = "Chromium Embedded Framework.framework";
    const FRAMEWORK_BINARY: &str = "Chromium Embedded Framework";
    /// What the browser process adds to every helper's command line, naming the framework it loaded.
    const FRAMEWORK_SWITCH: &str = "mewrk-cef-framework";

    pub fn run() -> i32 {
        let arguments = std::env::args().collect::<Vec<_>>();
        // The sandbox goes up before anything loads, unless the browser process launched this without one.
        let _sandbox = (!arguments.iter().any(|argument| argument == "--no-sandbox")).then(|| {
            let args = args::Args::new();
            let mut sandbox = cef::sandbox::Sandbox::new();
            sandbox.initialize(args.as_main_args());
            sandbox
        });
        let prefix = format!("--{FRAMEWORK_SWITCH}=");
        let framework = arguments.iter().find_map(|argument| argument.strip_prefix(&prefix)).map(PathBuf::from).or_else(beside_the_bundle);
        let Some(framework) = framework else {
            eprintln!("Mewrk helper: Chromium Embedded Framework not found");
            return 1;
        };
        let Ok(binary) = CString::new(framework.join(FRAMEWORK_BINARY).as_os_str().as_bytes()) else {
            eprintln!("Mewrk helper: the framework's path is not usable");
            return 1;
        };
        // SAFETY: a NUL-terminated path that outlives the call.
        if load_library(Some(unsafe { &*binary.as_ptr() })) != 1 {
            eprintln!("Mewrk helper: could not load {}", framework.display());
            return 1;
        }
        let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
        let args = args::Args::new();
        execute_process(Some(args.as_main_args()), None::<&mut App>, std::ptr::null_mut()).max(0)
    }

    /// Mewrk.app/Contents/Frameworks/Mewrk Helper.app/Contents/MacOS/<helper>, with the framework in Frameworks.
    fn beside_the_bundle() -> Option<PathBuf> {
        let executable = std::env::current_exe().ok()?;
        let framework = executable.ancestors().nth(4)?.join(FRAMEWORK_DIRECTORY);
        framework.join(FRAMEWORK_BINARY).is_file().then_some(framework)
    }
}
