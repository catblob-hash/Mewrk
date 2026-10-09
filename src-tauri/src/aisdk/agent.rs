//! Host side of the `claude-agent` family: which profile variables the CLI
//! needs, the login, and the per-run session lease.
//!
//! The sidecar owns the CLI process and its parked tool handlers; the host only
//! names the session, resolves the CLI and the SDK, and guarantees a `release`
//! frame when the run ends, whichever way it ends. Questions that run no model
//! turn — the login probe here and the model picker in [`models`] — the host
//! asks the CLI itself, without the sidecar.
//!
//! The CLI and the SDK are the Claude Agent components the user installs on the
//! provider page ([`claude_agent::runtime`]): the SDK and the CLI out of its own
//! platform package, one npm release, so the two cannot disagree. The user's
//! own Claude Code install is deliberately not consulted. It drifts with their
//! updates, and CLI releases change behaviour this family depends on — 2.1.278,
//! for one, dropped the switch that kept the CLI from attaching an
//! `# Environment` block of its own, and the replacement (a hooks module that
//! leaves the block out) needs a CLI that loads function hooks. The login is
//! the user's: it stays their own `claude auth login` in `~/.claude`, which the
//! installed CLI reads like any other, and this family has no credential of its
//! own.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use wait_timeout::ChildExt as _;

use crate::components::claude_agent;
use crate::model::{ApiProvider, ModelCapability, ModelProfile, ProviderFamily};
use crate::ui_text::{self, ui_text};

use super::protocol::AgentSession;

mod models;

pub(crate) use models::{list_models, AgentModel};

/// Directory under the app data root that serves as the CLI's working
/// directory. Claude Code keys its transcript folder by cwd, so a fixed private
/// directory keeps Mewrk sessions out of the user's own project folders.
const SESSION_DIR: &str = "claude-agent";

/// Profile-location variables the CLI needs to find its configuration and
/// login (`~/.claude`). The sidecar spawns with a cleared environment and the
/// SDK replaces the CLI environment wholesale, so these must be carried
/// explicitly. Credentials are deliberately absent: this family has none — the
/// CLI authenticates with the user's own `claude auth login`. Process-level
/// variables (`PATH`, the temp directories, the locale) are not repeated here:
/// the sidecar copies its own inherited environment (`process::INHERITED_ENV`)
/// into the CLI's, so they arrive by that route.
const CLAUDE_AGENT_ENV: &[&str] = &[
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "APPDATA",
    "LOCALAPPDATA",
    "ProgramData",
    "HOME",
    // macOS keeps the login in the Keychain under the account `$USER`. Without
    // the variable the installed CLI looks up a different account, and a
    // signed-in user reads as "Not logged in".
    "USER",
    "XDG_CONFIG_HOME",
    // Honors a user who relocated their Claude Code configuration.
    "CLAUDE_CONFIG_DIR",
];

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

/// The installed Claude Code ([`claude_agent::runtime`]), or why there is none.
fn installed_cli() -> Result<PathBuf, String> {
    claude_agent::runtime().map(|runtime| runtime.cli)
}

/// Working directory for the CLI. Created on demand because the CLI refuses a
/// missing cwd; an empty app data path (unit tests, ad-hoc requests) falls back
/// to a temp directory rather than the process cwd, which could be a user repo.
pub(crate) fn session_cwd(app_data_path: &str) -> Result<PathBuf, String> {
    let root = if app_data_path.trim().is_empty() {
        std::env::temp_dir().join("mewrk").join(SESSION_DIR)
    } else {
        Path::new(app_data_path.trim()).join(SESSION_DIR)
    };
    std::fs::create_dir_all(&root).map_err(|error| {
        let path = root.display();
        ui_text!(
            "无法创建 Claude Agent 工作目录 {path}: {error}",
            "Could not create Claude Agent's working folder {path}: {error}"
        )
    })?;
    Ok(root)
}

fn profile_env() -> BTreeMap<String, String> {
    CLAUDE_AGENT_ENV
        .iter()
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .map(|value| ((*name).to_owned(), value))
        })
        .collect()
}

/// Overrides that point the CLI at a fake upstream on this machine, read only
/// in the `browser-dev` build. An end-to-end test needs the CLI to talk to a
/// local test double instead of Anthropic, and `agent.env` is the sole channel
/// the sidecar accepts for that; the shipped build has no such channel at all,
/// so nothing outside [`CLAUDE_AGENT_ENV`] can reach the CLI environment.
///
/// The address must be loopback. A remote one would hand the user's own Claude
/// Code login to whoever runs that host, which is exactly what this family's
/// "no credential of our own" position exists to prevent.
#[cfg(feature = "browser-dev")]
fn test_upstream_env() -> Result<BTreeMap<String, String>, String> {
    let mut overrides = BTreeMap::new();
    let Some(base_url) = std::env::var("MEWRK_CLAUDE_AGENT_BASE_URL")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    else {
        return Ok(overrides);
    };
    let url = crate::http_util::normalized_base_url(&base_url)?;
    if !url.host().is_some_and(|host| match host {
        url::Host::Domain(domain) => {
            let domain = domain.trim_end_matches('.').to_ascii_lowercase();
            domain == "localhost" || domain.ends_with(".localhost")
        }
        url::Host::Ipv4(address) => address.is_loopback(),
        url::Host::Ipv6(address) => address.is_loopback(),
    }) {
        return Err(ui_text!(
            "MEWRK_CLAUDE_AGENT_BASE_URL 只允许本机测试桩地址（当前为 {base_url}）",
            "MEWRK_CLAUDE_AGENT_BASE_URL may only name a local test server (it is {base_url})"
        ));
    }
    overrides.insert("ANTHROPIC_BASE_URL".to_owned(), url.to_string());
    if let Some(key) = std::env::var("MEWRK_CLAUDE_AGENT_API_KEY")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    {
        overrides.insert("ANTHROPIC_API_KEY".to_owned(), key);
    }
    Ok(overrides)
}

