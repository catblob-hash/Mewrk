//! The `claude-agent` family: the user's own Claude Code CLI, which the host finds, driven directly over stream-json
//! (`claude-agent.ts`, and what `@anthropic-ai/claude-agent-sdk` did under it). The SDK itself is neither needed nor read.
//!
//! Boundary. One host round is still one sidecar `step`, and inside the CLI it is one Claude API call. Claude Code's own
//! behaviour is switched off (no built-in tools but native search's and native fetch's, no CLAUDE.md/settings/hooks, no
//! user MCP servers, no compaction, no background tasks); the host keeps owning approvals, tool execution, hooks and the
//! round loop. The CLI's tool loop is only borrowed: every host tool is published by one in-process MCP server, and when
//! the model calls one the handler *parks*. The step then ends with `done` carrying `calls`; the host executes them; the
//! next step for the same `agent.session` resolves the parked handlers with the results and streams the next reply.
//!
//! Tools that join mid-conversation. A live session lists them and the CLI hands them over itself (`publish_tools`).
//! Every new turn runs in a session rebuilt from the host's history, and there the host's tool-append markers become the
//! CLI's own record of those additions, on the models the host says take them (`agent.toolChanges`).
//!
//! Tool names. The CLI is started with `CLAUDE_AGENT_SDK_MCP_NO_PREFIX=1`, so the model sees the host's tool names
//! verbatim. The prefix is still stripped defensively wherever a name comes back from the CLI.
//!
//! Compliance. Authentication is the user's own Claude Code login and nothing else. Mewrk sends no credential for this
//! family: every authentication channel that could ride in from the sidecar's environment is stripped before the CLI is
//! spawned, and `apiKey` / `baseURL` on the request are not read at all. This module never reads, copies or moves
//! `~/.claude/.credentials.json`.
//!
//! Concurrency. Node ran all of this on one thread. Here each session's state sits behind a mutex that is never held
//! across an await, and the books shared between sessions behind a second one, always taken inside the first.

mod env;
mod mcp;
mod native;
mod plugin;
mod query;
mod shapes;
mod translate;
mod url;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::dialect::anthropic::{drop_foreign_signed_reasoning, strip_replay_tags};
use crate::error::{StepError, redact_error, secrets_of};
use crate::http::Cancel;
use crate::json::Json;
use crate::protocol::full_system_prompt;
use crate::service::Shared;
use crate::jobj;

use env::{SDK_PROTOCOL_VERSION, cli_env, cli_model_id, sdk_env, validate_agent};
use mcp::{McpHandled, MewrkMcpServer, call_response, list_changed};
use native::{NativeLedger, Permission, native_settings};
use plugin::context_plugin_dir;
use query::{CliArgs, StderrTail, control_error, control_success, initialize_request, is_native_binary, materialize_resume, mcp_notification, remove_resume_dir, spawn_error, user_message};
use shapes::{McpToolResult, SERVER_NAME, SplitMessages, TranscriptContext, random_uuid, split_messages, synthesize_transcript};
use translate::{StepOutcome, StepTranslator, cancelled, permanent};

/// Parked sessions the host never released are evicted after this idle period.
const IDLE_EVICTION: Duration = Duration::from_secs(24 * 60 * 60);
const EVICTION_SWEEP: Duration = Duration::from_secs(60 * 60);
/// How long a live session gets to list a tool that joined before it is rebuilt instead.
const PUBLISH_TIMEOUT: Duration = Duration::from_secs(5);
/// How long `shutdown` waits for torn-down sessions' processes.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);
/// CLI version stamped on synthesized transcript entries when neither the CLI's own `init` nor the host's `--version`
/// probe (`agent.cliVersion`) has named one: a last resort, never the normal path.
const FALLBACK_CLI_VERSION: &str = "2.1.284";
/// Prompt used when a tool round must continue in a fresh CLI session (the parked session is gone). The transcript then
/// already ends with the tool results, and the CLI does not resume a turn without a prompt. The text lives only in that
/// CLI session: the host's own history never contains it.
const CONTINUE_NOTICE: &str = "[SYSTEM NOTIFICATION - NOT USER INPUT]\nThe tool results above are complete. Continue the task.";
/// What a torn-down session's reader ends with: the pipe closes before the exit event, so the SDK's `waitForExit` rejects
/// with this (`GG`). The opposite order would have read "Claude Code process aborted by user" (`ah`), which is the text
/// when nothing was spawned yet. Neither is reported as `cancelled`: the SDK's error class leaves `name` at `Error`, so
/// `isAbortError` never recognized it upstream either.
const PROCESS_ABORTED: &str = "Operation aborted";
const ABORTED_BEFORE_SPAWN: &str = "Claude Code process aborted by user";

