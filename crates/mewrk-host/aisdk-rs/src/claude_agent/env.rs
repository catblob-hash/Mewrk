//! The CLI's environment and the host-resolved path (`claude-agent.ts`).
//!
//! The environment is derived from the sidecar's own with the credential variables removed, plus the profile-location
//! variables the host sends in `agent.env` (`USER` among them: it names the macOS Keychain account). `CLAUDE_CONFIG_DIR`
//! is never set here: the login lives in the CLI's default `~/.claude`. Authentication is the user's own Claude Code
//! login and nothing else; the one upstream override this family accepts is a local test stub, an `ANTHROPIC_BASE_URL` in
//! `agent.env` addressing a loopback host, which alone also lets a key from `agent.env` through.

use std::collections::BTreeMap;
use std::path::Path;

use crate::api::allowed_env;
use crate::json::Json;

use super::log;
use super::native::js_trim;
use super::url;

/// How long a parked tool handler may wait for the host: a week, i.e. never in practice.
pub(super) const PARK_TIMEOUT_MS: u64 = 7 * 24 * 60 * 60 * 1000;
/// What the user should do about a CLI that went away.
const REINSTALL_HINT: &str = "请重新安装 Claude Code，然后在「设置 → 提供商 → 模型提供商 → Claude Agent」里重新检测。";
/// Authentication channels that must never reach the CLI from the sidecar's own environment.
const CREDENTIAL_ENV: [&str; 5] = ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL", "ANTHROPIC_CUSTOM_HEADERS", "CLAUDE_CODE_OAUTH_TOKEN"];
/// Variables `agent.env` may only supply together with a loopback endpoint.
const UPSTREAM_OVERRIDE_ENV: [&str; 3] = ["ANTHROPIC_BASE_URL", "ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"];
/// The sidecar's own environment: what `process.env` held in the Node process, read through the one allowed door.
const SIDECAR_ENV: [&str; 16] = [
    "SystemRoot", "windir", "SystemDrive", "COMSPEC", "PATHEXT", "NUMBER_OF_PROCESSORS", "PROCESSOR_ARCHITECTURE", "TEMP", "TMP", "TMPDIR", "PATH",
    "HOME", "LANG", "LC_ALL", "LC_CTYPE", "TZ",
];
/// The context budget the CLI gives a model id without `[1m]` under a Console login.
const STANDARD_CONTEXT_BUDGET: f64 = 200_000.0;
/// The Agent SDK release whose protocol this module speaks, which the CLI is told it is talking to.
pub(super) const SDK_PROTOCOL_VERSION: &str = "0.3.284";