/// Mint a session block for one run or probe. Only the `ClaudeAgent` family
/// gets one; every other family returns `None` so callers can attach it
/// unconditionally. The CLI and the SDK are the installed components; a run
/// without them fails here, before the sidecar is asked, with the message that
/// says where to install them.
pub(crate) fn session_for(
    provider: &ApiProvider,
    app_data_path: &str,
) -> Result<Option<AgentSession>, String> {
    if provider.family != ProviderFamily::ClaudeAgent {
        return Ok(None);
    }
    let runtime = claude_agent::runtime()?;
    let cwd = session_cwd(app_data_path)?;
    #[cfg_attr(not(feature = "browser-dev"), allow(unused_mut))]
    let mut env = profile_env();
    #[cfg(feature = "browser-dev")]
    env.extend(test_upstream_env()?);
    Ok(Some(AgentSession {
        session: Uuid::new_v4().simple().to_string(),
        executable: runtime.cli.to_string_lossy().into_owned(),
        sdk: runtime.sdk_entry.to_string_lossy().into_owned(),
        cwd: cwd.to_string_lossy().into_owned(),
        env,
        // Per step: `SessionLease::session` sets it for the step's model.
        tool_changes: false,
    }))
}

/// Extra variables the login probe needs on top of [`CLAUDE_AGENT_ENV`]. The
/// probe runs with a cleared environment so nothing the user happens to export
/// — an `ANTHROPIC_API_KEY` above all — can change which account the CLI
/// reports; these are what remains necessary for a process to start at all.
const PROBE_ENV: &[&str] = &[
    // Windows: `SYSTEMROOT` must survive or the CLI cannot open a socket, and
    // `ComSpec` is what `cmd`-based launches resolve through. Both spellings are
    // listed because a cleared environment is matched case-sensitively here.
    "SYSTEMROOT",
    "SystemRoot",
    "ComSpec",
    "PATH",
    "TEMP",
    "TMP",
    // The POSIX spelling. On macOS it names a per-user private directory under
    // /var/folders; without it the CLI falls back to the shared, world-writable
    // /tmp for whatever it stages while answering.
    "TMPDIR",
];

/// Values pinned for the probe: it must answer from local state only, and must
/// not be the thing that triggers an auto-update or a telemetry upload.
const PROBE_CONTROL_ENV: &[(&str, &str)] = &[
    ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
    ("DISABLE_TELEMETRY", "1"),
    ("DISABLE_AUTOUPDATER", "1"),
];

/// How long the login probe may take before it is killed. `auth status` answers
/// from local state in well under a second; a hang means the CLI is waiting on
/// something the probe must not wait on.
const LOGIN_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Characters of stderr kept in a probe failure message.
const MAX_STDERR_TAIL: usize = 400;

/// Environment for the login probe. Never contains a credential: no `ANTHROPIC_*`
/// and no `CLAUDE_CODE_OAUTH_TOKEN`, so the answer describes the user's own CLI
/// login and nothing Mewrk supplied.
fn probe_env() -> BTreeMap<String, String> {
    let mut env = profile_env();
    for name in PROBE_ENV {
        if let Ok(value) = std::env::var(name) {
            env.insert((*name).to_owned(), value);
        }
    }
    for (name, value) in PROBE_CONTROL_ENV {
        env.insert((*name).to_owned(), (*value).to_owned());
    }
    env
}

/// Where the CLI keeps its configuration and login, as the probe environment
/// makes it resolve: an explicit `CLAUDE_CONFIG_DIR`, else `~/.claude`.
fn config_dir() -> String {
    std::env::var("CLAUDE_CONFIG_DIR")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .or_else(|| home_dir().map(|home| home.join(".claude").to_string_lossy().into_owned()))
        .unwrap_or_default()
}

/// `claude auth status --json`, as far as the host reads it. Unknown fields are
/// ignored so a CLI release that adds one keeps working.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuthStatus {
    #[serde(default)]
    logged_in: bool,
    #[serde(default)]
    auth_method: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    org_name: Option<String>,
    #[serde(default)]
    subscription_type: Option<String>,
}

/// Login state of the Claude Code CLI, as shown in provider settings. The
/// executable is the installed component; the login it reports is the user's.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeAgentLoginStatus {
    pub signed_in: bool,
    /// `claude.ai`, `console`, `none`, or whatever a newer CLI reports — passed
    /// through rather than mapped, so an unknown method is visible instead of
    /// being flattened into "not signed in".
    pub auth_method: String,
    pub email: Option<String>,
    pub org_name: Option<String>,
    pub subscription_type: Option<String>,
    pub executable: String,
    pub config_dir: String,
    /// The sign-in as a command for a terminal: the installed CLI by its
    /// absolute path, quoted for this platform's shell (`login_command`). It is
    /// what Copy command copies, so a user who never installed Claude Code can
    /// paste it anywhere.
    pub login_command: String,
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// Parse `auth status --json` output.
///
/// The JSON object is extracted rather than parsed from the whole of stdout: a
/// CLI release that prints a migration notice or an update banner first would
/// otherwise turn a perfectly good answer into "not signed in".
fn parse_auth_status(stdout: &str) -> Result<AuthStatus, String> {
    let trimmed = stdout.trim();
    let object = match (trimmed.find('{'), trimmed.rfind('}')) {
        (Some(start), Some(end)) if start < end => &trimmed[start..=end],
        _ => {
            return Err(ui_text!(
                "Claude Code 的登录状态输出不是 JSON",
                "Claude Code's sign-in status is not JSON"
            ))
        }
    };
    serde_json::from_str(object).map_err(|error| {
        ui_text!(
            "无法解析 Claude Code 的登录状态输出：{error}",
            "Could not read Claude Code's sign-in status: {error}"
        )
    })
}

