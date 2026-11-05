//! Which Claude Code the Claude Agent family runs: the user's own `claude`.
//!
//! Mewrk installs nothing for this family and reads no SDK. The model layer
//! speaks the CLI's stream-json protocol itself, so all a step needs is the
//! path of an executable, and that is looked up here every time it is asked
//! for: installing or updating Claude Code takes effect on the next run, and
//! Re-check on the provider page needs nothing else.
//!
//! # Where it looks
//!
//! 1. [`EXECUTABLE_ENV`], which replaces the search outright.
//! 2. `PATH`. On macOS a process started from Finder has adopted the login
//!    shell's by now (`child_environment::adopt_login_shell_path`).
//! 3. The places installers put it, for when that shell could not be read.
//! 4. The copy inside the Claude desktop app, newest first: a machine can have
//!    that one and no `claude` command.
//!
//! # Versions
//!
//! Nothing pins the CLI any more, so its releases reach this family as the user
//! takes them. What this code depends on is checked against a real CLI by
//! `aisdk-rs/tests/claude_agent.rs`. [`MINIMUM_VERSION`] refuses a CLI older
//! than the first one known to load the plugin that keeps its own context out of
//! the prompt: without that, the model would be told about Mewrk's private
//! session folder as if it were a workspace.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use serde::Serialize;
use wait_timeout::ChildExt as _;

use super::{home_dir, probe_env, with_stderr_tail};
use crate::ui_text::ui_text;

/// A path that replaces the search.
const EXECUTABLE_ENV: &str = "MEWRK_CLAUDE_BIN";
/// See the module's Versions.
const MINIMUM_VERSION: Release = Release(2, 1, 284);
/// How long `--version` may take. It answers from the executable alone, but a
/// large one can take seconds on its first run after an install.
const VERSION_TIMEOUT: Duration = Duration::from_secs(20);

/// The CLI's file names here, in the order a shell would try them.
const fn file_names() -> &'static [&'static str] {
    if cfg!(windows) {
        &["claude.exe", "claude.cmd"]
    } else {
        &["claude"]
    }
}

/// Directories under the home folder where an installer puts the command.
const HOME_DIRS: &[&str] = &[".local/bin", ".claude/local", ".npm-global/bin", ".bun/bin", ".volta/bin"];
/// Directories outside it.
#[cfg(unix)]
const SYSTEM_DIRS: &[&str] = &["/opt/homebrew/bin", "/usr/local/bin"];
#[cfg(not(unix))]
const SYSTEM_DIRS: &[&str] = &[];
/// Where the Claude desktop app keeps its copies, under the home folder:
/// `<version>/<build>/claude.app/Contents/MacOS/claude`.
#[cfg(target_os = "macos")]
const DESKTOP_APP_DIR: &str = "Library/Application Support/Claude/claude-code";

/// A CLI that can run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Cli {
    pub(crate) path: PathBuf,
    /// As `--version` says it; `None` when that was not in a form this reads.
    pub(crate) version: Option<String>,
}

/// What the provider page shows.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeCliStatus {
    /// `None`: no `claude` was found.
    pub path: Option<String>,
    pub version: Option<String>,
    /// Why the one found cannot be used, for people.
    pub problem: Option<String>,
    /// How to install it, for a terminal.
    pub install_command: String,
}

/// `major.minor.patch`, compared in that order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Release(u64, u64, u64);

impl std::fmt::Display for Release {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}.{}.{}", self.0, self.1, self.2)
    }
}

impl Release {
    fn parse(text: &str) -> Option<Self> {
        let mut parts = text.trim().splitn(3, '.');
        let mut number = || -> Option<u64> {
            let part = parts.next()?;
            let digits = part.find(|c: char| !c.is_ascii_digit()).unwrap_or(part.len());
            part[..digits].parse().ok()
        };
        Some(Self(number()?, number()?, number()?))
    }
}