/// Behaviour switches: everything Claude Code would do on its own is off.
///
/// Attachments stay on: a tool that joins mid-session reaches the model as the CLI's own `tool_addition`, which only
/// the attachment pipeline produces. `CLAUDE_CODE_ENABLE_FUNCTION_HOOKS` loads the hooks module of the context plugin,
/// which leaves the CLI's own environment block, model line and date out of the prompt, and is load-bearing: without it
/// the model is told about Mewrk's private session folder as if it were a workspace.
fn cli_control_env() -> [(&'static str, String); 20] {
    let on = || "1".to_owned();
    [
        ("CLAUDE_AGENT_SDK_MCP_NO_PREFIX", on()),
        ("CLAUDE_AGENT_SDK_CLIENT_APP", "mewrk/1.0".to_owned()),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", on()),
        ("DISABLE_AUTOUPDATER", on()),
        ("DISABLE_ERROR_REPORTING", on()),
        ("DISABLE_TELEMETRY", on()),
        ("DISABLE_BUG_COMMAND", on()),
        ("DISABLE_COST_WARNINGS", on()),
        ("CLAUDE_CODE_SKIP_PROMPT_HISTORY", on()),
        ("DISABLE_AUTO_COMPACT", on()),
        ("DISABLE_COMPACT", on()),
        ("CLAUDE_CODE_DISABLE_AUTO_MEMORY", on()),
        ("CLAUDE_CODE_DISABLE_CLAUDE_MDS", on()),
        ("DISABLE_BUILTIN_AGENTS", on()),
        ("CLAUDE_AGENT_SDK_DISABLE_BUILTIN_AGENTS", on()),
        ("CLAUDE_CODE_DISABLE_BACKGROUND_TASKS", on()),
        ("CLAUDE_CODE_DISABLE_TERMINAL_TITLE", on()),
        ("MCP_TOOL_TIMEOUT", PARK_TIMEOUT_MS.to_string()),
        ("CLAUDE_CODE_TOTAL_TOKENS_REMINDER", "off".to_owned()),
        ("CLAUDE_CODE_ENABLE_FUNCTION_HOOKS", on()),
    ]
}

/// `os.homedir()`.
pub(super) fn os_homedir() -> Option<String> {
    std::env::home_dir().map(|home| home.to_string_lossy().into_owned())
}

/// `os.tmpdir()`.
pub(super) fn os_tmpdir() -> String {
    let dir = if cfg!(windows) {
        allowed_env("TEMP").or_else(|| allowed_env("TMP")).unwrap_or_else(|| {
            let root = allowed_env("SystemRoot").or_else(|| allowed_env("windir")).unwrap_or_default();
            format!("{root}\\temp")
        })
    } else {
        allowed_env("TMPDIR").or_else(|| allowed_env("TMP")).or_else(|| allowed_env("TEMP")).unwrap_or_else(|| "/tmp".to_owned())
    };
    let trailing = if cfg!(windows) { '\\' } else { '/' };
    if dir.len() > 1 && dir.ends_with(trailing) && !(cfg!(windows) && dir.ends_with(":\\")) {
        return dir[..dir.len() - 1].to_owned();
    }
    dir
}

/// `os.userInfo().username`: the passwd entry of this uid, whatever the environment says.
#[cfg(unix)]
fn os_username() -> Option<String> {
    use std::ffi::{CStr, c_char, c_int};

    /// `struct passwd` starts with `char *pw_name` on every Unix; the rest of it fits this block on every one of them.
    #[repr(C)]
    struct Passwd {
        name: *mut c_char,
        rest: [u64; 15],
    }

    unsafe extern "C" {
        fn getuid() -> u32;
        fn getpwuid_r(uid: u32, entry: *mut Passwd, buffer: *mut c_char, length: usize, result: *mut *mut Passwd) -> c_int;
    }

    let mut entry = Passwd { name: std::ptr::null_mut(), rest: [0; 15] };
    let mut buffer = vec![0 as c_char; 16 * 1024];
    let mut result: *mut Passwd = std::ptr::null_mut();
    // SAFETY: every pointer is to live storage of the stated size, and the strings read below point into `buffer`.
    unsafe {
        let status = getpwuid_r(getuid(), &mut entry, buffer.as_mut_ptr(), buffer.len(), &mut result);
        if status != 0 || result.is_null() || entry.name.is_null() {
            return None;
        }
        Some(CStr::from_ptr(entry.name).to_string_lossy().into_owned())
    }
}

/// The root of a Windows path, as `path.win32.parse(home).root` reads the two forms a home directory takes.
fn win32_root(home: &str) -> &str {
    let bytes = home.as_bytes();
    if bytes.len() >= 3 && bytes[1] == b':' && matches!(bytes[2], b'\\' | b'/') {
        return &home[..3];
    }
    if bytes.len() >= 2 && bytes[1] == b':' {
        return &home[..2];
    }
    if home.starts_with("\\\\") {
        // `\\server\share\`.
        let mut slashes = home.char_indices().filter(|(_, c)| *c == '\\').skip(3);
        if let Some((share_end, _)) = slashes.next() {
            return &home[..=share_end];
        }
    }
    ""
}

/// `defaultProfileEnv`.
fn default_profile_env() -> Vec<(&'static str, String)> {
    let home = os_homedir().unwrap_or_default();
    if cfg!(windows) {
        let root = win32_root(&home);
        let home_path = home.get(root.len().saturating_sub(1)..).filter(|path| !path.is_empty()).unwrap_or("\\");
        return vec![
            ("USERPROFILE", home.clone()),
            ("HOMEDRIVE", root.trim_end_matches(['\\', '/']).to_owned()),
            ("HOMEPATH", home_path.to_owned()),
            ("APPDATA", format!("{home}\\AppData\\Roaming")),
            ("LOCALAPPDATA", format!("{home}\\AppData\\Local")),
        ];
    }
    // The CLI names its macOS Keychain account after `$USER`; without it the CLI looks up a different account, and the
    // sidecar's cleared environment would hide a signed-in user's login.
    #[allow(unused_mut)]
    let mut env = vec![("HOME", home)];
    #[cfg(unix)]
    if let Some(user) = os_username() {
        env.push(("USER", user));
    }
    env
}

/// `isLoopbackEndpoint`: whether an endpoint address belongs to this machine. `0.0.0.0` is a wildcard bind address rather
/// than a destination and does not qualify.
pub(super) fn is_loopback_endpoint(value: &str) -> bool {
    let Some(parsed) = url::parse(value) else { return false };
    let lowered = parsed.host.to_lowercase();
    let host = lowered.strip_prefix('[').unwrap_or(&lowered);
    let host = host.strip_suffix(']').unwrap_or(host);
    if host == "localhost" || host == "::1" {
        return true;
    }
    let octets: Vec<&str> = host.split('.').collect();
    octets.len() == 4
        && octets.iter().all(|octet| (1..=3).contains(&octet.len()) && octet.bytes().all(|b| b.is_ascii_digit()) && octet.parse::<u32>().is_ok_and(|n| n <= 255))
        && octets[0] == "127"
}

/// `agentEnvOverrides`: `agent.env` after the upstream rule. Everything but the three authentication variables passes
/// through; those exist for one purpose only, a local test stub, so a remote endpoint fails the whole request instead of
/// being dropped quietly, and a key without such an endpoint is discarded.
fn agent_env_overrides(agent: &Json) -> Result<Vec<(String, String)>, String> {
    let empty = Json::obj();
    let source = agent.get("env").filter(|env| env.is_obj()).unwrap_or(&empty);
    let mut overrides = Vec::new();
    for (name, value) in source.as_obj().map(|env| env.iter().collect::<Vec<_>>()).unwrap_or_default() {
        if let Some(value) = value.as_str() {
            if !UPSTREAM_OVERRIDE_ENV.contains(&name) {
                overrides.push((name.to_owned(), value.to_owned()));
            }
        }
    }
    let base_url = source.str_at("ANTHROPIC_BASE_URL").map_or("", js_trim);
    if !base_url.is_empty() {
        if !is_loopback_endpoint(base_url) {
            return Err("agent.env 里的 ANTHROPIC_BASE_URL 只允许本机测试桩".to_owned());
        }
        overrides.push(("ANTHROPIC_BASE_URL".to_owned(), base_url.to_owned()));
    }
    for name in ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"] {
        let Some(key) = source.str_at(name).filter(|key| !key.is_empty()) else { continue };
        if base_url.is_empty() {
            log(&format!("丢弃 agent.env 里的 {name}：没有随附本机测试桩地址"));
            continue;
        }
        overrides.push((name.to_owned(), key.to_owned()));
    }
    Ok(overrides)
}