/// Read the login state the CLI keeps on this machine.
///
/// The probe runs the installed CLI, but the state it reads is the user's own:
/// the login lives in `~/.claude`, written by their `claude auth login`, and
/// nothing about which executable asks changes whose account answers. With no
/// CLI installed this fails with [`claude_agent::not_installed`].
///
/// `--setting-sources ""` keeps project and enterprise settings files out of the
/// answer, and must precede the subcommand: the CLI rejects it as an unknown
/// option afterwards.
pub(crate) fn login_status() -> Result<ClaudeAgentLoginStatus, String> {
    let executable = installed_cli()?;
    let mut command = Command::new(&executable);
    command
        .args(["--setting-sources", "", "auth", "status", "--json"])
        .env_clear()
        .envs(probe_env())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // Without this the probe flashes a console window on every settings visit.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command.spawn().map_err(|error| {
        ui_text!(
            "无法启动 Claude Code 读取登录状态：{error}",
            "Could not start Claude Code to read its sign-in status: {error}"
        )
    })?;
    let status = child.wait_timeout(LOGIN_PROBE_TIMEOUT).map_err(|error| {
        ui_text!(
            "等待 Claude Code 登录状态失败：{error}",
            "Waiting for Claude Code's sign-in status failed: {error}"
        )
    })?;
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
        return Err(ui_text!(
            "读取 Claude Code 登录状态超时",
            "Reading Claude Code's sign-in status timed out"
        ));
    }
    let output = child.wait_with_output().map_err(|error| {
        ui_text!(
            "读取 Claude Code 登录状态失败：{error}",
            "Reading Claude Code's sign-in status failed: {error}"
        )
    })?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    // The answer, not the exit code, is the judge. A logged-out CLI still prints
    // a complete `--json` object, and some releases pair it with a non-zero exit;
    // reading the code first would make "not signed in" indistinguishable from
    // "the probe broke" and hide the only state the user can act on.
    let parsed = match parse_auth_status(&stdout) {
        Ok(parsed) => parsed,
        Err(error) => {
            let error = match output.status.code() {
                Some(code) if code != 0 => {
                    ui_text!("{error}（退出码 {code}）", "{error} (exit code {code})")
                }
                _ => error,
            };
            return Err(with_stderr_tail(&error, &output.stderr));
        }
    };
    Ok(ClaudeAgentLoginStatus {
        signed_in: parsed.logged_in,
        auth_method: non_empty(parsed.auth_method).unwrap_or_else(|| "none".to_owned()),
        email: non_empty(parsed.email),
        org_name: non_empty(parsed.org_name),
        subscription_type: non_empty(parsed.subscription_type),
        login_command: login_command(&executable),
        executable: executable.to_string_lossy().into_owned(),
        config_dir: config_dir(),
    })
}

/// [`login_status`], checked against the server: a claude.ai login is renewed
/// if it is about to lapse ([`models::refresh_login`]), and one the server no
/// longer accepts reads as signed out — which `auth status` alone, answering
/// from the stored login, never says. A check that could not reach the server
/// leaves the stored answer as it was.
pub(crate) fn checked_login_status() -> Result<ClaudeAgentLoginStatus, String> {
    let status = login_status()?;
    if !status.signed_in || status.auth_method != CLAUDE_AI_AUTH_METHOD {
        return Ok(status);
    }
    match models::refresh_login() {
        Ok(true) => Ok(status),
        // A login the server refused has been cleared from the CLI's store by
        // now, so asking again says so; a network failure changes nothing.
        Ok(false) => login_status(),
        Err(error) => {
            eprintln!("[claude-agent] checking the sign-in with the server failed: {error}");
            Ok(status)
        }
    }
}

/// The `authMethod` of a Claude subscription login: the OAuth one, whose access
/// token expires and must be renewed.
const CLAUDE_AI_AUTH_METHOD: &str = "claude.ai";

/// Renews the user's Claude subscription login if it is about to lapse, so a
/// Mewrk left alone does not find it expired. Nothing happens for a login of
/// another kind or none, nor while the Claude Agent components are not
/// installed: there is no CLI to ask, and nothing to tell anyone about. Returns
/// whether a renewable login was there to keep.
pub(crate) fn keep_login_alive() -> Result<bool, String> {
    if !claude_agent::is_installed() {
        return Ok(false);
    }
    let status = login_status()?;
    if !status.signed_in || status.auth_method != CLAUDE_AI_AUTH_METHOD {
        return Ok(false);
    }
    models::refresh_login()?;
    Ok(true)
}

/// The installed CLI's sign-in as one line a person can paste into a terminal.
///
/// Mewrk never assumes a `claude` on `PATH`: the executable it drives is the
/// one it installed, so the command names that one by absolute path. The login
/// it writes is the user's ordinary Claude Code login, which is also why
/// signing in from a Claude Code of the user's own works just as well.
///
/// On macOS and Linux the path is single-quoted, which bash, zsh and fish all
/// read the same way. On Windows a path that needs no quoting is written bare,
/// which `cmd` and PowerShell both run; one that does (a space, as under
/// `Program Files`) is written for PowerShell, the shell a Windows terminal
/// opens by default, where a quoted path only runs behind the call operator.
fn login_command(executable: &Path) -> String {
    let path = executable.to_string_lossy();
    #[cfg(windows)]
    {
        windows_login_command(&path)
    }
    #[cfg(not(windows))]
    {
        format!("{} auth login", shell_single_quote(&path))
    }
}

