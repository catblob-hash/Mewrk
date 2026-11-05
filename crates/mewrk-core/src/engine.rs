//! The agent loop: stream a turn, survive whatever the network does, run the tools it asked for, repeat.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_rt::error::Backoff;
use agent_rt::sse::{SseEvent, SseParser};
use agent_rt::wire::Rope;
use agent_rt::{Cancel, FailKind, NetError, RetryPolicy, Transport};
use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::oneshot;

use crate::inbox::*;
use crate::model::*;
use crate::prompt;
use crate::provider::{self, Delta, ToolSpec, TurnRequest};
use crate::store::Store;
use crate::tasks::{Session, Task, TaskKind, TaskState};
use crate::tools::{self, ToolCtx};

/// What a subagent gets told about itself.
#[derive(Clone)]
pub struct ChildInfo {
    pub task: Arc<Task>,
}

#[derive(Clone)]
pub struct RunSpec {
    pub conv_id: String,
    /// Where the items are appended. A subagent's is `<parent>/<child>`.
    pub store_key: String,
    pub items: Vec<Item>,
    pub settings: ConvSettings,
    pub provider: ProviderConfig,
    pub model: ModelProfile,
    pub api_key: Option<String>,
    /// The first is the working directory.
    pub workspaces: Vec<PathBuf>,
    pub global_instructions: String,
    pub session: Arc<Session>,
    pub child: Option<ChildInfo>,
}

enum Pending {
    Approval(oneshot::Sender<ApprovalDecision>),
    Question(oneshot::Sender<Value>),
}

pub struct RunHandle {
    pub id: String,
    pub conv_id: String,
    pub label: String,
    pub inbox: Inbox,
    pub cancel: Cancel,
    notifier: Arc<Notifier>,
    pending: Mutex<HashMap<u64, Pending>>,
}

/// Unique across runs: a subagent's card sits on its parent's composer, next to the parent's own.
static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);

impl std::fmt::Debug for RunHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RunHandle({})", self.id)
    }
}

impl RunHandle {
    pub fn emit(&self, ev: RunEvent) {
        self.inbox.push(ev);
        self.notifier.poke();
    }

    pub fn stop(&self) {
        self.cancel.cancel();
    }

    /// Answers a waiting approval card. A stale answer is ignored.
    pub fn approve(&self, request: u64, decision: ApprovalDecision) {
        if let Some(Pending::Approval(tx)) = self.pending.lock().remove(&request) {
            let _ = tx.send(decision);
        }
    }

    pub fn answer(&self, request: u64, answers: Value) {
        if let Some(Pending::Question(tx)) = self.pending.lock().remove(&request) {
            let _ = tx.send(answers);
        }
    }

    fn new_request(&self) -> u64 {
        NEXT_REQUEST.fetch_add(1, Ordering::Relaxed)
    }

    /// Asks the user and waits, or returns `None` if the run is stopped first.
    pub async fn ask_approval(&self, item: ItemId, tool: &str, args: &Value, summary: String) -> Option<ApprovalDecision> {
        let request = self.new_request();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().insert(request, Pending::Approval(tx));
        self.emit(RunEvent::Approval(ApprovalRequest { request, item, tool: tool.to_owned(), args: args.clone(), summary }));
        tokio::select! {
            d = rx => d.ok(),
            _ = self.cancel.cancelled() => None,
        }
    }

    pub async fn ask_question(&self, item: ItemId, questions: Value) -> Option<Value> {
        let request = self.new_request();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().insert(request, Pending::Question(tx));
        self.emit(RunEvent::Question(QuestionRequest { request, item, questions }));
        tokio::select! {
            a = rx => a.ok(),
            _ = self.cancel.cancelled() => None,
        }
    }
}

pub struct Engine {
    pub rt: tokio::runtime::Handle,
    pub store: Store,
    pub notifier: Arc<Notifier>,
    transports: Mutex<HashMap<String, Arc<Transport>>>,
}

impl Engine {
    pub fn new(rt: tokio::runtime::Handle, store: Store, notifier: Arc<Notifier>) -> Arc<Self> {
        Arc::new(Self { rt, store, notifier, transports: Mutex::new(HashMap::new()) })
    }