fn log(line: &str) {
    eprintln!("[claude-agent] {line}");
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A tool handler waiting for the host.
struct ParkedCall {
    /// The control request whose response answers it.
    request_id: String,
    rpc_id: Json,
}

/// How a session's reader finished.
#[derive(Clone)]
enum Ended {
    Ok,
    Failed(StepError),
}

struct Session {
    key: String,
    uid: u64,
    /// The CLI's stdin. `None` once torn down: nothing more may reach the CLI.
    stdin: Option<mpsc::UnboundedSender<String>>,
    /// Fires at teardown: the reader kills the process.
    abort: Cancel,
    closed: bool,
    server: MewrkMcpServer,
    stderr: Arc<Mutex<StderrTail>>,
    /// Request secrets to strip from any failure text this session produces.
    secrets: Vec<String>,
    /// Handlers that arrived before their result.
    parked: HashMap<String, ParkedCall>,
    /// Results that arrived before their handler.
    results: HashMap<String, McpToolResult>,
    /// Tool_use ids the last step reported; the next step must answer them.
    awaiting: Vec<String>,
    step: Option<StepTranslator>,
    /// Messages received while no step was attached.
    inbox: Vec<Json>,
    /// Set once the reader finished.
    ended: Option<Ended>,
    pump: Option<JoinHandle<()>>,
    last_used_at: Instant,
    /// The CLI this session runs; keys the version its `init` reported.
    executable: String,
    known_tools: HashSet<String>,
    /// Model id the host requested; keys the served-model cache.
    model_id: String,
    /// Set when this session serves a native-search or native-fetch request.
    native: Option<Arc<Mutex<NativeLedger>>>,
    /// The `initialize` request's id; its response may carry permission prompts to redeliver.
    initialize_id: String,
    /// `lastErrorResultText`: the text of an error `result`, which a following non-zero exit is reported as.
    last_error_result: Option<String>,
}

impl Session {
    fn write(&self, line: String) {
        if let Some(stdin) = &self.stdin {
            let mut line = line;
            line.push('\n');
            let _ = stdin.send(line);
        }
    }

    /// Answers a parked or ready `tools/call`.
    fn answer_call(&self, call: &ParkedCall, result: &McpToolResult) {
        let reply = call_response(&call.rpc_id, result.to_json());
        self.write(control_success(&call.request_id, jobj! { "mcp_response": reply }));
    }
}

type SessionRef = Arc<Mutex<Session>>;

/// What every session shares. A leaf lock: taken inside a session's, never around one.
#[derive(Default)]
struct Books {
    sessions: HashMap<String, (u64, SessionRef)>,
    /// Requested model id to the model name the CLI actually served, for transcript stamps.
    served_models: HashMap<String, String>,
    /// CLI executable to the version its own `init` last reported. Keyed by path: the host updates the CLI under a
    /// running sidecar, and a version learned from the old one must not be stamped on a transcript for the new one.
    reported_cli_versions: HashMap<String, String>,
}

struct Inner {
    shared: Arc<Shared>,
    books: Mutex<Books>,
    next_uid: AtomicU64,
    sweep: Mutex<Option<JoinHandle<()>>>,
}

impl Inner {
    fn forget(&self, key: &str, uid: u64) {
        let mut books = lock(&self.books);
        if books.sessions.get(key).is_some_and(|(known, _)| *known == uid) {
            books.sessions.remove(key);
        }
    }

    fn live(&self, key: &str) -> Option<SessionRef> {
        lock(&self.books).sessions.get(key).map(|(_, session)| session.clone())
    }

    /// `teardown`. Kill first: a parked handler must never be answered, or the CLI would run one more API call before
    /// a graceful shutdown reached it. A run the host ended must not cost anything more.
    fn teardown(&self, session: &mut Session) {
        self.forget(&session.key, session.uid);
        session.closed = true;
        session.abort.cancel();
        session.stdin = None;
        session.parked.clear();
        session.results.clear();
        session.awaiting.clear();
        session.server.close();
    }

    /// `deliver`.
    fn deliver(&self, session: &mut Session, message: Json) {
        session.last_used_at = Instant::now();
        let kind = message.str_at("type");
        let subtype = message.str_at("subtype");
        if kind == Some("system") && subtype == Some("init") {
            let version = message.str_at("claude_code_version");
            if let Some(version) = version {
                lock(&self.books).reported_cli_versions.insert(session.executable.clone(), version.to_owned());
            }
            log(&format!(
                "会话 {} 就绪：Claude Code {}，模型 {}，凭据来源 {}，工具 {} 个",
                session.key,
                version.unwrap_or(""),
                message.str_at("model").unwrap_or(""),
                message.str_at("apiKeySource").unwrap_or(""),
                message.get("tools").and_then(Json::as_arr).map_or(0, Vec::len),
            ));
            return;
        }
        if kind == Some("stream_event") && message.get("parent_tool_use_id").is_some_and(Json::is_null) {
            if let Some(event) = message.get("event").filter(|event| event.str_at("type") == Some("message_start")) {
                if let Some(model) = event.path(&["message", "model"]).and_then(Json::as_str) {
                    lock(&self.books).served_models.insert(session.model_id.clone(), model.to_owned());
                }
            }
        }
        if kind == Some("system") && subtype == Some("api_retry") {
            log(&format!("会话 {}：Claude Code 正在重试上游请求", session.key));
        }
        if let Some(step) = session.step.as_mut() {
            if let Err(failure) = step.handle(&message) {
                step.fail(permanent(failure));
                self.teardown(session);
            }
            self.detach_if_settled(session);
            return;
        }
        if matches!(kind, Some("stream_event") | Some("assistant") | Some("result") | Some("user")) {
            session.inbox.push(message);
        }
    }

    /// A settled step lets go of the session before the next message is read, where its continuation ran in Node.
    fn detach_if_settled(&self, session: &mut Session) {
        if session.step.as_ref().is_some_and(StepTranslator::is_settled) {
            if let Some(step) = session.step.take() {
                session.awaiting = step.call_ids().to_vec();
            }
        }
    }

    /// `handleControlRequest`: a question from the CLI.
    fn handle_control_request(&self, session: &mut Session, frame: &Json) {
        if session.closed {
            return;
        }
        let Some(request_id) = frame.str_at("request_id") else { return };
        // A duplicate delivery of a request still in flight is skipped; only a parked call stays in flight.
        if session.parked.values().any(|parked| parked.request_id == request_id) {
            return;
        }
        let empty = Json::obj();
        let request = frame.get("request").unwrap_or(&empty);
        let subtype = request.str_at("subtype").unwrap_or("");
        let reply = match subtype {
            // For the machine serving this session's tools, and a dialog nobody here can show: left unanswered.
            "remote_tool_call" | "remote_plumbing_call" | "remote_tools_probe" | "remote_tools_reannounce" | "request_user_dialog" => return,
            "can_use_tool" => {
                let tool_use_id = request.get("tool_use_id").cloned();
                let name = request.str_at("tool_name").unwrap_or("");
                let input = request.get("input").cloned().unwrap_or(Json::Null);
                // The host has already approved every call it sends back, so the CLI's own permission layer answers
                // "allow" for everything. The CLI's own search and fetch are the exception, held to the host's budget and
                // to the pages it named.
                let native = session.native.as_ref().filter(|native| lock(native).tool() == name);
                let permission = match native {
                    Some(native) => lock(native).admit(tool_use_id.as_ref().and_then(Json::as_str).unwrap_or(""), &input),
                    None => Permission::Allow(input),
                };
                Ok(match permission {
                    Permission::Allow(updated) => jobj! { "behavior": "allow", "updatedInput": updated, "toolUseID"?: tool_use_id },
                    Permission::Deny(message) => jobj! { "behavior": "deny", "message": message, "toolUseID"?: tool_use_id },
                })
            }
            "hook_callback" => Err(format!("No hook callback found for ID: {}", request.str_at("callback_id").unwrap_or("undefined"))),
            "mcp_message" => {
                let server_name = request.str_at("server_name").unwrap_or("undefined");
                if server_name != SERVER_NAME {
                    Err(format!("SDK MCP server not found: {server_name}"))
                } else {
                    let message = request.get("message").unwrap_or(&empty);
                    let is_request = message.get("method").is_some() && message.get("id").is_some_and(|id| !id.is_null());
                    match session.server.handle(message) {
                        McpHandled::Reply(reply) => Ok(jobj! { "mcp_response": reply }),
                        McpHandled::Call { rpc_id, tool_use_id, arguments } => {
                            let id = tool_use_id.unwrap_or_default();
                            if let Some(step) = session.step.as_mut() {
                                step.supply_tool_input(&id, arguments.as_ref());
                            }
                            self.detach_if_settled(session);
                            let call = ParkedCall { request_id: request_id.to_owned(), rpc_id };
                            match session.results.remove(&id) {
                                Some(ready) => session.answer_call(&call, &ready),
                                None => {
                                    session.parked.insert(id, call);
                                }
                            }
                            return;
                        }
                        McpHandled::Silent if is_request => return,
                        McpHandled::Silent => Ok(jobj! { "mcp_response": jobj! { "jsonrpc": "2.0", "result": Json::obj(), "id": 0u64 } }),
                    }
                }
            }
            "elicitation" => Ok(jobj! { "action": "decline" }),
            "oauth_token_refresh" => Err("getOAuthToken callback is not provided.".to_owned()),
            "host_auth_token_refresh" => Err("getHostAuthToken callback is not provided.".to_owned()),
            "remote_control_work_secret" => Err("refreshWorkSecret callback is not provided.".to_owned()),
            other => Err(format!("Unsupported control request subtype: {other}")),
        };
        session.write(match reply {
            Ok(response) => control_success(request_id, response),
            Err(error) => control_error(request_id, &error),
        });
    }

    /// One line off the CLI's stdout (`readMessages`).
    fn process_frame(&self, session: &mut Session, frame: Json) {
        match frame.str_at("type") {
            Some("control_response") => {
                // An `initialize` response may redeliver permission prompts a previous attach left open.
                if frame.path(&["response", "request_id"]).and_then(Json::as_str) == Some(session.initialize_id.as_str()) {
                    if let Some(pending) = frame.path(&["response", "pending_permission_requests"]).and_then(Json::as_arr) {
                        for request in pending {
                            if request.path(&["request", "subtype"]).and_then(Json::as_str) == Some("can_use_tool") {
                                self.handle_control_request(session, request);
                            }
                        }
                    }
                }
            }
            Some("control_request") => self.handle_control_request(session, &frame),
            Some("control_cancel_request") | Some("keep_alive") | Some("transcript_mirror") => {}
            kind => {
                // Bookkeeping for the error a following exit is reported as; these few leave it standing.
                let subtype = frame.str_at("subtype");
                let keeps_error = matches!(kind, Some("active_goal") | Some("autocompact_state"))
                    || (kind == Some("system") && matches!(subtype, Some("post_turn_summary") | Some("task_summary") | Some("session_metadata") | Some("session_state_changed")));
                if kind == Some("result") {
                    session.last_error_result = error_result_text(&frame);
                } else if !keeps_error {
                    session.last_error_result = None;
                }
                self.deliver(session, frame);
            }
        }
    }
}

/// What the SDK remembers of an error `result`: its text, if it has any.
fn error_result_text(frame: &Json) -> Option<String> {
    if !frame.get("is_error").and_then(Json::as_bool).unwrap_or(false) {
        return None;
    }
    let text = if frame.str_at("subtype") == Some("success") {
        frame.str_at("result").unwrap_or("").to_owned()
    } else {
        let errors = frame.get("errors").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice);
        errors.iter().filter_map(Json::as_str).map(str::trim).filter(|error| !error.is_empty()).collect::<Vec<_>>().join("; ")
    };
    (!text.is_empty()).then_some(text)
}