/// The Windows half of [`login_command`], separate so every platform's tests
/// can pin it.
#[cfg_attr(not(windows), allow(dead_code))]
fn windows_login_command(path: &str) -> String {
    let bare = !path.is_empty()
        && path
            .chars()
            .all(|character| character.is_alphanumeric() || "\\:._-".contains(character));
    if bare {
        format!("{path} auth login")
    } else {
        format!("& '{}' auth login", path.replace('\'', "''"))
    }
}

/// Append what the CLI wrote to stderr, redacted and clipped. The tail is the
/// only thing that distinguishes "no such subcommand" from "config unreadable".
fn with_stderr_tail(message: &str, stderr: &[u8]) -> String {
    let tail = String::from_utf8_lossy(stderr);
    let tail = crate::http_util::redact_inline_encoded_data(tail.trim());
    if tail.is_empty() {
        return message.to_owned();
    }
    let clipped = tail
        .char_indices()
        .rev()
        .nth(MAX_STDERR_TAIL)
        .map_or(tail.as_str(), |(index, _)| &tail[index..]);
    ui_text!("{message}（{clipped}）", "{message} ({clipped})")
}

/// Authentication channels removed from the interactive login's environment.
/// The login must produce the same credential the runtime later relies on, and
/// the runtime never sees these; a key or endpoint exported in the user's shell
/// would otherwise make `auth login` look already authenticated or send the
/// exchange somewhere else.
const LOGIN_STRIPPED_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_CUSTOM_HEADERS",
    "CLAUDE_CODE_OAUTH_TOKEN",
];

/// How long a freshly started terminal or CLI gets to fail. A launcher that
/// exits non-zero inside this window never showed a window at all; one that is
/// still running, or handed off to a terminal server and exited zero, did.
#[cfg(any(windows, all(unix, not(target_os = "macos"))))]
const LOGIN_LAUNCH_GRACE: Duration = Duration::from_millis(600);

/// How long `open` may take to hand the login script to Terminal. It exits as
/// soon as LaunchServices has delivered the document, launching Terminal first
/// if needed, which is seconds even on a cold start; past this it is stuck.
#[cfg(target_os = "macos")]
const LOGIN_OPEN_TIMEOUT: Duration = Duration::from_secs(15);

/// Environment for the interactive login. Unlike the probe this is the user's
/// own environment: a terminal needs the display, session bus and locale
/// variables the whitelist has no business enumerating, and the login must land
/// in the same `CLAUDE_CONFIG_DIR` the probe and the runtime read. Only the
/// authentication overrides are removed.
fn login_env() -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = std::env::vars().collect();
    for name in LOGIN_STRIPPED_ENV {
        env.remove(*name);
    }
    for (name, value) in PROBE_CONTROL_ENV {
        env.insert((*name).to_owned(), (*value).to_owned());
    }
    env
}

/// Waits out the launch grace window and reports a launcher that already died.
/// `Ok(true)` means the process is still alive or exited cleanly (a terminal
/// server hand-off); `Ok(false)` means it failed before it could show anything.
#[cfg(any(windows, all(unix, not(target_os = "macos"))))]
fn launched(child: &mut std::process::Child) -> Result<bool, String> {
    std::thread::sleep(LOGIN_LAUNCH_GRACE);
    match child.try_wait() {
        Ok(None) => Ok(true),
        Ok(Some(status)) => Ok(status.success()),
        Err(error) => Err(login_terminal_unconfirmed(error)),
    }
}

fn login_terminal_unconfirmed(error: impl std::fmt::Display) -> String {
    ui_text!(
        "无法确认登录终端是否启动：{error}",
        "Could not tell whether the sign-in terminal started: {error}"
    )
}

#[cfg(any(windows, target_os = "macos"))]
fn login_terminal_unopened(error: impl std::fmt::Display) -> String {
    ui_text!(
        "无法打开终端运行 claude auth login：{error}",
        "Could not open a terminal to run claude auth login: {error}"
    )
}

/// POSIX shell single-quoting; the only character that needs care is the quote itself.
#[cfg(not(windows))]
fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// The shell command Terminal runs for the login. Terminal starts the script
/// from a fresh shell with its own environment, not ours, so the config
/// directory and the stripped authentication variables have to travel inside
/// the command text: `unset` first, then the directory the probe and the
/// runtime use, then the CLI.
#[cfg(target_os = "macos")]
fn macos_login_shell_command(executable: &Path, config_dir: Option<&str>) -> String {
    let mut command = format!("unset {};", LOGIN_STRIPPED_ENV.join(" "));
    if let Some(dir) = config_dir {
        command.push_str(&format!(" CLAUDE_CONFIG_DIR={}", shell_single_quote(dir)));
    }
    command.push_str(&format!(
        " {} auth login",
        shell_single_quote(&executable.to_string_lossy())
    ));
    command
}

/// The `.command` file Terminal runs for the login. It deletes itself and its
/// private directory before anything else: `sh` keeps reading from the open
/// descriptor, and the executable and config paths it names do not outlive the
/// start. `rmdir` only ever removes an empty directory, so a `$0` that is not
/// the path Mewrk wrote cannot take anything else with it.
#[cfg(target_os = "macos")]
fn macos_login_script(executable: &Path, config_dir: Option<&str>) -> String {
    format!(
        "#!/bin/sh\nrm -f \"$0\"\nrmdir \"${{0%/*}}\" 2>/dev/null\n{}\n",
        macos_login_shell_command(executable, config_dir)
    )
}

