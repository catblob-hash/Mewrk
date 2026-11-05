//! What `@anthropic-ai/claude-agent-sdk`'s `query()` did for this family: the CLI's argv, the transcript it resumes from,
//! and the wire frames of its stream-json control protocol. Upstream loaded the SDK at run time; there is no JavaScript
//! here, so the protocol is spoken directly. The session logic that drives these pieces is in the module above.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::json::{Json, Obj};
use crate::{jarr, jobj};

use super::env::{PARK_TIMEOUT_MS, os_homedir, os_tmpdir};
use super::shapes::{SERVER_NAME, random_uuid};

/// Lines of CLI stderr retained for error messages.
const STDERR_TAIL_LINES: usize = 40;
/// Longest path a project directory name is taken from whole (`ao`).
const MAX_PROJECT_DIR: usize = 200;
/// Suffixes of the OAuth-environment config files the SDK copies alongside `.claude.json` (`mO`).
const CONFIG_SUFFIXES: [&str; 3] = ["-staging-oauth", "-local-oauth", "-custom-oauth"];
/// Plugin configuration a resumed session must not inherit (`Uze`).
const STRIPPED_SETTINGS: [&str; 3] = ["enabledPlugins", "extraKnownMarketplaces", "additionalMarketplaces"];

/// `ProcessTransport.initialize`'s argument list, for the options `startSession` passed.
pub(super) struct CliArgs<'a> {
    pub effort: Option<&'a str>,
    pub model: &'a str,
    /// `--resume=<id>` with the session store's mirror.
    pub resume: Option<&'a str>,
    /// The one built-in a native session gets; none otherwise.
    pub builtin: Option<&'a str>,
    pub plugin: &'a Path,
    pub settings: &'a Json,
}

impl CliArgs<'_> {
    pub fn to_vec(&self) -> Vec<String> {
        let mut args: Vec<String> = ["--output-format", "stream-json", "--verbose", "--input-format", "stream-json"].map(str::to_owned).to_vec();
        if let Some(effort) = self.effort.filter(|effort| !effort.is_empty()) {
            args.extend(["--effort".to_owned(), effort.to_owned()]);
        }
        if !self.model.is_empty() {
            args.extend(["--model".to_owned(), self.model.to_owned()]);
        }
        // The `canUseTool` callback: the CLI asks over the control channel.
        args.extend(["--permission-prompt-tool".to_owned(), "stdio".to_owned()]);
        if let Some(resume) = self.resume {
            args.push(format!("--resume={resume}"));
        }
        args.extend(["--tools".to_owned(), self.builtin.unwrap_or("").to_owned()]);
        args.push("--setting-sources=".to_owned());
        args.push("--strict-mcp-config".to_owned());
        args.extend(["--permission-mode".to_owned(), "default".to_owned()]);
        args.push("--include-partial-messages".to_owned());
        if self.resume.is_some() {
            args.push("--session-mirror".to_owned());
        }
        args.extend(["--plugin-dir".to_owned(), self.plugin.to_string_lossy().into_owned()]);
        if self.resume.is_none() {
            args.push("--no-session-persistence".to_owned());
        }
        args.extend(["--settings".to_owned(), self.settings.to_text()]);
        args
    }
}

/// A path ending in a script extension runs under `node`; anything else is the native binary (`WFe`).
pub(super) fn is_native_binary(path: &str) -> bool {
    ![".js", ".mjs", ".tsx", ".ts", ".jsx"].iter().any(|extension| path.ends_with(extension))
}

/// The message of a CLI that would not start.
pub(super) fn spawn_error(error: &io::Error, executable: &str) -> String {
    let native = is_native_binary(executable);
    let launch_problem = matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied | io::ErrorKind::NotADirectory | io::ErrorKind::InvalidFilename | io::ErrorKind::ReadOnlyFilesystem
    );
    if !launch_problem {
        return format!("Failed to spawn Claude Code process: {error}");
    }
    match (Path::new(executable).exists(), native) {
        (true, true) => format!(
            "Claude Code native binary at {executable} exists but failed to launch. This usually means the binary does not match this system's libc — e.g. spawning a musl-linked binary on a glibc Linux host fails because the musl dynamic loader (/lib/ld-musl-*) is missing. Specify a matching binary with options.pathToClaudeCodeExecutable."
        ),
        (true, false) => format!("Claude Code executable at {executable} exists but failed to launch."),
        (false, true) => format!(
            "Claude Code native binary not found at {executable}. Please ensure Claude Code is installed via native installer or specify a valid path with options.pathToClaudeCodeExecutable."
        ),
        (false, false) => format!("Claude Code executable not found at {executable}. Is options.pathToClaudeCodeExecutable set?"),
    }
}