/// What a session's reader needs to start the CLI.
struct Launch {
    executable: String,
    args: Vec<String>,
    cwd: String,
    env: BTreeMap<String, String>,
    /// Session id and transcript when the session resumes from host history.
    resume: Option<(String, Vec<Json>)>,
}

/// `spawn`, where a failure message is the transport's.
fn spawn_cli(launch: &Launch, env: &BTreeMap<String, String>) -> Result<tokio::process::Child, String> {
    let mut command = if is_native_binary(&launch.executable) {
        Command::new(&launch.executable)
    } else {
        let mut node = Command::new("node");
        node.arg(&launch.executable);
        node
    };
    command
        .args(&launch.args)
        .current_dir(&launch.cwd)
        .env_clear()
        .envs(env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // The host going away must not leave a CLI behind, whatever path got it there.
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);
    command.spawn().map_err(|error| spawn_error(&error, &launch.executable))
}

/// Reads the CLI until it is gone; `Err` is what the SDK's iterator threw.
async fn run_cli(inner: &Arc<Inner>, session: &SessionRef, mut launch: Launch, mut lines: mpsc::UnboundedReceiver<String>, abort: Cancel, resume_dir: &mut Option<PathBuf>) -> Result<(), String> {
    let mut env = launch.env.clone();
    if let Some((session_id, transcript)) = launch.resume.take() {
        let (cwd, for_key) = (launch.cwd.clone(), launch.env.clone());
        let written = tokio::task::spawn_blocking(move || materialize_resume(&session_id, &cwd, &for_key, &transcript))
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
        if let Some(dir) = written {
            env.insert("CLAUDE_CONFIG_DIR".to_owned(), dir.to_string_lossy().into_owned());
            *resume_dir = Some(dir);
        }
    }
    if abort.is_cancelled() {
        return Err(ABORTED_BEFORE_SPAWN.to_owned());
    }
    let mut child = spawn_cli(&launch, &env)?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let writer = tokio::spawn(async move {
        let Some(mut stdin) = stdin else { return };
        while let Some(line) = lines.recv().await {
            if stdin.write_all(line.as_bytes()).await.is_err() {
                // The child likely exited; its exit is what gets reported.
                return;
            }
        }
    });
    let stderr_tail = lock(session).stderr.clone();
    let stderr_reader = tokio::spawn(async move {
        let Some(mut stderr) = stderr else { return };
        let mut chunk = vec![0u8; 8192];
        while let Ok(read) = stderr.read(&mut chunk).await {
            if read == 0 {
                return;
            }
            lock(&stderr_tail).push_bytes(&chunk[..read]);
        }
    });

    let mut reader = BufReader::new(stdout.ok_or("Claude Code stdout is not available")?);
    let mut buffer = Vec::new();
    let mut aborted = false;
    loop {
        tokio::select! {
            biased;
            () = abort.cancelled() => {
                aborted = true;
                break;
            }
            read = reader.read_until(b'\n', &mut buffer) => {
                match read {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let text = String::from_utf8_lossy(&buffer);
                        let line = native::js_trim(&text);
                        // A line that is not JSON is the CLI's business; the SDK skipped it too.
                        if let Some(frame) = (!line.is_empty()).then(|| Json::parse(line).ok()).flatten() {
                            let mut guard = lock(session);
                            inner.process_frame(&mut guard, frame);
                        }
                        buffer.clear();
                    }
                }
            }
        }
    }
    let status = if aborted {
        let _ = child.start_kill();
        None
    } else {
        tokio::select! {
            biased;
            () = abort.cancelled() => {
                aborted = true;
                let _ = child.start_kill();
                None
            }
            status = child.wait() => status.ok(),
        }
    };
    if aborted {
        let _ = child.wait().await;
    }
    writer.abort();
    // The tail is only useful whole; a grandchild holding the pipe must not hold this up.
    let _ = tokio::time::timeout(Duration::from_millis(200), stderr_reader).await;
    if aborted {
        return Err(PROCESS_ABORTED.to_owned());
    }
    let Some(status) = status else { return Ok(()) };
    if status.success() {
        return Ok(());
    }
    let exit = match status.code() {
        Some(code) => format!("Claude Code process exited with code {code}"),
        #[cfg(unix)]
        None => {
            use std::os::unix::process::ExitStatusExt;
            format!("Claude Code process terminated by signal {}", status.signal().map_or_else(|| "unknown".to_owned(), query::signal_name))
        }
        #[cfg(not(unix))]
        None => "Claude Code process terminated".to_owned(),
    };
    // A result that failed says why better than the exit code does.
    Err(match lock(session).last_error_result.take() {
        Some(text) => format!("Claude Code returned an error result: {text}"),
        None => exit,
    })
}