/// `cliModelId`: the model id the CLI is started with. Mewrk keeps a model without Claude Code's `[1m]` suffix and states
/// its context window instead, and the suffix is derived here: a window above the CLI's standard budget asks for the 1M
/// one. The CLI enforces its budget locally, before any request; auto-compact is off, so a prompt past it ends the step
/// with "Prompt is too long", and a bare id's budget is only 200k under a Console login. The suffix never reaches the API.
pub(super) fn cli_model_id(model_id: &str, context_window: Option<f64>) -> String {
    let suffixed = model_id.len() >= 4 && model_id.is_char_boundary(model_id.len() - 4) && model_id[model_id.len() - 4..].eq_ignore_ascii_case("[1m]");
    if suffixed {
        return model_id.to_owned();
    }
    if context_window.is_some_and(|window| window > STANDARD_CONTEXT_BUDGET) {
        format!("{model_id}[1m]")
    } else {
        model_id.to_owned()
    }
}

/// `pinnedModelEnv`: the four model variables the CLI consults for its own routing. Only a real model id is pinned: an
/// alias is the CLI's own vocabulary, and feeding it back through the environment would only make it resolve to itself.
fn pinned_model_env(model_id: &str) -> Vec<(&'static str, String)> {
    let pinned = model_id.get(..7).is_some_and(|prefix| prefix.eq_ignore_ascii_case("claude-"));
    if !pinned {
        return Vec::new();
    }
    ["ANTHROPIC_MODEL", "ANTHROPIC_DEFAULT_OPUS_MODEL", "ANTHROPIC_DEFAULT_SONNET_MODEL", "ANTHROPIC_DEFAULT_HAIKU_MODEL"]
        .into_iter()
        .map(|name| (name, model_id.to_owned()))
        .collect()
}