/// The version in what `claude --version` prints, `2.1.309 (Claude Code)`: the
/// first word that reads as one, so a banner before it changes nothing.
fn version_in(output: &str) -> Option<(String, Release)> {
    output.split_whitespace().find_map(|word| Some((word.to_owned(), Release::parse(word)?)))
}

fn is_runnable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// The desktop app's copies under `dir`, newest version first. Its layout is its
/// own to change, and a layout this does not recognise simply finds nothing.
#[cfg(target_os = "macos")]
fn desktop_app_copies(dir: &Path) -> Vec<PathBuf> {
    let mut versions: Vec<(Release, PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| Some((Release::parse(entry.file_name().to_str()?)?, entry.path())))
        .collect();
    versions.sort_by(|left, right| right.0.cmp(&left.0));
    versions
        .into_iter()
        .flat_map(|(_, version)| std::fs::read_dir(version).into_iter().flatten().flatten().map(|build| build.path().join("claude.app/Contents/MacOS/claude")))
        .collect()
}

/// Everywhere to look, in order, as far as it can be told without touching the
/// disk (the desktop app's copies aside: those have to be listed).
fn candidates(path_env: Option<&OsStr>, home: Option<&Path>) -> Vec<PathBuf> {
    let mut directories: Vec<PathBuf> = path_env.map(|path| std::env::split_paths(path).collect()).unwrap_or_default();
    if let Some(home) = home {
        directories.extend(HOME_DIRS.iter().map(|directory| home.join(directory)));
    }
    directories.extend(SYSTEM_DIRS.iter().map(PathBuf::from));
    #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
    let mut found: Vec<PathBuf> = directories.iter().flat_map(|directory| file_names().iter().map(|name| directory.join(name))).collect();
    #[cfg(target_os = "macos")]
    if let Some(home) = home {
        found.extend(desktop_app_copies(&home.join(DESKTOP_APP_DIR)));
    }
    found
}

/// The executable to run, or `None` when there is none: [`EXECUTABLE_ENV`] when
/// it is set, whether or not it exists, and otherwise the first one found.
fn find() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(EXECUTABLE_ENV).filter(|path| !path.is_empty()) {
        return Some(PathBuf::from(path));
    }
    let path_env = std::env::var_os("PATH");
    candidates(path_env.as_deref(), home_dir().as_deref()).into_iter().find(|path| is_runnable(path))
}

/// `--version` answered once per file, so a run does not start the CLI twice:
/// an update replaces the file, and the answer goes with it.
struct Answer {
    modified: Option<SystemTime>,
    length: u64,
    output: Result<String, String>,
}

static ANSWERS: Mutex<BTreeMap<PathBuf, Answer>> = Mutex::new(BTreeMap::new());