/// `pump`: the session's reader, from spawn to the end of the process.
async fn pump(inner: Arc<Inner>, session: SessionRef, launch: Launch, lines: mpsc::UnboundedReceiver<String>, abort: Cancel) {
    let mut resume_dir = None;
    let finished = run_cli(&inner, &session, launch, lines, abort, &mut resume_dir).await;
    {
        let mut guard = lock(&session);
        let ended = match finished {
            Ok(()) => Ended::Ok,
            Err(message) => {
                // `classifyQueryError`.
                let tail = lock(&guard.stderr).text();
                Ended::Failed(permanent(if tail.is_empty() { message } else { format!("{message}\n{tail}") }))
            }
        };
        guard.ended = Some(ended.clone());
        // A step still attached when the iterator ends never got its message.
        if let Some(mut step) = guard.step.take() {
            step.fail(match ended {
                Ended::Ok => permanent("Claude Code 会话在回复完成前结束"),
                Ended::Failed(error) => error,
            });
        }
        inner.forget(&guard.key, guard.uid);
    }
    // Its CLI is gone. After the terminal frame, so a slow disk cannot hold that up, and still inside this task, so
    // `shutdown` waits for it.
    if let Some(dir) = resume_dir {
        remove_resume_dir(dir).await;
    }
}