    /// One transport per provider and key, so connections get reused. A changed setting makes a new one.
    fn transport(&self, p: &ProviderConfig, key: Option<&str>) -> Result<Arc<Transport>, String> {
        let cache_key = format!("{}\0{}\0{:?}\0{}", p.id, p.base_url, p.kind, key.unwrap_or(""));
        if let Some(t) = self.transports.lock().get(&cache_key) {
            return Ok(t.clone());
        }
        let t = provider::build_transport(p, key)?;
        self.transports.lock().insert(cache_key, t.clone());
        Ok(t)
    }

    /// Starts a run. `spec.items` is the whole conversation, ending in what the model should answer.
    pub fn start(self: &Arc<Self>, spec: RunSpec, label: impl Into<String>) -> Arc<RunHandle> {
        let handle = Arc::new(RunHandle {
            id: new_id(),
            conv_id: spec.conv_id.clone(),
            label: label.into(),
            inbox: Inbox::default(),
            cancel: Cancel::new(),
            notifier: self.notifier.clone(),
            pending: Mutex::new(HashMap::new()),
        });
        let engine = self.clone();
        let h = handle.clone();
        self.rt.spawn(async move {
            let child = spec.child.clone();
            let end = drive(engine, h.clone(), spec).await;
            if let Some(c) = child {
                let (state, summary) = match &end {
                    (RunEnd::Completed, text) => (TaskState::Completed, text.clone()),
                    (RunEnd::Cancelled, text) => (TaskState::Stopped, text.clone()),
                    (RunEnd::Failed(e), text) => (TaskState::Failed(e.clone()), text.clone()),
                };
                c.task.set_output(summary);
                c.task.finish(state);
            }
            h.emit(RunEvent::Finished(end.0));
        });
        handle
    }
}

const MAX_STEPS: u32 = 500;

/// A prompt this close to the window is summarised before the next request goes out.
const COMPACT_AT: f64 = 0.85;

struct Run {
    engine: Arc<Engine>,
    handle: Arc<RunHandle>,
    spec: RunSpec,
    transport: Arc<Transport>,
    items: Vec<Item>,
    next_id: ItemId,
    persisted: usize,
}

impl Run {
    fn emit(&self, ev: RunEvent) {
        self.handle.emit(ev);
    }

    fn fresh_id(&mut self) -> ItemId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn persist_new(&mut self) {
        if self.persisted < self.items.len() {
            self.engine.store.append(&self.spec.store_key, self.items[self.persisted..].to_vec());
            self.persisted = self.items.len();
        }
    }

    fn push(&mut self, item: Item) {
        self.items.push(item.clone());
        self.emit(RunEvent::Push(item));
    }

    fn notice(&mut self, level: NoticeLevel, text: String) {
        let id = self.fresh_id();
        self.push(Item::Notice { id, level, text });
    }

    fn stall_limit(&self) -> Duration {
        Duration::from_secs(if self.spec.provider.stall_secs > 0 { self.spec.provider.stall_secs } else { 180 })
    }
}

/// The partial state of one model request across however many connections it took.
#[derive(Default)]
struct Turn {
    reasoning: String,
    text: String,
    current: Option<(ItemId, bool)>,
    tool_items: HashMap<u32, usize>,
    usage: Usage,
    finish: Option<String>,
    ended: bool,
    delivered: u32,
}