/// `cliEnv`, the environment Mewrk composes for the CLI.
pub(super) fn cli_env(agent: &Json, model_id: &str, max_output_tokens: Option<f64>) -> Result<BTreeMap<String, String>, String> {
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    for name in SIDECAR_ENV {
        if let Some(value) = allowed_env(name) {
            env.insert(name.to_owned(), value);
        }
    }
    // The SDK sets this on its own process as it loads, so `process.env` held it by the time this ran.
    env.insert("NoDefaultCurrentDirectoryInExePath".to_owned(), "1".to_owned());
    // The CLI must authenticate with its own login. A developer shell's ambient credentials would override it silently,
    // and the CLI only falls back to the stored login when no credential is forced through the environment.
    for name in CREDENTIAL_ENV {
        env.remove(name);
    }
    env.remove("CLAUDE_CODE_ENTRYPOINT");
    for (name, value) in default_profile_env() {
        env.insert(name.to_owned(), value);
    }
    for (name, value) in cli_control_env() {
        env.insert(name.to_owned(), value);
    }
    for name in ["CLAUDE_CODE_USE_BEDROCK", "CLAUDE_CODE_USE_VERTEX", "CLAUDE_CODE_USE_FOUNDRY"] {
        env.insert(name.to_owned(), "0".to_owned());
    }
    for (name, value) in agent_env_overrides(agent)? {
        env.insert(name, value);
    }
    for (name, value) in pinned_model_env(model_id) {
        env.insert(name.to_owned(), value);
    }
    // Where the CLI keeps its login (the Keychain entry name, the plaintext `.credentials.json`, the lock that serializes
    // token refreshes) follows `CLAUDE_CONFIG_DIR` unless this names it. A resumed session runs under a temporary
    // `CLAUDE_CONFIG_DIR`; pinning the storage to the user's own directory (empty: the default `~/.claude`) keeps every
    // session on the real login.
    let storage = env.get("CLAUDE_CONFIG_DIR").cloned().unwrap_or_default();
    env.insert("CLAUDE_SECURESTORAGE_CONFIG_DIR".to_owned(), storage);
    if let Some(tokens) = max_output_tokens.filter(|tokens| tokens.is_finite() && *tokens > 0.0) {
        env.insert("CLAUDE_CODE_MAX_OUTPUT_TOKENS".to_owned(), Json::from(tokens.floor()).to_text());
    }
    Ok(env)
}

/// What the SDK's `query()` adds to the environment it is given, before and at spawn.
pub(super) fn sdk_env(env: &mut BTreeMap<String, String>, sdk_version: &str) {
    if env.get("CLAUDE_CODE_ENTRYPOINT").is_none_or(String::is_empty) {
        env.insert("CLAUDE_CODE_ENTRYPOINT".to_owned(), "sdk-ts".to_owned());
    }
    if env.get("CLAUDE_AGENT_SDK_VERSION").is_none_or(String::is_empty) {
        env.insert("CLAUDE_AGENT_SDK_VERSION".to_owned(), sdk_version.to_owned());
    }
    if !env.keys().any(|name| name.eq_ignore_ascii_case("CLAUDE_CODE_SDK_READS_SESSION_STATE")) {
        env.insert("CLAUDE_CODE_SDK_READS_SESSION_STATE".to_owned(), "1".to_owned());
    }
    env.remove("NODE_OPTIONS");
    let debug = env.get("DEBUG_CLAUDE_AGENT_SDK").is_some_and(|value| matches!(value.trim().to_lowercase().as_str(), "1" | "true" | "yes" | "on"));
    if debug {
        env.insert("DEBUG".to_owned(), "1".to_owned());
    } else {
        env.remove("DEBUG");
    }
}