/// `sessionServesToolSet`: whether a live session can still serve this step's tool set. Narrowing is not a mismatch: a
/// step may legitimately offer fewer tools, and the CLI simply never calls what the model was not shown.
fn session_serves_tool_set(session: &Session, tools: &[Json]) -> bool {
    tools.iter().all(|tool| tool.str_at("name").is_some_and(|name| session.known_tools.contains(name)))
}

/// `publishTools`: hands the tools that joined mid-run to a live session. The in-process server adds them to its list,
/// `tools/list_changed` makes the CLI ask for the list again, and the answer to that request is the confirmation. The CLI
/// reads its stdin in order, so the new list is in its pool before the parked result that follows it resumes the round.
/// From there the CLI appends the tool itself. `false` when the CLI does not ask in time; the caller rebuilds instead.
async fn publish_tools(session: &SessionRef, tools: &[Json]) -> bool {
    let (key, waiter) = {
        let mut guard = lock(session);
        let key = guard.key.clone();
        match guard.server.publish(tools) {
            Ok(Some(waiter)) => {
                guard.write(mcp_notification(list_changed()));
                (key, Some(waiter))
            }
            Ok(None) => (key, None),
            Err(error) => {
                log(&format!("会话 {key} 发布新工具失败：{error}"));
                return false;
            }
        }
    };
    if let Some(waiter) = waiter {
        if !matches!(tokio::time::timeout(PUBLISH_TIMEOUT, waiter).await, Ok(Ok(()))) {
            log(&format!("会话 {key} 的 CLI 没有重新列出工具"));
            return false;
        }
    }
    lock(session).known_tools = tools.iter().filter_map(|tool| tool.str_at("name")).map(str::to_owned).collect();
    true
}