fn run_version(path: &Path) -> Result<String, String> {
    let mut command = Command::new(path);
    command.arg("--version").env_clear().envs(probe_env()).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let shown = path.display();
    let mut child = command.spawn().map_err(|error| {
        ui_text!("{shown} 启动不了：{error}", "{shown} would not start: {error}")
    })?;
    match child.wait_timeout(VERSION_TIMEOUT) {
        Ok(Some(_)) => {}
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ui_text!("{shown} 没有回答版本号", "{shown} did not say its version"));
        }
        Err(error) => return Err(ui_text!("等待 {shown} 失败：{error}", "Waiting for {shown} failed: {error}")),
    }
    let output = child.wait_with_output().map_err(|error| {
        ui_text!("读取 {shown} 的输出失败：{error}", "Reading the output of {shown} failed: {error}")
    })?;
    if !output.status.success() {
        let code = output.status.code().map_or_else(|| "?".to_owned(), |code| code.to_string());
        let message = ui_text!("{shown} 运行失败（退出码 {code}）", "{shown} failed (exit code {code})");
        return Err(with_stderr_tail(&message, &output.stderr));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn version_output(path: &Path) -> Result<String, String> {
    let metadata = std::fs::metadata(path).map_err(|error| {
        let shown = path.display();
        ui_text!("{shown} 读不到：{error}", "Could not read {shown}: {error}")
    })?;
    let (modified, length) = (metadata.modified().ok(), metadata.len());
    if let Some(known) = ANSWERS.lock().unwrap_or_else(PoisonError::into_inner).get(path) {
        if known.modified == modified && known.length == length {
            return known.output.clone();
        }
    }
    let output = run_version(path);
    ANSWERS.lock().unwrap_or_else(PoisonError::into_inner).insert(path.to_path_buf(), Answer { modified, length, output: output.clone() });
    output
}

/// Whether the CLI at `path` can be used, from what `--version` said.
fn checked(path: &Path, output: Result<String, String>) -> Result<Cli, (Option<String>, String)> {
    let output = output.map_err(|problem| (None, problem))?;
    let Some((version, release)) = version_in(&output) else {
        // A form this does not read is no reason to refuse a CLI that runs.
        return Ok(Cli { path: path.to_path_buf(), version: None });
    };
    if release < MINIMUM_VERSION {
        let minimum = MINIMUM_VERSION;
        return Err((
            Some(version.clone()),
            ui_text!(
                "Claude Code {version} 太旧了，这个版本的 Mewrk 至少需要 {minimum}：在终端运行 claude update，然后在「设置 → 提供商 → 模型提供商 → Claude Agent」里重新检测。",
                "Claude Code {version} is too old, and this Mewrk needs at least {minimum}: run claude update in a terminal, then Re-check under Settings → Providers → Model providers → Claude Agent."
            ),
        ));
    }
    Ok(Cli { path: path.to_path_buf(), version: Some(version) })
}

/// The Claude Code to run, or what to tell the user. Blocks on `--version` the
/// first time it sees a file.
pub(crate) fn resolve() -> Result<Cli, String> {
    let Some(path) = find() else {
        return Err(not_found());
    };
    checked(&path, version_output(&path)).map_err(|(_, problem)| problem)
}

/// What every Claude Agent action says while there is no `claude`.
fn not_found() -> String {
    ui_text!(
        "没有找到 Claude Code（claude 命令）。安装后在「设置 → 提供商 → 模型提供商 → Claude Agent」里重新检测。",
        "Claude Code (the claude command) was not found. Install it, then Re-check under Settings → Providers → Model providers → Claude Agent."
    )
}

fn install_command() -> &'static str {
    if cfg!(windows) {
        "irm https://claude.ai/install.ps1 | iex"
    } else {
        "curl -fsSL https://claude.ai/install.sh | bash"
    }
}

