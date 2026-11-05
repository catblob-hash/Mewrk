//! The `claude-agent` family end to end, ported from `aisdk-service/selfcheck-claude-agent.mjs`: the real Claude Code
//! executable through this crate against a scripted Anthropic upstream on 127.0.0.1, with a throwaway `HOME` and
//! `CLAUDE_CONFIG_DIR` and a stub key. Nothing leaves this machine: `agent.env` also points the CLI at a local proxy that
//! refuses everything, and each test asserts nobody tried it.
//!
//! The real executable comes from `MEWRK_CLAUDE_EXECUTABLE` (or `claude` on `PATH`); without one those tests skip
//! themselves. `MEWRK_AISDK_NODE_SIDECAR` and `MEWRK_CLAUDE_SDK`, the old Node sidecar and the SDK it loads, additionally
//! enable the differential runs. The stand-in tests need none of it: a `/bin/sh` script speaks the protocol from the CLI's
//! side of it, which is why this file is Unix-only.
#![cfg(unix)]

mod common;

use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{Outcome, Rust, comparable_frames, node_sidecar_path, step_frame};
use mewrk_aisdk::json::{Json, Obj};
use mewrk_aisdk::{jarr, jobj};

const HOST_PROMPT: &str = "You are Mewrk's assistant. HOST PROMPT MARKER 7f3a.";
/// Key the CLI is meant to use: it rides in `agent.env` next to the loopback stub.
const UPSTREAM_KEY: &str = "sk-ant-fake-selfcheck";
/// Key placed on the request only. Nothing it touches may ever leave the sidecar.
const REQUEST_ONLY_KEY: &str = "sk-ant-request-only-a41c";
const PNG_1X1: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";
const STEP_LIMIT: Duration = Duration::from_secs(90);

fn j(text: &str) -> Json {
    Json::parse(text).unwrap_or_else(|error| panic!("{error}: {text}"))
}

fn tools() -> Vec<Json> {
    vec![
        j(r#"{"name":"read_file","description":"Read a file from the workspace","inputSchema":{"type":"object","properties":{"path":{"type":"string","description":"Path"}},"required":["path"]}}"#),
        j(r#"{"name":"bash","description":"Run a shell command","inputSchema":{"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}}"#),
        j(r#"{"name":"box","description":"Inert delivery box for host notifications","inputSchema":{"type":"object","properties":{"none":{"type":"array","items":{"type":"string"},"maxItems":0}},"required":["none"],"additionalProperties":false}}"#),
    ]
}

fn late_tool() -> Json {
    j(r#"{"name":"late_tool","description":"Joined mid-run","inputSchema":{"type":"object","properties":{}}}"#)
}

fn user(text: &str) -> Json {
    jobj! { "role": "user", "content": text }
}

fn tool_result(id: &str, name: &str, output: Json) -> Json {
    jobj! {
        "role": "tool",
        "content": jarr![jobj! { "type": "tool-result", "toolCallId": id, "toolName": name, "output": output }],
    }
}

fn text_output(value: &str) -> Json {
    jobj! { "type": "text", "value": value }
}

/// The host's tool-append marker (Rust `tool_append::marker_message`).
fn tool_addition_marker(names: &[&str]) -> Json {
    let names: Vec<Json> = names.iter().map(|name| Json::from(*name)).collect();
    jobj! { "role": "system", "content": "", "providerOptions": jobj! { "mewrk": jobj! { "toolAddition": names } } }
}

// ---------------------------------------------------------------- scratch space

static NEXT_SCRATCH: AtomicU64 = AtomicU64::new(0);

/// A directory of its own, gone at the end of the test.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let unique = NEXT_SCRATCH.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("mewrk-ca-{tag}-{}-{unique}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn text(&self, name: &str) -> String {
        self.path(name).to_string_lossy().into_owned()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// ---------------------------------------------------------------- a local proxy that refuses everything

/// What keeps a test from reaching the real network: every CLI gets this as its `HTTPS_PROXY`.
struct DenyProxy {
    url: String,
    seen: Arc<Mutex<Vec<String>>>,
}

impl DenyProxy {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                record.lock().unwrap().push(line.trim().to_owned());
                let _ = reader.get_mut().write_all(b"HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
            }
        });
        Self { url, seen }
    }

    fn assert_untouched(&self) {
        assert_eq!(self.seen.lock().unwrap().clone(), Vec::<String>::new(), "the CLI tried to leave this machine");
    }
}

// ---------------------------------------------------------------- fake Anthropic upstream

#[derive(Clone, Debug)]
struct Seen {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Json,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
    }

    fn is_message_call(&self) -> bool {
        self.method == "POST" && self.url.starts_with("/v1/messages")
    }
}

/// One scripted response: a list of blocks, or an error body, or a stall before the first byte.
#[derive(Clone, Default)]
struct Script {
    blocks: Vec<Json>,
    status: Option<u16>,
    body: Option<String>,
    stop_reason: Option<&'static str>,
    delay: Option<Duration>,
}

impl Script {
    fn blocks(blocks: Vec<Json>) -> Self {
        Self { blocks, ..Self::default() }
    }

    fn text(text: &str) -> Self {
        Self::blocks(vec![text_block(text)])
    }
}

fn text_block(text: &str) -> Json {
    jobj! { "type": "text", "text": text }
}

fn thinking_block(thinking: &str, signature: &str) -> Json {
    jobj! { "type": "thinking", "thinking": thinking, "signature": signature }
}

fn tool_use_block(id: &str, name: &str, input: Json) -> Json {
    jobj! { "type": "tool_use", "id": id, "name": name, "input": input }
}

#[derive(Default)]
struct UpstreamState {
    requests: Vec<Seen>,
    queue: VecDeque<Script>,
    /// What a call gets when the queue is empty.
    fallback: Option<Script>,
}

struct Upstream {
    base: String,
    state: Arc<Mutex<UpstreamState>>,
}

impl Upstream {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let state: Arc<Mutex<UpstreamState>> = Arc::default();
        let shared = state.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let state = shared.clone();
                std::thread::spawn(move || serve_upstream(stream, &state));
            }
        });
        Self { base, state }
    }

    fn push(&self, script: Script) {
        self.state.lock().unwrap().queue.push_back(script);
    }

    /// Every call from now on, however many a CLI makes of one failure.
    fn answer_every_call(&self, script: Script) {
        self.state.lock().unwrap().fallback = Some(script);
    }

    fn requests(&self) -> Vec<Seen> {
        self.state.lock().unwrap().requests.clone()
    }

    /// `POST /v1/messages` in arrival order.
    fn calls(&self) -> Vec<Seen> {
        self.requests().into_iter().filter(Seen::is_message_call).collect()
    }

    fn last_call(&self) -> Seen {
        self.calls().pop().expect("the upstream saw a call")
    }

    /// HEAD `/api/hello` is the CLI's fire-and-forget TLS preconnect. Anything else would be traffic to explain.
    fn assert_no_stray_requests(&self) {
        let stray: Vec<String> = self
            .requests()
            .iter()
            .filter(|seen| !seen.is_message_call() && !(seen.method == "HEAD" && seen.url == "/api/hello"))
            .map(|seen| format!("{} {}", seen.method, seen.url))
            .collect();
        assert_eq!(stray, Vec::<String>::new());
    }

    /// The request-only key appears nowhere, the stub key authenticated every call, and no call carried a bearer: a CLI
    /// that reached for a stored login would be handing a real OAuth token to the stub.
    fn assert_only_the_stub_key(&self, key: &str) {
        let calls = self.calls();
        assert!(!calls.is_empty());
        for seen in self.requests() {
            let headers = format!("{:?}", seen.headers);
            assert!(!headers.contains(REQUEST_ONLY_KEY) && !seen.body.to_text().contains(REQUEST_ONLY_KEY), "{headers}");
        }
        for call in calls {
            assert_eq!(call.header("x-api-key"), Some(key), "{:?}", call.headers);
            assert_eq!(call.header("authorization"), None, "{:?}", call.headers);
        }
    }
}

fn serve_upstream(mut stream: TcpStream, state: &Mutex<UpstreamState>) {
    let _ = stream.set_nodelay(true);
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let mut parts = line.trim_end().splitn(3, ' ');
        let (method, url) = (parts.next().unwrap_or("").to_owned(), parts.next().unwrap_or("").to_owned());
        let mut headers = Vec::new();
        let (mut length, mut chunked) = (0usize, false);
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).unwrap_or(0) == 0 {
                return;
            }
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':') {
                let value = value.trim().to_owned();
                if name.eq_ignore_ascii_case("content-length") {
                    length = value.parse().unwrap_or(0);
                }
                if name.eq_ignore_ascii_case("transfer-encoding") && value.eq_ignore_ascii_case("chunked") {
                    chunked = true;
                }
                headers.push((name.to_ascii_lowercase(), value));
            }
        }
        let mut body = vec![0; length];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        if chunked {
            loop {
                let mut size = String::new();
                if reader.read_line(&mut size).unwrap_or(0) == 0 {
                    return;
                }
                let size = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
                let mut chunk = vec![0; size + 2];
                if reader.read_exact(&mut chunk).is_err() {
                    return;
                }
                if size == 0 {
                    break;
                }
                body.extend_from_slice(&chunk[..size]);
            }
        }
        let text = String::from_utf8_lossy(&body).into_owned();
        let parsed = Json::parse(&text).unwrap_or(Json::Str(text));
        let call = method == "POST" && url.starts_with("/v1/messages");
        let script = {
            let mut state = state.lock().unwrap();
            let model = parsed.str_at("model").unwrap_or("claude-fake").to_owned();
            state.requests.push(Seen { method: method.clone(), url: url.clone(), headers, body: parsed });
            call.then(|| (state.queue.pop_front().or_else(|| state.fallback.clone()).unwrap_or_else(|| Script::text("(no scripted response)")), model))
        };
        let Some((script, model)) = script else {
            let payload = format!(r#"{{"type":"error","error":{{"type":"not_found_error","message":"no route {url}"}}}}"#);
            let head = format!("HTTP/1.1 404 Not Found\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n", payload.len());
            let _ = stream.write_all(head.as_bytes());
            if method != "HEAD" {
                let _ = stream.write_all(payload.as_bytes());
            }
            continue;
        };
        if let Some(delay) = script.delay {
            std::thread::sleep(delay);
        }
        if let Some(status) = script.status {
            let payload = script.body.clone().unwrap_or_else(|| r#"{"type":"error","error":{"type":"api_error","message":"scripted failure"}}"#.to_owned());
            let head = format!("HTTP/1.1 {status} Scripted\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n", payload.len());
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(payload.as_bytes());
            continue;
        }
        let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nconnection: close\r\n\r\n";
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(sse_body(&script, &model).as_bytes());
        return;
    }
}

fn pieces(text: &str, size: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return vec![String::new()];
    }
    chars.chunks(size).map(|chunk| chunk.iter().collect()).collect()
}

/// `writeSse`.
fn sse_body(script: &Script, model: &str) -> String {
    let mut out = String::new();
    let mut send = |event: &str, data: Json| out.push_str(&format!("event: {event}\ndata: {data}\n\n"));
    send(
        "message_start",
        jobj! {
            "type": "message_start",
            "message": jobj! {
                "id": format!("msg_{:08x}", fastrand::u32(..)),
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": jarr![],
                "stop_reason": Json::Null,
                "stop_sequence": Json::Null,
                "usage": jobj! { "input_tokens": 123u64, "output_tokens": 1u64, "cache_creation_input_tokens": 7u64, "cache_read_input_tokens": 20u64 },
            },
        },
    );
    let mut stop = "end_turn";
    for (index, block) in script.blocks.iter().enumerate() {
        match block.str_at("type") {
            Some("text") => {
                send("content_block_start", jobj! { "type": "content_block_start", "index": index, "content_block": jobj! { "type": "text", "text": "" } });
                for piece in pieces(block.str_at("text").unwrap_or(""), 7) {
                    send("content_block_delta", jobj! { "type": "content_block_delta", "index": index, "delta": jobj! { "type": "text_delta", "text": piece } });
                }
                send("content_block_stop", jobj! { "type": "content_block_stop", "index": index });
            }
            Some("thinking") => {
                send(
                    "content_block_start",
                    jobj! { "type": "content_block_start", "index": index, "content_block": jobj! { "type": "thinking", "thinking": "", "signature": "" } },
                );
                // Omitted thinking streams progress instead of text, on a delta whose text is empty.
                for estimated in block.get("estimatedTokens").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice) {
                    send(
                        "content_block_delta",
                        jobj! {
                            "type": "content_block_delta", "index": index,
                            "delta": jobj! { "type": "thinking_delta", "thinking": "", "estimated_tokens": estimated.clone() },
                        },
                    );
                }
                for piece in pieces(block.str_at("thinking").unwrap_or(""), 9) {
                    send("content_block_delta", jobj! { "type": "content_block_delta", "index": index, "delta": jobj! { "type": "thinking_delta", "thinking": piece } });
                }
                send(
                    "content_block_delta",
                    jobj! {
                        "type": "content_block_delta", "index": index,
                        "delta": jobj! { "type": "signature_delta", "signature": block.str_at("signature").unwrap_or("sig_fake") },
                    },
                );
                send("content_block_stop", jobj! { "type": "content_block_stop", "index": index });
            }
            Some("web_search_tool_result") => {
                // Arrives whole, in its start event.
                send("content_block_start", jobj! { "type": "content_block_start", "index": index, "content_block": block.clone() });
                send("content_block_stop", jobj! { "type": "content_block_stop", "index": index });
            }
            Some(kind @ ("tool_use" | "server_tool_use")) => {
                // The server runs its own tool within the message, which then goes on.
                if kind == "tool_use" {
                    stop = "tool_use";
                }
                send(
                    "content_block_start",
                    jobj! {
                        "type": "content_block_start", "index": index,
                        "content_block": jobj! { "type": kind, "id": block.str_at("id").unwrap_or(""), "name": block.str_at("name").unwrap_or(""), "input": Json::obj() },
                    },
                );
                let input = block.get("input").map_or_else(|| "{}".to_owned(), Json::to_text);
                for piece in pieces(&input, 11) {
                    send("content_block_delta", jobj! { "type": "content_block_delta", "index": index, "delta": jobj! { "type": "input_json_delta", "partial_json": piece } });
                }
                send("content_block_stop", jobj! { "type": "content_block_stop", "index": index });
            }
            _ => {}
        }
    }
    send(
        "message_delta",
        jobj! {
            "type": "message_delta",
            "delta": jobj! { "stop_reason": script.stop_reason.unwrap_or(stop), "stop_sequence": Json::Null },
            "usage": jobj! { "output_tokens": 42u64 },
        },
    );
    send("message_stop", jobj! { "type": "message_stop" });
    out
}

// ---------------------------------------------------------------- request reading

fn text_of(message: &Json) -> String {
    if let Some(text) = message.str_at("content") {
        return text.to_owned();
    }
    let content = message.get("content").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice);
    content
        .iter()
        .filter(|block| block.str_at("type") == Some("text"))
        .filter_map(|block| block.str_at("text"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn messages_of(body: &Json) -> &[Json] {
    body.get("messages").and_then(Json::as_arr).map_or(&[], Vec::as_slice)
}

fn blocks_of(message: &Json) -> &[Json] {
    message.get("content").and_then(Json::as_arr).map_or(&[], Vec::as_slice)
}

fn all_blocks(body: &Json) -> Vec<&Json> {
    messages_of(body).iter().flat_map(blocks_of).collect()
}

/// `{ role: "system", content: [], output_config }` and nothing more.
fn is_effort_marker(message: &Json) -> bool {
    let Some(object) = message.as_obj() else { return false };
    message.str_at("role") == Some("system") && blocks_of(message).is_empty() && object.len() == 3 && object.contains_key("output_config") && object.contains_key("content")
}

/// The last message of the conversation proper. Claude Code may append a `<system-reminder>` of its own as a trailing
/// `system`-role message, and since 2.1.284 an effort marker. Only these two are skipped.
fn last_turn_message(body: &Json) -> Option<&Json> {
    messages_of(body).iter().rev().find(|message| {
        let reminder = message.str_at("role") == Some("system") && text_of(message).contains("<system-reminder>");
        !reminder && !is_effort_marker(message)
    })
}

fn tool_names(body: &Json) -> Vec<String> {
    body.get("tools").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice).iter().filter_map(|tool| tool.str_at("name")).map(str::to_owned).collect()
}

fn declared_tools(body: &Json) -> Vec<String> {
    let tools = body.get("tools").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice);
    let mut names: Vec<String> = tools
        .iter()
        .filter(|tool| tool.get("defer_loading").and_then(Json::as_bool) != Some(true))
        .filter_map(|tool| tool.str_at("name"))
        .map(str::to_owned)
        .collect();
    names.sort();
    names
}

fn tool_named<'a>(body: &'a Json, name: &str) -> Option<&'a Json> {
    body.get("tools").and_then(Json::as_arr)?.iter().find(|tool| tool.str_at("name") == Some(name))
}

/// `value` as the API reads it: object keys in one order and no prompt-cache breakpoints, which move by design.
fn canonical(value: &Json) -> Json {
    match value {
        Json::Arr(items) => Json::Arr(items.iter().map(canonical).collect()),
        Json::Obj(object) => {
            let mut keys: Vec<&str> = object.keys().filter(|key| *key != "cache_control").collect();
            keys.sort_unstable();
            let mut out = Obj::new();
            for key in keys {
                out.insert(key, canonical(object.get(key).unwrap()));
            }
            Json::Obj(out)
        }
        other => other.clone(),
    }
}

/// `extendsRequest`: whether `next` only extends `previous`, the same system prompt and tool list and `previous`'s
/// messages as the start of its own: what lets the prompt cache `previous` wrote serve `next`.
fn assert_extends(previous: &Json, next: &Json, compare_tools: bool) {
    let (before, after) = (canonical(previous), canonical(next));
    assert_eq!(before.get("system"), after.get("system"), "system");
    if compare_tools {
        assert_eq!(before.get("tools"), after.get("tools"), "tools");
    }
    let (earlier, later) = (messages_of(&before), messages_of(&after));
    assert!(!earlier.is_empty() && later.len() > earlier.len(), "lengths {}/{}", earlier.len(), later.len());
    for (index, message) in earlier.iter().enumerate() {
        assert_eq!(Some(message), later.get(index), "message {index}");
    }
}

// ---------------------------------------------------------------- drivers

/// One way to reach a sidecar: this crate, or the original Node one.
trait Driver {
    fn send(&self, line: &str);
    fn wait(&self, id: &str, limit: Duration) -> Outcome;
}

impl Driver for Rust {
    fn send(&self, line: &str) {
        Rust::send(self, line);
    }

    fn wait(&self, id: &str, limit: Duration) -> Outcome {
        Rust::wait(self, id, limit)
    }
}

/// The names a sidecar's own environment may hold; both sides see the same.
const SIDECAR_ENV: [&str; 8] = ["PATH", "HOME", "TMPDIR", "TMP", "TEMP", "LANG", "LC_ALL", "TZ"];

/// The original sidecar, driven frame by frame like the host does.
struct Node {
    child: std::process::Child,
    stdin: Mutex<std::process::ChildStdin>,
    lines: Mutex<Receiver<String>>,
    stderr: Arc<Mutex<String>>,
}

impl Node {
    fn start(path: &str) -> Self {
        let mut command = Command::new("node");
        command.arg(path).env_clear().stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        for name in SIDECAR_ENV {
            if let Ok(value) = std::env::var(name) {
                command.env(name, value);
            }
        }
        let mut child = command.spawn().expect("node");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr_pipe = child.stderr.take().unwrap();
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });
        let stderr = Arc::new(Mutex::new(String::new()));
        let sink = stderr.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr_pipe).lines().map_while(Result::ok) {
                sink.lock().unwrap().push_str(&line);
                sink.lock().unwrap().push('\n');
            }
        });
        let node = Self { child, stdin: Mutex::new(stdin), lines: Mutex::new(lines), stderr };
        Driver::send(&node, r#"{"v":18,"type":"hello"}"#);
        node
    }
}

impl Driver for Node {
    fn send(&self, line: &str) {
        let mut stdin = self.stdin.lock().unwrap();
        writeln!(stdin, "{line}").unwrap();
        stdin.flush().unwrap();
    }

    fn wait(&self, id: &str, limit: Duration) -> Outcome {
        let lines = self.lines.lock().unwrap();
        collect_frames(id, limit, &lines)
    }
}

