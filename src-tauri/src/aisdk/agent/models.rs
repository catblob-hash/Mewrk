//! The installed Claude Code's own model picker, asked of the CLI directly.
//!
//! The model fetch for the Claude Agent family does not go through the sidecar.
//! The host starts the installed CLI the way the login probe does, speaks
//! the CLI's stream-json control protocol (the one the Agent SDK drives) just
//! long enough to read the picker, and kills it. No prompt is ever sent, so
//! nothing is billed.
//!
//! # The served picker
//!
//! For a claude.ai login, Claude Code takes its picker from Anthropic's model
//! selector (`/api/organizations/<org>/model_selector/cc`): the models this
//! account is offered today, older ones included, kept in
//! `<config>/cache/model-catalog/<…>-cc.json` and refreshed about hourly. Only
//! where that is off — an API key, no network — does it fall back to the list
//! compiled into its release. The CLI turns the served picker off whenever
//! non-essential traffic or telemetry is (the latter switches off the feature
//! flag that enables it), so the rows are read by a CLI with both on
//! ([`served_env`]); the CLI's own updater and error reports stay off.
//!
//! A CLI starting headless serves the cached list as it is, however old, and
//! refreshes the cache in the background for the next process. So when the
//! cache was already stale, the listing waits for that refresh to land and
//! reads the picker again from a new CLI: what the selector serves now, not
//! what it served the last time a Claude Code ran under this login.
//!
//! Context windows are read afterwards by a CLI with the network off
//! ([`listing_env`]), as they always were: switching models must never be the
//! thing that sends a request.

use std::collections::BTreeMap;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::{json, Value};

use super::{config_dir, installed_cli, probe_env, session_cwd, with_stderr_tail};

/// How long the whole listing may take. The CLI answers from local state in
/// under a second; the bound only covers one that hangs while starting.
const LIST_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a stale served picker is given to refresh: the CLI gives the
/// selector 5 s, and it usually answers in well under one.
const REFRESH_WAIT: Duration = Duration::from_secs(6);

/// The probe switches a served-picker read goes without: either one makes the
/// CLI serve its compiled list.
const SERVED_PICKER_OFF: &[&str] = &["CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "DISABLE_TELEMETRY"];

/// Pinned for a served-picker read in their place: what the CLI would report
/// on its own is not this read's to send.
const SERVED_CONTROL_ENV: &[(&str, &str)] = &[("DISABLE_ERROR_REPORTING", "1")];

/// The served picker's cache, under the CLI's configuration directory: one
/// file per login and surface, the CLI's own (`cc`) ending in this.
const CATALOG_DIR: &[&str] = &["cache", "model-catalog"];
const CATALOG_SUFFIX: &str = "-cc.json";

/// A CLI that only answers control requests: the SDK's stream-json transport,
/// and none of the CLI's own tools, settings files, MCP servers or transcript —
/// the switches a run is started with.
const LIST_ARGS: &[&str] = &[
    "--output-format",
    "stream-json",
    "--verbose",
    "--input-format",
    "stream-json",
    "--tools",
    "",
    "--setting-sources=",
    "--strict-mcp-config",
    "--no-session-persistence",
];

/// Pinned on top of the probe environment so the CLI resolves the login and
/// the provider a run resolves: the SDK's entrypoint label, which every run
/// carries, and first-party Anthropic rather than a cloud provider.
const LIST_CONTROL_ENV: &[(&str, &str)] = &[
    ("CLAUDE_CODE_ENTRYPOINT", "sdk-ts"),
    ("CLAUDE_CODE_USE_BEDROCK", "0"),
    ("CLAUDE_CODE_USE_VERTEX", "0"),
    ("CLAUDE_CODE_USE_FOUNDRY", "0"),
];

/// One row of the CLI's model picker, in its order. `value` may be an alias
/// (`sonnet`, `opus[1m]`, `default`); `resolved_model` is the model id it
/// stands for today. `context_window` is the CLI's window for an alias row, read
/// by switching to it; absent for a row named by an explicit id (switching to
/// one costs a request) and for a row the CLI would not switch to.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentModel {
    pub(crate) value: String,
    #[serde(default)]
    pub(crate) resolved_model: Option<String>,
    #[serde(default)]
    pub(crate) display_name: String,
    #[serde(skip)]
    pub(crate) context_window: Option<u64>,
}