/// The name of a signal, as Node reports `signalCode`.
#[cfg(unix)]
pub(super) fn signal_name(signal: i32) -> String {
    match signal {
        1 => "SIGHUP".to_owned(),
        2 => "SIGINT".to_owned(),
        3 => "SIGQUIT".to_owned(),
        4 => "SIGILL".to_owned(),
        6 => "SIGABRT".to_owned(),
        8 => "SIGFPE".to_owned(),
        9 => "SIGKILL".to_owned(),
        10 if cfg!(target_os = "macos") => "SIGBUS".to_owned(),
        11 => "SIGSEGV".to_owned(),
        13 => "SIGPIPE".to_owned(),
        15 => "SIGTERM".to_owned(),
        other => format!("SIG{other}"),
    }
}

// ---------------------------------------------------------------- the control protocol

fn random_request_id() -> String {
    // `Math.random().toString(36).substring(2, 15)`.
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    (0..13).map(|_| ALPHABET[fastrand::usize(..ALPHABET.len())] as char).collect()
}

/// The `initialize` control request (`buildInitializeRequest`): its id, and the line to write.
pub(super) fn initialize_request(manifest: Json, system_prompt: &str) -> (String, String) {
    let request_id = random_request_id();
    let mut configs = Obj::new();
    configs.insert(SERVER_NAME, jobj! { "timeout": PARK_TIMEOUT_MS });
    let mut manifests = Obj::new();
    manifests.insert(SERVER_NAME, manifest);
    let frame = jobj! {
        "request_id": request_id.as_str(),
        "type": "control_request",
        "request": jobj! {
            "subtype": "initialize",
            "sdkMcpServers": jarr![SERVER_NAME],
            "sdkMcpServerConfigs": configs,
            "sdkMcpServerManifests": manifests,
            "systemPrompt": jarr![system_prompt],
            // Rendered fresh on every request, never recorded. By default the CLI records a session's system prompt on
            // its first request and replays that record afterwards, even across a resume that passes different text; the
            // host rebuilds its prompt every step, and only the text it sent may reach the model.
            "systemPromptSnapshot": false,
        },
    };
    (request_id, frame.to_text())
}

/// The prompt the session starts with.
pub(super) fn user_message(content: Json) -> String {
    jobj! {
        "type": "user",
        "message": jobj! { "role": "user", "content": content },
        "parent_tool_use_id": Json::Null,
    }
    .to_text()
}

pub(super) fn control_success(request_id: &str, response: Json) -> String {
    jobj! {
        "type": "control_response",
        "response": jobj! { "subtype": "success", "request_id": request_id, "response": response },
    }
    .to_text()
}

pub(super) fn control_error(request_id: &str, error: &str) -> String {
    jobj! {
        "type": "control_response",
        "response": jobj! { "subtype": "error", "request_id": request_id, "error": error },
    }
    .to_text()
}

/// A message from the server to the CLI that answers nothing: it travels as a control request of its own.
pub(super) fn mcp_notification(message: Json) -> String {
    jobj! {
        "type": "control_request",
        "request_id": random_uuid(),
        "request": jobj! { "subtype": "mcp_message", "server_name": SERVER_NAME, "message": message },
    }
    .to_text()
}

/// Ring buffer of CLI stderr for diagnostics (`StderrTail`).
#[derive(Default)]
pub(super) struct StderrTail {
    lines: Vec<String>,
    /// The start of a UTF-8 sequence a chunk cut, kept for the next one, as a string decoder does.
    partial: Vec<u8>,
}