/// `validateAgent`.
pub(super) fn validate_agent(agent: Option<&Json>) -> Result<&Json, String> {
    let agent = agent.filter(|agent| !matches!(agent, Json::Null | Json::Bool(false))).ok_or("claude-agent 请求缺少 agent 会话参数")?;
    let executable = agent.str_at("executable").filter(|path| !path.is_empty()).ok_or("claude-agent 请求缺少 Claude Code 可执行文件路径")?;
    if !Path::new(executable).exists() {
        return Err(format!("找不到 Claude Code：{executable}。{REINSTALL_HINT}"));
    }
    agent.str_at("cwd").filter(|path| !path.is_empty()).ok_or("claude-agent 请求缺少工作目录")?;
    Ok(agent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobj;

    #[test]
    fn only_this_machine_is_loopback() {
        for yes in ["http://127.0.0.1:9", "http://localhost:9/x", "http://LOCALHOST", "http://[::1]:9", "http://127.1.2.3", "http://127.1"] {
            assert!(is_loopback_endpoint(yes), "{yes}");
        }
        for no in [
            "https://relay.example.com",
            "http://0.0.0.0:9",
            "http://127.0.0.1@evil.example",
            "http://evil.example\\@127.0.0.1",
            "http://127.0.0.1.evil.example",
            "http://localhost.evil.example",
            "http://[::2]",
            "127.0.0.1:9",
            "not a url",
            "",
        ] {
            assert!(!is_loopback_endpoint(no), "{no}");
        }
    }

    #[test]
    fn a_stub_key_needs_a_stub_endpoint() {
        let remote = jobj! { "env": jobj! { "ANTHROPIC_BASE_URL": "https://relay.example.com", "ANTHROPIC_API_KEY": "k" } };
        assert_eq!(agent_env_overrides(&remote).unwrap_err(), "agent.env 里的 ANTHROPIC_BASE_URL 只允许本机测试桩");
        let keyless = jobj! { "env": jobj! { "ANTHROPIC_API_KEY": "k", "ANTHROPIC_AUTH_TOKEN": "t", "USER": "u" } };
        assert_eq!(agent_env_overrides(&keyless).unwrap(), vec![("USER".to_owned(), "u".to_owned())]);
        let local = jobj! { "env": jobj! { "ANTHROPIC_BASE_URL": " http://127.0.0.1:9 ", "ANTHROPIC_API_KEY": "k", "ODD": 1u64 } };
        assert_eq!(
            agent_env_overrides(&local).unwrap(),
            vec![("ANTHROPIC_BASE_URL".to_owned(), "http://127.0.0.1:9".to_owned()), ("ANTHROPIC_API_KEY".to_owned(), "k".to_owned())]
        );
    }

    #[test]
    fn the_cli_model_carries_the_budget() {
        assert_eq!(cli_model_id("claude-sonnet-5", Some(1_000_000.0)), "claude-sonnet-5[1m]");
        assert_eq!(cli_model_id("claude-sonnet-5", Some(200_000.0)), "claude-sonnet-5");
        assert_eq!(cli_model_id("claude-sonnet-5", None), "claude-sonnet-5");
        assert_eq!(cli_model_id("claude-sonnet-5[1M]", Some(1.0)), "claude-sonnet-5[1M]");
        assert_eq!(pinned_model_env("CLAUDE-x").len(), 4);
        assert!(pinned_model_env("sonnet").is_empty());
        assert!(pinned_model_env("opus[1m]").is_empty());
    }

    #[test]
    fn the_environment_composes_in_upstream_order() {
        let agent = jobj! { "env": jobj! {
            "HOME": "/h", "USER": "me", "CLAUDE_CONFIG_DIR": "/cfg",
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "0",
            "ANTHROPIC_BASE_URL": "http://127.0.0.1:1", "ANTHROPIC_API_KEY": "stub",
            "NODE_OPTIONS": "--inspect", "DEBUG": "*",
        } };
        let mut env = cli_env(&agent, "claude-opus-5-5[1m]", Some(4096.9)).unwrap();
        sdk_env(&mut env, "9.9.9");
        assert_eq!(env["HOME"], "/h");
        assert_eq!(env["USER"], "me");
        assert_eq!(env["CLAUDE_SECURESTORAGE_CONFIG_DIR"], "/cfg");
        // `agent.env` is applied after the switches, as `Object.assign` did.
        assert_eq!(env["CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"], "0");
        assert_eq!(env["ANTHROPIC_MODEL"], "claude-opus-5-5[1m]");
        assert_eq!(env["CLAUDE_CODE_MAX_OUTPUT_TOKENS"], "4096");
        assert_eq!(env["MCP_TOOL_TIMEOUT"], "604800000");
        assert_eq!(env["CLAUDE_CODE_USE_BEDROCK"], "0");
        assert_eq!(env["CLAUDE_CODE_USE_VERTEX"], "0");
        assert_eq!(env["CLAUDE_CODE_USE_FOUNDRY"], "0");
        assert_eq!(env["CLAUDE_CODE_ENTRYPOINT"], "sdk-ts");
        assert_eq!(env["CLAUDE_AGENT_SDK_VERSION"], "9.9.9");
        assert_eq!(env["CLAUDE_CODE_SDK_READS_SESSION_STATE"], "1");
        assert!(!env.contains_key("NODE_OPTIONS") && !env.contains_key("DEBUG"));
        assert!(!env.contains_key("ANTHROPIC_AUTH_TOKEN") && !env.contains_key("CLAUDE_CODE_OAUTH_TOKEN"));
    }

    #[test]
    fn a_bad_agent_says_what_is_missing() {
        assert_eq!(validate_agent(None).unwrap_err(), "claude-agent 请求缺少 agent 会话参数");
        assert_eq!(validate_agent(Some(&Json::obj())).unwrap_err(), "claude-agent 请求缺少 Claude Code 可执行文件路径");
        let agent = jobj! { "executable": "/no/such/claude" };
        assert!(validate_agent(Some(&agent)).unwrap_err().contains("重新安装"));
    }

    #[test]
    #[cfg(unix)]
    fn the_passwd_entry_is_readable() {
        assert!(os_username().is_some_and(|name| !name.is_empty()));
    }
}