fn apply_delta(run: &mut Run, turn: &mut Turn, d: Delta<'_>) {
    match d {
        Delta::Reasoning(s) if !s.is_empty() => {
            turn.delivered += 1;
            turn.reasoning.push_str(s);
            let id = match turn.current {
                Some((id, true)) => id,
                _ => {
                    let id = run.fresh_id();
                    run.push(Item::Thinking { id, text: String::new(), signature: String::new(), tokens: 0 });
                    turn.current = Some((id, true));
                    id
                }
            };
            if let Some(Item::Thinking { text, tokens, .. }) = run.items.iter_mut().rev().find(|i| i.id() == id) {
                text.push_str(s);
                *tokens = estimate_tokens(text);
            }
            run.emit(RunEvent::AppendText { id, delta: s.to_owned() });
        }
        Delta::ReasoningSignature(sig) => {
            let id = match turn.current {
                Some((id, true)) => id,
                // Redacted reasoning, or a protocol that sends only the proof.
                _ => {
                    let id = run.fresh_id();
                    run.push(Item::Thinking { id, text: String::new(), signature: String::new(), tokens: 0 });
                    turn.current = Some((id, true));
                    id
                }
            };
            if let Some(Item::Thinking { signature, .. }) = run.items.iter_mut().rev().find(|i| i.id() == id) {
                signature.push_str(sig);
            }
        }
        Delta::Text(s) if !s.is_empty() => {
            turn.delivered += 1;
            turn.text.push_str(s);
            let id = match turn.current {
                Some((id, false)) => id,
                _ => {
                    let id = run.fresh_id();
                    run.push(Item::Text { id, text: String::new(), tokens: 0 });
                    turn.current = Some((id, false));
                    id
                }
            };
            if let Some(Item::Text { text, tokens, .. }) = run.items.iter_mut().rev().find(|i| i.id() == id) {
                text.push_str(s);
                *tokens = estimate_tokens(text);
            }
            run.emit(RunEvent::AppendText { id, delta: s.to_owned() });
        }
        Delta::ToolStart { index, call_id, name } => {
            turn.delivered += 1;
            turn.current = None;
            let id = run.fresh_id();
            let call_id = if call_id.is_empty() { format!("call_{id}") } else { call_id.to_owned() };
            let item = ToolItem { id, call_id, name: name.to_owned(), status: ToolStatus::Streaming, ..Default::default() };
            run.push(Item::Tool(item));
            turn.tool_items.insert(index, run.items.len() - 1);
        }
        Delta::ToolArgs { index, delta } => {
            turn.delivered += 1;
            if let Some(&i) = turn.tool_items.get(&index)
                && let Some(Item::Tool(t)) = run.items.get_mut(i)
            {
                t.args.push_str(delta);
                t.tokens = estimate_tokens(&t.args);
                let id = t.id;
                run.emit(RunEvent::AppendArgs { id, delta: delta.to_owned() });
            }
        }
        Delta::Usage(u) => turn.usage = u,
        Delta::Finish(r) => turn.finish = Some(r.to_owned()),
        Delta::End => turn.ended = true,
        Delta::Reasoning(_) | Delta::Text(_) => {}
    }
}

async fn stream_once(run: &mut Run, turn: &mut Turn, system: &str, tools: &[ToolSpec], body_override: Option<&[u8]>) -> Result<(), NetError> {
    let max_tokens = run.spec.model.max_output();
    let kind = run.spec.provider.kind;
    let body = match body_override {
        Some(b) => b.to_vec(),
        None => {
            let req = TurnRequest {
                model: &run.spec.model,
                system,
                items: &run.items,
                tools,
                effort: &run.spec.settings.effort,
                max_tokens,
                prefill_reasoning: &turn.reasoning,
                prefill_text: &turn.text,
            };
            provider::encode(kind, &run.spec.provider, &req).map_err(|e| NetError::new(FailKind::Protocol, e))?
        }
    };
    let limit = run.transport.config().first_byte_timeout;
    let mut stream = run.transport.post_stream(provider::path(kind), Rope::from_vec(body), limit, None).await?;
    if !stream.status.is_success() {
        let status = stream.status.as_u16();
        let body = stream.read_all(16 << 10, Duration::from_secs(10)).await.unwrap_or_default();
        return Err(NetError::new(FailKind::Http(status), error_message(&body)));
    }
    run.emit(RunEvent::Status(RunStatus::Streaming));

    let stall = run.stall_limit();
    let mut parser = SseParser::new();
    let mut decoder = provider::decoder(kind);
    let mut finish_deadline: Option<Instant> = None;
    while !turn.ended {
        if turn.finish.is_some() {
            finish_deadline.get_or_insert_with(|| Instant::now() + Duration::from_millis(1500));
        }
        let wait = finish_deadline.map_or(stall, |d| d.saturating_duration_since(Instant::now()));
        let chunk = match stream.next_chunk(wait).await {
            Ok(Some(c)) => c,
            Ok(None) => break,
            // Everything that matters is in, so whatever goes wrong now must not reopen the turn.
            Err(_) if turn.finish.is_some() => return Ok(()),
            Err(e) => return Err(e),
        };
        let mut failure: Option<NetError> = None;
        // Decoding happens on a buffer of deltas owned here, so `run` is free for `apply_delta` to use.
        let fed = parser.feed(&chunk, &mut |ev| {
            if failure.is_some() {
                return;
            }
            let outcome = match ev {
                SseEvent::Data(p) => decoder.feed(p, &mut |d| apply_delta(run, turn, d)),
                SseEvent::Error(p) => Err(NetError::new(FailKind::Server(0), String::from_utf8_lossy(p).into_owned())),
            };
            if let Err(e) = outcome {
                failure = Some(e);
            }
        });
        if let Some(e) = failure {
            return Err(e);
        }
        fed.map_err(|_| NetError::new(FailKind::Protocol, "SSE event over 16 MiB"))?;
    }
    if turn.ended || turn.finish.is_some() {
        stream.finish().await;
        Ok(())
    } else {
        Err(NetError::new(FailKind::Truncated, "the stream ended before the turn did"))
    }
}