impl StderrTail {
    pub fn push_bytes(&mut self, bytes: &[u8]) {
        self.partial.extend_from_slice(bytes);
        let text = match std::str::from_utf8(&self.partial) {
            Ok(text) => {
                let text = text.to_owned();
                self.partial.clear();
                text
            }
            Err(error) if error.error_len().is_none() => {
                let valid = error.valid_up_to();
                let text = String::from_utf8_lossy(&self.partial[..valid]).into_owned();
                self.partial.drain(..valid);
                text
            }
            Err(_) => {
                let text = String::from_utf8_lossy(&self.partial).into_owned();
                self.partial.clear();
                text
            }
        };
        self.push(&text);
    }

    fn push(&mut self, chunk: &str) {
        for line in chunk.split('\n') {
            let line = line.strip_suffix('\r').unwrap_or(line);
            if line.is_empty() {
                continue;
            }
            self.lines.push(crate::step::js_slice(line, 500));
            if self.lines.len() > STDERR_TAIL_LINES {
                self.lines.remove(0);
            }
        }
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }
}

// ---------------------------------------------------------------- the session store's resume

/// `normalize("NFC")` as far as a project directory name can tell: there is no Unicode table in this crate, and every
/// non-alphanumeric unit becomes a `-` anyway, so what matters is how many units a sequence composes into. That is one
/// for the sequences a decomposed macOS path holds: a letter and its combining mark, a kana and its voicing mark, a
/// Hangul syllable's jamo.
fn composed_units(path: &str) -> Vec<char> {
    let is_mark = |c: char| matches!(c, '\u{300}'..='\u{36F}' | '\u{3099}' | '\u{309A}');
    let mut out: Vec<char> = Vec::with_capacity(path.len());
    let mut chars = path.chars().peekable();
    while let Some(c) = chars.next() {
        if matches!(c, '\u{1100}'..='\u{1112}') && chars.peek().is_some_and(|next| matches!(next, '\u{1161}'..='\u{1175}')) {
            chars.next();
            if chars.peek().is_some_and(|next| matches!(next, '\u{11A8}'..='\u{11C2}')) {
                chars.next();
            }
            out.push('\u{AC00}');
            continue;
        }
        if c.is_alphabetic() && chars.peek().is_some_and(|next| is_mark(*next)) {
            while chars.peek().is_some_and(|next| is_mark(*next)) {
                chars.next();
            }
            // A composed letter is outside `[a-zA-Z0-9]` whatever it started as.
            out.push('\u{C0}');
            continue;
        }
        out.push(c);
    }
    out
}

/// `by`: the string hash the CLI suffixes a long project directory with.
fn path_hash(path: &str) -> i32 {
    path.encode_utf16().fold(0i32, |hash, unit| hash.wrapping_shl(5).wrapping_sub(hash).wrapping_add(i32::from(unit)))
}

fn base36(mut value: u64) -> String {
    if value == 0 {
        return "0".to_owned();
    }
    let mut digits = Vec::new();
    while value > 0 {
        digits.push(char::from_digit((value % 36) as u32, 36).unwrap_or('0'));
        value /= 36;
    }
    digits.iter().rev().collect()
}

/// `Ac`: a path as a project directory name, which is how the CLI finds a transcript.
fn sanitized_path(path: &str) -> String {
    let mut name = String::with_capacity(path.len());
    let chars = if cfg!(target_os = "macos") { composed_units(path) } else { path.chars().collect() };
    for c in chars {
        if c.is_ascii_alphanumeric() {
            name.push(c);
        } else {
            for _ in 0..c.len_utf16() {
                name.push('-');
            }
        }
    }
    if name.len() <= MAX_PROJECT_DIR {
        return name;
    }
    format!("{}-{}", &name[..MAX_PROJECT_DIR], base36(i64::from(path_hash(path)).unsigned_abs()))
}

/// What `CLAUDE_CODE_PROJECT_DIR_NAME` may be (`$_`).
fn valid_project_dir_name(name: &str) -> bool {
    let reserved = {
        let lower = name.to_ascii_lowercase();
        matches!(lower.as_str(), "con" | "prn" | "aux" | "nul")
            || ((lower.starts_with("com") || lower.starts_with("lpt")) && lower.len() == 4 && lower.as_bytes()[3].is_ascii_digit())
    };
    (1..=64).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')) && !reserved
}