/// `runAttached`'s first half: attach and drain the inbox.
fn attach(inner: &Inner, session: &SessionRef, id: &str) -> oneshot::Receiver<StepOutcome> {
    let mut guard = lock(session);
    let (mut translator, outcome) = StepTranslator::new(id, inner.shared.clone(), guard.known_tools.clone(), guard.native.clone());
    if let Some(ended) = &guard.ended {
        translator.fail(match ended {
            Ended::Ok => permanent("Claude Code 会话已结束"),
            Ended::Failed(error) => error.clone(),
        });
    }
    guard.step = Some(translator);
    inner.detach_if_settled(&mut guard);
    for message in std::mem::take(&mut guard.inbox) {
        inner.deliver(&mut guard, message);
    }
    outcome
}

/// `runAttached`'s second half: wait for the outcome and report it.
async fn wait_attached(inner: &Inner, session: &SessionRef, id: &str, cancel: &Cancel, mut outcome: oneshot::Receiver<StepOutcome>) {
    let finished = tokio::select! {
        biased;
        finished = &mut outcome => finished,
        () = cancel.cancelled() => {
            {
                let mut guard = lock(session);
                if let Some(step) = guard.step.as_mut() {
                    step.fail(cancelled());
                }
                inner.detach_if_settled(&mut guard);
                inner.teardown(&mut guard);
            }
            outcome.await
        }
    };
    match finished {
        Ok(StepOutcome::Done(result)) => inner.shared.done(id, result),
        Ok(StepOutcome::Failed(error)) => {
            let mut guard = lock(session);
            inner.shared.fail(id, redact_error(error.clone(), &guard.secrets));
            if error.kind != "cancelled" {
                inner.teardown(&mut guard);
            }
        }
        // The translator went away without an answer, which nothing does; the host still needs a terminal frame.
        Err(_) => inner.shared.fail(id, permanent("Claude Code 会话在回复完成前结束")),
    }
    lock(session).last_used_at = Instant::now();
}