/// The human part of an error body, when it has one.
fn error_message(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    if let Ok(v) = serde_json::from_str::<Value>(&text) {
        let m = v.get("error").and_then(|e| e.get("message").or(Some(e))).and_then(Value::as_str).or_else(|| v.get("message").and_then(Value::as_str));
        if let Some(m) = m {
            return m.to_owned();
        }
    }
    text.chars().take(600).collect()
}

/// Streams one model request to completion, reconnecting as often as the policy allows.
async fn stream_turn(run: &mut Run, turn: &mut Turn, system: &str, tools: &[ToolSpec]) -> Result<(), NetError> {
    let policy = RetryPolicy { max_attempts: 8, base: Duration::from_millis(400), cap: Duration::from_secs(20) };
    let mut backoff = Backoff::new(policy.clone(), now_ms());
    let first_id = run.next_id;
    let first_len = run.items.len();
    let mut fruitless = 0;
    let mut attempt = 0;
    loop {
        attempt += 1;
        run.emit(RunEvent::Status(RunStatus::Connecting));
        let delivered_before = turn.delivered;
        let cancel = run.handle.cancel.clone();
        let result = tokio::select! {
            r = stream_once(run, turn, system, tools, None) => r,
            _ = cancel.cancelled() => Err(NetError::new(FailKind::Cancelled, "cancelled")),
        };
        let error = match result {
            Ok(()) => return Ok(()),
            Err(e) => e,
        };
        if error.kind == FailKind::Cancelled {
            return Err(error);
        }
        // An attempt that delivered something proves the path works, so a long answer over a bad link is getting there, not failing.
        fruitless = if turn.delivered > delivered_before { 0 } else { fruitless + 1 };
        if !error.kind.retryable() || fruitless >= policy.max_attempts {
            return Err(error);
        }
        // Half a tool call cannot be resumed. With a server that continues from a prefill, text and reasoning stay and only
        // the calls are redone. Otherwise the whole attempt is taken back and starts over.
        if run.spec.provider.resume_prefill {
            if !turn.tool_items.is_empty() {
                let from = turn.tool_items.values().map(|&i| run.items[i].id()).min().unwrap_or(first_id);
                run.items.retain(|i| i.id() < from);
                turn.tool_items.clear();
                run.emit(RunEvent::Rollback { from });
            }
        } else if run.items.len() > first_len {
            run.items.truncate(first_len);
            *turn = Turn::default();
            run.emit(RunEvent::Rollback { from: first_id });
        }
        turn.finish = None;
        turn.ended = false;
        let delay = backoff.next_delay();
        run.emit(RunEvent::Status(RunStatus::Retrying { attempt, delay_ms: delay.as_millis() as u64, detail: error.to_string() }));
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = cancel.cancelled() => return Err(NetError::new(FailKind::Cancelled, "cancelled")),
        }
    }
}