/// `realpath` of an absolute path, in the plain spelling Node gives it.
fn real_path(path: &str) -> String {
    let real = fs::canonicalize(path).map_or_else(|_| path.to_owned(), |real| real.to_string_lossy().into_owned());
    match real.strip_prefix(r"\\?\UNC\") {
        Some(share) => format!(r"\\{share}"),
        None => real.strip_prefix(r"\\?\").map_or(real.clone(), str::to_owned),
    }
}

/// `cn`: the directory under `projects/` the CLI will look in for this working directory.
pub(super) fn project_key(cwd: &str, env: &BTreeMap<String, String>) -> String {
    if env.get("CLAUDE_CONFIG_DIR").is_some_and(|dir| !dir.is_empty()) {
        if let Some(name) = env.get("CLAUDE_CODE_PROJECT_DIR_NAME").filter(|name| valid_project_dir_name(name)) {
            return name.clone();
        }
    }
    sanitized_path(&real_path(cwd))
}

/// Drops what a resumed session must not inherit from a settings file (`IW`).
fn scrub_settings(bytes: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(bytes);
    let Ok(mut parsed) = Json::parse(text.strip_prefix('\u{FEFF}').unwrap_or(&text)) else { return bytes.to_vec() };
    let Some(object) = parsed.as_obj_mut() else { return bytes.to_vec() };
    let mut changed = false;
    for name in STRIPPED_SETTINGS {
        changed |= object.remove(name).is_some();
    }
    if let Some(env) = object.get_mut("env").and_then(Json::as_obj_mut) {
        changed |= env.remove("CLAUDE_CONFIG_DIR").is_some();
    }
    if changed { parsed.to_text().into_bytes() } else { bytes.to_vec() }
}

fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)
}

/// Copies a configuration file if it is there. A failure leaves the session without it, as the SDK's did.
fn copy_config(from: &Path, to: &Path, transform: Option<fn(&[u8]) -> Vec<u8>>) {
    let copied = match transform {
        Some(transform) => fs::read(from).and_then(|bytes| write_private(to, &transform(&bytes))),
        None => fs::copy(from, to).map(|_| ()),
    };
    if copied.is_err() {
        let _ = fs::remove_file(to);
    }
}

/// `FW`: writes the transcript where the CLI will look for it, in a temporary `CLAUDE_CONFIG_DIR`, with the user's
/// configuration beside it. `None` when the transcript is empty: the CLI is then started under the user's own directory.
///
/// One thing the SDK did here is left out on purpose: it also dropped a refresh-token-less copy of the login into that
/// directory, read from `.credentials.json` or the macOS Keychain. The CLI never reads that copy, because
/// `CLAUDE_SECURESTORAGE_CONFIG_DIR` (always set by `cli_env`) makes it look in the user's own storage, so this family
/// keeps its promise of never reading, copying or moving the user's credentials.
pub(super) fn materialize_resume(session_id: &str, cwd: &str, env: &BTreeMap<String, String>, transcript: &[Json]) -> io::Result<Option<PathBuf>> {
    if transcript.is_empty() {
        return Ok(None);
    }
    let root = PathBuf::from(os_tmpdir()).join(format!("claude-resume-{}", random_uuid()));
    let written = write_resume(&root, session_id, cwd, env, transcript);
    if let Err(error) = written {
        let _ = fs::remove_dir_all(&root);
        return Err(error);
    }
    Ok(Some(root))
}