/// The environment the listing CLI runs with: the login probe's, which carries
/// no credential, plus [`LIST_CONTROL_ENV`]. With the network off it serves the
/// picker compiled into its release; it reads the context windows.
fn listing_env() -> BTreeMap<String, String> {
    let mut env = probe_env();
    for (name, value) in LIST_CONTROL_ENV {
        env.insert((*name).to_owned(), (*value).to_owned());
    }
    env
}

/// [`listing_env`] with the network on, so that the CLI serves the picker
/// Anthropic's model selector offers this login: without
/// [`SERVED_PICKER_OFF`], plus [`SERVED_CONTROL_ENV`]. Still no credential.
fn served_env() -> BTreeMap<String, String> {
    let mut env = listing_env();
    for name in SERVED_PICKER_OFF {
        env.remove(*name);
    }
    for (name, value) in SERVED_CONTROL_ENV {
        env.insert((*name).to_owned(), (*value).to_owned());
    }
    env
}

/// The installed Claude Code's model picker under the user's current login.
pub(crate) fn list_models() -> Result<Vec<AgentModel>, String> {
    #[cfg_attr(not(feature = "browser-dev"), allow(unused_mut))]
    let (mut served, mut offline) = (served_env(), listing_env());
    #[cfg(feature = "browser-dev")]
    {
        let upstream = super::test_upstream_env()?;
        served.extend(upstream.clone());
        offline.extend(upstream);
    }
    let catalog = CATALOG_DIR.iter().fold(PathBuf::from(config_dir()), |path, part| path.join(part));
    // A fetch has no app data path; the fallback directory is private to Mewrk,
    // and `--no-session-persistence` keeps the CLI from writing anything there.
    list_models_with(&installed_cli()?, &session_cwd("")?, &served, &offline, &catalog)
}

/// The picker as `served` makes the CLI serve it ([`read_served_picker`]), with
/// the context windows of its alias rows read by a CLI started with `offline`.
fn list_models_with(
    executable: &Path,
    cwd: &Path,
    served: &BTreeMap<String, String>,
    offline: &BTreeMap<String, String>,
    catalog: &Path,
) -> Result<Vec<AgentModel>, String> {
    let mut models = read_served_picker(executable, cwd, served, catalog)?;
    if models.iter().all(|model| is_explicit_id(&model.value)) {
        return Ok(models);
    }
    let mut cli = ControlSession::start(executable, cwd, offline)?;
    let read = read_windows(&mut cli, &mut models);
    let stderr = cli.stop();
    read.map_err(|error| with_stderr_tail(&error, &stderr))?;
    Ok(models)
}

/// The picker rows a CLI started with `env` serves. When the served picker's
/// cache under `catalog` was already stale, the CLI that served it from there
/// is kept until its background refresh has written the cache again (for at
/// most [`REFRESH_WAIT`]), and a new CLI is asked: one already running keeps
/// the list it started with.
fn read_served_picker(
    executable: &Path,
    cwd: &Path,
    env: &BTreeMap<String, String>,
    catalog: &Path,
) -> Result<Vec<AgentModel>, String> {
    let before = catalog_entries(catalog);
    let stale = refresh_expected(&before, now_millis());
    let mut cli = ControlSession::start(executable, cwd, env)?;
    let read = read_rows(&mut cli);
    let refreshed = read.is_ok() && stale && wait_for_refresh(catalog, &before, REFRESH_WAIT);
    let stderr = cli.stop();
    let rows = read.map_err(|error| with_stderr_tail(&error, &stderr))?;
    if !refreshed {
        return Ok(rows);
    }
    let mut cli = ControlSession::start(executable, cwd, env)?;
    let read = read_rows(&mut cli);
    let stderr = cli.stop();
    read.map_err(|error| with_stderr_tail(&error, &stderr))
}