/// Whether the tool may run, and if not, what to tell the model.
async fn gate(run: &Run, tool: &dyn tools::Tool, item: ItemId, args: &Value) -> Result<(), (ToolStatus, String)> {
    let name = tool.name();
    if run.spec.session.plan_mode.load(Ordering::Relaxed) && tool.blocked_in_plan_mode(args) {
        return Err((ToolStatus::Denied, "Plan mode is active, so this change was not made. Write your plan with `plan`, then call `exit_plan_mode`.".to_owned()));
    }
    if !tool.dangerous(args) {
        return Ok(());
    }
    match run.spec.settings.permission {
        PermissionMode::FullAccess => Ok(()),
        PermissionMode::ReadOnly => Err((ToolStatus::Denied, "This conversation is read-only, so this tool was refused.".to_owned())),
        PermissionMode::Ask => {
            if run.spec.session.always_allowed.lock().contains(name) {
                return Ok(());
            }
            run.emit(RunEvent::Tool { id: item, status: ToolStatus::Pending, output: None, took_ms: 0, meta: None });
            run.emit(RunEvent::Status(RunStatus::WaitingForUser));
            let decision = run.handle.ask_approval(item, name, args, tool.summary(args)).await;
            run.emit(RunEvent::Status(RunStatus::RunningTools));
            match decision {
                Some(ApprovalDecision::Allow) => Ok(()),
                Some(ApprovalDecision::AllowAlways) => {
                    run.spec.session.always_allowed.lock().insert(name.to_owned());
                    Ok(())
                }
                Some(ApprovalDecision::Deny) => Err((ToolStatus::Denied, "The user denied this tool call.".to_owned())),
                None => Err((ToolStatus::Cancelled, String::new())),
            }
        }
    }
}

async fn run_one_tool(ctx: Arc<ToolCtx>, tool: Arc<dyn tools::Tool>, args: Value) -> tools::ToolResult {
    tool.run(ctx, args).await
}

/// Runs the calls of the last turn: consecutive read-only calls together, anything else one at a time.
async fn run_tools(run: &mut Run, turn_start: usize) {
    let indices: Vec<usize> = (turn_start..run.items.len()).filter(|&i| matches!(run.items[i], Item::Tool(_))).collect();
    let mut i = 0;
    while i < indices.len() {
        if run.handle.cancel.is_cancelled() {
            for &k in &indices[i..] {
                finish_tool(run, k, ToolStatus::Cancelled, String::new(), 0, ToolMeta::default());
            }
            return;
        }
        // Gather a batch of calls that are safe together.
        let mut batch: Vec<(usize, Arc<dyn tools::Tool>, Value)> = Vec::new();
        while i < indices.len() {
            let k = indices[i];
            let Item::Tool(t) = &run.items[k] else { unreachable!() };
            let (name, raw_args, id) = (t.name.clone(), t.args.clone(), t.id);
            let args: Value = if raw_args.trim().is_empty() {
                Value::Object(Default::default())
            } else {
                match serde_json::from_str(&raw_args) {
                    Ok(v) => v,
                    Err(e) => {
                        i += 1;
                        finish_tool(run, k, ToolStatus::Error, format!("The arguments were not valid JSON: {e}"), 0, ToolMeta::default());
                        continue;
                    }
                }
            };
            let disabled = run.spec.settings.disabled_tools.contains(&name);
            let Some(tool) = tools::find(&name).filter(|_| !disabled) else {
                i += 1;
                finish_tool(run, k, ToolStatus::Error, format!("There is no tool called {name}."), 0, ToolMeta::default());
                continue;
            };
            let safe = tool.parallel_safe(&args);
            if !batch.is_empty() && !safe {
                break;
            }
            i += 1;
            match gate(run, tool.as_ref(), id, &args).await {
                Ok(()) => batch.push((k, tool, args)),
                Err((status, message)) => finish_tool(run, k, status, message, 0, ToolMeta::default()),
            }
            if !safe {
                break;
            }
        }
        if batch.is_empty() {
            continue;
        }
        let mut joins = Vec::with_capacity(batch.len());
        for (k, tool, args) in batch {
            let Item::Tool(t) = &mut run.items[k] else { unreachable!() };
            t.status = ToolStatus::Running;
            let id = t.id;
            run.handle.emit(RunEvent::Tool { id, status: ToolStatus::Running, output: None, took_ms: 0, meta: None });
            let ctx = Arc::new(ToolCtx {
                engine: run.engine.clone(),
                run: run.handle.clone(),
                session: run.spec.session.clone(),
                spec: run.spec.clone_for_tools(),
                item: id,
                cwd: run.spec.workspaces.first().cloned().unwrap_or_else(|| PathBuf::from(".")),
                workspaces: run.spec.workspaces.clone(),
            });
            let started = Instant::now();
            let h = run.engine.rt.spawn(run_one_tool(ctx, tool, args));
            joins.push((k, h, started));
        }
        for (k, h, started) in joins {
            let took = started.elapsed().as_millis() as u64;
            match h.await {
                Ok(r) => {
                    let status = if r.is_error { ToolStatus::Error } else { ToolStatus::Done };
                    let took = started.elapsed().as_millis().max(took as u128) as u64;
                    finish_tool(run, k, status, r.output, took, r.meta);
                }
                Err(e) => finish_tool(run, k, ToolStatus::Error, format!("The tool crashed: {e}"), took, ToolMeta::default()),
            }
        }
    }
}