/// Write the login script into a fresh directory of its own under the user's
/// temp directory (`$TMPDIR`, per-user and private on macOS). The directory is
/// created 0700 without `-p`, and the file with `create_new`, so neither can be
/// a name someone planted in advance, even if the temp directory falls back to
/// the shared /tmp.
#[cfg(target_os = "macos")]
fn write_macos_login_script(contents: &str) -> Result<PathBuf, String> {
    use std::io::Write as _;
    use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};

    let dir = std::env::temp_dir().join(format!(
        "mewrk-claude-login-{}",
        Uuid::new_v4().simple()
    ));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .map_err(|error| {
            let path = dir.display();
            ui_text!(
                "无法创建 claude auth login 的临时目录 {path}：{error}",
                "Could not create the temporary folder for claude auth login {path}: {error}"
            )
        })?;
    let script = dir.join("claude-auth-login.command");
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(&script)
        .and_then(|mut file| {
            // `mode` above passes through the umask; the execute bit is what lets
            // Terminal run the file at all, so it is set outright.
            file.set_permissions(std::fs::Permissions::from_mode(0o700))?;
            file.write_all(contents.as_bytes())
        });
    if let Err(error) = written {
        remove_macos_login_script(&script);
        return Err(ui_text!(
            "无法写入 claude auth login 的临时脚本：{error}",
            "Could not write the temporary script for claude auth login: {error}"
        ));
    }
    Ok(script)
}

/// Best-effort removal of a login script Terminal never started, and of its
/// directory. Either may already be gone: the script removes both itself.
#[cfg(target_os = "macos")]
fn remove_macos_login_script(script: &Path) {
    let _ = std::fs::remove_file(script);
    if let Some(dir) = script.parent() {
        let _ = std::fs::remove_dir(dir);
    }
}

/// Hand the login script to Terminal through LaunchServices. Opening a
/// `.command` document is not an Apple Event, unlike `osascript … do script`:
/// no Automation prompt, no -1743 under the hardened runtime, and no "Don't
/// Allow" that sticks until the user digs through System Settings. `-a` pins
/// Terminal even where another app is the `.command` handler. `open` exits once
/// the document is delivered, so its status is the verdict; no grace window.
#[cfg(target_os = "macos")]
fn open_macos_login_script(script: &Path, env: &BTreeMap<String, String>) -> Result<(), String> {
    let mut child = Command::new("/usr/bin/open")
        .args(["-a", "Terminal"])
        .arg(script)
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(login_terminal_unopened)?;
    let status = child
        .wait_timeout(LOGIN_OPEN_TIMEOUT)
        .map_err(login_terminal_unconfirmed)?;
    let Some(status) = status else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(ui_text!(
            "等待 Terminal 打开 Claude Code 登录超时；{remedy}",
            "Terminal took too long to open the Claude Code sign-in; {remedy}",
            remedy = run_login_yourself()
        ));
    };
    if status.success() {
        return Ok(());
    }
    let output = child
        .wait_with_output()
        .map_err(login_terminal_unconfirmed)?;
    let remedy = run_login_yourself();
    let message = match status.code() {
        Some(code) => ui_text!(
            "无法让 Terminal 运行 Claude Code 登录（退出码 {code}）；{remedy}",
            "Terminal could not run the Claude Code sign-in (exit code {code}); {remedy}"
        ),
        None => ui_text!(
            "无法让 Terminal 运行 Claude Code 登录；{remedy}",
            "Terminal could not run the Claude Code sign-in; {remedy}"
        ),
    };
    Err(with_stderr_tail(&message, &output.stderr))
}

/// Terminal emulators tried in order on Linux, with the flag each one uses to
/// take a command. `x-terminal-emulator` is the Debian alternatives entry, so it
/// is whatever terminal the user actually installed.
#[cfg(all(unix, not(target_os = "macos")))]
const LINUX_TERMINALS: &[(&str, &str)] = &[
    ("x-terminal-emulator", "-e"),
    ("gnome-terminal", "--"),
    ("konsole", "-e"),
    ("xterm", "-e"),
];

/// Run `claude auth login` in a terminal the user can see and type into.
///
/// The login flow is interactive — it prints a URL and waits for a pasted code —
/// so it cannot run headless. Mewrk starts the terminal and stops there: it
/// never observes the exchange, and the resulting session belongs to the CLI.
pub(crate) fn open_login() -> Result<(), String> {
    let executable = installed_cli()?;
    let env = login_env();

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // The CLI is started directly in a console of its own rather than through
        // `cmd /c start`: `cmd` would re-parse the path and treat `&` or `^` in a
        // directory name as syntax.
        const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
        let mut child = Command::new(&executable)
            .args(["auth", "login"])
            .env_clear()
            .envs(&env)
            .creation_flags(CREATE_NEW_CONSOLE)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(login_terminal_unopened)?;
        return if launched(&mut child)? {
            Ok(())
        } else {
            Err(ui_text!(
                "Claude Code 登录启动后立即退出；{remedy}",
                "The Claude Code sign-in quit as soon as it started; {remedy}",
                remedy = run_login_yourself()
            ))
        };
    }

    #[cfg(target_os = "macos")]
    {
        let config_dir = env.get("CLAUDE_CONFIG_DIR").map(String::as_str);
        let script = write_macos_login_script(&macos_login_script(&executable, config_dir))?;
        let opened = open_macos_login_script(&script, &env);
        if opened.is_err() {
            // The script only deletes itself once Terminal runs it, which a failed
            // (or timed-out) hand-off gives no reason to expect; one Terminal did
            // start is already gone, and the removal is then a no-op.
            remove_macos_login_script(&script);
        }
        return opened;
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        for (terminal, flag) in LINUX_TERMINALS {
            let spawned = Command::new(terminal)
                .arg(flag)
                .arg(&executable)
                .arg("auth")
                .arg("login")
                .env_clear()
                .envs(&env)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            if let Ok(mut child) = spawned {
                if launched(&mut child)? {
                    return Ok(());
                }
            }
        }
        return Err(ui_text!(
            "找不到能运行 Claude Code 登录的终端程序；{remedy}",
            "No terminal was found to run the Claude Code sign-in in; {remedy}",
            remedy = run_login_yourself()
        ));
    }

    #[allow(unreachable_code)]
    Err(ui_text!(
        "当前平台不支持自动打开终端；{remedy}",
        "Mewrk cannot open a terminal on this platform; {remedy}",
        remedy = run_login_yourself()
    ))
}