fn read_rows(cli: &mut ControlSession) -> Result<Vec<AgentModel>, String> {
    let initialized = cli
        .request(json!({ "subtype": "initialize" }))?
        .map_err(|error| format!("Claude Code 拒绝了初始化请求：{error}"))?;
    let rows = initialized
        .get("models")
        .cloned()
        .ok_or("Claude Code 的初始化回答里没有模型列表")?;
    serde_json::from_value(rows).map_err(|error| format!("无法解析 Claude Code 的模型列表：{error}"))
}

/// Fills in the context window of each of `models` the CLI tells
/// ([`context_window_of`]).
fn read_windows(cli: &mut ControlSession, models: &mut [AgentModel]) -> Result<(), String> {
    cli.request(json!({ "subtype": "initialize" }))?
        .map_err(|error| format!("Claude Code 拒绝了初始化请求：{error}"))?;
    for model in models {
        model.context_window = context_window_of(cli, model)?;
    }
    Ok(())
}

/// One file of the served picker's cache, as far as it is read: when it was
/// fetched and when the CLI takes it for stale, in Unix milliseconds.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct CatalogEntry {
    #[serde(skip)]
    name: String,
    fetched_at: u64,
    stale_at: u64,
}

/// The CLI's own (`cc`) files of the served picker's cache under `catalog`,
/// one per login; any it cannot read are left out.
fn catalog_entries(catalog: &Path) -> Vec<CatalogEntry> {
    let Ok(entries) = std::fs::read_dir(catalog) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(CATALOG_SUFFIX) {
                return None;
            }
            let bytes = std::fs::read(entry.path()).ok()?;
            let parsed: CatalogEntry = serde_json::from_slice(&bytes).ok()?;
            Some(CatalogEntry { name, ..parsed })
        })
        .collect()
}

/// Whether a CLI starting now will serve the picker from a stale cache and
/// refresh it behind: the latest fetch — the current login's, as a rule — is
/// past its stale point. With no cache at all the CLI fetches before it
/// answers, and a login the selector does not serve (an API key) has none.
fn refresh_expected(entries: &[CatalogEntry], now: u64) -> bool {
    entries
        .iter()
        .max_by_key(|entry| entry.fetched_at)
        .is_some_and(|latest| latest.stale_at <= now)
}