fn finish_tool(run: &mut Run, index: usize, status: ToolStatus, output: String, took_ms: u64, meta: ToolMeta) {
    let Item::Tool(t) = &mut run.items[index] else { return };
    t.status = status;
    t.output = output.clone();
    t.took_ms = took_ms;
    t.meta = meta.clone();
    let id = t.id;
    if let Some(c) = &run.spec.child {
        c.task.tool_calls.fetch_add(1, Ordering::Relaxed);
    }
    run.emit(RunEvent::Tool { id, status, output: Some(output), took_ms, meta: Some(meta) });
}

/// Summarises everything so far into one item, so the conversation can go on past the context window.
async fn compact(run: &mut Run) -> Result<(), NetError> {
    run.emit(RunEvent::Status(RunStatus::Compacting));
    let instruction = prompt::compaction_prompt();
    let mut items = provider::request_items(&run.items).to_vec();
    items.push(Item::Host { id: run.next_id, text: instruction, at: now_ms() });
    let saved = std::mem::replace(&mut run.items, items);
    let (first_len, first_id) = (run.items.len(), run.next_id + 1);
    run.next_id += 1;
    let mut turn = Turn::default();
    let result = stream_turn(run, &mut turn, "", &[]).await;
    run.items.truncate(first_len);
    run.items = saved;
    // The attempt's partial items were streamed to the UI, so they have to come back out.
    run.emit(RunEvent::Rollback { from: first_id });
    result?;
    let id = run.fresh_id();
    let summary = format!("This conversation was compacted. Summary of everything before this point:\n\n{}", turn.text);
    run.push(Item::Compaction { id, summary, at: now_ms() });
    run.persist_new();
    Ok(())
}

impl RunSpec {
    fn clone_for_tools(&self) -> Arc<RunSpec> {
        let mut s = self.clone();
        s.items = Vec::new();
        Arc::new(s)
    }
}