/// What to do when Mewrk cannot start the sign-in terminal itself. The panel's
/// Copy command holds the installed CLI's login by absolute path
/// (`login_command`), so it runs in any terminal; a bare `claude auth login`
/// would need a Claude Code of the user's own on `PATH`.
fn run_login_yourself() -> &'static str {
    ui_text::pick(
        "请点「复制命令」，在任意终端里运行它",
        "use Copy command and run it in any terminal",
    )
}

/// A run's hold on a sidecar CLI session. Dropping it sends `release`, so
/// every exit path of the turn loop — completion, error, cancellation, panic
/// unwinding — tears the parked CLI query down. Releasing an unknown session
/// is a no-op on the sidecar, so a run that never reached the sidecar is fine.
pub(crate) struct SessionLease {
    session: Option<AgentSession>,
}

impl SessionLease {
    pub(crate) fn acquire(provider: &ApiProvider, app_data_path: &str) -> Result<Self, String> {
        Ok(Self {
            session: session_for(provider, app_data_path)?,
        })
    }

    /// The session block to attach to a step of this run on `model`; `None`
    /// for families without one. Whether the CLI takes tool changes is the
    /// model's declared `ToolAppend` capability, which Mewrk fills in from
    /// the CLI's own catalogue (`tool_append::known`).
    pub(crate) fn session(&self, model: &ModelProfile) -> Option<AgentSession> {
        self.session.clone().map(|session| AgentSession {
            tool_changes: model.has(ModelCapability::ToolAppend),
            ..session
        })
    }
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            super::process::release_session(&session.session);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_platform::host_platform;

    fn provider(family: ProviderFamily) -> ApiProvider {
        ApiProvider {
            id: "claude-agent".into(),
            name: "Claude Agent".into(),
            enabled: true,
            family,
            base_url: String::new(),
            family_settings: BTreeMap::new(),
            notes: String::new(),
            models: Vec::<ModelProfile>::new(),
            active_model_id: None,
        }
    }

    #[test]
    fn other_families_get_no_session_block() {
        let session = session_for(&provider(ProviderFamily::Anthropic), "").unwrap();
        assert!(session.is_none());
    }

    /// A run keeps one session but may switch models between steps, so whether
    /// the CLI takes tool changes is decided per step, by the host's own rule.
    #[test]
    fn each_step_says_whether_its_model_takes_tool_changes() {
        let lease = SessionLease {
            session: Some(AgentSession {
                session: "run-1".into(),
                executable: "claude".into(),
                sdk: "sdk.mjs".into(),
                cwd: "cwd".into(),
                env: BTreeMap::new(),
                tool_changes: false,
            }),
        };
        let model = |id: &str| {
            let mut profile = ModelProfile {
                id: id.into(),
                name: String::new(),
                group: String::new(),
                context_window: None,
                max_output_tokens: None,
                capabilities: Default::default(),
                reasoning_content: Default::default(),
                prompt_cache: true,
                cache_ttl_minutes: None,
            };
            profile.set_capability(
                ModelCapability::ToolAppend,
                crate::tool_append::known(ProviderFamily::ClaudeAgent, "", id) == Some(true),
            );
            profile
        };
        let opus = lease.session(&model("claude-opus-5-5")).unwrap();
        assert!(opus.tool_changes);
        assert_eq!(opus.session, "run-1");
        assert!(!lease.session(&model("claude-sonnet-5-5")).unwrap().tool_changes);
        assert!(SessionLease { session: None }.session(&model("claude-opus-5-5")).is_none());
    }