/// `startSession`.
fn start_session(inner: &Arc<Inner>, id: &str, request: &Json, agent: &Json, prompt: Json, history: &[Json], prompt_additions: &[Vec<String>]) -> Result<(SessionRef, oneshot::Receiver<StepOutcome>), String> {
    let cwd = agent.str_at("cwd").unwrap_or("");
    let executable = agent.str_at("executable").unwrap_or("");
    let key = agent.str_at("session").unwrap_or("");
    let model_id = request.str_at("modelId").unwrap_or("");
    // First, and failing: a session without the plugin must not start at all.
    let plugin = context_plugin_dir(cwd)?;
    let tools: &[Json] = request.get("tools").and_then(Json::as_arr).map_or(&[], Vec::as_slice);
    let known_tools: HashSet<String> = tools.iter().filter_map(|tool| tool.str_at("name")).map(str::to_owned).collect();
    let native = NativeLedger::of_request(request);

    // What the CLI runs; `modelId` stays the host's name for the model (signature tags, served-model stamps).
    let cli_model = cli_model_id(model_id, request.get("contextWindow").and_then(Json::as_f64));
    let resumed = !history.is_empty();
    let session_id = random_uuid();
    let transcript = if resumed {
        let (cli_version, model) = {
            let books = lock(&inner.books);
            (
                books.reported_cli_versions.get(executable).cloned().or_else(|| agent.str_at("cliVersion").map(str::to_owned)).unwrap_or_else(|| FALLBACK_CLI_VERSION.to_owned()),
                books.served_models.get(model_id).cloned().unwrap_or_else(|| model_id.to_owned()),
            )
        };
        let appendable = (agent.get("toolChanges").and_then(Json::as_bool) == Some(true))
            .then(|| tools.iter().filter_map(|tool| Some((tool.str_at("name")?.to_owned(), tool.clone()))).collect());
        synthesize_transcript(
            history,
            &TranscriptContext { session_id: &session_id, cwd, cli_version: &cli_version, model: &model, appendable, prompt_additions },
        )
    } else {
        Vec::new()
    };

    let mut env = cli_env(agent, &cli_model, request.get("maxOutputTokens").and_then(Json::as_f64))?;
    sdk_env(&mut env, SDK_PROTOCOL_VERSION);

    let builtin = native.as_ref().map(NativeLedger::tool);
    let settings = jobj! { "totalTokensReminder": "off", "permissions"?: native_settings(native.as_ref()) };
    let args = CliArgs {
        // Every level goes to Claude Code's `--effort` as named: the CLI knows each model's efforts and drops one the
        // model lacks to `high`. Nothing turns thinking off.
        effort: request.str_at("reasoning"),
        model: &cli_model,
        resume: resumed.then_some(session_id.as_str()),
        builtin,
        plugin: &plugin,
        settings: &settings,
    }
    .to_vec();

    let server = MewrkMcpServer::new(tools);
    let system_prompt = full_system_prompt(request).unwrap_or_default();
    let (initialize_id, initialize) = initialize_request(server.manifest(), &system_prompt);
    let (stdin, lines) = mpsc::unbounded_channel();
    let abort = Cancel::new();
    let uid = inner.next_uid.fetch_add(1, Ordering::Relaxed);
    let session = Session {
        key: key.to_owned(),
        uid,
        stdin: Some(stdin),
        abort: abort.clone(),
        closed: false,
        server,
        stderr: Arc::new(Mutex::new(StderrTail::default())),
        secrets: secrets_of(request),
        parked: HashMap::new(),
        results: HashMap::new(),
        awaiting: Vec::new(),
        step: None,
        inbox: Vec::new(),
        ended: None,
        pump: None,
        last_used_at: Instant::now(),
        executable: executable.to_owned(),
        known_tools,
        model_id: model_id.to_owned(),
        native: native.map(|native| Arc::new(Mutex::new(native))),
        initialize_id,
        last_error_result: None,
    };
    // Queued in the order the SDK wrote them, for the writer to send once the CLI is up.
    session.write(initialize);
    session.write(user_message(prompt));
    let session: SessionRef = Arc::new(Mutex::new(session));
    let launch = Launch {
        executable: executable.to_owned(),
        args,
        cwd: cwd.to_owned(),
        env,
        resume: resumed.then_some((session_id, transcript)),
    };
    lock(&inner.books).sessions.insert(key.to_owned(), (uid, session.clone()));
    // Attached before the reader exists, as nothing could run between the two in Node: a CLI that answers and exits at
    // once must still find its step.
    let outcome = attach(inner, &session, id);
    let handle = tokio::spawn(pump(inner.clone(), session.clone(), launch, lines, abort));
    lock(&session).pump = Some(handle);
    Ok((session, outcome))
}

/// `step`, up to the point where a failure becomes an error frame.
async fn run_step(inner: &Arc<Inner>, id: &str, request: &mut Json, cancel: &Cancel) -> Result<(), String> {
    let agent = validate_agent(request.get("agent"))?.clone();
    let key = agent.str_at("session").unwrap_or("").to_owned();
    let model_id = request.str_at("modelId").unwrap_or("").to_owned();
    // The host's tool-append markers stay in the history. Replayed reasoning parts are tagged by the host with the model
    // that signed them; Anthropic binds a signature to that model, so a switched conversation drops them and the tag
    // itself never reaches the CLI.
    if let Some(messages) = request.get_mut("messages").and_then(Json::as_arr_mut) {
        drop_foreign_signed_reasoning(messages, &model_id);
        strip_replay_tags(messages);
    }
    let request = &*request;
    let messages: &[Json] = request.get("messages").and_then(Json::as_arr).map_or(&[], Vec::as_slice);
    let tools: &[Json] = request.get("tools").and_then(Json::as_arr).map_or(&[], Vec::as_slice);
    let split = split_messages(messages);
    let live = inner.live(&key);

    // Continuation: the parked round gets its results.
    if let Some(live) = &live {
        let awaiting = {
            let guard = lock(live);
            if guard.ended.is_none() && !guard.awaiting.is_empty() && split.prompt.is_none() { guard.awaiting.clone() } else { Vec::new() }
        };
        if !awaiting.is_empty() {
            let missing: Vec<&str> = awaiting.iter().filter(|call| !split.results.contains_key(*call)).map(String::as_str).collect();
            if missing.is_empty() {
                // The parked call is answerable, but the step that follows it may need a tool this session never listed.
                // The session lists it before the round resumes; only when the CLI will not confirm it do the results go
                // into a rebuilt session's transcript instead.
                let served = session_serves_tool_set(&lock(live), tools);
                if served || publish_tools(live, tools).await {
                    continue_parked(inner, live, id, cancel, &split).await;
                    return Ok(());
                }
                log(&format!("会话 {key} 未能列出新增工具，改为重建会话"));
            } else {
                log(&format!("会话 {key} 缺少工具结果 {}，改为重建会话", missing.join(", ")));
            }
        }
        inner.teardown(&mut lock(live));
    }

    let (prompt, history, additions): (Json, &[Json], &[Vec<String>]) = if let Some(prompt) = &split.prompt {
        (Json::Arr(prompt.clone()), &split.history, &split.prompt_additions)
    } else if !split.results.is_empty() || !split.carriers.is_empty() {
        // The parked session is gone; the transcript already carries the results. Carriers alone happen when a background
        // delivery wakes a round of its own.
        (Json::from(CONTINUE_NOTICE), messages, &[])
    } else {
        return Err("Claude Code 需要一条用户消息才能开始回合".to_owned());
    };
    let (session, outcome) = start_session(inner, id, request, &agent, prompt, history, additions)?;
    wait_attached(inner, &session, id, cancel, outcome).await;
    Ok(())
}