/// Waits up to `limit` for a cache file under `catalog` fetched later than
/// `before` says, and whole: the file is read back, not just looked at.
fn wait_for_refresh(catalog: &Path, before: &[CatalogEntry], limit: Duration) -> bool {
    let until = Instant::now() + limit;
    loop {
        let refreshed = catalog_entries(catalog).iter().any(|entry| {
            let known = before.iter().find(|known| known.name == entry.name);
            known.is_none_or(|known| entry.fetched_at > known.fetched_at)
        });
        if refreshed {
            return true;
        }
        if Instant::now() >= until {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// A row named by an explicit model id rather than an alias.
fn is_explicit_id(value: &str) -> bool {
    value.to_ascii_lowercase().starts_with("claude-")
}

/// The context window the CLI uses for one picker row: switch the idle CLI to
/// it, then read the window from a `summary` context report, which answers from
/// local state without a token-count call.
///
/// Only an alias row is switched to. Switching to an explicit model id makes the
/// CLI validate it with a real one-token `/v1/messages` request, and listing
/// must never bill; the model fetch derives those rows' windows from the id
/// instead. `None` also when the CLI refuses the switch or reports a model other
/// than the row's; the row itself is still offered. A CLI that stopped
/// answering fails the whole listing.
fn context_window_of(cli: &mut ControlSession, model: &AgentModel) -> Result<Option<u64>, String> {
    if is_explicit_id(&model.value) {
        return Ok(None);
    }
    if cli
        .request(json!({ "subtype": "set_model", "model": model.value }))?
        .is_err()
    {
        return Ok(None);
    }
    let Ok(usage) = cli.request(json!({ "subtype": "get_context_usage", "detail": "summary" }))?
    else {
        return Ok(None);
    };
    if let Some(resolved) = &model.resolved_model {
        if usage.get("model").and_then(Value::as_str) != Some(resolved.as_str()) {
            return Ok(None);
        }
    }
    Ok(usage
        .get("rawMaxTokens")
        .and_then(Value::as_u64)
        .filter(|window| *window > 0))
}

/// Asks the installed Claude Code, under the user's login, for the plan's usage —
/// one authenticated read that spends no tokens — and so has it renew the
/// login's access token when that is about to lapse or already has. Claude Code
/// renews only on its own requests, so a login nothing uses runs down: its
/// refresh token expires weeks after the last renewal, and then only signing in
/// again brings it back.
///
/// `Ok(true)` when the server answered with the plan's usage, which a dead login
/// cannot get; the CLI clears such a login from its store, so its own status
/// says signed out afterwards. `Ok(false)` when nothing confirmed the login —
/// that, or no network.
pub(crate) fn refresh_login() -> Result<bool, String> {
    let mut env = listing_env();
    // The usage read is "nonessential traffic" to the CLI, which the probe
    // environment turns off; with it off the CLI answers from nothing.
    env.remove("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC");
    #[cfg(feature = "browser-dev")]
    env.extend(super::test_upstream_env()?);
    let mut cli = ControlSession::start(&installed_cli()?, &session_cwd("")?, &env)?;
    let answered = read_usage(&mut cli);
    let stderr = cli.stop();
    answered.map_err(|error| with_stderr_tail(&error, &stderr))
}

fn read_usage(cli: &mut ControlSession) -> Result<bool, String> {
    cli.request(json!({ "subtype": "initialize" }))?
        .map_err(|error| format!("Claude Code 拒绝了初始化请求：{error}"))?;
    let usage = cli
        .request(json!({ "subtype": "get_usage", "skip_behaviors": true }))?
        .map_err(|error| format!("Claude Code 拒绝了用量请求：{error}"))?;
    let present = |field: &str| usage.get(field).is_some_and(|value| !value.is_null());
    Ok(present("rate_limits") || present("subscription_type"))
}

/// A CLI started for control requests only. Dropping it kills the process.
struct ControlSession {
    child: Child,
    stdin: ChildStdin,
    /// Stdout lines, from a reader thread; disconnected once the CLI exits.
    lines: Receiver<String>,
    stderr: Option<JoinHandle<Vec<u8>>>,
    deadline: Instant,
    next_id: u64,
}

impl ControlSession {
    fn start(
        executable: &Path,
        cwd: &Path,
        env: &BTreeMap<String, String>,
    ) -> Result<Self, String> {
        let mut command = Command::new(executable);
        command
            .args(LIST_ARGS)
            .current_dir(cwd)
            .env_clear()
            .envs(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            // Without this every fetch flashes a console window.
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = command
            .spawn()
            .map_err(|error| format!("无法启动 Claude Code 读取模型列表：{error}"))?;
        let (Some(stdin), Some(stdout), Some(mut stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("Claude Code 的标准输入输出没有接上".into());
        };
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    return;
                }
            }
        });
        // Drained even though it is read only on failure: a full pipe would
        // block the CLI.
        let stderr = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = stderr.read_to_end(&mut bytes);
            bytes
        });
        Ok(Self {
            child,
            stdin,
            lines,
            stderr: Some(stderr),
            deadline: Instant::now() + LIST_TIMEOUT,
            next_id: 0,
        })
    }

    /// Send one control request and wait for its response. The outer error is
    /// the transport (the CLI exited or timed out); the inner one is the CLI
    /// refusing this request.
    fn request(&mut self, request: Value) -> Result<Result<Value, String>, String> {
        self.next_id += 1;
        let id = format!("mewrk-{}", self.next_id);
        let frame = json!({ "type": "control_request", "request_id": id, "request": request });
        writeln!(self.stdin, "{frame}")
            .and_then(|()| self.stdin.flush())
            .map_err(|error| format!("写入 Claude Code 失败：{error}"))?;
        loop {
            let remaining = self.deadline.saturating_duration_since(Instant::now());
            let line = match self.lines.recv_timeout(remaining) {
                Ok(line) => line,
                Err(RecvTimeoutError::Timeout) => {
                    return Err("读取 Claude Code 模型列表超时".into());
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err("Claude Code 在回答模型列表之前退出了".into());
                }
            };
            // Anything but this request's answer — a banner, a system message —
            // is not what is being waited on.
            let Ok(frame) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let Some(response) = frame
                .get("response")
                .filter(|_| frame.get("type").and_then(Value::as_str) == Some("control_response"))
            else {
                continue;
            };
            if response.get("request_id").and_then(Value::as_str) != Some(id.as_str()) {
                continue;
            }
            return Ok(match response.get("subtype").and_then(Value::as_str) {
                Some("success") => Ok(response.get("response").cloned().unwrap_or(Value::Null)),
                _ => Err(response
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("未说明原因")
                    .to_owned()),
            });
        }
    }

    /// Kill the CLI and return what it wrote to stderr.
    fn stop(&mut self) -> Vec<u8> {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.stderr
            .take()
            .and_then(|stderr| stderr.join().ok())
            .unwrap_or_default()
    }
}