    /// The CLI and the SDK are one npm release: whatever this build resolves
    /// as *the* Claude Code must report the version the SDK beside it says it
    /// drives (its `package.json`'s `claudeCodeVersion`, which an install
    /// records and the provider page shows). In a test that is the source
    /// tree's `node_modules`, so a platform package that drifted from the SDK
    /// fails here rather than silently changing CLI behaviour the family
    /// depends on.
    #[test]
    fn the_cli_reports_the_claude_code_version_its_sdk_names() {
        let runtime = claude_agent::runtime()
            .expect("测试里应能找到 aisdk-service/node_modules 的 Claude Code");
        let expected = runtime
            .claude_code_version
            .clone()
            .expect("SDK 的 package.json 应写明 claudeCodeVersion");
        let mut command = Command::new(&runtime.cli);
        command
            .arg("--version")
            .env_clear()
            .envs(probe_env())
            .stdin(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let output = command.output().expect("run Claude Code");
        let reported = String::from_utf8_lossy(&output.stdout);
        assert!(
            reported.trim().starts_with(&expected),
            "Claude Code 报告的是 {reported:?}，而 SDK {} 写的是 {expected}",
            runtime.sdk_version,
        );
    }

    /// The session names the installed CLI and SDK — the provider has no say in
    /// them — and carries the profile variables the CLI needs to find its own
    /// login.
    #[test]
    fn the_session_carries_the_installed_components_and_the_profile_env() {
        let runtime = claude_agent::runtime()
            .expect("测试里应能找到 aisdk-service/node_modules 的 Claude Agent 组件");
        let app_data = tempfile::tempdir().unwrap();
        let session = session_for(
            &provider(ProviderFamily::ClaudeAgent),
            &app_data.path().to_string_lossy(),
        )
        .unwrap()
        .expect("Claude Agent 必须铸出会话块");
        assert_eq!(session.executable, runtime.cli.to_string_lossy());
        assert_eq!(session.sdk, runtime.sdk_entry.to_string_lossy());
        assert!(session.sdk.ends_with("sdk.mjs"), "{}", session.sdk);
        assert_eq!(
            Path::new(&session.cwd),
            app_data.path().join(SESSION_DIR),
            "cwd 必须是应用数据目录下的私有子目录"
        );
        assert!(Path::new(&session.cwd).is_dir(), "cwd 必须已经创建");
        assert_eq!(session.session.len(), 32, "session 是 simple uuid");
        // The profile variables present in this process must be forwarded; nothing else may be.
        for (name, value) in &session.env {
            assert!(
                CLAUDE_AGENT_ENV.contains(&name.as_str()),
                "{name} 不在白名单里"
            );
            assert_eq!(std::env::var(name).ok().as_deref(), Some(value.as_str()));
        }
        for name in [
            "ANTHROPIC_API_KEY",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "ANTHROPIC_BASE_URL",
            "PATH",
        ] {
            assert!(
                !session.env.contains_key(name),
                "{name} 不得出现在 agent.env"
            );
        }
        // Two runs never share a session key.
        let second = session_for(
            &provider(ProviderFamily::ClaudeAgent),
            &app_data.path().to_string_lossy(),
        )
        .unwrap()
        .unwrap();
        assert_ne!(session.session, second.session);
    }

    #[test]
    fn the_probe_environment_carries_no_credential() {
        let env = probe_env();
        for name in env.keys() {
            assert!(!name.starts_with("ANTHROPIC_"), "{name} 会改写探测到的账号");
            assert_ne!(name, "CLAUDE_CODE_OAUTH_TOKEN");
        }
        for (name, value) in PROBE_CONTROL_ENV {
            assert_eq!(env.get(*name).map(String::as_str), Some(*value));
        }
        // A cleared environment still needs the variables a process needs to run.
        if std::env::var("PATH").is_ok() {
            assert!(env.contains_key("PATH"));
        }
        // macOS's per-user temp directory, not the shared /tmp.
        if let Ok(tmpdir) = std::env::var("TMPDIR") {
            assert_eq!(env.get("TMPDIR"), Some(&tmpdir));
        }
        if host_platform().is_windows() {
            assert!(
                env.contains_key("SYSTEMROOT") || env.contains_key("SystemRoot"),
                "Windows 上缺 SystemRoot 会让 CLI 连 socket 都开不了"
            );
        }
    }

    /// The Keychain account the CLI reads its macOS login from is `$USER`, so
    /// both the probe and every run must carry it; a probe without it reports a
    /// signed-in user as signed out.
    #[test]
    fn the_keychain_account_reaches_the_probe_and_the_run() {
        let Ok(user) = std::env::var("USER") else {
            return;
        };
        assert_eq!(probe_env().get("USER"), Some(&user));
        assert_eq!(profile_env().get("USER"), Some(&user));
    }

    /// The login runs in the user's own environment (a terminal needs far more
    /// than the probe whitelist), minus every authentication override; the
    /// config directory the probe honours must reach it unchanged.
    #[test]
    fn the_login_environment_is_the_users_own_minus_the_authentication_overrides() {
        let env = login_env();
        for name in LOGIN_STRIPPED_ENV {
            assert!(
                !env.contains_key(*name),
                "{name} 会让 auth login 看起来已经登录"
            );
        }
        for (name, value) in PROBE_CONTROL_ENV {
            assert_eq!(env.get(*name).map(String::as_str), Some(*value));
        }
        for (name, value) in std::env::vars() {
            if LOGIN_STRIPPED_ENV.contains(&name.as_str())
                || PROBE_CONTROL_ENV
                    .iter()
                    .any(|(control, _)| *control == name)
            {
                continue;
            }
            assert_eq!(env.get(&name), Some(&value), "{name} 必须原样保留");
        }
    }

    /// Terminal starts its own shell, so the stripped variables and the config
    /// directory have to be spelled out in the command text, quoted for `sh`.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_macos_login_command_carries_the_environment_in_the_text() {
        let command = macos_login_shell_command(
            Path::new("/Users/me/it's here/claude"),
            Some("/Users/me/.claude-mewrk"),
        );
        assert_eq!(
            command,
            "unset ANTHROPIC_API_KEY ANTHROPIC_AUTH_TOKEN ANTHROPIC_BASE_URL \
             ANTHROPIC_CUSTOM_HEADERS CLAUDE_CODE_OAUTH_TOKEN; \
             CLAUDE_CONFIG_DIR='/Users/me/.claude-mewrk' \
             '/Users/me/it'\\''s here/claude' auth login"
        );
        let plain = macos_login_shell_command(Path::new("/usr/local/bin/claude"), None);
        assert!(
            plain.ends_with(" '/usr/local/bin/claude' auth login"),
            "{plain}"
        );
        assert!(!plain.contains("CLAUDE_CONFIG_DIR"), "{plain}");
    }

    /// Copy command names the installed executable, never a `claude` on `PATH`,
    /// quoted so the path survives a space or a quote in any POSIX shell.
    #[cfg(not(windows))]
    #[test]
    fn the_copied_login_command_runs_the_installed_cli_by_its_path() {
        assert_eq!(
            login_command(Path::new("/Applications/Mewrk.app/Contents/Resources/claude")),
            "'/Applications/Mewrk.app/Contents/Resources/claude' auth login"
        );
        assert_eq!(
            login_command(Path::new("/Users/me/it's here/claude")),
            "'/Users/me/it'\\''s here/claude' auth login"
        );
    }