/// What the provider page shows, looked up afresh.
pub fn status() -> ClaudeCliStatus {
    let install_command = install_command().to_owned();
    let Some(path) = find() else {
        return ClaudeCliStatus { path: None, version: None, problem: None, install_command };
    };
    let shown = Some(path.to_string_lossy().into_owned());
    match checked(&path, version_output(&path)) {
        Ok(cli) => ClaudeCliStatus { path: shown, version: cli.version, problem: None, install_command },
        Err((version, problem)) => ClaudeCliStatus { path: shown, version, problem: Some(problem), install_command },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn a_version_is_read_from_what_the_cli_prints() {
        assert_eq!(version_in("2.1.309 (Claude Code)\n"), Some(("2.1.309".to_owned(), Release(2, 1, 309))));
        assert_eq!(version_in("update available\n2.1.309\n"), Some(("2.1.309".to_owned(), Release(2, 1, 309))));
        assert_eq!(version_in("2.2.0-beta.1 (Claude Code)"), Some(("2.2.0-beta.1".to_owned(), Release(2, 2, 0))));
        assert_eq!(version_in("Claude Code"), None);
        assert_eq!(version_in(""), None);
    }

    #[test]
    fn versions_compare_as_numbers() {
        assert!(Release(2, 1, 284) < Release(2, 1, 309));
        assert!(Release(2, 1, 284) > Release(2, 1, 99));
        assert!(Release(3, 0, 0) > Release(2, 99, 99));
    }

    /// `PATH` wins, then the installers' places, and the desktop app comes last,
    /// newest first.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_search_goes_from_the_users_path_to_the_desktop_app() {
        let home = tempfile::tempdir().unwrap();
        for version in ["2.1.99", "2.1.309", "not-a-version"] {
            std::fs::create_dir_all(home.path().join(DESKTOP_APP_DIR).join(version).join("build")).unwrap();
        }
        let path = std::env::join_paths(["/first", "/second"]).unwrap();
        let found = candidates(Some(&path), Some(home.path()));
        let position = |wanted: &Path| found.iter().position(|path| path == wanted).unwrap_or_else(|| panic!("{wanted:?} is not in {found:?}"));
        let desktop = |version: &str| home.path().join(DESKTOP_APP_DIR).join(version).join("build/claude.app/Contents/MacOS/claude");
        assert_eq!(found[0], Path::new("/first/claude"));
        assert_eq!(found[1], Path::new("/second/claude"));
        assert!(position(&home.path().join(".local/bin/claude")) < position(Path::new("/opt/homebrew/bin/claude")));
        assert!(position(Path::new("/opt/homebrew/bin/claude")) < position(&desktop("2.1.309")));
        assert!(position(&desktop("2.1.309")) < position(&desktop("2.1.99")));
        assert!(!found.iter().any(|path| path.to_string_lossy().contains("not-a-version")));
    }

    #[test]
    fn nothing_found_is_said_to_be_nothing_found() {
        assert!(not_found().contains("claude"));
        assert!(!install_command().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_file_that_cannot_run_is_not_a_cli() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("claude");
        std::fs::write(&plain, "text").unwrap();
        assert!(!is_runnable(&plain), "no execute bit");
        assert!(!is_runnable(dir.path()), "a directory");
        assert!(!is_runnable(&dir.path().join("gone")));
        script(&plain, "exit 0");
        assert!(is_runnable(&plain));
    }

    #[cfg(unix)]
    #[test]
    fn a_cli_older_than_the_minimum_is_refused_and_a_newer_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("claude");
        script(&path, "echo '2.1.200 (Claude Code)'");
        let (version, problem) = checked(&path, version_output(&path)).unwrap_err();
        assert_eq!(version.as_deref(), Some("2.1.200"));
        assert!(problem.contains("claude update") && problem.contains("2.1.284"), "{problem}");

        let newer = dir.path().join("newer").join("claude");
        script(&newer, "echo '9.0.0 (Claude Code)'");
        assert_eq!(checked(&newer, version_output(&newer)), Ok(Cli { path: newer.clone(), version: Some("9.0.0".to_owned()) }));

        let odd = dir.path().join("odd").join("claude");
        script(&odd, "echo 'Claude Code'");
        assert_eq!(checked(&odd, version_output(&odd)), Ok(Cli { path: odd.clone(), version: None }));
    }

    #[cfg(unix)]
    #[test]
    fn a_cli_that_fails_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("claude");
        script(&path, "echo 'no such library' >&2; exit 3");
        let (version, problem) = checked(&path, version_output(&path)).unwrap_err();
        assert_eq!(version, None);
        assert!(problem.contains("no such library"), "{problem}");
    }

    /// The answer belongs to the file: replacing it, as an update does, asks again.
    #[cfg(unix)]
    #[test]
    fn the_version_is_asked_again_when_the_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("claude");
        let count = dir.path().join("count");
        let body = |version: &str| format!("echo x >> '{}'; echo '{version} (Claude Code)'", count.display());
        script(&path, &body("2.1.300"));
        assert!(version_output(&path).unwrap().starts_with("2.1.300"));
        assert!(version_output(&path).unwrap().starts_with("2.1.300"));
        assert_eq!(std::fs::read_to_string(&count).unwrap().lines().count(), 1, "asked once");
        script(&path, &format!("{} # a longer file", body("2.1.301")));
        assert!(version_output(&path).unwrap().starts_with("2.1.301"));
        assert_eq!(std::fs::read_to_string(&count).unwrap().lines().count(), 2);
    }
}