/// The results go to the parked handlers, with the carriers folded into the last of them.
async fn continue_parked(inner: &Inner, live: &SessionRef, id: &str, cancel: &Cancel, split: &SplitMessages) {
    let ordered = std::mem::take(&mut lock(live).awaiting);
    let outcome = attach(inner, live, id);
    {
        let mut guard = lock(live);
        let last = ordered.len().saturating_sub(1);
        for (index, call_id) in ordered.iter().enumerate() {
            let mut result = split.results.get(call_id).cloned().unwrap_or_default();
            if index == last {
                result.content.extend(split.carriers.iter().cloned());
            }
            match guard.parked.remove(call_id) {
                Some(parked) => guard.answer_call(&parked, &result),
                None => {
                    guard.results.insert(call_id.clone(), result);
                }
            }
        }
    }
    wait_attached(inner, live, id, cancel, outcome).await;
}

pub struct ClaudeAgentRuntime {
    inner: Arc<Inner>,
}

impl ClaudeAgentRuntime {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self {
            inner: Arc::new(Inner {
                shared,
                books: Mutex::new(Books::default()),
                next_uid: AtomicU64::new(0),
                sweep: Mutex::new(None),
            }),
        }
    }

    /// The hourly eviction sweep, started with the first step: `new` runs outside the async runtime.
    fn ensure_sweep(&self) {
        let mut slot = lock(&self.inner.sweep);
        if slot.is_some() {
            return;
        }
        let inner: Weak<Inner> = Arc::downgrade(&self.inner);
        *slot = Some(tokio::spawn(async move {
            let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + EVICTION_SWEEP, EVICTION_SWEEP);
            loop {
                ticks.tick().await;
                let Some(inner) = inner.upgrade() else { return };
                let sessions: Vec<SessionRef> = lock(&inner.books).sessions.values().map(|(_, session)| session.clone()).collect();
                for session in sessions {
                    let mut guard = lock(&session);
                    if guard.step.is_none() && guard.last_used_at.elapsed() > IDLE_EVICTION {
                        log(&format!("会话 {} 闲置超过 24 小时，回收", guard.key));
                        inner.teardown(&mut guard);
                    }
                }
            }
        }));
    }

    pub async fn step(&self, id: String, mut payload: Json) {
        let cancel = Cancel::new();
        self.inner.shared.begin(&id, cancel.clone(), true);
        self.ensure_sweep();
        let inner = self.inner.clone();
        let task_id = id.clone();
        // A step that panics still owes the host a terminal frame, or the host waits for its absolute limit.
        let task = tokio::spawn(async move {
            let secrets = secrets_of(&payload);
            if let Err(message) = run_step(&inner, &task_id, &mut payload, &cancel).await {
                inner.shared.fail(&task_id, redact_error(permanent(message), &secrets));
            }
        });
        if let Err(error) = task.await {
            self.inner.shared.fail(&id, permanent(format!("侧车内部错误: {error}")));
        }
        self.inner.shared.end(&id);
    }

    pub async fn release(&self, session: String) {
        if let Some(live) = self.inner.live(&session) {
            self.inner.teardown(&mut lock(&live));
        }
    }

    pub async fn shutdown(&self) {
        if let Some(sweep) = lock(&self.inner.sweep).take() {
            sweep.abort();
        }
        let sessions: Vec<SessionRef> = lock(&self.inner.books).sessions.values().map(|(_, session)| session.clone()).collect();
        let mut pumps = Vec::new();
        for session in sessions {
            let mut guard = lock(&session);
            pumps.extend(guard.pump.take());
            self.inner.teardown(&mut guard);
        }
        let deadline = tokio::time::Instant::now() + SHUTDOWN_GRACE;
        for pump in pumps {
            let _ = tokio::time::timeout_at(deadline, pump).await;
        }
    }
}