impl Drop for ControlSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    use super::*;

    /// An upstream that records each request line and answers nothing useful.
    fn recording_upstream() -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本机端口");
        let address = format!("http://{}", listener.local_addr().expect("本机地址"));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&seen);
        std::thread::spawn(move || {
            for mut stream in listener.incoming().map_while(Result::ok) {
                let mut reader = BufReader::new(&stream);
                let mut request_line = String::new();
                let _ = reader.read_line(&mut request_line);
                recorded
                    .lock()
                    .unwrap()
                    .push(request_line.trim().to_owned());
                let _ = stream.write_all(
                    b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
                );
            }
        });
        (address, seen)
    }

    /// The listing, end to end against the installed CLI (the source tree's in
    /// a test): the picker's rows, an alias row's window read by switching to
    /// it, and no model request. A loopback stub key keeps the developer's own
    /// login out of it (and so the served picker, which a stub is not), and the
    /// stub records whatever the CLI asked for. Telemetry stays off in a test.
    #[test]
    fn the_cli_answers_its_picker_without_a_model_request() {
        let (address, seen) = recording_upstream();
        let stub = |mut env: BTreeMap<String, String>| {
            env.insert("ANTHROPIC_BASE_URL".to_owned(), address.clone());
            env.insert(
                "ANTHROPIC_API_KEY".to_owned(),
                "sk-ant-fake-listing".to_owned(),
            );
            env.insert("DISABLE_TELEMETRY".to_owned(), "1".to_owned());
            env
        };
        let executable =
            installed_cli().expect("测试里应能找到 aisdk-service/node_modules 的 Claude Code");
        let cwd = session_cwd("").expect("工作目录");
        let catalog = tempfile::tempdir().expect("缓存目录");

        let models = list_models_with(
            &executable,
            &cwd,
            &stub(served_env()),
            &stub(listing_env()),
            catalog.path(),
        )
        .expect("CLI 应回答模型列表");

        assert!(!models.is_empty(), "CLI 没有列出任何模型");
        assert!(
            models.iter().all(|model| !model.value.is_empty()),
            "{models:?}"
        );
        assert!(
            models.iter().any(|model| model.context_window.is_some()),
            "没有一行带上下文窗口: {models:?}"
        );
        assert!(
            models
                .iter()
                .filter(|model| model.value.starts_with("claude-"))
                .all(|model| model.context_window.is_none()),
            "显式 id 的行不该被切换过去: {models:?}"
        );
        let seen = seen.lock().unwrap();
        assert!(
            seen.iter().all(|line| !line.contains("/v1/messages")),
            "列模型发起了模型请求: {seen:?}"
        );
    }

    #[test]
    fn the_served_picker_is_read_with_the_network_on_and_no_credential() {
        let env = served_env();
        for name in SERVED_PICKER_OFF {
            assert!(!env.contains_key(*name), "{name} 会让 CLI 退回编译进去的列表");
        }
        for name in env.keys() {
            assert!(!name.starts_with("ANTHROPIC_"), "{name} 会改写列出的账号");
            assert_ne!(name, "CLAUDE_CODE_OAUTH_TOKEN");
            assert_ne!(name, "CLAUDE_CODE_MODEL_CATALOG");
        }
        assert_eq!(env.get("DISABLE_AUTOUPDATER").map(String::as_str), Some("1"));
        assert_eq!(env.get("DISABLE_ERROR_REPORTING").map(String::as_str), Some("1"));
        assert_eq!(env.get("CLAUDE_CODE_ENTRYPOINT").map(String::as_str), Some("sdk-ts"));
        // The windows are still read with the network off.
        let offline = listing_env();
        for name in SERVED_PICKER_OFF {
            assert_eq!(offline.get(*name).map(String::as_str), Some("1"));
        }
    }

    fn write_catalog(dir: &Path, name: &str, fetched_at: u64, stale_at: u64) {
        let body = json!({ "version": 2, "fetchedAt": fetched_at, "staleAt": stale_at, "catalog": {} });
        std::fs::write(dir.join(name), body.to_string()).unwrap();
    }

    #[test]
    fn only_a_stale_cache_of_the_clis_own_picker_is_waited_on() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!refresh_expected(&catalog_entries(dir.path()), 1_000), "没有缓存时 CLI 先取再答");
        write_catalog(dir.path(), "org-acct-ccd.json", 10, 20);
        std::fs::write(dir.path().join("org-acct-cc.headless-failed.json"), "{}").unwrap();
        assert!(catalog_entries(dir.path()).is_empty(), "只认 CLI 自己（cc）的缓存");

        write_catalog(dir.path(), "org-acct-cc.json", 100, 200);
        assert!(!refresh_expected(&catalog_entries(dir.path()), 150));
        assert!(refresh_expected(&catalog_entries(dir.path()), 200));
        // Another login's older cache does not decide it: the latest fetch does.
        write_catalog(dir.path(), "org-other-cc.json", 50, 60);
        assert!(!refresh_expected(&catalog_entries(dir.path()), 150));
    }

    #[test]
    fn a_refresh_is_seen_once_a_whole_newer_cache_is_written() {
        let dir = tempfile::tempdir().unwrap();
        write_catalog(dir.path(), "org-acct-cc.json", 100, 200);
        let before = catalog_entries(dir.path());
        assert!(!wait_for_refresh(dir.path(), &before, Duration::from_millis(200)));
        // Half written, or another surface's: not yet.
        std::fs::write(dir.path().join("org-acct-cc.json"), "{\"version\":2,\"fetchedAt\":3").unwrap();
        write_catalog(dir.path(), "org-acct-ccd.json", 300, 400);
        assert!(!wait_for_refresh(dir.path(), &before, Duration::from_millis(200)));

        let writer = {
            let dir = dir.path().to_path_buf();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(150));
                write_catalog(&dir, "org-acct-cc.json", 300, 400);
            })
        };
        let started = Instant::now();
        assert!(wait_for_refresh(dir.path(), &before, Duration::from_secs(5)));
        assert!(started.elapsed() < Duration::from_secs(2));
        writer.join().unwrap();

        // A login with no cache before gets its first.
        let before = catalog_entries(dir.path());
        write_catalog(dir.path(), "org-new-cc.json", 10, 20);
        assert!(wait_for_refresh(dir.path(), &before, Duration::from_millis(200)));
    }

    #[test]
    fn the_listing_environment_is_the_probes_plus_the_run_switches() {
        let env = listing_env();
        for name in env.keys() {
            assert!(!name.starts_with("ANTHROPIC_"), "{name} 会改写列出的账号");
            assert_ne!(name, "CLAUDE_CODE_OAUTH_TOKEN");
        }
        for (name, value) in LIST_CONTROL_ENV {
            assert_eq!(env.get(*name).map(String::as_str), Some(*value));
        }
        for (name, value) in probe_env() {
            assert_eq!(env.get(&name), Some(&value));
        }
    }
}