fn last_text(items: &[Item]) -> String {
    items
        .iter()
        .rev()
        .take_while(|i| !matches!(i, Item::User { .. } | Item::Host { .. }))
        .find_map(|i| match i {
            Item::Text { text, .. } if !text.is_empty() => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Runs to the end. Returns how it ended and the final answer, which is a subagent's result.
async fn drive(engine: Arc<Engine>, handle: Arc<RunHandle>, spec: RunSpec) -> (RunEnd, String) {
    let transport = match engine.transport(&spec.provider, spec.api_key.as_deref()) {
        Ok(t) => t,
        Err(e) => return (RunEnd::Failed(e), String::new()),
    };
    let next_id = spec.items.iter().map(Item::id).max().unwrap_or(0) + 1;
    let items = spec.items.clone();
    let persisted = items.len();
    let mut run = Run { engine, handle, spec, transport, items, next_id, persisted };

    let is_child = run.spec.child.is_some();
    let offered = tools::offered(is_child, &run.spec.settings.disabled_tools);
    let specs = prompt::tool_specs_for(&offered, is_child, run.spec.workspaces.len());
    let global_memory = run.engine.store.dir().join("memory");

    let mut last_context = 0u64;
    let window = run.spec.model.context_window();
    for step in 1..=MAX_STEPS {
        for note in run.spec.session.take_notes() {
            let id = run.fresh_id();
            run.push(Item::Host { id, text: note, at: now_ms() });
        }
        if let Some(c) = &run.spec.child {
            let mail: Vec<String> = std::mem::take(&mut *c.task.mailbox.lock());
            for text in mail {
                let id = run.fresh_id();
                run.push(Item::Host { id, text, at: now_ms() });
            }
        }
        if last_context as f64 > window as f64 * COMPACT_AT {
            match compact(&mut run).await {
                Ok(()) => {}
                Err(e) if e.kind == FailKind::Cancelled => return (RunEnd::Cancelled, last_text(&run.items)),
                Err(e) => run.notice(NoticeLevel::Warning, format!("压缩上下文失败，继续用完整上下文：{e}")),
            }
        }
        let plan_mode = run.spec.session.plan_mode.load(Ordering::Relaxed);
        let system = prompt::system_prompt(&prompt::PromptInput {
            workspaces: &run.spec.workspaces,
            plan_mode,
            offered: &offered,
            instructions: &run.spec.settings.instructions,
            global_instructions: &run.spec.global_instructions,
            is_child,
            global_memory: Some(&global_memory),
        });
        let turn_start = run.items.len();
        let mut turn = Turn::default();
        // The last step has to end in an answer, and a model with no tools left can only give one.
        let tools_now: &[ToolSpec] = if step == MAX_STEPS { &[] } else { &specs };
        let result = stream_turn(&mut run, &mut turn, &system, tools_now).await;
        if let Err(e) = result {
            let cancelled = e.kind == FailKind::Cancelled;
            // What the user has seen stays. Half-streamed calls are marked, so the next request still pairs them up.
            for item in &mut run.items[turn_start..] {
                if let Item::Tool(t) = item
                    && !t.status.finished()
                {
                    t.status = ToolStatus::Cancelled;
                }
            }
            run.persist_new();
            let text = last_text(&run.items);
            return if cancelled { (RunEnd::Cancelled, text) } else { (RunEnd::Failed(e.to_string()), text) };
        }
        let id = run.fresh_id();
        let finish = turn.finish.take().unwrap_or_else(|| "stop".to_owned());
        last_context = turn.usage.context();
        run.emit(RunEvent::Usage(turn.usage));
        if let Some(c) = &run.spec.child {
            c.task.tokens.fetch_add(turn.usage.output + turn.usage.input + turn.usage.cache_read + turn.usage.cache_write, Ordering::Relaxed);
        }
        let model = run.spec.model.id.clone();
        let tool_count = run.items[turn_start..].iter().filter(|i| matches!(i, Item::Tool(_))).count();
        run.push(Item::Turn { id, model, usage: turn.usage, finish, at: now_ms() });
        run.persist_new();
        if tool_count == 0 {
            return (RunEnd::Completed, last_text(&run.items));
        }
        run.emit(RunEvent::Status(RunStatus::RunningTools));
        run_tools(&mut run, turn_start).await;
        // Later lines win on load, so the finished state of each tool simply goes after the started one.
        let finished: Vec<Item> = run.items[turn_start..].iter().filter(|i| matches!(i, Item::Tool(_))).cloned().collect();
        run.engine.store.append(&run.spec.store_key, finished);
        if run.handle.cancel.is_cancelled() {
            return (RunEnd::Cancelled, last_text(&run.items));
        }
    }
    (RunEnd::Failed("达到单次运行的步数上限".to_owned()), last_text(&run.items))
}

/// Starts a subagent for a tool call and registers it on the session.
pub fn spawn_child(ctx: &ToolCtx, name: String, label: String, prompt_text: String) -> Arc<Task> {
    let task = Task::new(name, label.clone(), TaskKind::Agent);
    let store_key = format!("{}/{}", ctx.spec.conv_id, task.id);
    let mut spec = (*ctx.spec).clone();
    spec.store_key = store_key;
    spec.items = vec![Item::User { id: 1, text: prompt_text, images: Vec::new(), at: now_ms() }];
    spec.child = Some(ChildInfo { task: task.clone() });
    ctx.engine.store.append(&spec.store_key, spec.items.clone());
    let handle = ctx.engine.start(spec, label);
    // Stopping the task stops the run, and stopping the parent stops both.
    let (parent_cancel, task_cancel, run_cancel) = (ctx.run.cancel.clone(), task.cancel.clone(), handle.cancel.clone());
    ctx.engine.rt.spawn(async move {
        tokio::select! {
            _ = parent_cancel.cancelled() => {}
            _ = task_cancel.cancelled() => {}
        }
        run_cancel.cancel();
    });
    *task.run.lock() = Some(handle.clone());
    ctx.session.add_task(task.clone());
    ctx.run.emit(RunEvent::Child(handle));
    task
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_bodies_yield_their_message() {
        assert_eq!(error_message(br#"{"error":{"message":"bad key","type":"auth"}}"#), "bad key");
        assert_eq!(error_message(br#"{"message":"nope"}"#), "nope");
        assert_eq!(error_message(b"plain"), "plain");
    }
}