fn write_resume(root: &Path, session_id: &str, cwd: &str, env: &BTreeMap<String, String>, transcript: &[Json]) -> io::Result<()> {
    let project = root.join("projects").join(project_key(cwd, env));
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&project)?;
    let mut lines = String::new();
    for entry in transcript {
        lines.push_str(&entry.to_text());
        lines.push('\n');
    }
    write_private(&project.join(format!("{session_id}.jsonl")), lines.as_bytes())?;

    let own_dir = env.get("CLAUDE_CONFIG_DIR").filter(|dir| !dir.is_empty()).cloned();
    let home = os_homedir().unwrap_or_default();
    let config_dir = PathBuf::from(own_dir.clone().unwrap_or_else(|| format!("{home}{}.claude", std::path::MAIN_SEPARATOR)));
    let global_dir = PathBuf::from(own_dir.unwrap_or(home));
    copy_config(&global_dir.join(".claude.json"), &root.join(".claude.json"), None);
    copy_config(&config_dir.join(".config.json"), &root.join(".config.json"), None);
    for suffix in CONFIG_SUFFIXES {
        let name = format!(".claude{suffix}.json");
        copy_config(&global_dir.join(&name), &root.join(&name), None);
    }
    copy_config(&config_dir.join("settings.json"), &root.join("settings.json"), Some(scrub_settings));
    copy_config(&config_dir.join("cowork_settings.json"), &root.join("cowork_settings.json"), Some(scrub_settings));
    Ok(())
}