impl Drop for Node {
    /// Gives it the chance to tear its sessions down and clean up after them.
    fn drop(&mut self) {
        Driver::send(self, r#"{"v":18,"type":"shutdown"}"#);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && matches!(self.child.try_wait(), Ok(None)) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn collect_frames(id: &str, limit: Duration, lines: &Receiver<String>) -> Outcome {
    let deadline = Instant::now() + limit;
    let mut outcome = Outcome::default();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let line = match lines.recv_timeout(left) {
            Ok(line) => line,
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => panic!("no terminal frame for {id}: {:?}", outcome.frames),
        };
        let frame = Json::parse(&line).expect("a frame is JSON");
        if frame.str_at("id") != Some(id) {
            continue;
        }
        let terminal = matches!(frame.str_at("type"), Some("done") | Some("error"));
        outcome.frames.push(frame);
        if terminal {
            return outcome;
        }
    }
}

fn release_frame(session: &str) -> String {
    format!(r#"{{"v":18,"type":"release","session":{}}}"#, Json::from(session).to_text())
}

// ---------------------------------------------------------------- the real executable

struct Installed {
    executable: String,
}

/// The CLI these tests drive, or `None`: those tests then skip themselves.
fn installed() -> Option<Installed> {
    let executable = std::env::var("MEWRK_CLAUDE_EXECUTABLE").ok().or_else(|| {
        std::env::split_paths(&std::env::var_os("PATH")?).map(|dir| dir.join("claude")).find(|path| path.is_file()).map(|path| path.to_string_lossy().into_owned())
    })?;
    Path::new(&executable).is_file().then_some(Installed { executable })
}

/// The SDK the Node sidecar loads, for the differential runs alone: the Rust side never reads it.
fn node_sdk() -> Option<String> {
    std::env::var("MEWRK_CLAUDE_SDK").ok()
}

/// What the CLI says its version is.
fn reported_version(executable: &str) -> String {
    let output = Command::new(executable).arg("--version").env("DISABLE_AUTOUPDATER", "1").output().expect("run Claude Code");
    String::from_utf8_lossy(&output.stdout).split_whitespace().next().expect("a version").to_owned()
}

/// A scripted upstream and a throwaway profile, for one sidecar's worth of steps. Every step's agent points at them.
struct Bench {
    upstream: Upstream,
    deny: DenyProxy,
    scratch: Scratch,
    executable: String,
    sdk: Option<String>,
}

impl Bench {
    fn new(installed: Installed) -> Self {
        let scratch = Scratch::new("bench");
        fs::create_dir_all(scratch.path("cwd")).unwrap();
        fs::create_dir_all(scratch.path("home")).unwrap();
        fs::create_dir_all(scratch.path("config")).unwrap();
        Self { upstream: Upstream::start(), deny: DenyProxy::start(), scratch, executable: installed.executable, sdk: node_sdk() }
    }

    fn cwd(&self) -> String {
        self.scratch.text("cwd")
    }

    fn env(&self, key: &str) -> Obj {
        let mut env = Obj::new();
        env.insert("HOME", self.scratch.text("home"));
        env.insert("USER", std::env::var("USER").unwrap_or_else(|_| "mewrk-test".to_owned()));
        env.insert("CLAUDE_CONFIG_DIR", self.scratch.text("config"));
        env.insert("ANTHROPIC_BASE_URL", self.upstream.base.as_str());
        env.insert("ANTHROPIC_API_KEY", key);
        env.insert("HTTPS_PROXY", self.deny.url.as_str());
        env.insert("HTTP_PROXY", self.deny.url.as_str());
        env.insert("NO_PROXY", "127.0.0.1,localhost");
        env
    }

    fn agent(&self, session: &str) -> Obj {
        let mut agent = Obj::new();
        agent.insert("session", session);
        agent.insert("executable", self.executable.as_str());
        if let Some(sdk) = &self.sdk {
            agent.insert("sdk", sdk.as_str());
        }
        agent.insert("cwd", self.cwd());
        agent.insert("env", self.env(UPSTREAM_KEY));
        agent
    }

    /// `agent` with the switches the tool-change checks turn on (`additionAgent`). `ENABLE_TOOL_SEARCH` stands in for the
    /// real host the stub is not.
    fn addition_agent(&self, session: &str, tool_changes: bool) -> Obj {
        let mut agent = self.agent(session);
        let mut env = self.env(UPSTREAM_KEY);
        env.insert("ENABLE_TOOL_SEARCH", "true");
        agent.insert("env", env);
        agent.insert("toolChanges", tool_changes);
        agent
    }

    /// `stepFrame`'s payload; `extra` adjusts it last.
    fn payload(&self, session: &str, messages: Vec<Json>, extra: impl FnOnce(&mut Obj)) -> Json {
        let mut payload = Obj::new();
        payload.insert("family", "claude-agent");
        // Both are inert for this family; the host no longer sends them at all.
        payload.insert("baseURL", "https://api.anthropic.com");
        payload.insert("apiKey", REQUEST_ONLY_KEY);
        payload.insert("modelId", "sonnet");
        payload.insert("system", HOST_PROMPT);
        payload.insert("messages", messages);
        payload.insert("tools", tools());
        payload.insert("maxSteps", 1u64);
        payload.insert("reasoning", "low");
        payload.insert("agent", self.agent(session));
        extra(&mut payload);
        Json::Obj(payload)
    }
}

/// Runs one step on `driver`.
fn run(driver: &dyn Driver, id: &str, payload: &Json) -> Outcome {
    driver.send(&step_frame(id, payload));
    driver.wait(id, STEP_LIMIT)
}

/// What a step's `responseMessages` hold, to replay.
fn assistant_of(outcome: &Outcome) -> Json {
    outcome.result().get("responseMessages").and_then(Json::as_arr).and_then(|messages| messages.first()).cloned().unwrap_or_else(|| jobj! { "role": "assistant", "content": jarr![] })
}

fn events_named(outcome: &Outcome, kind: &str) -> usize {
    outcome.events_of(kind).len()
}

macro_rules! real_cli {
    () => {
        match installed() {
            Some(installed) => Bench::new(installed),
            None => {
                eprintln!("skipped: no Claude Code executable (MEWRK_CLAUDE_EXECUTABLE, or claude on PATH)");
                return;
            }
        }
    };
}

// ---------------------------------------------------------------- the real CLI: rounds

/// 30b, 30c: a fresh session; the model thinks, speaks and calls a host tool; the parked handler then receives the
/// host's result plus a folded notice, in the same session.
#[test]
fn a_tool_round_parks_and_continues_in_one_session() {
    let bench = real_cli!();
    let rust = Rust::start();
    bench.upstream.push(Script::blocks(vec![
        thinking_block("Let me look at the file first.", "sig_A"),
        text_block("I will read the file."),
        tool_use_block("toolu_01", "read_file", jobj! { "path": "a.txt" }),
    ]));
    let first = user("Please read a.txt");
    let round1 = run(&rust, "ca-1", &bench.payload("ca-run-1", vec![first.clone()], |_| {}));
    let result = round1.result();
    assert_eq!(result.str_at("finishReason"), Some("tool-calls"));
    assert_eq!(result.str_at("text"), Some("I will read the file."));
    assert_eq!(
        result.get("calls").unwrap().to_text(),
        r#"[{"callId":"toolu_01","toolName":"read_file","input":{"path":"a.txt"}}]"#,
    );
    // Thinking, text and tool events flow in order, under the host's bare names.
    assert_eq!(round1.events_of("reasoning-start")[0].get("item").and_then(Json::as_u64), Some(0));
    assert!(events_named(&round1, "reasoning-delta") > 0 && events_named(&round1, "text-delta") > 0);
    assert!(round1.events_of("reasoning-done")[0].get("durationMs").is_some_and(Json::is_num));
    assert_eq!(round1.events_of("tool-call-announced")[0].str_at("toolName"), Some("read_file"));
    assert_eq!(round1.events_of("tool-call")[0].str_at("callId"), Some("toolu_01"));
    let usage = round1.events_of("usage")[0].get("usage").unwrap();
    assert_eq!((usage.get("inputTokens").and_then(Json::as_u64), usage.get("cacheReadTokens").and_then(Json::as_u64)), (Some(150), Some(20)));
    // `responseMessages` is the AI SDK shape, with the signed thinking.
    let assistant = assistant_of(&round1);
    assert_eq!(assistant.str_at("role"), Some("assistant"));
    let parts = blocks_of(&assistant);
    assert!(parts.iter().any(|part| part.str_at("type") == Some("reasoning")
        && part.str_at("text") == Some("Let me look at the file first.")
        && part.path(&["providerOptions", "anthropic", "signature"]).and_then(Json::as_str) == Some("sig_A")));
    assert!(parts.iter().any(|part| part.str_at("type") == Some("tool-call") && part.str_at("toolCallId") == Some("toolu_01") && part.str_at("toolName") == Some("read_file")));

    // What the upstream saw: the SDK's identity line and the host's prompt, not Claude Code's body.
    let call1 = bench.upstream.calls()[0].body.clone();
    let system_blocks: Vec<Json> = match call1.get("system") {
        Some(Json::Arr(blocks)) => blocks.clone(),
        Some(Json::Str(text)) => vec![jobj! { "text": text.as_str() }],
        _ => Vec::new(),
    };
    let system_text = system_blocks.iter().filter_map(|block| block.str_at("text")).collect::<Vec<_>>().join("\n");
    assert!(system_text.contains(HOST_PROMPT) && system_text.contains("Claude Agent SDK"), "{system_text}");
    assert!(!system_text.contains("Claude Code, Anthropic's official CLI") && !system_text.contains("# Tone and style") && !system_text.contains("# Tool usage policy"));
    assert!(system_blocks.len() <= 3, "{system_text}");
    // The CLI stamps its own version into the billing header it prepends.
    let declared = reported_version(&bench.executable);
    let billing = system_blocks.iter().filter_map(|block| block.str_at("text")).find(|text| text.contains("cc_version=")).unwrap_or("").to_owned();
    assert!(billing.contains(&format!("cc_version={declared}")), "{billing}");
    // The context plugin leaves out the CLI's environment block, model line and date, and the prompt arrives as sent.
    let request_text = std::iter::once(system_text.clone()).chain(messages_of(&call1).iter().map(text_of)).collect::<Vec<_>>().join("\n");
    for leak in ["You have been invoked in the following environment", "# Environment", "You are powered by the model", "Today's date is", bench.cwd().as_str()] {
        assert!(!request_text.contains(leak), "leaked {leak}: {request_text}");
    }
    let prompt = last_turn_message(&call1).expect("a prompt");
    assert_eq!((prompt.str_at("role"), text_of(prompt).trim()), (Some("user"), "Please read a.txt"));
    // Exactly the host's tools, by their bare names, with the schema verbatim.
    assert_eq!(declared_tools(&call1), ["bash", "box", "read_file"]);
    assert_eq!(tool_named(&call1, "read_file").unwrap().path(&["input_schema", "properties", "path", "description"]).and_then(Json::as_str), Some("Path"));
    // reasoning:low is an effort and nothing turns thinking off.
    assert_eq!(call1.path(&["output_config", "effort"]).and_then(Json::as_str), Some("low"));
    assert_ne!(call1.path(&["thinking", "type"]).and_then(Json::as_str), Some("disabled"));

    // The continuation: a host message rides behind the round's results.
    bench.upstream.push(Script::text("The file says hello."));
    let notification = "<system-reminder>\n<task-notification>done</task-notification>\n</system-reminder>";
    let continuation = vec![
        first,
        assistant,
        tool_result("toolu_01", "read_file", text_output("hello from a.txt")),
        jobj! { "role": "user", "content": notification, "providerOptions": jobj! { "mewrk": jobj! { "hostMessage": true } } },
    ];
    let round2 = run(&rust, "ca-2", &bench.payload("ca-run-1", continuation, |_| {}));
    let result = round2.result();
    assert_eq!(result.str_at("finishReason"), Some("stop"));
    assert_eq!(result.str_at("text"), Some("The file says hello."));
    assert_eq!(result.get("calls").unwrap().as_arr().unwrap().len(), 0);
    assert_eq!(bench.upstream.calls().len(), 2);
    let call2 = bench.upstream.calls()[1].body.clone();
    let last = last_turn_message(&call2).expect("a last message");
    let tool_block = blocks_of(last).iter().find(|block| block.str_at("type") == Some("tool_result")).expect("a tool_result");
    assert_eq!(tool_block.str_at("tool_use_id"), Some("toolu_01"));
    let text: String = blocks_of(tool_block).iter().filter_map(|block| block.str_at("text")).collect::<Vec<_>>().join("\n");
    assert!(text.contains("hello from a.txt") && text.contains("<task-notification>done</task-notification>"), "{text}");
    // The same session keeps the thinking signature.
    assert!(all_blocks(&call2).iter().any(|block| block.str_at("type") == Some("thinking") && block.str_at("signature") == Some("sig_A")));
    bench.upstream.assert_only_the_stub_key(UPSTREAM_KEY);
    bench.upstream.assert_no_stray_requests();
    bench.deny.assert_untouched();
}

/// 30d: a new run resumes from host history through a synthesized transcript, laid out as the host projects it: the
/// round's signed reasoning card lands after the tool exchange, and the transcript must merge it so the CLI keeps the
/// signature.
#[test]
fn a_new_run_resumes_from_host_history() {
    let bench = real_cli!();
    let rust = Rust::start();
    // The CLI keeps a thinking signature only on an assistant entry stamped with the model it runs, and the alias
    // `sonnet` is not one until a session has reported what it serves.
    bench.upstream.push(Script::text("learned the model"));
    let learned = run(&rust, "ca-learn", &bench.payload("ca-learn", vec![user("hello")], |_| {}));
    assert_eq!(learned.terminal().str_at("type"), Some("done"));
    rust.send(&release_frame("ca-learn"));
    bench.upstream.push(Script::text("resumed answer"));
    let history = vec![
        user("Please read a.txt"),
        jobj! { "role": "assistant", "content": jarr![
            jobj! { "type": "text", "text": "I will read the file." },
            jobj! { "type": "tool-call", "toolCallId": "toolu_01", "toolName": "read_file", "input": jobj! { "path": "a.txt" } },
        ] },
        tool_result("toolu_01", "read_file", text_output("hello from a.txt")),
        jobj! { "role": "assistant", "content": jarr![jobj! {
            "type": "reasoning",
            "text": "Let me look at the file first.",
            "providerOptions": jobj! { "anthropic": jobj! { "signature": "sig_A" }, "mewrk": jobj! { "model": "sonnet" } },
        }] },
        jobj! { "role": "assistant", "content": jarr![text_block("The file says hello.")] },
        user("Thanks. And now?"),
    ];
    let round = run(&rust, "ca-3", &bench.payload("ca-run-2", history, |_| {}));
    assert_eq!(round.result().str_at("text"), Some("resumed answer"));
    assert_eq!(bench.upstream.calls().len(), 2);
    let call = bench.upstream.last_call().body;
    let replayed_thinking = messages_of(&call).iter().any(|message| {
        let content = blocks_of(message);
        message.str_at("role") == Some("assistant")
            && content.first().is_some_and(|block| block.str_at("type") == Some("thinking") && block.str_at("signature") == Some("sig_A"))
            && content.iter().any(|block| block.str_at("type") == Some("text") && block.str_at("text") == Some("The file says hello."))
    });
    assert!(replayed_thinking, "{call}");
    let blocks = all_blocks(&call);
    assert!(blocks.iter().any(|block| block.str_at("type") == Some("tool_use") && block.str_at("id") == Some("toolu_01") && block.str_at("name") == Some("read_file")));
    assert!(blocks.iter().any(|block| block.str_at("type") == Some("tool_result") && block.str_at("tool_use_id") == Some("toolu_01")));
    let last = last_turn_message(&call).expect("a last message");
    assert_eq!(last.str_at("role"), Some("user"));
    assert!(text_of(last).contains("Thanks. And now?"));
    rust.send(&release_frame("ca-run-2"));
    rust.send(&release_frame("never-existed"));
    bench.deny.assert_untouched();
}

/// 30e: release tears a session down; a later continuation for the same key rebuilds it from the transcript and nudges
/// the CLI to continue.
#[test]
fn a_lost_session_is_rebuilt_with_the_continue_notice() {
    let bench = real_cli!();
    let rust = Rust::start();
    bench.upstream.push(Script::blocks(vec![tool_use_block("toolu_02", "bash", jobj! { "command": "ls" })]));
    let round = run(&rust, "ca-4", &bench.payload("ca-run-3", vec![user("list files")], |_| {}));
    let calls = round.result().get("calls").unwrap().as_arr().unwrap().clone();
    assert_eq!((calls[0].str_at("toolName"), calls[0].str_at("callId")), (Some("bash"), Some("toolu_02")));
    rust.send(&release_frame("ca-run-3"));
    std::thread::sleep(Duration::from_millis(300));
    bench.upstream.push(Script::text("continued after loss"));
    let lost = vec![user("list files"), assistant_of(&round), tool_result("toolu_02", "bash", text_output("a.txt\nb.txt"))];
    let round = run(&rust, "ca-5", &bench.payload("ca-run-3", lost, |_| {}));
    assert_eq!(round.result().str_at("text"), Some("continued after loss"));
    let call = bench.upstream.last_call().body;
    assert!(all_blocks(&call).iter().any(|block| block.str_at("type") == Some("tool_result") && block.str_at("tool_use_id") == Some("toolu_02")));
    let last = last_turn_message(&call).expect("a last message");
    assert_eq!(last.str_at("role"), Some("user"));
    assert!(text_of(last).contains("[SYSTEM NOTIFICATION - NOT USER INPUT]"), "{last}");
    bench.deny.assert_untouched();
}

/// 30e', 30e'': a background result delivered on its own, with no round parked for it, plain or in `box`. Nothing but the
/// notice can start the turn; the step must rebuild rather than demand a user message.
#[test]
fn a_delivery_alone_rebuilds_and_does_not_demand_a_prompt() {
    let bench = real_cli!();
    let rust = Rust::start();
    bench.upstream.push(Script::text("noted the background result"));
    let messages = vec![
        user("kick off the background job"),
        jobj! { "role": "assistant", "content": jarr![text_block("Started it.")] },
        jobj! {
            "role": "user",
            "content": "<system-reminder>\n<task-notification>bg done</task-notification>\n</system-reminder>",
            "providerOptions": jobj! { "mewrk": jobj! { "hostMessage": true } },
        },
    ];
    let round = run(&rust, "ca-notice", &bench.payload("ca-notice", messages, |_| {}));
    assert_eq!(round.result().str_at("text"), Some("noted the background result"));
    let call = bench.upstream.last_call().body.to_text();
    assert!(call.contains("<task-notification>bg done</task-notification>") && !call.contains("hostMessage"), "{call}");
    rust.send(&release_frame("ca-notice"));

    bench.upstream.push(Script::text("noted the boxed result"));
    let box_id = format!("toolu_{}", "f0e1d2c3".repeat(6));
    let messages = vec![
        user("kick off the background job"),
        jobj! { "role": "assistant", "content": jarr![text_block("Started it.")] },
        jobj! { "role": "assistant", "content": jarr![jobj! { "type": "tool-call", "toolCallId": box_id.as_str(), "toolName": "box", "input": jobj! { "none": jarr![] } }] },
        tool_result(&box_id, "box", text_output("<task-notification>bg boxed</task-notification>")),
    ];
    let round = run(&rust, "ca-box", &bench.payload("ca-box", messages, |_| {}));
    assert_eq!(round.result().str_at("text"), Some("noted the boxed result"));
    let call = bench.upstream.last_call().body;
    assert!(all_blocks(&call).iter().any(|block| block.str_at("type") == Some("tool_result") && block.str_at("tool_use_id") == Some(box_id.as_str())));
    assert!(call.to_text().contains("<task-notification>bg boxed</task-notification>"));
    rust.send(&release_frame("ca-box"));
    bench.deny.assert_untouched();
}

/// 30f: two tool calls in one reply; the CLI calls handlers sequentially and both results must reach the next request.
#[test]
fn two_calls_in_one_reply_park_and_answer_together() {
    let bench = real_cli!();
    let rust = Rust::start();
    bench.upstream.push(Script::blocks(vec![
        tool_use_block("toolu_p1", "read_file", jobj! { "path": "one.txt" }),
        tool_use_block("toolu_p2", "read_file", jobj! { "path": "two.txt" }),
    ]));
    let round = run(&rust, "ca-6", &bench.payload("ca-run-4", vec![user("read both")], |_| {}));
    let ids: Vec<&str> = round.result().get("calls").unwrap().as_arr().unwrap().iter().filter_map(|call| call.str_at("callId")).collect();
    assert_eq!(ids, ["toolu_p1", "toolu_p2"]);
    bench.upstream.push(Script::text("both read"));
    let results = jobj! {
        "role": "tool",
        "content": jarr![
            jobj! { "type": "tool-result", "toolCallId": "toolu_p1", "toolName": "read_file", "output": text_output("one") },
            jobj! { "type": "tool-result", "toolCallId": "toolu_p2", "toolName": "read_file", "output": jobj! { "type": "error-text", "value": "two failed" } },
        ],
    };
    let round = run(&rust, "ca-7", &bench.payload("ca-run-4", vec![user("read both"), assistant_of(&round), results], |_| {}));
    assert_eq!(round.result().str_at("text"), Some("both read"));
    let call = bench.upstream.last_call().body;
    let blocks = all_blocks(&call);
    assert!(blocks.iter().any(|block| block.str_at("tool_use_id") == Some("toolu_p1")));
    assert!(blocks.iter().any(|block| block.str_at("tool_use_id") == Some("toolu_p2") && block.get("is_error").and_then(Json::as_bool) == Some(true)));
    rust.send(&release_frame("ca-run-4"));
    bench.deny.assert_untouched();
}

/// 30f': a tool joins while a round is parked. The live session lists it instead of being rebuilt: the resumed request
/// carries the parked result as-is, no continue notice, and declares the new tool.
#[test]
fn a_tool_that_joins_is_listed_by_the_live_session() {
    let bench = real_cli!();
    let rust = Rust::start();
    // A model Claude Code does not append tools for (see the table below).
    let model = "claude-sonnet-5";
    bench.upstream.push(Script::blocks(vec![tool_use_block("toolu_w1", "read_file", jobj! { "path": "w.txt" })]));
    let round = run(&rust, "ca-w1", &bench.payload("ca-run-widen", vec![user("read w")], |payload| {
        payload.insert("modelId", model);
    }));
    bench.upstream.push(Script::text("listed the new tool"));
    let messages = vec![user("read w"), assistant_of(&round), tool_result("toolu_w1", "read_file", text_output("w"))];
    let round = run(&rust, "ca-w2", &bench.payload("ca-run-widen", messages, |payload| {
        payload.insert("modelId", model);
        let mut all = tools();
        all.push(late_tool());
        payload.insert("tools", all);
    }));
    assert_eq!(round.result().str_at("text"), Some("listed the new tool"));
    let call = bench.upstream.last_call().body;
    let last = last_turn_message(&call).expect("a last message");
    assert!(blocks_of(last).iter().any(|block| block.str_at("type") == Some("tool_result") && block.str_at("tool_use_id") == Some("toolu_w1")));
    assert!(!call.get("messages").unwrap().to_text().contains("[SYSTEM NOTIFICATION - NOT USER INPUT]"));
    assert!(tool_named(&call, "late_tool").is_some(), "{:?}", tool_names(&call));
    assert_eq!(bench.upstream.calls().len(), 2);
    rust.send(&release_frame("ca-run-widen"));
    bench.deny.assert_untouched();
}

/// The CLI's catalogue against `tool_append.rs`'s `CLAUDE_CODE_TOOL_CHANGE_MODELS`. The host tells a rebuilt session whether to
/// hand the CLI its tool additions by that table, and the CLI is whichever the user has, so this is where a new release
/// that moved a model shows up. A wrong entry costs prompt cache on a rebuild, never a tool.
#[test]
fn the_cli_takes_tool_changes_on_the_models_the_host_names() {
    let bench = real_cli!();
    let rust = Rust::start();
    for (index, (model, takes)) in [
        ("claude-fable-5-1", true),
        ("claude-mythos-5", true),
        ("claude-opus-5-5", true),
        ("claude-opus-4-8", true),
        ("claude-sonnet-5-5", true),
        ("claude-haiku-5-5", true),
        ("claude-sonnet-5", false),
        ("claude-opus-4-7", false),
        ("claude-sonnet-4-6", false),
    ]
    .into_iter()
    .enumerate()
    {
        bench.upstream.push(Script::text("ok"));
        let id = format!("ca-catalogue-{index}");
        run(&rust, &id, &bench.payload(&id, vec![user("hi")], |payload| {
            payload.insert("modelId", model);
        }));
        rust.send(&release_frame(&id));
        let call = bench.upstream.last_call();
        let betas = call.header("anthropic-beta").unwrap_or("").to_owned();
        assert_eq!(betas.contains("mid-conversation-tool-changes"), takes, "{model}: {betas}");
    }
    bench.deny.assert_untouched();
}

/// 30f-addition, 30f-rebuild: the same widening on a model the CLI knows takes tool changes. The CLI hands the tool over
/// itself, and a rebuild from the host's marker gets the same request back, its prompt cache intact.
#[test]
fn a_tool_addition_survives_a_rebuild_with_the_prefix_intact() {
    let bench = real_cli!();
    let rust = Rust::start();
    let model = "claude-opus-5-5";
    bench.upstream.push(Script::blocks(vec![tool_use_block("toolu_a1", "read_file", jobj! { "path": "a.txt" })]));
    let addition1 = run(&rust, "ca-a1", &bench.payload("ca-run-addition", vec![user("read a")], |payload| {
        payload.insert("modelId", model);
        payload.insert("agent", bench.addition_agent("ca-run-addition", true));
    }));
    let call_a1 = bench.upstream.last_call();
    bench.upstream.push(Script::text("took the addition"));
    let with_late = || {
        let mut all = tools();
        all.push(late_tool());
        all
    };
    let exchange = vec![user("read a"), assistant_of(&addition1), tool_result("toolu_a1", "read_file", text_output("a"))];
    let addition2 = run(&rust, "ca-a2", &bench.payload("ca-run-addition", exchange.clone(), |payload| {
        payload.insert("modelId", model);
        payload.insert("agent", bench.addition_agent("ca-run-addition", true));
        payload.insert("tools", with_late());
    }));
    assert_eq!(addition2.result().str_at("text"), Some("took the addition"));
    let call_a2 = bench.upstream.last_call();
    let addition_message = messages_of(&call_a2.body).iter().find(|message| {
        message.str_at("role") == Some("system")
            && blocks_of(message).iter().any(|block| block.str_at("type") == Some("tool_addition") && block.path(&["tool", "name"]).and_then(Json::as_str) == Some("late_tool"))
    });
    assert!(addition_message.is_some(), "{}", call_a2.body);
    assert_eq!(tool_named(&call_a2.body, "late_tool").and_then(|tool| tool.get("defer_loading")).and_then(Json::as_bool), Some(true));
    assert_eq!(declared_tools(&call_a1.body), declared_tools(&call_a2.body));
    assert!(call_a2.header("anthropic-beta").unwrap_or("").contains("mid-conversation-tool-changes-2026-07-01"), "{:?}", call_a2.header("anthropic-beta"));
    rust.send(&release_frame("ca-run-addition"));

    // The next user turn is a run of its own, rebuilt from the host's history, where the addition is the host's marker
    // behind the tool result. The rebuilt request only extends the last one.
    let mut history = exchange;
    history.push(tool_addition_marker(&["late_tool"]));
    history.push(assistant_of(&addition2));
    history.push(user("again"));
    bench.upstream.push(Script::text("rebuilt with the addition"));
    let rebuilt = run(&rust, "ca-r1", &bench.payload("ca-run-rebuild", history, |payload| {
        payload.insert("modelId", model);
        payload.insert("agent", bench.addition_agent("ca-run-rebuild", true));
        payload.insert("tools", with_late());
    }));
    assert_eq!(rebuilt.result().str_at("text"), Some("rebuilt with the addition"));
    assert_extends(&call_a2.body, &bench.upstream.last_call().body, true);
    rust.send(&release_frame("ca-run-rebuild"));
    bench.deny.assert_untouched();
}

/// 30f-between, 30f-no-changes: a tool enabled between two turns is handed over right behind the new prompt; on a model
/// the CLI takes no tool changes for, the markers are dropped and the tool declared with the rest.
#[test]
fn a_tool_enabled_between_turns_follows_the_prompt() {
    let bench = real_cli!();
    let rust = Rust::start();
    let model = "claude-opus-5-5";
    let between = j(r#"{"name":"between_tool","description":"Enabled between turns","inputSchema":{"type":"object","properties":{"q":{"type":"string"}}}}"#);
    let with_between = || {
        let mut all = tools();
        all.push(between.clone());
        all
    };
    bench.upstream.push(Script::text("hi there"));
    let between1 = run(&rust, "ca-b1", &bench.payload("ca-run-between-1", vec![user("hello")], |payload| {
        payload.insert("modelId", model);
        payload.insert("agent", bench.addition_agent("ca-run-between-1", true));
    }));
    let call_b1 = bench.upstream.last_call();
    rust.send(&release_frame("ca-run-between-1"));
    let history = vec![user("hello"), assistant_of(&between1), user("use the new tool"), tool_addition_marker(&["between_tool"])];
    bench.upstream.push(Script::text("it is here"));
    let between2 = run(&rust, "ca-b2", &bench.payload("ca-run-between-2", history.clone(), |payload| {
        payload.insert("modelId", model);
        payload.insert("agent", bench.addition_agent("ca-run-between-2", true));
        payload.insert("tools", with_between());
    }));
    assert_eq!(between2.result().str_at("text"), Some("it is here"));
    let call_b2 = bench.upstream.last_call();
    rust.send(&release_frame("ca-run-between-2"));
    let messages = messages_of(&call_b2.body);
    let prompt_at = messages.iter().position(|message| text_of(message) == "use the new tool").expect("the prompt");
    let behind = &messages[prompt_at + 1];
    assert_eq!(behind.str_at("role"), Some("system"), "{}", call_b2.body);
    assert!(blocks_of(behind).iter().any(|block| block.str_at("type") == Some("tool_addition") && block.path(&["tool", "name"]).and_then(Json::as_str) == Some("between_tool")));
    assert_eq!(tool_named(&call_b2.body, "between_tool").and_then(|tool| tool.get("defer_loading")).and_then(Json::as_bool), Some(true));
    assert_eq!(declared_tools(&call_b1.body), declared_tools(&call_b2.body));

    let mut next = history.clone();
    next.push(assistant_of(&between2));
    next.push(user("and again"));
    bench.upstream.push(Script::text("still here"));
    let between3 = run(&rust, "ca-b3", &bench.payload("ca-run-between-3", next, |payload| {
        payload.insert("modelId", model);
        payload.insert("agent", bench.addition_agent("ca-run-between-3", true));
        payload.insert("tools", with_between());
    }));
    assert_eq!(between3.terminal().str_at("type"), Some("done"));
    rust.send(&release_frame("ca-run-between-3"));
    assert_extends(&call_b1.body, &call_b2.body, false);
    assert_extends(&call_b2.body, &bench.upstream.last_call().body, true);

    // On a model the CLI takes no tool changes for, the host says so and the markers are dropped.
    bench.upstream.push(Script::text("declared plainly"));
    let plain = run(&rust, "ca-n1", &bench.payload("ca-run-no-changes", history, |payload| {
        payload.insert("modelId", model);
        payload.insert("agent", bench.addition_agent("ca-run-no-changes", false));
        payload.insert("tools", with_between());
    }));
    assert_eq!(plain.terminal().str_at("type"), Some("done"));
    rust.send(&release_frame("ca-run-no-changes"));
    let call = bench.upstream.last_call().body;
    assert!(tool_named(&call, "between_tool").is_some_and(|tool| tool.get("defer_loading").is_none()));
    let text = call.get("messages").unwrap().to_text();
    assert!(!text.contains("tool_addition") && !text.contains("between_tool"), "{text}");
    bench.deny.assert_untouched();
}

/// 30f-mention: nothing else the attachment pipeline makes may reach the model, an `@` mention of a local file included.
#[test]
fn a_mentioned_local_file_stays_out_of_the_request() {
    let bench = real_cli!();
    let rust = Rust::start();
    let image = bench.scratch.path("probe.png");
    let text = bench.scratch.path("probe.txt");
    let bytes: Vec<u8> = base64_simd::STANDARD.decode_to_vec(PNG_1X1).unwrap();
    fs::write(&image, bytes).unwrap();
    fs::write(&text, "PROBE-MENTIONED-FILE-CONTENT\n").unwrap();
    bench.upstream.push(Script::text("mentions seen"));
    let prompt = format!("compare @{} with @{}", image.display(), text.display());
    let round = run(&rust, "ca-m1", &bench.payload("ca-run-mention", vec![user(&prompt)], |_| {}));
    assert_eq!(round.terminal().str_at("type"), Some("done"));
    let call = bench.upstream.last_call().body.get("messages").unwrap().to_text();
    assert!(!call.contains(r#""type":"image""#) && !call.contains("PROBE-MENTIONED-FILE-CONTENT"), "{call}");
    rust.send(&release_frame("ca-run-mention"));
    bench.deny.assert_untouched();
}

/// 30g: cancel while the upstream stalls ends the step `cancelled`, and takes the process with it.
#[test]
fn a_cancel_during_a_stall_kills_the_session() {
    let bench = real_cli!();
    let rust = Rust::start();
    bench.upstream.push(Script { delay: Some(Duration::from_secs(20)), ..Script::text("never") });
    let session = "ca-run-5";
    rust.send(&step_frame("ca-8", &bench.payload(session, vec![user("slow")], |_| {})));
    std::thread::sleep(Duration::from_millis(2500));
    let survivors = || processes_under(&bench.cwd());
    assert!(!survivors().is_empty(), "the CLI should be running");
    rust.send(r#"{"v":18,"type":"cancel","id":"ca-8"}"#);
    let cancelled = rust.wait("ca-8", Duration::from_secs(30));
    assert_eq!(cancelled.error().to_text(), r#"{"kind":"cancelled","message":"请求已取消"}"#);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(survivors(), Vec::<String>::new());
}

/// The process ids whose command line names `marker`.
fn processes_under(marker: &str) -> Vec<String> {
    let output = Command::new("pgrep").arg("-f").arg(marker).output().expect("pgrep");
    String::from_utf8_lossy(&output.stdout).lines().map(str::to_owned).collect()
}

/// 30h: an upstream error body that echoes the stub key, as some gateways do. The key travels only in `agent.env`, so
/// redaction has to be fed from there. A 400 is not retried by the CLI, which keeps the check bounded.
#[test]
fn an_error_echoing_the_stub_key_is_redacted() {
    let bench = real_cli!();
    let rust = Rust::start();
    for key in ["abc123", "sk-ant-fake-selfcheck", "  abc123  "] {
        let trimmed = key.trim();
        // Claude Code tries a 400 again without its optional betas before it gives up.
        bench.upstream.answer_every_call(Script {
            status: Some(400),
            body: Some(format!(r#"{{"type":"error","error":{{"type":"invalid_request_error","message":"Invalid API key: {trimmed}"}}}}"#)),
            ..Script::default()
        });
        let id = format!("ca-400-{}", key.len());
        let payload = bench.payload(&id, vec![user("hi")], |payload| {
            let mut agent = bench.agent(&id);
            agent.insert("env", bench.env(trimmed));
            payload.insert("agent", agent);
            payload.remove("apiKey");
        });
        let failed = run(&rust, &id, &payload);
        let error = failed.error();
        assert_eq!(error.str_at("kind"), Some("permanent"));
        assert!(!failed.terminal().to_text().contains(trimmed), "{}", failed.terminal());
        assert!(error.str_at("message").unwrap().contains("[redacted]"), "{error}");
    }
    bench.deny.assert_untouched();
}

/// 30h': a complete tool_use cut off by max_tokens. The CLI still invokes the handler, so the step must end as a tool round
/// instead of waiting for a `tool_use` stop reason that never comes.
#[test]
fn a_tool_call_cut_by_max_tokens_is_still_a_tool_round() {
    let bench = real_cli!();
    let rust = Rust::start();
    bench.upstream.push(Script {
        stop_reason: Some("max_tokens"),
        ..Script::blocks(vec![tool_use_block("toolu_cut", "read_file", jobj! { "path": "abc" })])
    });
    let cut = run(&rust, "ca-cut-1", &bench.payload("ca-cut", vec![user("read abc")], |_| {}));
    let result = cut.result();
    assert_eq!((result.str_at("finishReason"), result.str_at("rawFinishReason")), (Some("tool-calls"), Some("max_tokens")));
    assert_eq!(result.get("calls").unwrap().as_arr().unwrap()[0].str_at("callId"), Some("toolu_cut"));
    bench.upstream.push(Script::text("after cut"));
    let messages = vec![user("read abc"), assistant_of(&cut), tool_result("toolu_cut", "read_file", text_output("abc!"))];
    let after = run(&rust, "ca-cut-2", &bench.payload("ca-cut", messages, |_| {}));
    assert_eq!(after.result().str_at("text"), Some("after cut"));
    rust.send(&release_frame("ca-cut"));
    bench.deny.assert_untouched();
}

/// 30i, 30j: the reasoning level and output ceiling reach the request as effort, thinking and max_tokens.
#[test]
fn reasoning_and_the_output_ceiling_reach_the_request() {
    let bench = real_cli!();
    let rust = Rust::start();
    bench.upstream.push(Script::text("effort ok"));
    let round = run(&rust, "ca-effort", &bench.payload("ca-effort", vec![user("hi")], |payload| {
        payload.insert("reasoning", "xhigh");
        payload.insert("maxOutputTokens", 4096u64);
    }));
    assert_eq!(round.terminal().str_at("type"), Some("done"));
    let body = bench.upstream.last_call().body;
    assert_eq!(body.path(&["output_config", "effort"]).and_then(Json::as_str), Some("xhigh"));
    assert_eq!(body.path(&["thinking", "type"]).and_then(Json::as_str), Some("adaptive"));
    assert_eq!(body.get("max_tokens").and_then(Json::as_u64), Some(4096));
    rust.send(&release_frame("ca-effort"));

    bench.upstream.push(Script::text("max ok"));
    let round = run(&rust, "ca-max", &bench.payload("ca-max", vec![user("hi")], |payload| {
        payload.insert("reasoning", "max");
    }));
    assert_eq!(round.terminal().str_at("type"), Some("done"));
    let body = bench.upstream.last_call().body;
    assert_eq!(body.path(&["output_config", "effort"]).and_then(Json::as_str), Some("max"));
    assert_eq!(body.path(&["thinking", "type"]).and_then(Json::as_str), Some("adaptive"));
    rust.send(&release_frame("ca-max"));
    bench.deny.assert_untouched();
}

/// 30q: under `display: "updates"` the thinking block's text is omitted and a narration block follows. The pair is one
/// reasoning item whose text is the summary, and each block keeps its own signed part.
#[test]
fn omitted_thinking_and_its_narration_are_one_item() {
    let bench = real_cli!();
    let rust = Rust::start();
    let narration = "Godot is installed; next I build a minimal project.\n\n";
    bench.upstream.push(Script::blocks(vec![
        jobj! { "type": "thinking", "thinking": "", "signature": "sig_omitted", "estimatedTokens": jarr![120u64, 200u64] },
        thinking_block(narration, "sig_narration"),
        tool_use_block("toolu_narr", "read_file", jobj! { "path": "project.godot" }),
    ]));
    let narrated = run(&rust, "ca-narr", &bench.payload("ca-narr", vec![user("set up godot")], |_| {}));
    assert_eq!(narrated.result().get("reasoning").unwrap(), &jarr![narration]);
    let reasoning: Vec<&Json> = narrated.events().into_iter().filter(|event| event.str_at("k").is_some_and(|k| k.starts_with("reasoning-"))).collect();
    assert!(reasoning.iter().all(|event| event.get("item").and_then(Json::as_u64) == Some(0)), "{reasoning:?}");
    assert_eq!(reasoning[0].str_at("k"), Some("reasoning-start"));
    assert_eq!(events_named(&narrated, "reasoning-start"), 1);
    assert_eq!(events_named(&narrated, "reasoning-done"), 1);
    assert_eq!(reasoning.last().unwrap().str_at("k"), Some("reasoning-done"));
    let deltas: String = narrated.events_of("reasoning-delta").iter().filter_map(|event| event.str_at("delta")).collect();
    assert_eq!(deltas, narration);
    // The omitted thinking is most of the wait and streams no text: its progress travels as an estimate.
    let progress: Vec<u64> = narrated.events_of("reasoning-progress").iter().filter_map(|event| event.get("estimatedTokens").and_then(Json::as_u64)).collect();
    assert_eq!(&progress[..2], [120, 320]);
    assert_eq!(*progress.last().unwrap(), 320);
    let assistant = assistant_of(&narrated);
    let parts: Vec<&Json> = blocks_of(&assistant).iter().filter(|part| part.str_at("type") == Some("reasoning")).collect();
    assert_eq!(parts.len(), 2);
    assert_eq!((parts[0].str_at("text"), parts[0].path(&["providerOptions", "anthropic", "signature"]).and_then(Json::as_str)), (Some(""), Some("sig_omitted")));
    assert_eq!((parts[1].str_at("text"), parts[1].path(&["providerOptions", "anthropic", "signature"]).and_then(Json::as_str)), (Some(narration), Some("sig_narration")));
    rust.send(&release_frame("ca-narr"));
    bench.deny.assert_untouched();
}

/// 30p: the host sends a bare model id plus its window; the 1M budget is asked for only above 200k, and an id installed
/// with the suffix by an earlier version passes through. The suffix itself never reaches the wire.
#[test]
fn the_context_window_picks_the_clis_budget() {
    let bench = real_cli!();
    let rust = Rust::start();
    let budget_of = |id: &str, model: &str, window: u64| {
        bench.upstream.push(Script::text("budget ok"));
        let before = bench.upstream.calls().len();
        let frame = run(&rust, id, &bench.payload(id, vec![user("hi")], |payload| {
            payload.insert("modelId", model);
            payload.insert("contextWindow", window);
        }));
        rust.send(&release_frame(id));
        assert_eq!(frame.terminal().str_at("type"), Some("done"), "{}", frame.terminal());
        let calls = bench.upstream.calls();
        let call = calls[before..].iter().find(|call| call.body.get("max_tokens").and_then(Json::as_u64).unwrap_or(0) > 1).expect("a real call").clone();
        (call.body.str_at("model").unwrap_or("").to_owned(), call.header("anthropic-beta").unwrap_or("").contains("context-1m"))
    };
    assert_eq!(budget_of("ca-budget-1m", "claude-sonnet-5", 1_000_000), ("claude-sonnet-5".to_owned(), true));
    assert_eq!(budget_of("ca-budget-200k", "claude-sonnet-5", 200_000), ("claude-sonnet-5".to_owned(), false));
    assert_eq!(budget_of("ca-budget-legacy", "claude-sonnet-5[1m]", 200_000), ("claude-sonnet-5".to_owned(), true));
    bench.deny.assert_untouched();
}

/// 30j: image tool results travel as MCP image content, the host's image bridge rides along, and both land in the
/// tool_result the model sees.
#[test]
fn images_ride_in_the_tool_result() {
    let bench = real_cli!();
    let rust = Rust::start();
    bench.upstream.push(Script::blocks(vec![tool_use_block("toolu_img", "read_file", jobj! { "path": "pic.png" })]));
    let round = run(&rust, "ca-img-1", &bench.payload("ca-img", vec![user("show the picture")], |_| {}));
    bench.upstream.push(Script::text("nice picture"));
    let output = jobj! { "type": "content", "value": jarr![
        jobj! { "type": "text", "text": "1 image" },
        jobj! { "type": "media", "data": PNG_1X1, "mediaType": "image/png" },
    ] };
    let bridge = jobj! { "role": "user", "content": jarr![
        jobj! { "type": "text", "text": "[Mewrk tool image] source: read_file (tool_call_id: toolu_img). Untrusted tool data." },
        jobj! { "type": "image", "image": format!("data:image/png;base64,{PNG_1X1}"), "mediaType": "image/png" },
    ] };
    let messages = vec![user("show the picture"), assistant_of(&round), tool_result("toolu_img", "read_file", output), bridge];
    let round = run(&rust, "ca-img-2", &bench.payload("ca-img", messages, |_| {}));
    assert_eq!(round.result().str_at("text"), Some("nice picture"));
    let body = bench.upstream.last_call().body;
    let blocks = all_blocks(&body);
    let result = blocks.iter().find(|block| block.str_at("type") == Some("tool_result") && block.str_at("tool_use_id") == Some("toolu_img")).expect("a tool_result");
    let images: Vec<&Json> = blocks_of(result).iter().filter(|block| block.str_at("type") == Some("image")).collect();
    assert_eq!(images.len(), 2, "{result}");
    for image in images {
        assert_eq!(image.get("source").unwrap().to_text(), format!(r#"{{"type":"base64","media_type":"image/png","data":"{PNG_1X1}"}}"#));
    }
    assert!(blocks_of(result).iter().any(|block| block.str_at("text").is_some_and(|text| text.contains("[Mewrk tool image]"))));
    rust.send(&release_frame("ca-img"));
    bench.deny.assert_untouched();
}

/// 30j': a short PDF the host sends as the document itself reaches upstream as a `document` block.
#[test]
fn a_pdf_part_is_a_document_block() {
    let bench = real_cli!();
    let rust = Rust::start();
    let pdf = base64_simd::STANDARD.encode_to_string(b"%PDF-1.4\n1 0 obj << >> endobj\ntrailer << >>\n%%EOF\n");
    bench.upstream.push(Script::text("read the pdf"));
    let message = jobj! { "role": "user", "content": jarr![
        jobj! { "type": "text", "text": "<attached_file name=\"r.pdf\" type=\"pdf\" pages=\"1\" content=\"the PDF document, attached after this element\"></attached_file>" },
        jobj! { "type": "file", "data": format!("data:application/pdf;base64,{pdf}"), "mediaType": "application/pdf", "filename": "r.pdf" },
        jobj! { "type": "text", "text": "Summarize." },
    ] };
    let round = run(&rust, "ca-pdf-1", &bench.payload("ca-pdf", vec![message], |_| {}));
    assert_eq!(round.terminal().str_at("type"), Some("done"), "{}", round.terminal());
    let body = bench.upstream.last_call().body;
    let documents: Vec<&Json> = all_blocks(&body).into_iter().filter(|block| block.str_at("type") == Some("document")).collect();
    assert_eq!(documents.len(), 1, "{body}");
    assert_eq!(documents[0].str_at("title"), Some("r.pdf"));
    assert_eq!(documents[0].get("source").unwrap().to_text(), format!(r#"{{"type":"base64","media_type":"application/pdf","data":"{pdf}"}}"#));
    rust.send(&release_frame("ca-pdf"));
    bench.deny.assert_untouched();
}

/// 30o: native search. The session gets Claude Code's own `WebSearch` and nothing else; the CLI answers that tool itself
/// with a second API request, so the step spans the whole turn. The permission callback holds the budget, the hits become
/// sources, and the usage counts the CLI's own request too.
#[test]
fn native_search_runs_one_whole_turn_under_the_budget() {
    let bench = real_cli!();
    let rust = Rust::start();
    bench.upstream.push(Script::blocks(vec![
        text_block("Searching."),
        tool_use_block("toolu_ws_a", "WebSearch", jobj! { "query": "mewrk native search" }),
        tool_use_block("toolu_ws_b", "WebSearch", jobj! { "query": "mewrk native search again" }),
    ]));
    bench.upstream.push(Script::blocks(vec![
        jobj! { "type": "server_tool_use", "id": "srvtoolu_01", "name": "web_search", "input": jobj! { "query": "mewrk native search" } },
        jobj! {
            "type": "web_search_tool_result",
            "tool_use_id": "srvtoolu_01",
            "content": jarr![
                jobj! { "type": "web_search_result", "title": "Result A", "url": "https://a.example/page", "encrypted_content": "enc_a", "page_age": Json::Null },
                jobj! { "type": "web_search_result", "title": "Result B", "url": "https://b.example/doc", "encrypted_content": "enc_b", "page_age": Json::Null },
            ],
        },
        text_block("Two pages matched."),
    ]));
    bench.upstream.push(Script::text("Findings: see https://a.example/page"));
    let searched = run(&rust, "ca-search", &bench.payload("ca-search", vec![user("Search the web for: mewrk native search")], |payload| {
        payload.insert("tools", Vec::<Json>::new());
        payload.insert("nativeSearch", jobj! { "maxUses": 1u64 });
    }));
    rust.send(&release_frame("ca-search"));
    let result = searched.result();
    assert_eq!(result.str_at("finishReason"), Some("stop"));
    assert_eq!(result.str_at("text"), Some("Searching.\n\nFindings: see https://a.example/page"));
    assert_eq!(result.get("calls").unwrap().as_arr().unwrap().len(), 0);
    assert_eq!(result.get("nativeSearchUses").and_then(Json::as_u64), Some(1));
    let ids = result.get("nativeSearchCallIds").unwrap().as_arr().unwrap();
    assert_eq!(ids.len(), 1);
    assert!(matches!(ids[0].as_str(), Some("toolu_ws_a" | "toolu_ws_b")));
    assert_eq!(
        result.get("sources").unwrap().to_text(),
        r#"[{"id":"search:https://a.example/page","url":"https://a.example/page","title":"Result A"},{"id":"search:https://b.example/doc","url":"https://b.example/doc","title":"Result B"}]"#
    );
    let calls = bench.upstream.calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(declared_tools(&calls[0].body), ["WebSearch"]);
    let inner = calls[1].body.get("tools").unwrap().as_arr().unwrap();
    assert_eq!(inner.len(), 1);
    assert!(inner[0].str_at("type").is_some_and(|kind| kind.starts_with("web_search_")), "{}", calls[1].body);
    let last = last_turn_message(&calls[2].body).expect("a last message");
    let answered: Vec<&Json> = blocks_of(last).iter().filter(|block| block.str_at("type") == Some("tool_result")).collect();
    assert_eq!(answered.len(), 2, "{last}");
    let refused = answered.iter().filter(|block| block.get("content").unwrap().to_text().contains("search budget for this query is spent")).count();
    assert_eq!(refused, 1, "{last}");
    let usage = result.get("usage").unwrap();
    assert_eq!((usage.get("inputTokens").and_then(Json::as_u64), usage.get("cacheReadTokens").and_then(Json::as_u64)), (Some(450), Some(60)));
    bench.deny.assert_untouched();
}

// ---------------------------------------------------------------- fake web for WebFetch

/// A throwaway certificate for `hosts`, made with `openssl` where there is one.
fn throwaway_certificate(scratch: &Scratch, hosts: &[&str]) -> Option<(String, Vec<u8>, Vec<u8>)> {
    let (key_file, cert_file) = (scratch.text("key.pem"), scratch.text("cert.pem"));
    let names = hosts.iter().map(|host| format!("DNS:{host}")).collect::<Vec<_>>().join(",");
    let made = Command::new("openssl")
        .args(["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", &key_file, "-out", &cert_file, "-days", "1", "-subj", "/CN=mewrk-selfcheck", "-addext"])
        .arg(format!("subjectAltName={names}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    if !made.success() {
        return None;
    }
    Some((cert_file.clone(), fs::read(key_file).ok()?, fs::read(cert_file).ok()?))
}

/// The proxy and the web behind it: every CONNECT reaches a TLS server of its own. Every host's domain check passes;
/// `pages` maps a path to the HTML served there, and any other path is a 404.
struct FakeWeb {
    url: String,
    seen: Arc<Mutex<Vec<String>>>,
}

impl FakeWeb {
    fn start(key: &[u8], certificate: &[u8], pages: Vec<(String, String)>) -> Self {
        use rustls::pki_types::pem::PemObject;
        use rustls::pki_types::{CertificateDer, PrivateKeyDer};
        let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(certificate).map(|der| der.unwrap()).collect();
        let private = PrivateKeyDer::from_pem_slice(key).expect("a private key");
        let config = rustls::ServerConfig::builder_with_provider(rustls_graviola::default_provider().into())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(chain, private)
            .unwrap();
        let config = Arc::new(config);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let record = seen.clone();
        let pages = Arc::new(pages);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (config, record, pages) = (config.clone(), record.clone(), pages.clone());
                std::thread::spawn(move || serve_tunnel(stream, config, &record, &pages));
            }
        });
        Self { url, seen }
    }
}

fn serve_tunnel(mut stream: TcpStream, config: Arc<rustls::ServerConfig>, seen: &Mutex<Vec<String>>, pages: &[(String, String)]) {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).unwrap_or(0) == 0 {
            return;
        }
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    let line = head.lines().next().unwrap_or("").to_owned();
    let mut parts = line.split(' ');
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    if method != "CONNECT" {
        // A plain-HTTP proxy request: nothing the CLI should send.
        seen.lock().unwrap().push(format!("PROXY {method} {target}"));
        let _ = stream.write_all(b"HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\n\r\n");
        return;
    }
    seen.lock().unwrap().push(format!("CONNECT {target}"));
    if stream.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").is_err() {
        return;
    }
    let Ok(connection) = rustls::ServerConnection::new(config) else { return };
    let mut tls = rustls::StreamOwned::new(connection, stream);
    loop {
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            if tls.read(&mut byte).unwrap_or(0) == 0 {
                return;
            }
            request.push(byte[0]);
        }
        let request = String::from_utf8_lossy(&request).into_owned();
        let line = request.lines().next().unwrap_or("");
        let mut parts = line.split(' ');
        let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        let host = request.lines().find_map(|line| line.to_ascii_lowercase().strip_prefix("host:").map(|host| host.trim().to_owned())).unwrap_or_default();
        seen.lock().unwrap().push(format!("{method} {host}{path}"));
        let (status, kind, body) = if path.starts_with("/api/web/domain_info") {
            ("200 OK", "application/json", r#"{"can_fetch":true}"#.to_owned())
        } else if let Some((_, page)) = pages.iter().find(|(known, _)| known == path) {
            ("200 OK", "text/html; charset=utf-8", page.clone())
        } else {
            ("404 Not Found", "text/plain", "not found".to_owned())
        };
        let reply = format!("HTTP/1.1 {status}\r\ncontent-type: {kind}\r\ncontent-length: {}\r\n\r\n{body}", body.len());
        if tls.write_all(reply.as_bytes()).is_err() || tls.flush().is_err() {
            return;
        }
    }
}

/// 30p: native fetch. The permission callback holds the CLI to the pages the host named: a listed page spelled differently
/// is admitted and fetched as the host spelled it, a second call for it is refused, and so is an address nobody named, here
/// one on the CLI's preapproved documentation list. Everything the CLI does on the web goes through a local proxy that
/// answers for every host, Anthropic's domain check included, so nothing leaves this machine.
#[test]
fn native_fetch_is_held_to_the_named_pages() {
    let bench = real_cli!();
    let Some((cert_file, key, certificate)) = throwaway_certificate(&bench.scratch, &["api.anthropic.com", "example.com", "docs.python.org"]) else {
        eprintln!("skipped: no openssl to make a certificate for the fake web");
        return;
    };
    let rust = Rust::start();
    let web = FakeWeb::start(
        &key,
        &certificate,
        vec![("/a".to_owned(), "<html><head><title>Page A</title></head><body><nav>menu</nav><h1>Page A</h1><p>The body of page A.</p></body></html>".to_owned())],
    );
    bench.upstream.push(Script::blocks(vec![
        text_block("Fetching."),
        tool_use_block("toolu_wf_a", "WebFetch", jobj! { "url": "http://EXAMPLE.com/a#intro", "prompt": "Only the title." }),
        tool_use_block("toolu_wf_b", "WebFetch", jobj! { "url": "https://example.com/b", "prompt": "All of it." }),
        tool_use_block("toolu_wf_c", "WebFetch", jobj! { "url": "https://docs.python.org/3/", "prompt": "All of it." }),
        tool_use_block("toolu_wf_d", "WebFetch", jobj! { "url": "https://example.com/a", "prompt": "All of it." }),
    ]));
    // The CLI's own model, rendering page A.
    bench.upstream.push(Script::text("# Page A\n\nThe body of page A."));
    bench.upstream.push(Script::text("https://example.com/b could not be fetched (404)."));
    let fetched = run(&rust, "ca-fetch", &bench.payload("ca-fetch", vec![user("Fetch each of these URLs, one call per URL:\n\nhttps://example.com/a\nhttps://example.com/b")], |payload| {
        payload.insert("tools", Vec::<Json>::new());
        payload.insert("nativeFetch", jobj! { "maxUses": 2u64, "urls": jarr!["https://example.com/a", "https://example.com/b"] });
        let mut agent = bench.agent("ca-fetch");
        let mut env = bench.env(UPSTREAM_KEY);
        env.insert("HTTPS_PROXY", web.url.as_str());
        env.insert("HTTP_PROXY", web.url.as_str());
        env.insert("NO_PROXY", "127.0.0.1,localhost");
        env.insert("NODE_EXTRA_CA_CERTS", cert_file.as_str());
        agent.insert("env", env);
        payload.insert("agent", agent);
    }));
    rust.send(&release_frame("ca-fetch"));
    let result = fetched.result();
    let calls = bench.upstream.calls();
    assert_eq!(result.str_at("finishReason"), Some("stop"));
    assert_eq!(result.get("calls").unwrap().as_arr().unwrap().len(), 0);
    assert_eq!(result.get("webDocuments").unwrap().to_text(), r##"[{"url":"https://example.com/a","text":"# Page A\n\nThe body of page A."}]"##);
    assert!(result.get("nativeSearchUses").is_none());
    assert_eq!(calls.len(), 3);
    let errors = result.get("providerToolErrors").unwrap().as_arr().unwrap();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].str_at("toolName"), Some("web_fetch"));
    assert!(errors[0].str_at("message").unwrap().starts_with("https://example.com/b: HTTP 404"), "{}", errors[0]);
    assert_eq!(declared_tools(&calls[0].body), ["WebFetch"]);
    let rendering = calls[1].body.get("messages").unwrap().to_text();
    assert!(rendering.contains("The body of page A.") && rendering.contains("Reproduce this page's main content") && !rendering.contains("Only the title."), "{rendering}");
    let last = last_turn_message(&calls[2].body).expect("a last message");
    let answers: Vec<&Json> = blocks_of(last).iter().filter(|block| block.str_at("type") == Some("tool_result")).collect();
    assert_eq!(answers.len(), 4, "{last}");
    let answer_of = |id: &str| answers.iter().find(|block| block.str_at("tool_use_id") == Some(id)).map_or(String::new(), |block| block.get("content").unwrap().to_text());
    assert!(answer_of("toolu_wf_c").contains("may fetch only the URLs it was given") && answer_of("toolu_wf_c").contains("https://example.com/b"));
    assert_eq!(["toolu_wf_a", "toolu_wf_d"].iter().filter(|id| answer_of(id).contains("already fetched")).count(), 1);
    let seen = web.seen.lock().unwrap().clone();
    let tunnels: Vec<&String> = seen.iter().filter(|entry| entry.starts_with("CONNECT ")).collect();
    assert!(!tunnels.is_empty() && tunnels.iter().all(|entry| *entry == "CONNECT api.anthropic.com:443" || *entry == "CONNECT example.com:443"), "{seen:?}");
    assert!(seen.iter().any(|entry| entry == "GET example.com/a"), "{seen:?}");
    assert!(!seen.iter().any(|entry| entry.contains("docs.python.org") || entry.starts_with("PROXY ")), "{seen:?}");
    let usage = result.get("usage").unwrap();
    assert_eq!((usage.get("inputTokens").and_then(Json::as_u64), usage.get("cacheReadTokens").and_then(Json::as_u64)), (Some(450), Some(60)));
    bench.deny.assert_untouched();
}

/// Sidecar shutdown with a parked session takes the CLI with it, so a host restart never leaves a `claude` behind.
#[test]
fn shutdown_takes_a_parked_cli_with_it() {
    let bench = real_cli!();
    let rust = Rust::start();
    bench.upstream.push(Script::blocks(vec![tool_use_block("toolu_park", "bash", jobj! { "command": "sleep" })]));
    let parked = run(&rust, "ca-park", &bench.payload("ca-park", vec![user("park")], |_| {}));
    assert_eq!(parked.terminal().str_at("type"), Some("done"));
    assert!(!processes_under(&bench.cwd()).is_empty(), "the parked CLI should be alive");
    rust.send(r#"{"v":18,"type":"shutdown"}"#);
    std::thread::sleep(Duration::from_millis(1000));
    assert_eq!(processes_under(&bench.cwd()), Vec::<String>::new());
    bench.deny.assert_untouched();
}

// ---------------------------------------------------------------- stand-ins: the protocol from the SDK's side

/// A `/bin/sh` stand-in for the CLI, for the checks that need no real executable.
struct StandIn {
    scratch: Scratch,
    sdk: Option<String>,
}

/// One line of NDJSON for a script to print, quoted for `printf '%s\n'`.
fn say(frame: &str) -> String {
    format!("printf '%s\\n' '{frame}'\n")
}

impl StandIn {
    fn new() -> Self {
        let scratch = Scratch::new("standin");
        fs::create_dir_all(scratch.path("cwd")).unwrap();
        fs::create_dir_all(scratch.path("config")).unwrap();
        Self { scratch, sdk: None }
    }

    /// One whose agent names a real SDK, which the Node sidecar can load.
    fn with_sdk(sdk: &str) -> Self {
        Self { sdk: Some(sdk.to_owned()), ..Self::new() }
    }

    /// Writes `body` as an executable script named `name`.
    fn script(&self, name: &str, body: &str) -> String {
        let path = self.scratch.path(name);
        fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        path.to_string_lossy().into_owned()
    }

    fn payload(&self, session: &str, executable: &str, messages: Vec<Json>, extra: impl FnOnce(&mut Obj)) -> Json {
        let mut payload = Obj::new();
        payload.insert("family", "claude-agent");
        payload.insert("modelId", "claude-test");
        payload.insert("system", HOST_PROMPT);
        payload.insert("messages", messages);
        payload.insert("tools", tools());
        payload.insert(
            "agent",
            jobj! {
                "session": session,
                "executable": executable,
                "sdk"?: self.sdk.clone(),
                "cliVersion": "7.6.5",
                "cwd": self.scratch.text("cwd"),
                "env": jobj! { "CLAUDE_CONFIG_DIR": self.scratch.text("config"), "HOME": self.scratch.text("cwd") },
            },
        );
        extra(&mut payload);
        Json::Obj(payload)
    }

    fn lines(&self, name: &str) -> Vec<String> {
        fs::read_to_string(self.scratch.path(name)).unwrap_or_default().lines().map(str::to_owned).collect()
    }

    /// Waits for `count` lines in a file a stand-in appends to.
    fn wait_lines(&self, name: &str, count: usize) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let lines = self.lines(name);
            if lines.len() >= count {
                return lines;
            }
            assert!(Instant::now() < deadline, "only {} lines in {name}: {lines:?}", lines.len());
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

const RESULT_OK: &str = r#"{"type":"result","subtype":"success","is_error":false,"result":"ok"}"#;

fn stream_event(event: &str) -> String {
    format!(r#"{{"type":"stream_event","parent_tool_use_id":null,"event":{event}}}"#)
}

/// A stream event as `printf` needs it.
fn say_event(event: &str) -> String {
    say(&stream_event(event))
}

const MESSAGE_START: &str = r#"{"type":"message_start","message":{"model":"served-model","usage":{"input_tokens":10,"output_tokens":1}}}"#;
const MESSAGE_DELTA_TOOL: &str = r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}}"#;
const MESSAGE_STOP: &str = r#"{"type":"message_stop"}"#;

/// The argv, the environment and the frames the SDK would have produced, and a failed exit reported with stderr's tail.
#[test]
fn a_fresh_session_starts_the_way_the_sdk_starts_it() {
    let stand_in = StandIn::new();
    let (argv, env, stdin) = (stand_in.scratch.path("argv"), stand_in.scratch.path("env"), stand_in.scratch.path("stdin"));
    let script = stand_in.script(
        "claude",
        &format!(
            "for arg in \"$@\"; do printf '%s\\n' \"$arg\" >> '{}'; done\nenv > '{}'\nIFS= read -r one; IFS= read -r two\nprintf '%s\\n%s\\n' \"$one\" \"$two\" > '{}'\necho 'the stand-in says no' >&2\nexit 3\n",
            argv.display(),
            env.display(),
            stdin.display()
        ),
    );
    let rust = Rust::start();
    let outcome = run(&rust, "s1", &stand_in.payload("s1", &script, vec![user("hi")], |payload| {
        payload.insert("reasoning", "max");
        payload.insert("maxOutputTokens", 512u64);
        payload.insert("contextWindow", 1_000_000u64);
    }));
    assert_eq!(outcome.error().to_text(), r#"{"kind":"permanent","message":"Claude Code process exited with code 3\nthe stand-in says no"}"#);

    let cwd = fs::canonicalize(stand_in.scratch.path("cwd")).unwrap();
    let plugins: Vec<PathBuf> = fs::read_dir(cwd.join("plugins")).unwrap().flatten().map(|entry| entry.path()).collect();
    assert_eq!(plugins.len(), 1);
    assert!(plugins[0].join("hooks/context.ts").is_file() && plugins[0].join(".claude-plugin/plugin.json").is_file());
    let argv = stand_in.lines("argv");
    let plugin_at = argv.iter().position(|arg| arg == "--plugin-dir").unwrap();
    assert_eq!(fs::canonicalize(&argv[plugin_at + 1]).unwrap(), fs::canonicalize(&plugins[0]).unwrap());
    let mut shown = argv.clone();
    shown[plugin_at + 1] = "<plugin>".to_owned();
    assert_eq!(
        shown,
        [
            "--output-format", "stream-json", "--verbose", "--input-format", "stream-json", "--effort", "max", "--model", "claude-test[1m]",
            "--permission-prompt-tool", "stdio", "--tools", "", "--setting-sources=", "--strict-mcp-config", "--permission-mode", "default",
            "--include-partial-messages", "--plugin-dir", "<plugin>", "--no-session-persistence", "--settings", r#"{"totalTokensReminder":"off"}"#,
        ]
    );

    let env_lines = stand_in.lines("env");
    let var = |name: &str| env_lines.iter().find_map(|line| line.strip_prefix(&format!("{name}="))).map(str::to_owned);
    let config = stand_in.scratch.text("config");
    assert_eq!(var("CLAUDE_CONFIG_DIR").as_deref(), Some(config.as_str()));
    assert_eq!(var("CLAUDE_SECURESTORAGE_CONFIG_DIR").as_deref(), Some(config.as_str()));
    assert_eq!(var("CLAUDE_AGENT_SDK_VERSION").as_deref(), Some("0.3.284"));
    assert_eq!(var("CLAUDE_CODE_ENTRYPOINT").as_deref(), Some("sdk-ts"));
    assert_eq!(var("CLAUDE_CODE_SDK_READS_SESSION_STATE").as_deref(), Some("1"));
    assert_eq!(var("CLAUDE_AGENT_SDK_MCP_NO_PREFIX").as_deref(), Some("1"));
    assert_eq!(var("CLAUDE_CODE_ENABLE_FUNCTION_HOOKS").as_deref(), Some("1"));
    assert_eq!(var("MCP_TOOL_TIMEOUT").as_deref(), Some("604800000"));
    assert_eq!(var("CLAUDE_CODE_MAX_OUTPUT_TOKENS").as_deref(), Some("512"));
    assert_eq!(var("ANTHROPIC_MODEL").as_deref(), Some("claude-test[1m]"));
    for name in ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL", "CLAUDE_CODE_OAUTH_TOKEN", "DEBUG", "NODE_OPTIONS"] {
        assert_eq!(var(name), None, "{name}");
    }
    #[cfg(unix)]
    assert!(var("USER").is_some_and(|user| !user.is_empty()));

    let stdin = stand_in.lines("stdin");
    let initialize = j(&stdin[0]);
    assert_eq!(initialize.str_at("type"), Some("control_request"));
    assert_eq!(initialize.as_obj().unwrap().keys().collect::<Vec<_>>(), ["request_id", "type", "request"]);
    let request = initialize.get("request").unwrap();
    assert_eq!(
        request.as_obj().unwrap().keys().collect::<Vec<_>>(),
        ["subtype", "sdkMcpServers", "sdkMcpServerConfigs", "sdkMcpServerManifests", "systemPrompt", "systemPromptSnapshot"]
    );
    assert_eq!(request.get("sdkMcpServers").unwrap().to_text(), r#"["mewrk"]"#);
    assert_eq!(request.get("sdkMcpServerConfigs").unwrap().to_text(), r#"{"mewrk":{"timeout":604800000}}"#);
    assert_eq!(request.path(&["sdkMcpServerManifests", "mewrk", "initializeResult", "protocolVersion"]).and_then(Json::as_str), Some("2025-11-25"));
    let listed = request.path(&["sdkMcpServerManifests", "mewrk", "toolsListResult", "tools"]).unwrap().as_arr().unwrap();
    assert_eq!(listed.len(), 3);
    assert_eq!(listed[0].get("_meta").unwrap().to_text(), r#"{"anthropic/alwaysLoad":true}"#);
    assert_eq!(request.get("systemPrompt").unwrap(), &jarr![HOST_PROMPT]);
    assert_eq!(request.get("systemPromptSnapshot"), Some(&Json::Bool(false)));
    assert_eq!(stdin[1], r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"hi"}]},"parent_tool_use_id":null}"#);
}

/// A session with a past runs under a temporary config directory holding its transcript, and the credentials never move.
#[test]
fn a_resume_runs_under_a_temporary_config_dir_without_credentials() {
    let stand_in = StandIn::new();
    // A login where the SDK would have looked for it, and configuration it would have copied.
    fs::write(stand_in.scratch.path("config/.credentials.json"), r#"{"claudeAiOauth":{"accessToken":"secret-a","refreshToken":"secret-r"}}"#).unwrap();
    fs::write(stand_in.scratch.path("config/.claude.json"), r#"{"hasCompletedOnboarding":true}"#).unwrap();
    fs::write(stand_in.scratch.path("config/settings.json"), r#"{"enabledPlugins":{"x":true},"env":{"CLAUDE_CONFIG_DIR":"/elsewhere","KEEP":"1"}}"#).unwrap();
    let dump = stand_in.scratch.path("dump");
    let script = stand_in.script(
        "claude",
        &format!(
            "{{ printf 'dir=%s\\nstorage=%s\\n' \"$CLAUDE_CONFIG_DIR\" \"$CLAUDE_SECURESTORAGE_CONFIG_DIR\"; for arg in \"$@\"; do printf 'arg=%s\\n' \"$arg\"; done; }} > '{dump}.tmp'\nmv '{dump}.tmp' '{dump}'\n{}cat > /dev/null\n",
            say(RESULT_OK),
            dump = dump.display(),
        ),
    );
    let history = vec![
        user("earlier"),
        jobj! { "role": "assistant", "content": jarr![
            jobj! { "type": "reasoning", "text": "thought", "providerOptions": jobj! { "anthropic": jobj! { "signature": "sig" } } },
            text_block("earlier answer"),
        ] },
        user("and now"),
    ];
    let rust = Rust::start();
    let outcome = run(&rust, "s2", &stand_in.payload("s2", &script, history, |_| {}));
    assert_eq!(outcome.terminal().str_at("type"), Some("done"), "{}", outcome.terminal());
    let lines = stand_in.lines("dump");
    let field = |prefix: &str| lines.iter().find_map(|line| line.strip_prefix(prefix)).unwrap_or("").to_owned();
    let dir = PathBuf::from(field("dir="));
    let own = stand_in.scratch.text("config");
    assert_ne!(dir.to_string_lossy(), own);
    assert!(dir.file_name().unwrap().to_str().unwrap().starts_with("claude-resume-"));
    assert_eq!(field("storage="), own);
    let resume = lines.iter().find_map(|line| line.strip_prefix("arg=--resume=")).expect("a resume flag").to_owned();
    assert!(lines.iter().any(|line| line == "arg=--session-mirror"));
    assert!(!lines.iter().any(|line| line == "arg=--no-session-persistence"));

    let cwd = fs::canonicalize(stand_in.scratch.path("cwd")).unwrap();
    let key: String = cwd.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let transcript = fs::read_to_string(dir.join("projects").join(&key).join(format!("{resume}.jsonl"))).expect("the transcript");
    let entries: Vec<Json> = transcript.lines().map(j).collect();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].get("parentUuid"), Some(&Json::Null));
    assert_eq!(entries[1].get("parentUuid"), entries[0].get("uuid"));
    assert_eq!(entries[1].path(&["message", "model"]).and_then(Json::as_str), Some("claude-test"));
    assert_eq!(entries[1].get("version").and_then(Json::as_str), Some("7.6.5"));
    assert_eq!(entries[0].get("cwd").and_then(Json::as_str), Some(stand_in.scratch.text("cwd").as_str()));
    assert_eq!(entries[1].path(&["message", "content"]).unwrap().as_arr().unwrap()[0].str_at("signature"), Some("sig"));
    // The configuration came along, filtered; the login did not.
    assert_eq!(fs::read_to_string(dir.join(".claude.json")).unwrap(), r#"{"hasCompletedOnboarding":true}"#);
    assert_eq!(fs::read_to_string(dir.join("settings.json")).unwrap(), r#"{"env":{"KEEP":"1"}}"#);
    assert!(!dir.join(".credentials.json").exists());
    assert!(!transcript.contains("secret-a") && !lines.iter().any(|line| line.contains("secret-a")));
    // The temporary directory goes with the session.
    rust.send(&release_frame("s2"));
    let deadline = Instant::now() + Duration::from_secs(5);
    while dir.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!dir.exists());
}

/// Every control request the SDK answers is answered as it answered, and the ones it stayed silent on stay unanswered.
#[test]
fn control_requests_are_answered_like_the_sdk_answers_them() {
    let stand_in = StandIn::new();
    let replies = stand_in.scratch.path("replies");
    let mut body = String::new();
    // Discards `initialize` and the prompt, then asks.
    body.push_str("IFS= read -r one; IFS= read -r two\n");
    for (id, request) in [
        ("r-allow", r#"{"subtype":"can_use_tool","tool_name":"read_file","input":{"path":"x"},"tool_use_id":"t1"}"#),
        ("r-hook", r#"{"subtype":"hook_callback","callback_id":"hook_0","input":{}}"#),
        ("r-elicit", r#"{"subtype":"elicitation","mcp_server_name":"mewrk","message":"m"}"#),
        ("r-oauth", r#"{"subtype":"oauth_token_refresh"}"#),
        ("r-host", r#"{"subtype":"host_auth_token_refresh"}"#),
        ("r-secret", r#"{"subtype":"remote_control_work_secret","session_id":"s"}"#),
        ("r-silent-1", r#"{"subtype":"remote_tool_call"}"#),
        ("r-silent-2", r#"{"subtype":"request_user_dialog","dialog_kind":"k"}"#),
        ("r-odd", r#"{"subtype":"something_new"}"#),
        ("r-server", r#"{"subtype":"mcp_message","server_name":"other","message":{"jsonrpc":"2.0","id":1,"method":"ping"}}"#),
        ("r-ping", r#"{"subtype":"mcp_message","server_name":"mewrk","message":{"jsonrpc":"2.0","id":2,"method":"ping"}}"#),
        ("r-note", r#"{"subtype":"mcp_message","server_name":"mewrk","message":{"jsonrpc":"2.0","method":"notifications/initialized"}}"#),
        ("r-init", r#"{"subtype":"mcp_message","server_name":"mewrk","message":{"jsonrpc":"2.0","id":3,"method":"initialize","params":{"protocolVersion":"2030-01-01"}}}"#),
        ("r-unknown", r#"{"subtype":"mcp_message","server_name":"mewrk","message":{"jsonrpc":"2.0","id":4,"method":"prompts/list"}}"#),
    ] {
        body.push_str(&say(&format!(r#"{{"type":"control_request","request_id":"{id}","request":{request}}}"#)));
    }
    body.push_str(&format!("i=0; while [ $i -lt 11 ] && IFS= read -r line; do printf '%s\\n' \"$line\" >> '{}'; i=$((i+1)); done\n", replies.display()));
    body.push_str(&say(RESULT_OK));
    body.push_str("cat > /dev/null\n");
    let script = stand_in.script("claude", &body);
    let rust = Rust::start();
    let outcome = run(&rust, "s3", &stand_in.payload("s3", &script, vec![user("hi")], |_| {}));
    assert_eq!(outcome.terminal().str_at("type"), Some("done"), "{}", outcome.terminal());
    rust.send(&release_frame("s3"));
    assert_eq!(
        stand_in.lines("replies"),
        [
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"r-allow","response":{"behavior":"allow","updatedInput":{"path":"x"},"toolUseID":"t1"}}}"#,
            r#"{"type":"control_response","response":{"subtype":"error","request_id":"r-hook","error":"No hook callback found for ID: hook_0"}}"#,
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"r-elicit","response":{"action":"decline"}}}"#,
            r#"{"type":"control_response","response":{"subtype":"error","request_id":"r-oauth","error":"getOAuthToken callback is not provided."}}"#,
            r#"{"type":"control_response","response":{"subtype":"error","request_id":"r-host","error":"getHostAuthToken callback is not provided."}}"#,
            r#"{"type":"control_response","response":{"subtype":"error","request_id":"r-secret","error":"refreshWorkSecret callback is not provided."}}"#,
            r#"{"type":"control_response","response":{"subtype":"error","request_id":"r-odd","error":"Unsupported control request subtype: something_new"}}"#,
            r#"{"type":"control_response","response":{"subtype":"error","request_id":"r-server","error":"SDK MCP server not found: other"}}"#,
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"r-ping","response":{"mcp_response":{"jsonrpc":"2.0","id":2,"result":{}}}}}"#,
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"r-note","response":{"mcp_response":{"jsonrpc":"2.0","result":{},"id":0}}}}"#,
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"r-init","response":{"mcp_response":{"jsonrpc":"2.0","id":3,"result":{"protocolVersion":"2030-01-01","capabilities":{"tools":{"listChanged":true}},"serverInfo":{"name":"mewrk","version":"1.0.0"}}}}}}"#,
        ]
    );
}

/// A `tools/call` the CLI sends only after the results arrived is answered at once; one that came first stays parked. A
/// tool input the stream could not parse is supplied by the CLI's own call, and a duplicate delivery of a parked request is
/// skipped.
#[test]
fn parked_handlers_and_early_results_meet_in_either_order() {
    let stand_in = StandIn::new();
    let (replies, marker) = (stand_in.scratch.path("replies"), stand_in.scratch.path("marker"));
    let mut body = String::new();
    body.push_str("IFS= read -r one; IFS= read -r two\n");
    body.push_str(&say_event(MESSAGE_START));
    // `early`'s input arrives whole; `late`'s stream is cut, so only its `tools/call` can complete it.
    body.push_str(&say_event(r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"early","name":"mcp__mewrk__read_file","input":{}}}"#));
    body.push_str(&say_event(r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"e\"}"}}"#));
    body.push_str(&say_event(r#"{"type":"content_block_stop","index":0}"#));
    body.push_str(&say_event(r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"late","name":"read_file","input":{}}}"#));
    body.push_str(&say_event(r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#));
    body.push_str(&say_event(r#"{"type":"content_block_stop","index":1}"#));
    body.push_str(&say_event(MESSAGE_DELTA_TOOL));
    body.push_str(&say_event(MESSAGE_STOP));
    // The handler for `late` arrives now and parks; delivered twice, as a reconnecting transport may.
    let late_call = r#"{"type":"control_request","request_id":"call-late","request":{"subtype":"mcp_message","server_name":"mewrk","message":{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"read_file","arguments":{"path":"l"},"_meta":{"claudecode/toolUseId":"late"}}}}}"#;
    body.push_str(&say(late_call));
    body.push_str(&say(late_call));
    // The handler for `early` waits until the host's results must already be in.
    body.push_str(&format!("while [ ! -f '{}' ]; do sleep 0.02; done\n", marker.display()));
    body.push_str(&say(r#"{"type":"control_request","request_id":"call-early","request":{"subtype":"mcp_message","server_name":"mewrk","message":{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"read_file","arguments":{"path":"e"},"_meta":{"claudecode/toolUseId":"early"}}}}}"#));
    body.push_str(&format!("i=0; while [ $i -lt 2 ] && IFS= read -r line; do printf '%s\\n' \"$line\" >> '{}'; i=$((i+1)); done\n", replies.display()));
    body.push_str(&say_event(MESSAGE_START));
    body.push_str(&say_event(r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#));
    body.push_str(&say_event(r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"finished"}}"#));
    body.push_str(&say_event(r#"{"type":"content_block_stop","index":0}"#));
    body.push_str(&say_event(r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}"#));
    body.push_str(&say_event(MESSAGE_STOP));
    body.push_str(&say(RESULT_OK));
    body.push_str("cat > /dev/null\n");
    let script = stand_in.script("claude", &body);
    let rust = Rust::start();
    let round = run(&rust, "p1", &stand_in.payload("p", &script, vec![user("go")], |_| {}));
    let result = round.result();
    assert_eq!(result.str_at("finishReason"), Some("tool-calls"));
    assert_eq!(result.str_at("model"), Some("served-model"));
    assert_eq!(
        result.get("calls").unwrap().to_text(),
        r#"[{"callId":"early","toolName":"read_file","input":{"path":"e"}},{"callId":"late","toolName":"read_file","input":{"path":"l"}}]"#
    );
    assert_eq!(result.get("usage").unwrap().to_text(), r#"{"inputTokens":10,"outputTokens":5,"totalTokens":15}"#);
    let messages = vec![
        user("go"),
        assistant_of(&round),
        jobj! { "role": "tool", "content": jarr![
            jobj! { "type": "tool-result", "toolCallId": "late", "toolName": "read_file", "output": text_output("late result") },
            jobj! { "type": "tool-result", "toolCallId": "early", "toolName": "read_file", "output": jobj! { "type": "error-text", "value": "early failed" } },
        ] },
        jobj! { "role": "user", "content": "<note/>", "providerOptions": jobj! { "mewrk": jobj! { "hostMessage": true } } },
    ];
    rust.send(&step_frame("p2", &stand_in.payload("p", &script, messages, |_| {})));
    std::thread::sleep(Duration::from_millis(200));
    fs::write(&marker, "").unwrap();
    let round = rust.wait("p2", Duration::from_secs(20));
    assert_eq!(round.result().str_at("text"), Some("finished"));
    assert_eq!(round.result().str_at("finishReason"), Some("stop"));
    assert_eq!(round.result().str_at("rawFinishReason"), Some("end_turn"));
    assert_eq!(
        stand_in.lines("replies"),
        [
            // The call that parked first answers first; carriers fold into the last of `awaiting`, which is `late`.
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"call-late","response":{"mcp_response":{"jsonrpc":"2.0","id":11,"result":{"content":[{"type":"text","text":"late result"},{"type":"text","text":"<note/>"}]}}}}}"#,
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"call-early","response":{"mcp_response":{"jsonrpc":"2.0","id":10,"result":{"content":[{"type":"text","text":"early failed"}],"isError":true}}}}}"#,
        ]
    );
    rust.send(&release_frame("p"));
}

/// What a failed turn says: the CLI's login and rate-limit reports arrive as an `assistant` message with `error`, and a
/// failed `result` as its subtype and errors. All of it is permanent here; the host owns retries.
#[test]
fn failed_turns_report_what_the_cli_said() {
    let cases: [(&str, &str, &str); 6] = [
        (
            "auth",
            r#"{"type":"assistant","parent_tool_use_id":null,"error":"authentication_failed","message":{"content":[{"type":"text","text":"Invalid API key · Please run /login"}]}}"#,
            r#"{"kind":"permanent","message":"Invalid API key · Please run /login"}"#,
        ),
        (
            "rate",
            r#"{"type":"assistant","parent_tool_use_id":null,"error":"rate_limit","message":{"content":[]}}"#,
            r#"{"kind":"permanent","message":"Claude Code 报告错误：rate_limit"}"#,
        ),
        (
            "turns",
            r#"{"type":"result","subtype":"error_max_turns","is_error":false,"errors":["too many","turns"]}"#,
            r#"{"kind":"permanent","message":"error_max_turns: too many: turns"}"#,
        ),
        (
            "text",
            r#"{"type":"result","subtype":"success","is_error":true,"result":"API Error: 529 overloaded"}"#,
            r#"{"kind":"permanent","message":"API Error: 529 overloaded"}"#,
        ),
        (
            "empty",
            r#"{"type":"result","subtype":"success","is_error":true,"result":""}"#,
            r#"{"kind":"permanent","message":"Claude Code 回合失败"}"#,
        ),
        // A subagent's messages are not the step's.
        (
            "nested",
            r#"{"type":"assistant","parent_tool_use_id":"t9","error":"rate_limit","message":{"content":[]}}"#,
            r#"{"kind":"permanent","message":"Claude Code 会话在回复完成前结束"}"#,
        ),
    ];
    let rust = Rust::start();
    for (name, frame, expected) in cases {
        let stand_in = StandIn::new();
        let tail = if name == "nested" { "exit 0\n" } else { "cat > /dev/null\n" };
        let script = stand_in.script("claude", &format!("IFS= read -r one; IFS= read -r two\n{}{tail}", say(frame)));
        let outcome = run(&rust, name, &stand_in.payload(name, &script, vec![user("hi")], |_| {}));
        assert_eq!(outcome.error().to_text(), expected, "{name}");
    }
}

/// A process that dies in the middle of a turn says how it died, with a code or by a signal; one that answers and exits at
/// once still reaches its step.
#[test]
fn a_cli_that_dies_mid_turn_says_how() {
    let stand_in = StandIn::new();
    let rust = Rust::start();
    let script = stand_in.script("claude", &say(RESULT_OK));
    let outcome = run(&rust, "e0", &stand_in.payload("e0", &script, vec![user("hi")], |_| {}));
    assert_eq!(outcome.terminal().str_at("type"), Some("done"), "{}", outcome.terminal());

    let script = stand_in.script(
        "claude",
        &format!(
            "IFS= read -r one; IFS= read -r two\n{}{}{}{}{}exit 1\n",
            say_event(MESSAGE_START),
            say_event(r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#),
            say_event(r#"{"type":"content_block_stop","index":0}"#),
            say_event(MESSAGE_STOP),
            say(r#"{"type":"system","subtype":"session_state_changed","state":"idle"}"#),
        ),
    );
    let outcome = run(&rust, "e1", &stand_in.payload("e1", &script, vec![user("hi")], |_| {}));
    assert_eq!(outcome.error().to_text(), r#"{"kind":"permanent","message":"Claude Code process exited with code 1"}"#);

    let stand_in = StandIn::new();
    let script = stand_in.script("claude", "IFS= read -r one; IFS= read -r two\nkill -9 $$\n");
    let outcome = run(&rust, "e2", &stand_in.payload("e2", &script, vec![user("hi")], |_| {}));
    assert_eq!(outcome.error().to_text(), r#"{"kind":"permanent","message":"Claude Code process terminated by signal SIGKILL"}"#);
}

/// An unparsable `tool_use` waits for the CLI's own `assistant` message; a tool the host did not publish is the CLI's
/// business; a nested stream is not this step's; a redacted block is an item of its own.
#[test]
fn stream_edge_cases_translate_like_upstream() {
    let stand_in = StandIn::new();
    let mut body = String::from("IFS= read -r one; IFS= read -r two\n");
    body.push_str(&say(&format!(r#"{{"type":"stream_event","parent_tool_use_id":"nested","event":{MESSAGE_START}}}"#)));
    body.push_str(&say_event(MESSAGE_START));
    body.push_str(&say_event(r#"{"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"enc"}}"#));
    body.push_str(&say_event(r#"{"type":"content_block_stop","index":0}"#));
    body.push_str(&say_event(r#"{"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":""}}"#));
    body.push_str(&say_event(r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"own","name":"WebSearch","input":{}}}"#));
    body.push_str(&say_event(r#"{"type":"content_block_stop","index":2}"#));
    body.push_str(&say_event(r#"{"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"cut","name":"bash","input":{}}}"#));
    body.push_str(&say_event(r#"{"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"{oops"}}"#));
    body.push_str(&say_event(r#"{"type":"content_block_stop","index":3}"#));
    body.push_str(&say_event(MESSAGE_DELTA_TOOL));
    body.push_str(&say_event(MESSAGE_STOP));
    body.push_str(&say(r#"{"type":"assistant","parent_tool_use_id":null,"message":{"content":[{"type":"tool_use","id":"cut","name":"bash","input":{"command":"ls"}}]}}"#));
    body.push_str("cat > /dev/null\n");
    let script = stand_in.script("claude", &body);
    let rust = Rust::start();
    let outcome = run(&rust, "s4", &stand_in.payload("s4", &script, vec![user("hi")], |_| {}));
    let kinds: Vec<&str> = outcome.events().iter().filter_map(|event| event.str_at("k")).filter(|kind| *kind != "heartbeat").collect();
    assert_eq!(kinds, ["reasoning-start", "reasoning-done", "tool-call-announced", "usage", "tool-call"]);
    assert_eq!(outcome.events_of("reasoning-start")[0].str_at("form"), Some("encrypted"));
    assert_eq!(outcome.events_of("tool-call-announced")[0].str_at("toolName"), Some("bash"));
    let result = outcome.result();
    assert_eq!(result.get("reasoning").unwrap(), &jarr![""]);
    assert!(result.get("reasoningMs").is_some());
    assert_eq!(result.get("calls").unwrap().to_text(), r#"[{"callId":"cut","toolName":"bash","input":{"command":"ls"}}]"#);
    assert_eq!(
        result.get("responseMessages").unwrap().to_text(),
        r#"[{"role":"assistant","content":[{"type":"reasoning","text":"","providerOptions":{"anthropic":{"redactedData":"enc"}}},{"type":"tool-call","toolCallId":"cut","toolName":"bash","input":{"command":"ls"}}]}]"#
    );
    rust.send(&release_frame("s4"));
}

/// The host's limits on what one step may stream end the step and the session.
#[test]
fn oversized_streams_are_refused() {
    let rust = Rust::start();
    let big = |bytes: usize| format!("head -c {bytes} /dev/zero | tr '\\0' x");
    for (id, start, kind, field, bytes, message) in [
        ("big-args", r#"{"type":"tool_use","id":"t","name":"bash","input":{}}"#, "input_json_delta", "partial_json", 4 * 1024 * 1024 + 1, "工具 bash 的参数超过 4194304 字节上限"),
        ("big-text", r#"{"type":"text","text":""}"#, "text_delta", "text", 16 * 1024 * 1024 + 1, "单轮可见文本超过 16777216 字节上限"),
    ] {
        let stand_in = StandIn::new();
        let body = format!(
            "IFS= read -r one; IFS= read -r two\n{}{}printf '%s' '{{\"type\":\"stream_event\",\"parent_tool_use_id\":null,\"event\":{{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"{kind}\",\"{field}\":\"'; {}; printf '\"}}}}}}\\n'\nsleep 30\n",
            say_event(MESSAGE_START),
            say_event(&format!(r#"{{"type":"content_block_start","index":0,"content_block":{start}}}"#)),
            big(bytes),
        );
        let script = stand_in.script("claude", &body);
        let outcome = run(&rust, id, &stand_in.payload(id, &script, vec![user("hi")], |_| {}));
        assert_eq!(outcome.error().str_at("kind"), Some("permanent"));
        assert_eq!(outcome.error().str_at("message"), Some(message));
    }
}

/// Failures before the CLI starts: 30k, 30k2, 30l. Nothing is spawned, and each says what it means.
#[test]
fn a_bad_install_says_where_to_repair_it() {
    let stand_in = StandIn::new();
    let rust = Rust::start();
    let script = stand_in.script("claude", "exit 0\n");
    let with_agent = |id: &str, edit: &dyn Fn(&mut Obj)| {
        stand_in.payload(id, &script, vec![user("hi")], |payload| {
            let mut agent = payload.get("agent").unwrap().as_obj().unwrap().clone();
            edit(&mut agent);
            payload.insert("agent", agent);
        })
    };
    let missing = run(&rust, "m1", &with_agent("m1", &|agent| agent.insert("executable", "/no/such/claude.exe")));
    let message = missing.error().str_at("message").unwrap().to_owned();
    assert_eq!(missing.error().str_at("kind"), Some("permanent"));
    assert!(message.contains("Claude Code") && message.contains("重新安装"), "{message}");

    let no_cwd = run(&rust, "no-cwd", &with_agent("no-cwd", &|agent| {
        agent.remove("cwd");
    }));
    assert_eq!(no_cwd.error().str_at("message"), Some("claude-agent 请求缺少工作目录"));
    let no_agent = run(&rust, "no-agent", &jobj! { "family": "claude-agent", "messages": jarr![user("hi")] });
    assert_eq!(no_agent.error().str_at("message"), Some("claude-agent 请求缺少 agent 会话参数"));

    // A remote endpoint fails the whole request rather than pointing the user's own login at a third party.
    let remote = run(&rust, "remote", &with_agent("remote", &|agent| {
        agent.insert("env", jobj! { "ANTHROPIC_BASE_URL": "https://relay.example.com", "ANTHROPIC_API_KEY": UPSTREAM_KEY });
    }));
    assert_eq!(remote.error().str_at("kind"), Some("permanent"));
    assert_eq!(remote.error().str_at("message"), Some("agent.env 里的 ANTHROPIC_BASE_URL 只允许本机测试桩"));

    let no_prompt = run(&rust, "none", &stand_in.payload("none", &script, vec![], |_| {}));
    assert_eq!(no_prompt.error().str_at("message"), Some("Claude Code 需要一条用户消息才能开始回合"));
}

/// A CLI that will not start reports it the way the transport did; a CLI that exits cleanly before replying fails the step.
#[test]
fn a_cli_that_will_not_run_fails_the_step() {
    let stand_in = StandIn::new();
    let rust = Rust::start();
    let plain = stand_in.scratch.path("not-executable");
    fs::write(&plain, "not a program").unwrap();
    let path = plain.to_string_lossy().into_owned();
    let outcome = run(&rust, "x1", &stand_in.payload("x1", &path, vec![user("hi")], |_| {}));
    let message = outcome.error().str_at("message").unwrap().to_owned();
    assert_eq!(outcome.error().str_at("kind"), Some("permanent"));
    assert!(message.contains(&path) && message.contains("exists but failed to launch"), "{message}");

    let script = stand_in.script("claude", "exit 0\n");
    let outcome = run(&rust, "x2", &stand_in.payload("x2", &script, vec![user("hi")], |_| {}));
    assert_eq!(outcome.error().to_text(), r#"{"kind":"permanent","message":"Claude Code 会话在回复完成前结束"}"#);
}

/// A live step hears heartbeats; a release under it ends it the way the SDK's abort did; `cancel` ends one as cancelled.
#[test]
fn a_running_step_hears_heartbeats_and_release_and_cancel_end_it() {
    let stand_in = StandIn::new();
    let rust = Rust::start();
    let script = stand_in.script("claude", "exec sleep 60\n");
    rust.send(&step_frame("h1", &stand_in.payload("h", &script, vec![user("hi")], |_| {})));
    std::thread::sleep(Duration::from_millis(1300));
    rust.send(&release_frame("h"));
    let released = rust.wait("h1", Duration::from_secs(5));
    assert!(events_named(&released, "heartbeat") >= 2, "{:?}", released.frames);
    assert_eq!(released.error().to_text(), r#"{"kind":"permanent","message":"Operation aborted"}"#);

    let stand_in = StandIn::new();
    let script = stand_in.script("claude", "exec sleep 60\n");
    rust.send(&step_frame("h2", &stand_in.payload("h2", &script, vec![user("hi")], |_| {})));
    std::thread::sleep(Duration::from_millis(200));
    rust.send(r#"{"v":18,"type":"cancel","id":"h2"}"#);
    let cancelled = rust.wait("h2", Duration::from_secs(5));
    assert_eq!(cancelled.error().to_text(), r#"{"kind":"cancelled","message":"请求已取消"}"#);
}

/// A second turn on a live session is rebuilt, and a continuation that leaves a parked call unanswered is too.
#[test]
fn a_new_turn_or_a_missing_result_rebuilds_the_session() {
    let stand_in = StandIn::new();
    let starts = stand_in.scratch.path("starts");
    let mut body = format!("printf '%s\\n' \"$$\" >> '{}'\nIFS= read -r one; IFS= read -r two\n", starts.display());
    body.push_str(&say_event(MESSAGE_START));
    body.push_str(&say_event(r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"bash","input":{}}}"#));
    body.push_str(&say_event(r#"{"type":"content_block_stop","index":0}"#));
    body.push_str(&say_event(MESSAGE_STOP));
    body.push_str("cat > /dev/null\n");
    let script = stand_in.script("claude", &body);
    let rust = Rust::start();
    let round = run(&rust, "r1", &stand_in.payload("r", &script, vec![user("one")], |_| {}));
    assert_eq!(round.result().get("calls").unwrap().as_arr().unwrap().len(), 1);
    assert_eq!(stand_in.wait_lines("starts", 1).len(), 1);
    // A result for some other call: `t1` is still owed, so this cannot continue the parked round.
    let wrong = vec![user("one"), assistant_of(&round), tool_result("other", "bash", text_output("x"))];
    let round = run(&rust, "r2", &stand_in.payload("r", &script, wrong, |_| {}));
    assert_eq!(round.terminal().str_at("type"), Some("done"));
    assert_eq!(stand_in.wait_lines("starts", 2).len(), 2);
    // A genuine user message starts a new turn, in a session of its own.
    let next = vec![user("one"), assistant_of(&round), user("two")];
    let round = run(&rust, "r3", &stand_in.payload("r", &script, next, |_| {}));
    assert_eq!(round.terminal().str_at("type"), Some("done"));
    assert_eq!(stand_in.wait_lines("starts", 3).len(), 3);
}

// ---------------------------------------------------------------- differential: the same scenario through both sidecars

/// Text that legitimately differs from run to run: generated ids, the mock's port, the scratch directory.
fn normalize(text: &str, bench: &Bench) -> String {
    let mut out = text.replace(&bench.upstream.base, "<upstream>").replace(&bench.scratch.0.to_string_lossy().into_owned(), "<scratch>");
    let uuid = regex::Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").unwrap();
    out = uuid.replace_all(&out, "<uuid>").into_owned();
    out
}

/// The CLI runs a reply's read-only tools concurrently, and writes their results in the order they finished. That order
/// is the CLI's and the network's, so where calls are parallel the results are compared as a set.
fn results_as_a_set(body: &mut Json) {
    let Some(messages) = body.get_mut("messages").and_then(Json::as_arr_mut) else { return };
    for message in messages {
        let Some(content) = message.get_mut("content").and_then(Json::as_arr_mut) else { continue };
        if !content.iter().all(|block| block.str_at("type") == Some("tool_result")) {
            continue;
        }
        for block in content.iter_mut() {
            if let Some(object) = block.as_obj_mut() {
                object.remove("cache_control");
            }
        }
        content.sort_by_key(|block| block.str_at("tool_use_id").unwrap_or("").to_owned());
    }
}

/// A call as a differential comparison reads it: what the CLI itself randomizes per process is out.
fn comparable_call(call: &Seen, parallel_tools: bool) -> String {
    let mut headers: Vec<String> = call
        .headers
        .iter()
        .filter(|(name, _)| !matches!(name.as_str(), "host" | "connection" | "content-length" | "x-claude-code-session-id" | "x-client-request-id"))
        .map(|(name, value)| format!("{name}: {value}"))
        .collect();
    headers.sort();
    let mut body = call.body.clone();
    if let Some(object) = body.as_obj_mut() {
        object.remove("metadata");
    }
    if parallel_tools {
        results_as_a_set(&mut body);
    }
    format!("{} {}\n{}\n{}", call.method, call.url, headers.join("\n"), body.to_text())
}

/// The steps of one scenario, on a fresh bench for each sidecar.
fn differential_scenario(name: &str, scenario: &dyn Fn(&Bench, &dyn Driver) -> Vec<Outcome>) {
    differential_scenario_with(name, false, scenario);
}

fn differential_scenario_with(name: &str, parallel_tools: bool, scenario: &dyn Fn(&Bench, &dyn Driver) -> Vec<Outcome>) {
    let Some(path) = node_sidecar_path() else {
        eprintln!("{name}: skipped, MEWRK_AISDK_NODE_SIDECAR is not set");
        return;
    };
    let Some(first) = installed() else {
        eprintln!("{name}: skipped, no Claude Code executable");
        return;
    };
    let node_bench = Bench::new(first);
    let node = Node::start(&path);
    let node_outcomes = scenario(&node_bench, &node);
    let rust_bench = Bench::new(installed().expect("still installed"));
    let rust = Rust::start();
    let rust_outcomes = scenario(&rust_bench, &rust);

    let frames = |outcomes: &[Outcome], bench: &Bench| -> Vec<Vec<String>> {
        outcomes.iter().map(|outcome| comparable_frames(outcome).iter().map(|frame| normalize(frame, bench)).collect()).collect()
    };
    let (node_frames, rust_frames) = (frames(&node_outcomes, &node_bench), frames(&rust_outcomes, &rust_bench));
    assert_eq!(node_frames.len(), rust_frames.len(), "{name}: steps");
    for (index, (from_node, from_rust)) in node_frames.iter().zip(&rust_frames).enumerate() {
        assert_eq!(from_node, from_rust, "{name}: frames of step {index}");
    }
    let calls = |bench: &Bench| -> Vec<String> { bench.upstream.calls().iter().map(|call| normalize(&comparable_call(call, parallel_tools), bench)).collect() };
    let (node_calls, rust_calls) = (calls(&node_bench), calls(&rust_bench));
    assert_eq!(node_calls.len(), rust_calls.len(), "{name}: call count");
    for (index, (from_node, from_rust)) in node_calls.iter().zip(&rust_calls).enumerate() {
        assert_eq!(from_node, from_rust, "{name}: call {index}");
    }
    node_bench.deny.assert_untouched();
    rust_bench.deny.assert_untouched();
    let node_requests: Vec<String> = node_bench.upstream.requests().iter().map(|seen| format!("{} {}", seen.method, seen.url)).collect();
    let rust_requests: Vec<String> = rust_bench.upstream.requests().iter().map(|seen| format!("{} {}", seen.method, seen.url)).collect();
    let (mut sorted_node, mut sorted_rust) = (node_requests, rust_requests);
    sorted_node.sort();
    sorted_rust.sort();
    assert_eq!(sorted_node, sorted_rust, "{name}: requests");
    assert!(!node.stderr.lock().unwrap().contains(UPSTREAM_KEY), "{name}: the key reached Node's stderr");
}

#[test]
fn differential_tool_round_and_continuation() {
    differential_scenario("tool round", &|bench, driver| {
        bench.upstream.push(Script::blocks(vec![
            thinking_block("Let me look at the file first.", "sig_A"),
            text_block("I will read the file."),
            tool_use_block("toolu_01", "read_file", jobj! { "path": "a.txt" }),
        ]));
        let first = run(driver, "d1", &bench.payload("d-run", vec![user("Please read a.txt")], |_| {}));
        // Separators a line reader might trip over and characters a chunk might cut, raw from the upstream.
        bench.upstream.push(Script::text("The file says \u{2028}hello\u{2029} 工程 😀😀😀😀😀😀😀 \u{85} done."));
        let messages = vec![
            user("Please read a.txt"),
            assistant_of(&first),
            tool_result("toolu_01", "read_file", text_output("hello from a.txt")),
            jobj! { "role": "user", "content": "<system-reminder>\n<task-notification>done</task-notification>\n</system-reminder>", "providerOptions": jobj! { "mewrk": jobj! { "hostMessage": true } } },
        ];
        let second = run(driver, "d2", &bench.payload("d-run", messages, |_| {}));
        driver.send(&release_frame("d-run"));
        vec![first, second]
    });
}

#[test]
fn differential_resume_rebuild_and_carriers() {
    differential_scenario("resume", &|bench, driver| {
        let mut outcomes = Vec::new();
        bench.upstream.push(Script::text("learned the model"));
        outcomes.push(run(driver, "d0", &bench.payload("d-learn", vec![user("hello")], |_| {})));
        driver.send(&release_frame("d-learn"));
        bench.upstream.push(Script::text("resumed answer"));
        let history = vec![
            user("Please read a.txt"),
            jobj! { "role": "assistant", "content": jarr![
                jobj! { "type": "text", "text": "I will read the file." },
                jobj! { "type": "tool-call", "toolCallId": "toolu_01", "toolName": "read_file", "input": jobj! { "path": "a.txt" } },
            ] },
            tool_result("toolu_01", "read_file", jobj! { "type": "content", "value": jarr![
                jobj! { "type": "text", "text": "1 image" },
                jobj! { "type": "media", "data": PNG_1X1, "mediaType": "image/png" },
            ] }),
            jobj! { "role": "assistant", "content": jarr![jobj! {
                "type": "reasoning",
                "text": "Let me look at the file first.",
                "providerOptions": jobj! { "anthropic": jobj! { "signature": "sig_A" }, "mewrk": jobj! { "model": "sonnet" } },
            }] },
            jobj! { "role": "assistant", "content": jarr![
                jobj! { "type": "reasoning", "text": "", "providerOptions": jobj! { "anthropic": jobj! { "redactedData": "enc" } } },
                jobj! { "type": "reasoning", "text": "foreign", "providerOptions": jobj! { "anthropic": jobj! { "signature": "sig_F" }, "mewrk": jobj! { "model": "other-model" } } },
                text_block("The file says hello."),
            ] },
            jobj! { "role": "user", "content": jarr![
                jobj! { "type": "text", "text": "Thanks. And now?" },
                jobj! { "type": "image", "image": format!("data:image/png;base64,{PNG_1X1}"), "mediaType": "image/png" },
            ] },
        ];
        outcomes.push(run(driver, "d1", &bench.payload("d-resume", history, |_| {})));
        driver.send(&release_frame("d-resume"));
        // A round whose parked session is gone comes back through the transcript and the continue notice.
        bench.upstream.push(Script::text("continued after loss"));
        let lost = vec![
            user("list files"),
            jobj! { "role": "assistant", "content": jarr![jobj! { "type": "tool-call", "toolCallId": "toolu_02", "toolName": "bash", "input": jobj! { "command": "ls" } }] },
            tool_result("toolu_02", "bash", jobj! { "type": "error-json", "value": jobj! { "code": 1u64 } }),
        ];
        outcomes.push(run(driver, "d2", &bench.payload("d-lost", lost, |_| {})));
        driver.send(&release_frame("d-lost"));
        // Deliveries alone: a host message, then the same in `box`.
        bench.upstream.push(Script::text("noted the background result"));
        let notice = vec![
            user("kick off the background job"),
            jobj! { "role": "assistant", "content": jarr![text_block("Started it.")] },
            jobj! { "role": "user", "content": "<task-notification>bg done</task-notification>", "providerOptions": jobj! { "mewrk": jobj! { "hostMessage": true } } },
        ];
        outcomes.push(run(driver, "d3", &bench.payload("d-notice", notice, |_| {})));
        driver.send(&release_frame("d-notice"));
        bench.upstream.push(Script::text("noted the boxed result"));
        let boxed = vec![
            user("kick off the background job"),
            jobj! { "role": "assistant", "content": jarr![text_block("Started it.")] },
            jobj! { "role": "assistant", "content": jarr![jobj! { "type": "tool-call", "toolCallId": "toolu_box", "toolName": "box", "input": jobj! { "none": jarr![] } }] },
            tool_result("toolu_box", "box", text_output("<task-notification>bg boxed</task-notification>")),
        ];
        outcomes.push(run(driver, "d4", &bench.payload("d-box", boxed, |_| {})));
        driver.send(&release_frame("d-box"));
        outcomes
    });
}

#[test]
fn differential_parallel_calls_widening_and_narration() {
    differential_scenario("calls", &|bench, driver| {
        let mut outcomes = Vec::new();
        bench.upstream.push(Script::blocks(vec![
            tool_use_block("toolu_p1", "read_file", jobj! { "path": "one.txt" }),
            tool_use_block("toolu_p2", "read_file", jobj! { "path": "two.txt" }),
        ]));
        let parallel = run(driver, "d1", &bench.payload("d-p", vec![user("read both")], |_| {}));
        bench.upstream.push(Script::text("both read"));
        let results = jobj! { "role": "tool", "content": jarr![
            jobj! { "type": "tool-result", "toolCallId": "toolu_p1", "toolName": "read_file", "output": text_output("one") },
            jobj! { "type": "tool-result", "toolCallId": "toolu_p2", "toolName": "read_file", "output": jobj! { "type": "error-text", "value": "two failed" } },
        ] };
        let answered = run(driver, "d2", &bench.payload("d-p", vec![user("read both"), assistant_of(&parallel), results], |_| {}));
        outcomes.extend([parallel, answered]);
        driver.send(&release_frame("d-p"));

        bench.upstream.push(Script::blocks(vec![tool_use_block("toolu_w1", "read_file", jobj! { "path": "w.txt" })]));
        let widen = run(driver, "d3", &bench.payload("d-w", vec![user("read w")], |_| {}));
        bench.upstream.push(Script::text("listed the new tool"));
        let messages = vec![user("read w"), assistant_of(&widen), tool_result("toolu_w1", "read_file", text_output("w"))];
        let widened = run(driver, "d4", &bench.payload("d-w", messages, |payload| {
            let mut all = tools();
            all.push(late_tool());
            payload.insert("tools", all);
        }));
        outcomes.extend([widen, widened]);
        driver.send(&release_frame("d-w"));

        bench.upstream.push(Script::blocks(vec![
            jobj! { "type": "thinking", "thinking": "", "signature": "sig_omitted", "estimatedTokens": jarr![120u64, 200u64] },
            thinking_block("A narration.\n\n", "sig_narration"),
            tool_use_block("toolu_narr", "read_file", jobj! { "path": "project.godot" }),
        ]));
        outcomes.push(run(driver, "d5", &bench.payload("d-n", vec![user("set up godot")], |_| {})));
        driver.send(&release_frame("d-n"));

        bench.upstream.push(Script { stop_reason: Some("max_tokens"), ..Script::blocks(vec![tool_use_block("toolu_cut", "read_file", jobj! { "path": "abc" })]) });
        outcomes.push(run(driver, "d6", &bench.payload("d-cut", vec![user("read abc")], |payload| {
            payload.insert("reasoning", "xhigh");
            payload.insert("maxOutputTokens", 4096u64);
        })));
        driver.send(&release_frame("d-cut"));
        outcomes
    });
}

#[test]
fn differential_tool_additions() {
    differential_scenario("additions", &|bench, driver| {
        let model = "claude-opus-5-5";
        let with_late = || {
            let mut all = tools();
            all.push(late_tool());
            all
        };
        bench.upstream.push(Script::blocks(vec![tool_use_block("toolu_a1", "read_file", jobj! { "path": "a.txt" })]));
        let one = run(driver, "d1", &bench.payload("d-a", vec![user("read a")], |payload| {
            payload.insert("modelId", model);
            payload.insert("agent", bench.addition_agent("d-a", true));
        }));
        bench.upstream.push(Script::text("took the addition"));
        let exchange = vec![user("read a"), assistant_of(&one), tool_result("toolu_a1", "read_file", text_output("a"))];
        let two = run(driver, "d2", &bench.payload("d-a", exchange.clone(), |payload| {
            payload.insert("modelId", model);
            payload.insert("agent", bench.addition_agent("d-a", true));
            payload.insert("tools", with_late());
        }));
        driver.send(&release_frame("d-a"));
        let mut history = exchange;
        history.push(tool_addition_marker(&["late_tool"]));
        history.push(assistant_of(&two));
        history.push(user("again"));
        history.push(tool_addition_marker(&["late_tool", "not_declared"]));
        bench.upstream.push(Script::text("rebuilt with the addition"));
        let three = run(driver, "d3", &bench.payload("d-r", history.clone(), |payload| {
            payload.insert("modelId", model);
            payload.insert("agent", bench.addition_agent("d-r", true));
            payload.insert("tools", with_late());
        }));
        driver.send(&release_frame("d-r"));
        bench.upstream.push(Script::text("declared plainly"));
        let four = run(driver, "d4", &bench.payload("d-n", history, |payload| {
            payload.insert("modelId", model);
            payload.insert("agent", bench.addition_agent("d-n", false));
            payload.insert("tools", with_late());
        }));
        driver.send(&release_frame("d-n"));
        vec![one, two, three, four]
    });
}

#[test]
fn differential_native_search_and_budgets() {
    differential_scenario_with("native search", true, &|bench, driver| {
        let mut outcomes = Vec::new();
        bench.upstream.push(Script::blocks(vec![
            text_block("Searching."),
            tool_use_block("toolu_ws_a", "WebSearch", jobj! { "query": "mewrk native search" }),
            tool_use_block("toolu_ws_b", "WebSearch", jobj! { "query": "mewrk native search again" }),
        ]));
        bench.upstream.push(Script::blocks(vec![
            jobj! { "type": "server_tool_use", "id": "srvtoolu_01", "name": "web_search", "input": jobj! { "query": "mewrk native search" } },
            jobj! {
                "type": "web_search_tool_result",
                "tool_use_id": "srvtoolu_01",
                "content": jarr![
                    jobj! { "type": "web_search_result", "title": "Result A", "url": "https://a.example/page", "encrypted_content": "enc_a", "page_age": Json::Null },
                    jobj! { "type": "web_search_result", "title": "Result B", "url": "https://b.example/doc", "encrypted_content": "enc_b", "page_age": Json::Null },
                ],
            },
            text_block("Two pages matched."),
        ]));
        bench.upstream.push(Script::text("Findings: see https://a.example/page"));
        outcomes.push(run(driver, "d1", &bench.payload("d-s", vec![user("Search the web for: mewrk native search")], |payload| {
            payload.insert("tools", Vec::<Json>::new());
            payload.insert("nativeSearch", jobj! { "maxUses": 1u64 });
        })));
        driver.send(&release_frame("d-s"));
        for (id, model, window) in [("d2", "claude-sonnet-5", 1_000_000u64), ("d3", "claude-sonnet-5", 200_000), ("d4", "claude-sonnet-5[1m]", 200_000)] {
            bench.upstream.push(Script::text("budget ok"));
            outcomes.push(run(driver, id, &bench.payload(id, vec![user("hi")], |payload| {
                payload.insert("modelId", model);
                payload.insert("contextWindow", window);
                payload.remove("reasoning");
            })));
            driver.send(&release_frame(id));
        }
        outcomes
    });
}

#[test]
fn differential_failures() {
    differential_scenario("failures", &|bench, driver| {
        let mut outcomes = Vec::new();
        // An upstream error that echoes the key: permanent and redacted.
        bench.upstream.push(Script {
            status: Some(400),
            body: Some(format!(r#"{{"type":"error","error":{{"type":"invalid_request_error","message":"Invalid API key: {UPSTREAM_KEY}"}}}}"#)),
            ..Script::default()
        });
        outcomes.push(run(driver, "d1", &bench.payload("d-400", vec![user("hi")], |_| {})));
        // A rate limit the CLI is told not to retry.
        bench.upstream.push(Script { status: Some(404), body: Some(r#"{"type":"error","error":{"type":"not_found_error","message":"no such model"}}"#.to_owned()), ..Script::default() });
        outcomes.push(run(driver, "d2", &bench.payload("d-404", vec![user("hi")], |_| {})));
        // Before the CLI is even started.
        let with_agent = |id: &str, edit: &dyn Fn(&mut Obj)| {
            bench.payload(id, vec![user("hi")], |payload| {
                let mut agent = bench.agent(id);
                edit(&mut agent);
                payload.insert("agent", agent);
            })
        };
        outcomes.push(run(driver, "d3", &with_agent("d3", &|agent| agent.insert("executable", "/no/such/claude"))));
        outcomes.push(run(driver, "d6", &with_agent("d6", &|agent| {
            agent.remove("cwd");
        })));
        outcomes.push(run(driver, "d7", &with_agent("d7", &|agent| {
            agent.insert("env", jobj! { "ANTHROPIC_BASE_URL": "https://relay.example.com", "ANTHROPIC_API_KEY": "k" });
        })));
        outcomes.push(run(driver, "d8", &bench.payload("d8", vec![], |_| {})));
        // Stalled upstream calls, ended from the host's side: a release, then a cancel.
        bench.upstream.push(Script { delay: Some(Duration::from_secs(6)), ..Script::text("never") });
        driver.send(&step_frame("d9", &bench.payload("d9", vec![user("slow")], |_| {})));
        std::thread::sleep(Duration::from_millis(1500));
        driver.send(&release_frame("d9"));
        outcomes.push(driver.wait("d9", Duration::from_secs(20)));
        bench.upstream.push(Script { delay: Some(Duration::from_secs(6)), ..Script::text("never") });
        driver.send(&step_frame("d10", &bench.payload("d10", vec![user("slow")], |_| {})));
        std::thread::sleep(Duration::from_millis(1500));
        driver.send(r#"{"v":18,"type":"cancel","id":"d10"}"#);
        outcomes.push(driver.wait("d10", Duration::from_secs(20)));
        outcomes
    });
}

/// A stand-in for the CLI, under `node`, that writes down exactly what it was started with: the argv, the environment, what
/// arrived on stdin and what lies in its config directory.
const RECORDER: &str = r#"
import { readdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import path from "node:path";
const lines = [];
let buffer = "";
function walk(dir, prefix, out) {
  let names = [];
  try { names = readdirSync(dir); } catch { return out; }
  for (const name of names.sort()) {
    const full = path.join(dir, name);
    if (statSync(full).isDirectory()) walk(full, `${prefix}${name}/`, out);
    else out[`${prefix}${name}`] = [readFileSync(full, "utf8"), (statSync(full).mode & 0o777).toString(8)];
  }
  return out;
}
function finish() {
  const { MEWRK_DUMP, ...env } = process.env;
  writeFileSync(MEWRK_DUMP, JSON.stringify({
    argv: process.argv.slice(2),
    env,
    cwd: process.cwd(),
    stdin: lines,
    config: env.CLAUDE_CONFIG_DIR ? walk(env.CLAUDE_CONFIG_DIR, "", {}) : {},
  }));
  process.stderr.write("recorder line one\n\nrecorder line two\r\n");
  process.exit(7);
}
process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  for (let at = buffer.indexOf("\n"); at >= 0; at = buffer.indexOf("\n")) {
    lines.push(buffer.slice(0, at));
    buffer = buffer.slice(at + 1);
    if (lines.length === 2) finish();
  }
});
"#;

/// A dump with what is random per process replaced.
fn comparable_dump(dump: &Json, marks: &[(&str, &str)]) -> String {
    let mut text = dump.to_text();
    for (from, to) in marks {
        text = text.replace(from, to);
    }
    let uuid = regex::Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").unwrap();
    text = uuid.replace_all(&text, "<uuid>").into_owned();
    let stamp = regex::Regex::new(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z").unwrap();
    text = stamp.replace_all(&text, "<time>").into_owned();
    let request_id = regex::Regex::new(r#"\\"request_id\\":\\"[0-9a-z]+\\""#).unwrap();
    request_id.replace_all(&text, r#"\"request_id\":\"<id>\""#).into_owned()
}

/// Node's SDK and this crate start the CLI identically: byte for byte on stdin, name for name in the environment, and the
/// transcript and configuration files the same. The one deliberate difference is the credential copy, which is out of
/// this scenario because the configuration directory holds no login, and has a test of its own above.
#[test]
fn differential_what_the_cli_is_started_with() {
    let Some(path) = node_sidecar_path() else {
        eprintln!("skipped, MEWRK_AISDK_NODE_SIDECAR is not set");
        return;
    };
    let Some(sdk) = node_sdk() else {
        eprintln!("skipped, MEWRK_CLAUDE_SDK is not set");
        return;
    };
    let scratch = Scratch::new("recorder");
    fs::create_dir_all(scratch.path("cwd")).unwrap();
    fs::create_dir_all(scratch.path("config")).unwrap();
    fs::write(scratch.path("config/.claude.json"), r#"{"hasCompletedOnboarding":true}"#).unwrap();
    fs::write(scratch.path("config/.config.json"), r#"{"a":1}"#).unwrap();
    fs::write(scratch.path("config/.claude-staging-oauth.json"), r#"{"s":1}"#).unwrap();
    fs::write(scratch.path("config/settings.json"), "\u{FEFF}{\"enabledPlugins\":{\"x\":true},\"extraKnownMarketplaces\":{},\"keep\":1,\"env\":{\"CLAUDE_CONFIG_DIR\":\"/x\",\"K\":\"v\"}}").unwrap();
    fs::write(scratch.path("config/cowork_settings.json"), "{ \"untouched\": true }").unwrap();
    let recorder = scratch.path("recorder.mjs");
    fs::write(&recorder, RECORDER).unwrap();
    let dump = scratch.path("dump.json");
    let agent = |session: &str, extra: &[(&str, &str)]| {
        let mut env = Obj::new();
        env.insert("HOME", scratch.text("cwd"));
        env.insert("USER", "someone");
        env.insert("CLAUDE_CONFIG_DIR", scratch.text("config"));
        env.insert("MEWRK_DUMP", dump.to_string_lossy().into_owned());
        env.insert("ANTHROPIC_BASE_URL", "http://127.0.0.1:9");
        env.insert("ANTHROPIC_API_KEY", "stub-key");
        env.insert("NODE_OPTIONS", "--inspect");
        env.insert("DEBUG", "*");
        for (name, value) in extra {
            env.insert(*name, *value);
        }
        jobj! {
            "session": session,
            "executable": recorder.to_string_lossy().into_owned(),
            "sdk": sdk.as_str(),
            "cwd": scratch.text("cwd"),
            "env": env,
            "toolChanges": true,
        }
    };
    let late = late_tool();
    let cases: Vec<(&str, Json)> = vec![
        ("fresh", jobj! {
            "family": "claude-agent",
            "apiKey": REQUEST_ONLY_KEY,
            "modelId": "claude-opus-5-5",
            "contextWindow": 1_000_000u64,
            "maxOutputTokens": 2048.5,
            "system": HOST_PROMPT,
            "systemDynamic": "the dynamic tail",
            "messages": jarr![user("héllo 😀 工程 \u{2028} \u{7f} \t \" \\ </script>")],
            "tools": tools(),
            "reasoning": "xhigh",
            "agent": agent("fresh", &[]),
        }),
        // Nothing in `agent.env` but where to write: the defaults come from the sidecar itself.
        ("profile-defaults", jobj! {
            "family": "claude-agent",
            "modelId": "sonnet",
            "messages": jarr![user("hi")],
            "agent": jobj! {
                "session": "profile-defaults",
                "executable": recorder.to_string_lossy().into_owned(),
                "sdk": sdk.as_str(),
                "cwd": scratch.text("cwd"),
                "env": jobj! { "CLAUDE_CONFIG_DIR": scratch.text("config"), "MEWRK_DUMP": dump.to_string_lossy().into_owned() },
            },
        }),
        ("no-system", jobj! {
            "family": "claude-agent",
            "modelId": "sonnet",
            "messages": jarr![jobj! { "role": "user", "content": jarr![
                jobj! { "type": "text", "text": "with a pdf" },
                jobj! { "type": "file", "data": "data:application/pdf;base64,QUJD", "mediaType": "application/pdf", "filename": "r.pdf" },
                jobj! { "type": "image", "image": "https://example.com/x.png" },
            ] }],
            "agent": agent("no-system", &[("CLAUDE_CODE_ENTRYPOINT", "custom"), ("CLAUDE_AGENT_SDK_VERSION", "1.2.3"), ("claude_code_sdk_reads_session_state", "0")]),
        }),
        ("search", jobj! {
            "family": "claude-agent",
            "modelId": "sonnet",
            "system": HOST_PROMPT,
            "messages": jarr![user("search")],
            "tools": jarr![],
            "nativeSearch": jobj! { "maxUses": 1u64 },
            "agent": agent("search", &[]),
        }),
        ("fetch", jobj! {
            "family": "claude-agent",
            "modelId": "sonnet",
            "messages": jarr![user("fetch")],
            "nativeFetch": jobj! { "maxUses": 1u64, "urls": jarr!["https://example.com/a"] },
            "agent": agent("fetch", &[]),
        }),
        ("resume", jobj! {
            "family": "claude-agent",
            "modelId": "claude-opus-5-5",
            "system": HOST_PROMPT,
            "messages": jarr![
                jobj! { "role": "user", "content": "first" },
                tool_addition_marker(&["late_tool"]),
                jobj! { "role": "assistant", "content": jarr![
                    jobj! { "type": "text", "text": "calling" },
                    jobj! { "type": "reasoning", "text": "t", "providerOptions": jobj! { "anthropic": jobj! { "signature": "s" }, "mewrk": jobj! { "model": "claude-opus-5-5" } } },
                    jobj! { "type": "reasoning", "text": "dropped" },
                    jobj! { "type": "tool-call", "toolCallId": "c1", "toolName": "mcp__mewrk__read_file", "input": jobj! { "path": "x" } },
                ] },
                tool_result("c1", "read_file", jobj! { "type": "content", "value": jarr![
                    jobj! { "type": "text", "text": "t" },
                    jobj! { "type": "media", "data": "AA", "mediaType": "image/png" },
                ] }),
                jobj! { "role": "user", "content": "<note/>", "providerOptions": jobj! { "mewrk": jobj! { "hostMessage": true } } },
                tool_addition_marker(&["late_tool", "bash"]),
                jobj! { "role": "assistant", "content": "as a string" },
                tool_addition_marker(&["dropped_after_assistant"]),
                jobj! { "role": "assistant", "content": jarr![] },
                jobj! { "role": "user", "content": "the prompt" },
                tool_addition_marker(&["late_tool", "box"]),
            ],
            "tools": { let mut all = tools(); all.push(late); all },
            "agent": agent("resume", &[]),
        }),
        ("empty-history", jobj! {
            "family": "claude-agent",
            "modelId": "sonnet",
            "messages": jarr![tool_addition_marker(&["x"]), jobj! { "role": "user", "content": "alone" }],
            "agent": agent("empty-history", &[]),
        }),
    ];
    let node = Node::start(&path);
    let rust = Rust::start();
    for (name, payload) in cases {
        let id = format!("rec-{name}");
        let _ = fs::remove_file(&dump);
        let from_node = run(&node, &id, &payload);
        let node_dump = j(&fs::read_to_string(&dump).unwrap_or_else(|_| panic!("{name}: no dump from Node: {}", from_node.terminal())));
        let _ = fs::remove_file(&dump);
        let from_rust = run(&rust, &id, &payload);
        let rust_dump = j(&fs::read_to_string(&dump).unwrap_or_else(|_| panic!("{name}: no dump from Rust: {}", from_rust.terminal())));
        let config_of = |dump: &Json| dump.path(&["env", "CLAUDE_CONFIG_DIR"]).and_then(Json::as_str).unwrap_or("").to_owned();
        let (node_config, rust_config) = (config_of(&node_dump), config_of(&rust_dump));
        let tmp = std::env::temp_dir().to_string_lossy().trim_end_matches('/').to_owned();
        let marks = |config: String| vec![(config, "<config>".to_owned()), (tmp.clone(), "<tmp>".to_owned())];
        let (node_marks, rust_marks) = (marks(node_config), marks(rust_config));
        let as_refs = |marks: &'_ [(String, String)]| marks.iter().map(|(a, b)| (a.clone(), b.clone())).collect::<Vec<_>>();
        let render = |dump: &Json, marks: Vec<(String, String)>| {
            let refs: Vec<(&str, &str)> = marks.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
            comparable_dump(dump, &refs)
        };
        let (node_text, rust_text) = (j(&render(&node_dump, as_refs(&node_marks))), j(&render(&rust_dump, as_refs(&rust_marks))));
        for field in ["argv", "cwd", "stdin", "config"] {
            assert_eq!(node_text.get(field), rust_text.get(field), "{name}: {field}");
        }
        // Not vacuous: the frames arrived, and a session with a past has its transcript on disk.
        assert_eq!(rust_text.get("stdin").and_then(Json::as_arr).map(Vec::len), Some(2), "{name}");
        let transcripts = rust_text.get("config").and_then(Json::as_obj).map_or(0, |config| config.keys().filter(|key| key.ends_with(".jsonl")).count());
        assert_eq!(transcripts, usize::from(name == "resume"), "{name}");
        // `env` is a set of names: `process.env` of the stand-in, whatever order the two spawns gave it.
        let sorted = |dump: &Json| -> Vec<(String, String)> {
            let mut pairs: Vec<(String, String)> = dump.get("env").unwrap().as_obj().unwrap().iter().map(|(k, v)| (k.to_owned(), v.as_str().unwrap_or("").to_owned())).collect();
            pairs.sort();
            pairs
        };
        assert_eq!(sorted(&node_text), sorted(&rust_text), "{name}: env");
        // The exit, and stderr's tail, are reported the same.
        let frames = |outcome: &Outcome| comparable_frames(outcome);
        assert_eq!(frames(&from_node), frames(&from_rust), "{name}: frames");
        driver_release(&node, &rust, &id);
    }
}

fn driver_release(node: &Node, rust: &Rust, session: &str) {
    Driver::send(node, &release_frame(session));
    Driver::send(rust, &release_frame(session));
}

// ---------------------------------------------------------------- differential: stand-ins under both sidecars

/// One scenario on a fresh stand-in per sidecar; what it returns must match, frames and files alike.
fn differential_stand_in(name: &str, scenario: &dyn Fn(&StandIn, &dyn Driver) -> Vec<String>) {
    let Some(path) = node_sidecar_path() else {
        eprintln!("{name}: skipped, MEWRK_AISDK_NODE_SIDECAR is not set");
        return;
    };
    let Some(sdk) = node_sdk() else {
        eprintln!("{name}: skipped, MEWRK_CLAUDE_SDK is not set");
        return;
    };
    let render = |stand_in: &StandIn, lines: Vec<String>| -> Vec<String> {
        lines.iter().map(|line| line.replace(&stand_in.scratch.0.to_string_lossy().into_owned(), "<scratch>")).collect()
    };
    let (node, rust) = (Node::start(&path), Rust::start());
    let (from_node_side, from_rust_side) = (StandIn::with_sdk(&sdk), StandIn::with_sdk(&sdk));
    let from_node = render(&from_node_side, scenario(&from_node_side, &node));
    let from_rust = render(&from_rust_side, scenario(&from_rust_side, &rust));
    assert_eq!(from_node.len(), from_rust.len(), "{name}: {from_node:#?}\n{from_rust:#?}");
    for (index, (left, right)) in from_node.iter().zip(&from_rust).enumerate() {
        assert_eq!(left, right, "{name}: line {index}");
    }
}

fn frames_of(outcome: &Outcome) -> Vec<String> {
    comparable_frames(outcome)
}

const READ_START: &str = "IFS= read -r one; IFS= read -r two\n";

#[test]
fn differential_control_replies() {
    differential_stand_in("control", &|stand_in, driver| {
        let replies = stand_in.scratch.path("replies");
        let mut body = READ_START.to_owned();
        let requests = [
            ("r-allow", r#"{"subtype":"can_use_tool","tool_name":"read_file","input":{"path":"x"},"tool_use_id":"t1"}"#),
            ("r-no-id", r#"{"subtype":"can_use_tool","tool_name":"read_file","input":{"path":"x"}}"#),
            ("r-hook", r#"{"subtype":"hook_callback","callback_id":"hook_0","input":{}}"#),
            ("r-elicit", r#"{"subtype":"elicitation","mcp_server_name":"mewrk","message":"m"}"#),
            ("r-oauth", r#"{"subtype":"oauth_token_refresh"}"#),
            ("r-host", r#"{"subtype":"host_auth_token_refresh"}"#),
            ("r-secret", r#"{"subtype":"remote_control_work_secret","session_id":"s"}"#),
            ("r-silent-1", r#"{"subtype":"remote_tool_call"}"#),
            ("r-silent-2", r#"{"subtype":"remote_plumbing_call"}"#),
            ("r-silent-3", r#"{"subtype":"remote_tools_probe"}"#),
            ("r-silent-4", r#"{"subtype":"remote_tools_reannounce"}"#),
            ("r-silent-5", r#"{"subtype":"request_user_dialog","dialog_kind":"k"}"#),
            ("r-odd", r#"{"subtype":"something_new"}"#),
            ("r-server", r#"{"subtype":"mcp_message","server_name":"other","message":{"jsonrpc":"2.0","id":1,"method":"ping"}}"#),
            ("r-ping", r#"{"subtype":"mcp_message","server_name":"mewrk","message":{"jsonrpc":"2.0","id":2,"method":"ping"}}"#),
            ("r-note", r#"{"subtype":"mcp_message","server_name":"mewrk","message":{"jsonrpc":"2.0","method":"notifications/initialized"}}"#),
            ("r-init", r#"{"subtype":"mcp_message","server_name":"mewrk","message":{"jsonrpc":"2.0","id":3,"method":"initialize","params":{"protocolVersion":"2030-01-01"}}}"#),
            ("r-init2", r#"{"subtype":"mcp_message","server_name":"mewrk","message":{"jsonrpc":"2.0","id":"s","method":"initialize","params":{"protocolVersion":7}}}"#),
            ("r-list", r#"{"subtype":"mcp_message","server_name":"mewrk","message":{"jsonrpc":"2.0","id":5,"method":"tools/list"}}"#),
            ("r-unknown", r#"{"subtype":"mcp_message","server_name":"mewrk","message":{"jsonrpc":"2.0","id":4,"method":"prompts/list"}}"#),
            ("r-unknown-note", r#"{"subtype":"mcp_message","server_name":"mewrk","message":{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}}}"#),
            ("r-reply", r#"{"subtype":"mcp_message","server_name":"mewrk","message":{"jsonrpc":"2.0","id":9,"result":{}}}"#),
        ];
        for (id, request) in requests {
            body.push_str(&say(&format!(r#"{{"type":"control_request","request_id":"{id}","request":{request}}}"#)));
        }
        body.push_str(&say(r#"{"type":"keep_alive"}"#));
        body.push_str(&say(r#"{"type":"transcript_mirror","filePath":"/x","entries":[]}"#));
        body.push_str(&say(r#"{"type":"control_cancel_request","request_id":"r-silent-1"}"#));
        body.push_str("printf 'not json at all\\n\\n'\n");
        // The first answers arrive in a fixed order and everything silent is silent: the count is every answered request.
        body.push_str(&format!("i=0; while [ $i -lt 16 ] && IFS= read -r line; do printf '%s\\n' \"$line\" >> '{}'; i=$((i+1)); done\n", replies.display()));
        body.push_str(&say(RESULT_OK));
        body.push_str("cat > /dev/null\n");
        let script = stand_in.script("claude", &body);
        let outcome = run(driver, "c1", &stand_in.payload("c1", &script, vec![user("hi")], |_| {}));
        driver.send(&release_frame("c1"));
        let mut lines = frames_of(&outcome);
        lines.extend(stand_in.lines("replies"));
        lines
    });
}

#[test]
fn differential_parked_handlers() {
    differential_stand_in("parked", &|stand_in, driver| {
        let (replies, marker) = (stand_in.scratch.path("replies"), stand_in.scratch.path("marker"));
        let mut body = READ_START.to_owned();
        body.push_str(&say_event(MESSAGE_START));
        body.push_str(&say_event(r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"early","name":"mcp__mewrk__read_file","input":{}}}"#));
        body.push_str(&say_event(r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"e\"}"}}"#));
        body.push_str(&say_event(r#"{"type":"content_block_stop","index":0}"#));
        body.push_str(&say_event(r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"late","name":"read_file","input":{}}}"#));
        body.push_str(&say_event(r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#));
        body.push_str(&say_event(r#"{"type":"content_block_stop","index":1}"#));
        body.push_str(&say_event(MESSAGE_DELTA_TOOL));
        body.push_str(&say_event(MESSAGE_STOP));
        let late_call = r#"{"type":"control_request","request_id":"call-late","request":{"subtype":"mcp_message","server_name":"mewrk","message":{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"read_file","arguments":{"path":"l"},"_meta":{"claudecode/toolUseId":"late"}}}}}"#;
        body.push_str(&say(late_call));
        body.push_str(&say(late_call));
        body.push_str(&format!("while [ ! -f '{}' ]; do sleep 0.02; done\n", marker.display()));
        body.push_str(&say(r#"{"type":"control_request","request_id":"call-early","request":{"subtype":"mcp_message","server_name":"mewrk","message":{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"read_file","arguments":{"path":"e"},"_meta":{"claudecode/toolUseId":"early"}}}}}"#));
        body.push_str(&format!("i=0; while [ $i -lt 2 ] && IFS= read -r line; do printf '%s\\n' \"$line\" >> '{}'; i=$((i+1)); done\n", replies.display()));
        body.push_str(&say_event(MESSAGE_START));
        body.push_str(&say_event(r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#));
        body.push_str(&say_event(r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"thought"}}"#));
        body.push_str(&say_event(r#"{"type":"content_block_stop","index":0}"#));
        body.push_str(&say_event(r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#));
        body.push_str(&say_event(r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"finished 🎉"}}"#));
        body.push_str(&say_event(r#"{"type":"content_block_stop","index":1}"#));
        body.push_str(&say_event(r#"{"type":"message_delta","delta":{"stop_reason":"refusal"},"usage":{"output_tokens":7,"output_tokens_details":{"thinking_tokens":3},"cache_read_input_tokens":2}}"#));
        body.push_str(&say_event(MESSAGE_STOP));
        body.push_str(&say(RESULT_OK));
        body.push_str("cat > /dev/null\n");
        let script = stand_in.script("claude", &body);
        let round = run(driver, "p1", &stand_in.payload("p", &script, vec![user("go")], |_| {}));
        let messages = vec![
            user("go"),
            assistant_of(&round),
            jobj! { "role": "tool", "content": jarr![
                jobj! { "type": "tool-result", "toolCallId": "late", "toolName": "read_file", "output": text_output("late result") },
                jobj! { "type": "tool-result", "toolCallId": "early", "toolName": "read_file", "output": jobj! { "type": "error-text", "value": "early failed" } },
            ] },
            jobj! { "role": "user", "content": "<note/>", "providerOptions": jobj! { "mewrk": jobj! { "hostMessage": true } } },
            jobj! { "role": "user", "content": jarr![
                jobj! { "type": "text", "text": "[Mewrk tool image] bridge" },
                jobj! { "type": "image", "image": format!("data:image/png;base64,{PNG_1X1}") },
            ] },
        ];
        driver.send(&step_frame("p2", &stand_in.payload("p", &script, messages, |_| {})));
        std::thread::sleep(Duration::from_millis(300));
        fs::write(&marker, "").unwrap();
        let second = driver.wait("p2", Duration::from_secs(20));
        driver.send(&release_frame("p"));
        let mut lines = frames_of(&round);
        lines.extend(frames_of(&second));
        lines.extend(stand_in.lines("replies"));
        lines
    });
}

#[test]
fn differential_failed_turns_and_exits() {
    differential_stand_in("failed turns", &|stand_in, driver| {
        let cases: [(&str, String); 12] = [
            ("auth", say(r#"{"type":"assistant","parent_tool_use_id":null,"error":"authentication_failed","message":{"content":[{"type":"text","text":"Invalid API key · Please run /login"},{"type":"text","text":" twice"},{"type":"text"}]}}"#) + "cat > /dev/null\n"),
            ("rate", say(r#"{"type":"assistant","parent_tool_use_id":null,"error":"rate_limit","message":{"content":[]}}"#) + "cat > /dev/null\n"),
            ("turns", say(r#"{"type":"result","subtype":"error_max_turns","is_error":false,"errors":["too many","turns"]}"#) + "cat > /dev/null\n"),
            ("none", say(r#"{"type":"result","subtype":"error_during_execution","is_error":true,"errors":[]}"#) + "cat > /dev/null\n"),
            ("text", say(r#"{"type":"result","subtype":"success","is_error":true,"result":"API Error: 529 overloaded"}"#) + "exit 1\n"),
            ("empty", say(r#"{"type":"result","subtype":"success","is_error":true,"result":""}"#) + "cat > /dev/null\n"),
            ("nested", say(r#"{"type":"assistant","parent_tool_use_id":"t9","error":"rate_limit","message":{"content":[]}}"#) + "exit 0\n"),
            ("exit0", "exit 0\n".to_owned()),
            ("exit5", "echo 'one' >&2\necho 'two' >&2\nexit 5\n".to_owned()),
            ("signal", "kill -9 $$\n".to_owned()),
            ("ok-then-exit", say(RESULT_OK) + "exit 0\n"),
            ("ok-then-fail", say(RESULT_OK) + "exit 2\n"),
        ];
        let mut lines = Vec::new();
        for (name, tail) in cases {
            let script = stand_in.script(&format!("claude-{name}"), &format!("{READ_START}{tail}"));
            let outcome = run(driver, name, &stand_in.payload(name, &script, vec![user("hi")], |_| {}));
            lines.push(format!("--- {name}"));
            lines.extend(frames_of(&outcome));
        }
        lines
    });
}

#[test]
fn differential_stream_edges() {
    differential_stand_in("stream edges", &|stand_in, driver| {
        let mut body = READ_START.to_owned();
        body.push_str(&say(r#"{"type":"system","subtype":"init","claude_code_version":"7.7.7","model":"m","apiKeySource":"none","tools":[]}"#));
        body.push_str(&say(r#"{"type":"system","subtype":"api_retry","attempt":1}"#));
        body.push_str(&say(&format!(r#"{{"type":"stream_event","parent_tool_use_id":"nested","event":{MESSAGE_START}}}"#)));
        body.push_str(&say(&format!(r#"{{"type":"stream_event","event":{MESSAGE_START}}}"#)));
        body.push_str(&say_event(MESSAGE_START));
        body.push_str(&say_event(r#"{"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"enc"}}"#));
        body.push_str(&say_event(r#"{"type":"content_block_stop","index":0}"#));
        body.push_str(&say_event(r#"{"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":""}}"#));
        body.push_str(&say_event(r#"{"type":"content_block_stop","index":1}"#));
        body.push_str(&say_event(r#"{"type":"content_block_start","content_block":{"type":"text","text":""}}"#));
        body.push_str(&say_event(r#"{"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"indexless é 😀 \u2028 escaped"}}"#));
        body.push_str(&say_event(r#"{"type":"content_block_stop","index":2}"#));
        body.push_str(&say_event(r#"{"type":"content_block_delta","index":99,"delta":{"type":"text_delta","text":"nowhere"}}"#));
        body.push_str(&say_event(r#"{"type":"content_block_start","index":3,"content_block":{"type":"thinking","thinking":"","signature":"presigned"}}"#));
        body.push_str(&say_event(r#"{"type":"content_block_delta","index":3,"delta":{"type":"signature_delta","signature":"-more"}}"#));
        body.push_str(&say_event(r#"{"type":"content_block_delta","index":3,"delta":{"type":"text_delta","text":"wrong kind"}}"#));
        body.push_str(&say_event(r#"{"type":"content_block_stop","index":3}"#));
        body.push_str(&say_event(r#"{"type":"content_block_start","index":4,"content_block":{"type":"thinking","thinking":""}}"#));
        body.push_str(&say_event(r#"{"type":"content_block_delta","index":4,"delta":{"type":"thinking_delta","thinking":"unsigned"}}"#));
        body.push_str(&say_event(r#"{"type":"content_block_stop","index":4}"#));
        body.push_str(&say_event(r#"{"type":"content_block_start","index":5,"content_block":{"type":"thinking","thinking":""}}"#));
        body.push_str(&say_event(r#"{"type":"content_block_stop","index":5}"#));
        body.push_str(&say_event(r#"{"type":"content_block_start","index":6,"content_block":{"type":"tool_use","id":"own","name":"WebSearch","input":{}}}"#));
        body.push_str(&say_event(r#"{"type":"content_block_stop","index":6}"#));
        body.push_str(&say_event(r#"{"type":"content_block_start","index":7,"content_block":{"type":"tool_use","id":"list","name":"bash","input":{}}}"#));
        body.push_str(&say_event(r#"{"type":"content_block_delta","index":7,"delta":{"type":"input_json_delta","partial_json":"[1,2]"}}"#));
        body.push_str(&say_event(r#"{"type":"content_block_stop","index":7}"#));
        body.push_str(&say_event(r#"{"type":"content_block_start","index":8,"content_block":{"type":"tool_use","id":"cut","name":"bash","input":{}}}"#));
        body.push_str(&say_event(r#"{"type":"content_block_delta","index":8,"delta":{"type":"input_json_delta","partial_json":"{oops"}}"#));
        body.push_str(&say_event(r#"{"type":"content_block_stop","index":8}"#));
        body.push_str(&say_event(r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"input_tokens":1,"output_tokens":5,"output_tokens_details":{"thinking_tokens":2}}}"#));
        body.push_str(&say_event(MESSAGE_STOP));
        body.push_str(&say(r#"{"type":"assistant","parent_tool_use_id":null,"message":{"content":[{"type":"tool_use","id":"cut","name":"bash","input":"not an object"}]}}"#));
        body.push_str("cat > /dev/null\n");
        let script = stand_in.script("claude", &body);
        let outcome = run(driver, "s", &stand_in.payload("s", &script, vec![user("hi")], |_| {}));
        driver.send(&release_frame("s"));
        // A message whose only calls are the CLI's own keeps the step going until its `result`.
        let mut body = READ_START.to_owned();
        body.push_str(&say_event(MESSAGE_START));
        body.push_str(&say_event(r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"own","name":"Bash","input":{}}}"#));
        body.push_str(&say_event(r#"{"type":"content_block_stop","index":0}"#));
        body.push_str(&say_event(MESSAGE_STOP));
        body.push_str(&say_event(MESSAGE_START));
        body.push_str(&say_event(r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#));
        body.push_str(&say_event(r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"second message"}}"#));
        body.push_str(&say_event(r#"{"type":"content_block_stop","index":0}"#));
        body.push_str(&say_event(r#"{"type":"message_delta","delta":{"stop_reason":"stop_sequence"}}"#));
        body.push_str(&say_event(MESSAGE_STOP));
        body.push_str(&say(RESULT_OK));
        body.push_str("cat > /dev/null\n");
        let script = stand_in.script("claude2", &body);
        let own = run(driver, "o", &stand_in.payload("o", &script, vec![user("hi")], |_| {}));
        driver.send(&release_frame("o"));
        let mut lines = frames_of(&outcome);
        lines.extend(frames_of(&own));
        lines
    });
}

#[test]
fn differential_release_cancel_and_rebuild() {
    differential_stand_in("release", &|stand_in, driver| {
        let mut lines = Vec::new();
        let script = stand_in.script("sleeper", "exec sleep 60\n");
        driver.send(&step_frame("h1", &stand_in.payload("h1", &script, vec![user("hi")], |_| {})));
        std::thread::sleep(Duration::from_millis(400));
        driver.send(&release_frame("h1"));
        lines.extend(frames_of(&driver.wait("h1", Duration::from_secs(10))));
        driver.send(&step_frame("h2", &stand_in.payload("h2", &script, vec![user("hi")], |_| {})));
        std::thread::sleep(Duration::from_millis(400));
        driver.send(r#"{"v":18,"type":"cancel","id":"h2"}"#);
        lines.extend(frames_of(&driver.wait("h2", Duration::from_secs(10))));

        let starts = stand_in.scratch.path("starts");
        let mut body = format!("printf '%s\\n' \"$$\" >> '{}'\n{READ_START}", starts.display());
        body.push_str(&say_event(MESSAGE_START));
        body.push_str(&say_event(r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"bash","input":{}}}"#));
        body.push_str(&say_event(r#"{"type":"content_block_stop","index":0}"#));
        body.push_str(&say_event(MESSAGE_STOP));
        body.push_str("cat > /dev/null\n");
        let tool_round = stand_in.script("claude", &body);
        let round = run(driver, "r1", &stand_in.payload("r", &tool_round, vec![user("one")], |_| {}));
        lines.extend(frames_of(&round));
        stand_in.wait_lines("starts", 1);
        let wrong = vec![user("one"), assistant_of(&round), tool_result("other", "bash", text_output("x"))];
        let round = run(driver, "r2", &stand_in.payload("r", &tool_round, wrong, |_| {}));
        lines.extend(frames_of(&round));
        stand_in.wait_lines("starts", 2);
        let next = vec![user("one"), assistant_of(&round), user("two")];
        let round = run(driver, "r3", &stand_in.payload("r", &tool_round, next, |_| {}));
        lines.extend(frames_of(&round));
        stand_in.wait_lines("starts", 3);
        driver.send(&release_frame("r"));
        driver.send(&release_frame("never-existed"));
        lines
    });
}

#[test]
fn differential_oversized_streams() {
    differential_stand_in("oversized", &|stand_in, driver| {
        let big = |bytes: usize| format!("head -c {bytes} /dev/zero | tr '\\0' x");
        let mut lines = Vec::new();
        for (id, start, kind, field, bytes) in [
            ("big-args", r#"{"type":"tool_use","id":"t","name":"bash","input":{}}"#, "input_json_delta", "partial_json", 4 * 1024 * 1024 + 1),
            ("big-text", r#"{"type":"text","text":""}"#, "text_delta", "text", 16 * 1024 * 1024 + 1),
        ] {
            let body = format!(
                "{READ_START}{}{}printf '%s' '{{\"type\":\"stream_event\",\"parent_tool_use_id\":null,\"event\":{{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"{kind}\",\"{field}\":\"'; {}; printf '\"}}}}}}\\n'\nsleep 30\n",
                say_event(MESSAGE_START),
                say_event(&format!(r#"{{"type":"content_block_start","index":0,"content_block":{start}}}"#)),
                big(bytes),
            );
            let script = stand_in.script(&format!("claude-{id}"), &body);
            let outcome = run(driver, id, &stand_in.payload(id, &script, vec![user("hi")], |_| {}));
            lines.extend(frames_of(&outcome));
        }
        lines
    });
}

/// The loopback rule against the real `new URL`: every spelling is either refused the same way or lets the same CLI start.
#[test]
fn differential_what_counts_as_loopback() {
    differential_stand_in("loopback", &|stand_in, driver| {
        let script = stand_in.script("claude", "exit 0\n");
        let mut lines = Vec::new();
        let urls = [
            "http://127.0.0.1:9",
            "http://127.0.0.1:9/v1/",
            "http://localhost",
            "http://LOCALHOST:1/x",
            "HTTP://LOCALHOST",
            "http://[::1]:1",
            "http://[0:0:0:0:0:0:0:1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://[::2]/",
            "http://127.1",
            "http://0x7f.1/",
            "http://0177.0.0.1/",
            "http://2130706433",
            "http://127.0.0.1.",
            "http://127.0.0.256/",
            "http://128.0.0.1/",
            "http://0.0.0.0",
            "http://127.0.0.1@evil.example",
            "http://user:pw@127.0.0.1/",
            "http://evil.example\\@127.0.0.1",
            "http://evil.example#@127.0.0.1",
            "http://evil.example?@127.0.0.1",
            "http://127.0.0.1.evil.example",
            "http://localhost.evil.example",
            "http://%6c%6f%63%61%6c%68%6f%73%74/",
            "http://local%20host/",
            "http://local host/",
            "localhost:9",
            "127.0.0.1:9",
            "foo://localhost/",
            "foo://LOCALHOST/",
            "foo://127.0.0.1/",
            "foo:localhost",
            "http:localhost",
            "http:/127.0.0.1/",
            "http:///127.0.0.1/",
            "https://relay.example.com",
            "  http://127.0.0.1:9  ",
            "http://127.0.0.1:99999",
            "http://127.0.0.1:x",
            "http://[::1",
            "not a url",
            "http://",
            "//localhost/",
        ];
        for (index, url) in urls.iter().enumerate() {
            let id = format!("l{index}");
            let payload = stand_in.payload(&id, &script, vec![user("hi")], |payload| {
                let mut agent = payload.get("agent").unwrap().as_obj().unwrap().clone();
                agent.insert("env", jobj! { "ANTHROPIC_BASE_URL": *url, "ANTHROPIC_API_KEY": "k", "HOME": stand_in.scratch.text("cwd") });
                payload.insert("agent", agent);
            });
            let outcome = run(driver, &id, &payload);
            lines.push(format!("--- {url}"));
            lines.extend(frames_of(&outcome));
        }
        lines
    });
}

/// The native ledgers against the real `new URL` and the real upstream code: what each call is told, and what the step
/// reports from what the CLI said it found.
#[test]
fn differential_native_ledgers() {
    differential_stand_in("ledgers", &|stand_in, driver| {
        let mut lines = Vec::new();
        // `WebFetch`: spellings of pages the host named and of pages it did not.
        let asked = [
            "https://example.com/a",
            "https://example.com/a?q=1",
            "http://plain.example/p",
            "https://127.0.0.1/x",
            "https://[::1]:8443/y",
            "https://example.com/路径",
            "https://user:pw@example.com/auth",
            "https://example.com/sp ace",
            "https://example.com:8443/port",
            "https://example.com/quote'\"<>",
            "https://example.com/dup",
            "https://EXAMPLE.com/dup#other",
            "not a url",
            "ftp://example.com/f",
        ];
        let spellings: Vec<Json> = [
            "http://EXAMPLE.com/a#x",
            "https://example.com:443/a",
            "https://example.com/b/../a",
            "https://example.com/./a",
            "https://example.com/%2e/a",
            "https://example.com/%61",
            "https://example.com/a?",
            "https://example.com/a?q=1",
            "https://example.com/a?q=2",
            "https://plain.example/p",
            "http://plain.example:443/p",
            "http://plain.example:80/p",
            "https://0x7f.0.0.1/x",
            "https://127.1/x",
            "https://[0:0:0:0:0:0:0:1]:8443/y",
            "https://example.com/%E8%B7%AF%E5%BE%84",
            "https://user:pw@example.com/auth",
            "https://example.com/auth",
            "https://example.com/sp%20ace",
            "https://example.com:8443/port",
            "https://example.com/quote'%22%3C%3E",
            "https:\\\\example.com\\a",
            "https:example.com/a",
            "https:///example.com/a",
            "  https://example.com/a  ",
            "https://exa\tmple.com/a",
            "ftp://example.com/f",
            "mailto:someone@example.com",
            "",
            "not a url",
            "https://example.com./a",
            "https://example.com/dup",
        ]
        .iter()
        .map(|url| Json::from(*url))
        .collect();
        let mut inputs: Vec<(String, Json)> = spellings.iter().enumerate().map(|(index, url)| (format!("f{index}"), jobj! { "url": url.clone(), "prompt": "mine", "extra": 1u64 })).collect();
        inputs.push(("f-number".to_owned(), jobj! { "url": 7u64 }));
        inputs.push(("f-missing".to_owned(), jobj! { "prompt": "no url" }));
        inputs.push(("f-other-tool".to_owned(), Json::Null));
        let replies = stand_in.scratch.path("fetch-replies");
        let mut body = READ_START.to_owned();
        let count = inputs.len();
        for (id, input) in &inputs {
            let tool = if id == "f-other-tool" { "SomethingElse" } else { "WebFetch" };
            let request = jobj! { "subtype": "can_use_tool", "tool_name": tool, "input": input.clone(), "tool_use_id": id.as_str() };
            let frame = jobj! { "type": "control_request", "request_id": format!("r-{id}"), "request": request }.to_text();
            let quoted = frame.replace('\'', "'\\''");
            body.push_str(&format!("printf '%s\\n' '{quoted}'\n"));
        }
        body.push_str(&format!("i=0; while [ $i -lt {count} ] && IFS= read -r line; do printf '%s\\n' \"$line\" >> '{}'; i=$((i+1)); done\n", replies.display()));
        // What the admitted calls returned, in every shape the record reads.
        for frame in [
            r#"{"type":"user","parent_tool_use_id":null,"message":{"content":[{"type":"tool_result","tool_use_id":"f0","content":"from text"}]},"tool_use_result":{"url":"u","code":200,"codeText":"OK","result":"the page","bytes":1}}"#,
            r#"{"type":"user","parent_tool_use_id":null,"message":{"content":[{"type":"tool_result","tool_use_id":"f0","content":"again, settled"}]},"tool_use_result":{"code":200,"result":"second time"}}"#,
            r#"{"type":"user","parent_tool_use_id":"nested","message":{"content":[{"type":"tool_result","tool_use_id":"f7","content":"nested"}]},"tool_use_result":{"code":200,"result":"not read"}}"#,
            r#"{"type":"user","parent_tool_use_id":null,"message":{"content":[{"type":"tool_result","tool_use_id":"f7","content":[{"type":"text","text":"line one"},{"type":"text","text":"line two"},{"type":"image"}]}]}}"#,
            r#"{"type":"user","parent_tool_use_id":null,"message":{"content":[{"type":"tool_result","tool_use_id":"f9","content":"x"}]},"tool_use_result":{"code":404,"codeText":" Not Found ","result":"  "}}"#,
            r#"{"type":"user","parent_tool_use_id":null,"message":{"content":[{"type":"tool_result","tool_use_id":"f12","content":"x"}]},"tool_use_result":{"code":301,"result":"see https://else/"}}"#,
            r#"{"type":"user","parent_tool_use_id":null,"message":{"content":[{"type":"tool_result","tool_use_id":"f14","is_error":true,"content":"<tool_use_error>boom</tool_use_error>"}]},"tool_use_result":"structured"}"#,
            r#"{"type":"user","parent_tool_use_id":null,"message":{"content":[{"type":"tool_result","tool_use_id":"f15","is_error":true,"content":""}]},"tool_use_result":" from output "}"#,
            r#"{"type":"user","parent_tool_use_id":null,"message":{"content":[{"type":"tool_result","tool_use_id":"f16","is_error":true}]}}"#,
            r#"{"type":"user","parent_tool_use_id":null,"message":{"content":[{"type":"tool_result","tool_use_id":"f18","content":" "}]},"tool_use_result":{"code":200}}"#,
            r#"{"type":"user","parent_tool_use_id":null,"message":{"content":[{"type":"tool_result","tool_use_id":"f19","content":"both a"},{"type":"tool_result","tool_use_id":"f20","content":"both b"}]},"tool_use_result":{"code":500,"result":"ignored, two answers"}}"#,
            r#"{"type":"user","parent_tool_use_id":null,"message":{"content":"a string"}}"#,
        ] {
            body.push_str(&say(frame));
        }
        body.push_str(&say(r#"{"type":"result","subtype":"success","is_error":false,"result":"done","modelUsage":{"a":{"inputTokens":10,"outputTokens":2,"cacheReadInputTokens":3,"cacheCreationInputTokens":4,"thinkingTokens":6},"b":{"inputTokens":1,"outputTokens":1}}}"#));
        body.push_str("cat > /dev/null\n");
        let script = stand_in.script("fetcher", &body);
        let urls: Vec<Json> = asked.iter().map(|url| Json::from(*url)).collect();
        let fetched = run(driver, "n1", &stand_in.payload("n1", &script, vec![user("fetch")], |payload| {
            payload.insert("tools", Vec::<Json>::new());
            payload.insert("nativeFetch", jobj! { "maxUses": 0u64, "urls": urls.clone() });
        }));
        driver.send(&release_frame("n1"));
        lines.extend(frames_of(&fetched));
        lines.extend(stand_in.lines("fetch-replies"));

        // The budget, and a request that names nothing.
        let replies = stand_in.scratch.path("budget-replies");
        let mut body = READ_START.to_owned();
        for (id, url) in [("b1", "https://example.com/a"), ("b2", "https://example.com/b"), ("b1", "https://example.com/a"), ("b3", "https://example.com/c")] {
            let request = jobj! { "subtype": "can_use_tool", "tool_name": "WebFetch", "input": jobj! { "url": url }, "tool_use_id": id };
            body.push_str(&say(&jobj! { "type": "control_request", "request_id": format!("r-{id}-{url}"), "request": request }.to_text()));
        }
        body.push_str(&format!("i=0; while [ $i -lt 4 ] && IFS= read -r line; do printf '%s\\n' \"$line\" >> '{}'; i=$((i+1)); done\n", replies.display()));
        body.push_str(&say(RESULT_OK));
        body.push_str("cat > /dev/null\n");
        let script = stand_in.script("budget", &body);
        let budget = run(driver, "n2", &stand_in.payload("n2", &script, vec![user("fetch")], |payload| {
            payload.insert("tools", Vec::<Json>::new());
            payload.insert("nativeFetch", jobj! { "maxUses": 2u64, "urls": jarr!["https://example.com/a", "https://example.com/b", "https://example.com/c"] });
        }));
        driver.send(&release_frame("n2"));
        lines.extend(frames_of(&budget));
        lines.extend(stand_in.lines("budget-replies"));
        let replies = stand_in.scratch.path("none-replies");
        let body = format!(
            "{READ_START}{}IFS= read -r line; printf '%s\\n' \"$line\" >> '{}'\n{}cat > /dev/null\n",
            say(r#"{"type":"control_request","request_id":"r","request":{"subtype":"can_use_tool","tool_name":"WebFetch","input":{"url":"https://x/"},"tool_use_id":"t"}}"#),
            replies.display(),
            say(RESULT_OK),
        );
        let script = stand_in.script("none", &body);
        let none = run(driver, "n3", &stand_in.payload("n3", &script, vec![user("fetch")], |payload| {
            payload.insert("nativeFetch", jobj! { "maxUses": 0u64 });
        }));
        driver.send(&release_frame("n3"));
        lines.extend(frames_of(&none));
        lines.extend(stand_in.lines("none-replies"));

        // `WebSearch`: what counts as a source.
        let replies = stand_in.scratch.path("search-replies");
        let mut body = READ_START.to_owned();
        for id in ["s1", "s2", "s3"] {
            let request = jobj! { "subtype": "can_use_tool", "tool_name": "WebSearch", "input": jobj! { "query": "q" }, "tool_use_id": id };
            body.push_str(&say(&jobj! { "type": "control_request", "request_id": format!("r-{id}"), "request": request }.to_text()));
        }
        body.push_str(&format!("i=0; while [ $i -lt 3 ] && IFS= read -r line; do printf '%s\\n' \"$line\" >> '{}'; i=$((i+1)); done\n", replies.display()));
        let hits = [
            r#"{"title":"A","url":"https://a.example/x"}"#,
            r#"{"title":"dup","url":"https://a.example/x"}"#,
            r#"{"title":"","url":"http://b.example/"}"#,
            r#"{"url":"HTTP://C.EXAMPLE/keeps spelling"}"#,
            r#"{"title":"not a url","url":"nope"}"#,
            r#"{"title":"scheme","url":"ftp://d.example/"}"#,
            r#"{"title":"no url"}"#,
            r#"{"title":"number","url":7}"#,
            r#"{"title":7,"url":"https://e.example/"}"#,
            r#"{"title":"bad host","url":"https://exa mple.com/"}"#,
            r#"{"title":"bad port","url":"https://f.example:99999/"}"#,
            r#"{"title":"javascript","url":"javascript:alert(1)"}"#,
        ];
        let results = format!(r#"{{"results":["just text",{{"content":"no list"}},{{"content":[{}]}}]}}"#, hits.join(","));
        body.push_str(&say(&format!(r#"{{"type":"user","parent_tool_use_id":null,"message":{{"content":[{{"type":"tool_result","tool_use_id":"s1","content":"x"}}]}},"tool_use_result":{results}}}"#)));
        body.push_str(&say(&format!(r#"{{"type":"user","parent_tool_use_id":null,"message":{{"content":[{{"type":"tool_result","tool_use_id":"unadmitted","content":"x"}}]}},"tool_use_result":{{"results":[{{"content":[{{"title":"not read","url":"https://z.example/"}}]}}]}}}}"#)));
        body.push_str(&say_event(MESSAGE_START));
        body.push_str(&say_event(r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#));
        body.push_str(&say_event(r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"first"}}"#));
        body.push_str(&say_event(r#"{"type":"content_block_stop","index":0}"#));
        body.push_str(&say_event(MESSAGE_STOP));
        body.push_str(&say_event(MESSAGE_START));
        body.push_str(&say_event(r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#));
        body.push_str(&say_event(r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"second"}}"#));
        body.push_str(&say_event(r#"{"type":"content_block_stop","index":0}"#));
        body.push_str(&say_event(MESSAGE_STOP));
        body.push_str(&say(r#"{"type":"result","subtype":"success","is_error":false,"result":"done","modelUsage":{}}"#));
        body.push_str("cat > /dev/null\n");
        let script = stand_in.script("searcher", &body);
        let searched = run(driver, "n4", &stand_in.payload("n4", &script, vec![user("search")], |payload| {
            payload.insert("nativeSearch", jobj! { "maxUses": 2u64 });
        }));
        driver.send(&release_frame("n4"));
        lines.extend(frames_of(&searched));
        lines.extend(stand_in.lines("search-replies"));
        lines
    });
}

/// `WebFetch` through both sidecars with the real CLI, against the fake web.
#[test]
fn differential_native_fetch() {
    let certificate = Scratch::new("cert");
    let Some((cert_file, key, cert)) = throwaway_certificate(&certificate, &["api.anthropic.com", "example.com", "docs.python.org"]) else {
        eprintln!("skipped: no openssl");
        return;
    };
    let seen_by: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
    let record = seen_by.clone();
    differential_scenario_with("native fetch", true, &move |bench, driver| {
        let web = FakeWeb::start(
            &key,
            &cert,
            vec![("/a".to_owned(), "<html><head><title>Page A</title></head><body><nav>menu</nav><h1>Page A</h1><p>The body of page A.</p></body></html>".to_owned())],
        );
        bench.upstream.push(Script::blocks(vec![
            text_block("Fetching."),
            tool_use_block("toolu_wf_a", "WebFetch", jobj! { "url": "http://EXAMPLE.com/a#intro", "prompt": "Only the title." }),
            tool_use_block("toolu_wf_b", "WebFetch", jobj! { "url": "https://example.com/b", "prompt": "All of it." }),
            tool_use_block("toolu_wf_c", "WebFetch", jobj! { "url": "https://docs.python.org/3/", "prompt": "All of it." }),
            tool_use_block("toolu_wf_d", "WebFetch", jobj! { "url": "https://example.com/a", "prompt": "All of it." }),
        ]));
        bench.upstream.push(Script::text("# Page A\n\nThe body of page A."));
        bench.upstream.push(Script::text("https://example.com/b could not be fetched (404)."));
        let fetched = run(driver, "f", &bench.payload("f", vec![user("Fetch each of these URLs, one call per URL:\n\nhttps://example.com/a\nhttps://example.com/b")], |payload| {
            payload.insert("tools", Vec::<Json>::new());
            payload.insert("nativeFetch", jobj! { "maxUses": 2u64, "urls": jarr!["https://example.com/a", "https://example.com/b"] });
            let mut agent = bench.agent("f");
            let mut env = bench.env(UPSTREAM_KEY);
            env.insert("HTTPS_PROXY", web.url.as_str());
            env.insert("HTTP_PROXY", web.url.as_str());
            env.insert("NODE_EXTRA_CA_CERTS", cert_file.as_str());
            agent.insert("env", env);
            payload.insert("agent", agent);
        }));
        driver.send(&release_frame("f"));
        let mut seen = web.seen.lock().unwrap().clone();
        seen.sort();
        record.lock().unwrap().push(seen);
        vec![fetched]
    });
    let seen = seen_by.lock().unwrap().clone();
    if let [from_node, from_rust] = seen.as_slice() {
        assert_eq!(from_node, from_rust);
    }
}

/// What a transcript is stamped with is learned from the CLI: the version its `init` reported and the model it served.
#[test]
fn differential_transcript_stamps_are_learned() {
    differential_stand_in("stamps", &|stand_in, driver| {
        let dump = stand_in.scratch.path("transcript");
        let mut body = format!("[ -n \"$CLAUDE_CODE_ENTRYPOINT\" ] && cat \"$CLAUDE_CONFIG_DIR\"/projects/*/*.jsonl >> '{}' 2>/dev/null\n{READ_START}", dump.display());
        body.push_str(&say(r#"{"type":"system","subtype":"init","claude_code_version":"7.7.7","model":"m","apiKeySource":"none","tools":[]}"#));
        body.push_str(&say_event(MESSAGE_START));
        body.push_str(&say(RESULT_OK));
        body.push_str("cat > /dev/null\n");
        let script = stand_in.script("claude", &body);
        let first = run(driver, "t1", &stand_in.payload("t", &script, vec![user("one")], |_| {}));
        driver.send(&release_frame("t"));
        let history = vec![user("one"), jobj! { "role": "assistant", "content": jarr![text_block("a")] }, user("two")];
        let second = run(driver, "t2", &stand_in.payload("t", &script, history, |_| {}));
        driver.send(&release_frame("t"));
        let uuid = regex::Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").unwrap();
        let stamp = regex::Regex::new(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z").unwrap();
        let mut lines = frames_of(&first);
        lines.extend(frames_of(&second));
        let transcript = stand_in.lines("transcript");
        assert_eq!(transcript.len(), 2, "{transcript:?}");
        assert!(transcript[0].contains(r#""version":"7.7.7""#), "{}", transcript[0]);
        assert!(transcript[1].contains(r#""model":"served-model""#), "{}", transcript[1]);
        lines.extend(transcript.iter().map(|line| stamp.replace_all(&uuid.replace_all(line, "<uuid>"), "<time>").into_owned()));
        lines
    });
}

/// A CLI that will not list a tool that joined is not waited for past the deadline: the results go into a rebuilt session.
#[test]
fn differential_a_cli_that_will_not_relist_is_rebuilt() {
    differential_stand_in("relist", &|stand_in, driver| {
        let stdin = stand_in.scratch.path("stdin");
        let mut body = format!("{READ_START}printf 'start\\n' >> '{}'\n", stdin.display());
        body.push_str(&say_event(MESSAGE_START));
        body.push_str(&say_event(r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"bash","input":{}}}"#));
        body.push_str(&say_event(r#"{"type":"content_block_stop","index":0}"#));
        body.push_str(&say_event(MESSAGE_STOP));
        body.push_str(&format!("while IFS= read -r line; do printf '%s\\n' \"$line\" >> '{}'; done\n", stdin.display()));
        let script = stand_in.script("claude", &body);
        let round = run(driver, "w1", &stand_in.payload("w", &script, vec![user("one")], |_| {}));
        let messages = vec![user("one"), assistant_of(&round), tool_result("t1", "bash", text_output("x"))];
        let widened = run(driver, "w2", &stand_in.payload("w", &script, messages, |payload| {
            let mut all = tools();
            all.push(late_tool());
            payload.insert("tools", all);
        }));
        driver.send(&release_frame("w"));
        let uuid = regex::Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").unwrap();
        let mut lines = frames_of(&round);
        lines.extend(frames_of(&widened));
        let seen = stand_in.wait_lines("stdin", 3);
        lines.extend(seen.iter().map(|line| uuid.replace_all(line, "<uuid>").into_owned()));
        lines
    });
}
