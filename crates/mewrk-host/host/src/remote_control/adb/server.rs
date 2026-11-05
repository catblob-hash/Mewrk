//! Starting the adb server on this computer, for the one case that needs it:
//! nothing is listening on its port.
//!
//! The caller has already established that — a connect to the port was
//! refused — and that the place is this computer. Running `adb` while a server
//! answers is never done: an `adb` client of a different version than the
//! running server kills that server and starts its own, taking down whatever
//! the user (or their IDE) had connected through it.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

/// How long `adb start-server` may take. The daemon forks off and the client
/// returns as soon as it answers; a first start on Windows can take seconds.
const START_TIMEOUT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(50);
/// The port `adb` uses without `-P`.
const DEFAULT_PORT: u16 = 5037;

const NOT_FOUND: &str = "adb was not found on PATH or in the Android SDK's platform-tools folder. Install the Android platform-tools and put adb on PATH, or run `adb start-server` yourself and try again.";

/// The program name `adb` has on this OS.
fn program_name() -> &'static str {
    if cfg!(windows) {
        "adb.exe"
    } else {
        "adb"
    }
}

/// Where an `adb` may be, best first: the PATH, then the Android SDK as the
/// environment names it, then where Android Studio puts the SDK by default.
fn candidate_directories(env: &dyn Fn(&str) -> Option<OsString>) -> Vec<PathBuf> {
    let mut directories: Vec<PathBuf> = Vec::new();
    let non_empty = |name: &str| env(name).filter(|value| !value.is_empty());
    if let Some(path) = non_empty("PATH") {
        directories
            .extend(std::env::split_paths(&path).filter(|entry| !entry.as_os_str().is_empty()));
    }
    for sdk in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        if let Some(root) = non_empty(sdk) {
            directories.push(PathBuf::from(root).join("platform-tools"));
        }
    }
    if cfg!(windows) {
        if let Some(local) = non_empty("LOCALAPPDATA") {
            directories.push(PathBuf::from(local).join("Android/Sdk/platform-tools"));
        }
    } else if let Some(home) = non_empty("HOME") {
        let home = PathBuf::from(home);
        if cfg!(target_os = "macos") {
            directories.push(home.join("Library/Android/sdk/platform-tools"));
        } else {
            directories.push(home.join("Android/Sdk/platform-tools"));
        }
    }
    directories
}

fn locate(
    env: &dyn Fn(&str) -> Option<OsString>,
    is_file: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    candidate_directories(env)
        .into_iter()
        .map(|directory| directory.join(program_name()))
        .find(|candidate| is_file(candidate))
}

/// The arguments that start a server on `port`. The default port needs none;
/// any other is passed so that the server comes up where the entry looks.
fn start_arguments(port: u16) -> Vec<String> {
    let mut arguments = Vec::new();
    if port != DEFAULT_PORT {
        arguments.push("-P".to_owned());
        arguments.push(port.to_string());
    }
    arguments.push("start-server".to_owned());
    arguments
}

/// Runs `adb start-server` (on `port`, if it is not adb's own). Output is
/// discarded: the daemon it forks inherits the pipes, and a reader waiting for
/// them to close would wait for as long as the daemon lives.
pub(super) fn start_server(port: u16) -> Result<(), String> {
    let adb = locate(&|name| std::env::var_os(name), &|path| path.is_file())
        .ok_or_else(|| NOT_FOUND.to_owned())?;
    let mut command = Command::new(&adb);
    command
        .args(start_arguments(port))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("could not run {}: {error}", adb.display()))?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                return Err(format!(
                    "`adb start-server` failed ({status}); run it in a terminal to see why"
                ))
            }
            Ok(None) if started.elapsed() >= START_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "`adb start-server` did not finish within {} seconds",
                    START_TIMEOUT.as_secs()
                ));
            }
            Ok(None) => thread::sleep(POLL),
            Err(error) => return Err(format!("could not wait for adb: {error}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn environment(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, OsString> = pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), OsString::from(value)))
            .collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn adb_is_looked_for_on_the_path_first_then_in_the_sdk() {
        let separator = if cfg!(windows) { ";" } else { ":" };
        let path = format!("{}{separator}{}", "/first/bin", "/second/bin");
        let env = environment(&[
            ("PATH", &path),
            ("ANDROID_HOME", "/sdk-home"),
            ("ANDROID_SDK_ROOT", "/sdk-root"),
        ]);
        let directories = candidate_directories(&env);
        assert_eq!(directories[0], PathBuf::from("/first/bin"));
        assert_eq!(directories[1], PathBuf::from("/second/bin"));
        assert_eq!(directories[2], PathBuf::from("/sdk-home/platform-tools"));
        assert_eq!(directories[3], PathBuf::from("/sdk-root/platform-tools"));

        let name = program_name();
        let wanted = PathBuf::from("/sdk-root/platform-tools").join(name);
        let found = locate(&env, &|candidate| candidate == wanted);
        assert_eq!(found, Some(wanted));
        let on_path = PathBuf::from("/second/bin").join(name);
        let also_in_sdk = PathBuf::from("/sdk-home/platform-tools").join(name);
        assert_eq!(
            locate(&env, &|candidate| candidate == on_path
                || candidate == also_in_sdk),
            Some(on_path)
        );
        assert_eq!(locate(&env, &|_| false), None);
    }

    #[test]
    fn the_default_sdk_location_depends_on_the_os() {
        let env = if cfg!(windows) {
            environment(&[("LOCALAPPDATA", "C:/Users/me/AppData/Local")])
        } else {
            environment(&[("HOME", "/home/me")])
        };
        let directories = candidate_directories(&env);
        let expected = if cfg!(windows) {
            PathBuf::from("C:/Users/me/AppData/Local/Android/Sdk/platform-tools")
        } else if cfg!(target_os = "macos") {
            PathBuf::from("/home/me/Library/Android/sdk/platform-tools")
        } else {
            PathBuf::from("/home/me/Android/Sdk/platform-tools")
        };
        assert_eq!(directories, vec![expected]);
        // Nothing in the environment, nothing to look in; empty entries are not the current directory.
        assert!(candidate_directories(&environment(&[("PATH", "")])).is_empty());
    }

    #[test]
    fn a_server_on_another_port_is_started_there() {
        assert_eq!(start_arguments(5037), vec!["start-server"]);
        assert_eq!(start_arguments(5038), vec!["-P", "5038", "start-server"]);
    }
}