    /// A Windows path that needs no quoting runs bare in `cmd` and PowerShell
    /// alike; one that does is written for PowerShell, behind the call operator.
    #[test]
    fn the_copied_windows_login_command_quotes_only_what_needs_it() {
        assert_eq!(
            windows_login_command(r"C:\Users\me\AppData\Local\Mewrk\claude.exe"),
            r"C:\Users\me\AppData\Local\Mewrk\claude.exe auth login"
        );
        assert_eq!(
            windows_login_command(r"C:\Program Files\WindowsApps\Mewrk\claude.exe"),
            r"& 'C:\Program Files\WindowsApps\Mewrk\claude.exe' auth login"
        );
        assert_eq!(
            windows_login_command(r"C:\Users\O'Brien\Mewrk\claude.exe"),
            r"& 'C:\Users\O''Brien\Mewrk\claude.exe' auth login"
        );
    }

    /// When Mewrk cannot open the terminal itself, the remedy points at Copy
    /// command rather than at a bare `claude auth login`.
    #[test]
    fn a_failed_terminal_points_at_copy_command() {
        assert!(run_login_yourself().contains("复制命令"));
        let english = crate::ui_text::with_language(crate::model::ResolvedLanguage::EnUs, || {
            run_login_yourself()
        });
        assert_eq!(english, "use Copy command and run it in any terminal");
    }

    /// The `.command` file is the same command behind a self-removing prologue.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_macos_login_script_removes_itself_before_the_command() {
        let executable = Path::new("/Applications/Mewrk.app/Contents/MacOS/claude");
        let script = macos_login_script(executable, Some("/Users/me/.claude"));
        assert_eq!(
            script,
            format!(
                "#!/bin/sh\nrm -f \"$0\"\nrmdir \"${{0%/*}}\" 2>/dev/null\n{}\n",
                macos_login_shell_command(executable, Some("/Users/me/.claude"))
            )
        );
    }

    /// End to end through `sh`, as Terminal runs it: hostile characters in both
    /// interpolated paths stay data, the authentication overrides are gone, the
    /// CLI gets `auth login`, and nothing is left on disk afterwards.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_macos_login_script_is_private_quoted_and_self_removing() {
        use std::os::unix::fs::PermissionsExt as _;

        let bin = tempfile::tempdir().unwrap();
        let fake = bin.path().join("it's a \"claude\" $HOME `id`");
        std::fs::write(
            &fake,
            "#!/bin/sh\nprintf '%s|%s|%s' \"$CLAUDE_CONFIG_DIR\" \"$*\" \"${ANTHROPIC_API_KEY-unset}\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
        let config_dir = "/tmp/it's $(id) `id` \\ \"dir\"";

        let script = write_macos_login_script(&macos_login_script(&fake, Some(config_dir))).unwrap();
        let dir = script.parent().unwrap().to_owned();
        assert_eq!(script.extension().and_then(|value| value.to_str()), Some("command"));
        for path in [&dir, &script] {
            let mode = std::fs::metadata(path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "{} 必须只有属主可访问", path.display());
        }

        let output = Command::new(&script)
            .env("ANTHROPIC_API_KEY", "sk-ant-should-never-reach-login")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            format!("{config_dir}|auth login|unset")
        );
        assert!(!script.exists(), "脚本启动后必须删掉自己");
        assert!(!dir.exists(), "脚本的私有目录也必须一起删掉");
    }

    /// The fallback cleanup for a hand-off that failed removes both the script
    /// and its directory.
    #[cfg(target_os = "macos")]
    #[test]
    fn an_unopened_macos_login_script_is_cleaned_up() {
        let script = write_macos_login_script("#!/bin/sh\n").unwrap();
        let dir = script.parent().unwrap().to_owned();
        remove_macos_login_script(&script);
        assert!(!script.exists());
        assert!(!dir.exists());
        // Already gone: still quiet.
        remove_macos_login_script(&script);
    }

    #[test]
    fn a_signed_in_answer_is_read_field_by_field() {
        let parsed = parse_auth_status(
            r#"{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"anthropic",
                "email":"user@example.com","orgName":"Example Inc","subscriptionType":"max"}"#,
        )
        .unwrap();
        assert!(parsed.logged_in);
        assert_eq!(parsed.auth_method.as_deref(), Some("claude.ai"));
        assert_eq!(parsed.email.as_deref(), Some("user@example.com"));
        assert_eq!(parsed.org_name.as_deref(), Some("Example Inc"));
        assert_eq!(parsed.subscription_type.as_deref(), Some("max"));
    }

    /// Everything but `loggedIn` is optional, and a console login has no
    /// subscription at all. Missing fields must not fail the parse, or the
    /// panel would report a broken CLI to a user who is merely logged out.
    #[test]
    fn a_minimal_answer_still_parses() {
        let parsed = parse_auth_status(r#"{"loggedIn":false}"#).unwrap();
        assert!(!parsed.logged_in);
        assert_eq!(parsed.auth_method, None);
        assert_eq!(parsed.email, None);
        assert_eq!(parsed.subscription_type, None);
    }

    /// A CLI release that prints a notice before its JSON is still answering.
    #[test]
    fn a_banner_before_the_json_does_not_hide_the_answer() {
        let parsed = parse_auth_status("Update available: 2.1.259\n{\"loggedIn\":true}\n").unwrap();
        assert!(parsed.logged_in);
    }

    #[test]
    fn output_without_an_object_is_an_error_not_a_logged_out_answer() {
        let error = parse_auth_status("command not found").unwrap_err();
        assert!(error.contains("JSON"), "{error}");
        parse_auth_status("{ not json }").unwrap_err();
    }

    #[test]
    fn a_failure_message_carries_a_clipped_stderr_tail() {
        let noise = "x".repeat(MAX_STDERR_TAIL * 2);
        let message = with_stderr_tail("读取失败", noise.as_bytes());
        assert!(message.starts_with("读取失败（"));
        assert!(
            message.chars().count() < noise.chars().count(),
            "尾巴必须被裁剪"
        );
        assert_eq!(with_stderr_tail("读取失败", b"   "), "读取失败");
    }
}