/// Removes a temporary config directory once its CLI is gone, retrying what a just-exited process can still hold (`fh`).
pub(super) async fn remove_resume_dir(dir: PathBuf) {
    for attempt in 0..5u64 {
        let target = dir.clone();
        let removed = tokio::task::spawn_blocking(move || fs::remove_dir_all(target)).await;
        match removed {
            Ok(Ok(())) => return,
            Ok(Err(error)) if error.kind() == io::ErrorKind::NotFound => return,
            Ok(Err(_)) if attempt < 4 => tokio::time::sleep(std::time::Duration::from_millis((attempt + 1) * 100)).await,
            _ => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_argv_is_the_sdks_for_a_fresh_session() {
        let settings = jobj! { "totalTokensReminder": "off" };
        let args = CliArgs { effort: Some("low"), model: "sonnet", resume: None, builtin: None, plugin: Path::new("/p"), settings: &settings };
        assert_eq!(
            args.to_vec().join(" "),
            r#"--output-format stream-json --verbose --input-format stream-json --effort low --model sonnet --permission-prompt-tool stdio --tools  --setting-sources= --strict-mcp-config --permission-mode default --include-partial-messages --plugin-dir /p --no-session-persistence --settings {"totalTokensReminder":"off"}"#
        );
    }

    #[test]
    fn the_argv_is_the_sdks_for_a_resumed_native_session() {
        let settings = jobj! { "totalTokensReminder": "off", "permissions": jobj! { "ask": jarr!["WebSearch"] } };
        let args = CliArgs { effort: None, model: "m[1m]", resume: Some("id"), builtin: Some("WebSearch"), plugin: Path::new("/p"), settings: &settings };
        assert_eq!(
            args.to_vec().join(" "),
            r#"--output-format stream-json --verbose --input-format stream-json --model m[1m] --permission-prompt-tool stdio --resume=id --tools WebSearch --setting-sources= --strict-mcp-config --permission-mode default --include-partial-messages --session-mirror --plugin-dir /p --settings {"totalTokensReminder":"off","permissions":{"ask":["WebSearch"]}}"#
        );
    }

    #[test]
    fn a_script_path_runs_under_node() {
        assert!(is_native_binary("/x/claude"));
        assert!(is_native_binary("/x/claude.exe"));
        assert!(!is_native_binary("/x/fake.mjs"));
        assert!(!is_native_binary("/x/fake.js"));
    }

    #[test]
    fn project_directories_are_named_the_way_the_cli_names_them() {
        assert_eq!(sanitized_path("/Users/me/my_app.v2"), "-Users-me-my-app-v2");
        assert_eq!(sanitized_path("/tmp/工程"), "-tmp---");
        // An astral character is two units, so two dashes.
        assert_eq!(sanitized_path("/a/😀"), "-a---");
        let long = format!("/{}", "x".repeat(300));
        let name = sanitized_path(&long);
        assert!(name.starts_with(&format!("-{}", "x".repeat(199))));
        assert_eq!(name.len(), 200 + 1 + base36(i64::from(path_hash(&long)).unsigned_abs()).len());
    }

    #[test]
    fn the_long_path_hash_is_the_javascript_one() {
        // `by` in `sdk.mjs`, evaluated in Node for these strings.
        assert_eq!(path_hash(""), 0);
        assert_eq!(path_hash("a"), 97);
        assert_eq!(path_hash("hello"), 99162322);
        assert_eq!(path_hash("the quick brown fox jumps over the lazy dog"), -2082818701);
        assert_eq!(path_hash("/Users/工程/😀"), -836003626);
        assert_eq!(base36(i64::from(path_hash("/Users/工程/😀")).unsigned_abs()), "dtqgiy");
        assert_eq!(base36(35), "z");
        assert_eq!(base36(36), "10");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn a_decomposed_path_counts_as_composed() {
        assert_eq!(sanitized_path("/cafe\u{301}"), "-caf-");
        assert_eq!(sanitized_path("/\u{1112}\u{1161}\u{11AB}"), "--");
        assert_eq!(sanitized_path("/か\u{3099}"), "--");
    }

    #[test]
    fn a_project_dir_name_is_honoured_only_under_a_config_dir() {
        let mut env = BTreeMap::new();
        env.insert("CLAUDE_CODE_PROJECT_DIR_NAME".to_owned(), "named".to_owned());
        assert_ne!(project_key("/no/such/dir", &env), "named");
        env.insert("CLAUDE_CONFIG_DIR".to_owned(), "/c".to_owned());
        assert_eq!(project_key("/no/such/dir", &env), "named");
        env.insert("CLAUDE_CODE_PROJECT_DIR_NAME".to_owned(), "nul".to_owned());
        assert_eq!(project_key("/no/such/dir", &env), "-no-such-dir");
    }

    #[test]
    fn settings_lose_what_a_resume_must_not_inherit() {
        let scrubbed = scrub_settings(br#"{"enabledPlugins":{"x":true},"model":"m","env":{"CLAUDE_CONFIG_DIR":"/c","A":"1"}}"#);
        assert_eq!(String::from_utf8(scrubbed).unwrap(), r#"{"model":"m","env":{"A":"1"}}"#);
        let untouched = br#"{ "model": "m" }"#;
        assert_eq!(scrub_settings(untouched), untouched);
        assert_eq!(scrub_settings(b"not json"), b"not json");
    }

    #[test]
    fn a_resume_directory_holds_the_transcript_and_no_credentials() {
        let own = std::env::temp_dir().join(format!("mewrk-resume-own-{}", random_uuid()));
        fs::create_dir_all(&own).unwrap();
        fs::write(own.join(".claude.json"), "{\"a\":1}").unwrap();
        fs::write(own.join(".credentials.json"), "{\"claudeAiOauth\":{\"accessToken\":\"secret\"}}").unwrap();
        fs::write(own.join("settings.json"), r#"{"enabledPlugins":{},"k":1}"#).unwrap();
        let mut env = BTreeMap::new();
        env.insert("CLAUDE_CONFIG_DIR".to_owned(), own.to_string_lossy().into_owned());
        assert!(materialize_resume("s", "/no/such/dir", &env, &[]).unwrap().is_none());
        let entries = [jobj! { "a": 1u64 }, jobj! { "b": 2u64 }];
        let root = materialize_resume("s", "/no/such/dir", &env, &entries).unwrap().unwrap();
        assert_eq!(fs::read_to_string(root.join("projects/-no-such-dir/s.jsonl")).unwrap(), "{\"a\":1}\n{\"b\":2}\n");
        assert_eq!(fs::read_to_string(root.join(".claude.json")).unwrap(), "{\"a\":1}");
        assert_eq!(fs::read_to_string(root.join("settings.json")).unwrap(), "{\"k\":1}");
        assert!(!root.join(".credentials.json").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(root.join("projects/-no-such-dir/s.jsonl")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(own);
    }

    #[test]
    fn stderr_keeps_a_bounded_tail_and_whole_characters() {
        let mut tail = StderrTail::default();
        let line = "错误\r\n".as_bytes();
        tail.push_bytes(&line[..2]);
        tail.push_bytes(&line[2..]);
        assert_eq!(tail.text(), "错误");
        for index in 0..50 {
            tail.push_bytes(format!("line {index}\n").as_bytes());
        }
        assert_eq!(tail.lines.len(), STDERR_TAIL_LINES);
        assert!(tail.text().ends_with("line 49"));
        assert!(tail.text().starts_with("line 10"));
    }
}
