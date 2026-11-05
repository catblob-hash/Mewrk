//! The agent loop: stream a turn, survive whatever the network does to it, run tools, repeat.
//!
//! The key property is that a broken connection is invisible to whoever consumes the text. When a stream
//! dies, the text received so far goes back to the server as an assistant prefill, and the model
//! continues from exactly that point. Thanks to the server's prompt cache that costs a prompt evaluation,
//! not a regeneration.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

use crate::error::{Backoff, FailKind, NetError, RetryPolicy};
use crate::sse::{SseEvent, SseParser};
use crate::tools::ToolRegistry;
use crate::transport::{ConnTiming, Transport};
use crate::wire::{build_chat_request, ChatParams, Chunk, Message, Part, Prefill, Role, Timings, ToolCall, Usage};

pub type SessionId = u32;

/// Everything observable about a run. Text is borrowed from the network buffer: a sink that only counts
/// tokens costs nothing, and one that keeps them decides for itself how to store them.
#[derive(Debug)]
pub enum Event<'a> {
    Queued,
    Attempt { step: u32, attempt: u32, resumed_bytes: usize },
    Connected(ConnTiming),
    Headers { ttfb: Duration },
    Progress { processed: u32, total: u32 },
    Reasoning(&'a str),
    Text(&'a str),
    ToolStart { index: u32, id: &'a str, name: &'a str },
    ToolArgs { index: u32, delta: &'a str },
    /// A partial tool call was dropped because the stream died in the middle of it.
    ToolRewind,
    ToolResult { id: &'a str, name: &'a str, output: &'a str, is_error: bool, took: Duration },
    Retry { attempt: u32, kind: FailKind, detail: &'a str, delay: Duration },
    TurnEnd { finish: &'a str, usage: Option<Usage>, timings: Option<Timings> },
}

pub trait Sink: Send + Sync {
    fn event(&self, id: SessionId, event: Event<'_>);
}

/// Cooperative cancellation shared with whoever started the run.
#[derive(Clone, Default)]
pub struct Cancel(Arc<CancelInner>);

#[derive(Default)]
struct CancelInner {
    flag: AtomicBool,
    notify: Notify,
}

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.flag.store(true, Ordering::SeqCst);
        self.0.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.flag.load(Ordering::SeqCst)
    }

    pub async fn cancelled(&self) {
        loop {
            let notified = self.0.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
            if self.is_cancelled() {
                return;
            }
        }
    }
}

#[derive(Clone)]
pub struct RuntimeConfig {
    /// The server's slot count. Going beyond it only moves the queue from here to there.
    pub max_concurrency: usize,
    /// Requests with media allowed before their first token. Encoding is serial on the server and stops
    /// the decode loop for every other stream while it runs.
    pub max_media_inflight: usize,
    pub stall_timeout: Duration,
    pub retry: RetryPolicy,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            max_concurrency: 128,
            // Measured with a 70/20/10 text, image and video mix at 128 streams. Against no limit, 16 cuts the median
            // time to first token from 3.7 s to 1.5 s on a clean link, and costs 5 s of 22 s in total at 10% loss. A limit
            // of 8 does better when clean, but doubles the total at 10% loss, as the gate is held during uploads.
            max_media_inflight: 16,
            // Against 20 s this about doubled text throughput at burst loss and 20% loss, and cost a little at 5%: a reconnect
            // plus prefill is far cheaper than waiting out a connection deep in exponential backoff.
            stall_timeout: Duration::from_secs(8),
            retry: RetryPolicy::default(),
        }
    }
}

pub struct Runtime {
    pub transport: Arc<Transport>,
    pub config: RuntimeConfig,
    admission: Arc<Semaphore>,
    media_gate: Arc<Semaphore>,
    /// Estimated encoder time of media requests that have not produced a first token yet.
    media_load_ms: AtomicU64,
}

impl Runtime {
    pub fn new(transport: Arc<Transport>, config: RuntimeConfig) -> Arc<Self> {
        Arc::new(Self {
            admission: Arc::new(Semaphore::new(config.max_concurrency)),
            media_gate: Arc::new(Semaphore::new(config.max_media_inflight.max(1))),
            media_load_ms: AtomicU64::new(0),
            transport,
            config,
        })
    }

    /// How long a stream may stay silent before it counts as dead.
    ///
    /// Silence has two very different causes. A dead link gets better when you reconnect. A busy server gets
    /// worse: encoding is serial and blocks the decode loop for every stream, so abandoning a request that is
    /// merely waiting re-queues the work on top of work the server is still doing. A fixed 8 s limit turned
    /// 128 concurrent images into 1321 attempts and a 135 s time to first token on a perfect network. So the
    /// limit grows with the encoding load this client knows about, and doubles every time a retry fails
    /// without making progress, which makes retries converge even under load that nobody here can see.
    pub fn stall_limit(&self, patience: u32) -> Duration {
        let base = self.config.stall_timeout.saturating_mul(1 << patience.min(6));
        let load = Duration::from_millis(self.media_load_ms.load(Ordering::Relaxed)).saturating_mul(3) / 2;
        (base + load).min(self.transport.config().first_byte_timeout)
    }
}

#[derive(Clone)]
pub struct AgentConfig {
    pub model: Arc<str>,
    pub max_tokens: u32,
    pub max_steps: u32,
    pub temperature: Option<f32>,
    pub seed: Option<u64>,
    pub thinking: Option<bool>,
    pub tools: ToolRegistry,
    pub tool_timeout: Duration,
}

impl AgentConfig {
    pub fn new(model: impl Into<Arc<str>>) -> Self {
        Self {
            model: model.into(),
            max_tokens: 2048,
            max_steps: 8,
            temperature: None,
            seed: None,
            thinking: None,
            tools: ToolRegistry::new(),
            tool_timeout: Duration::from_secs(30),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct RunSummary {
    pub steps: u32,
    pub attempts: u32,
    pub resumes: u32,
    pub tokens: u32,
    pub usage_completion: u32,
    pub finish: String,
    pub text: String,
    pub reasoning: String,
    pub tool_calls: u32,
    pub fail_counts: Vec<(FailKind, u32)>,
}

impl RunSummary {
    fn count_fail(&mut self, kind: FailKind) {
        match self.fail_counts.iter_mut().find(|(k, _)| *k == kind) {
            Some((_, n)) => *n += 1,
            None => self.fail_counts.push((kind, 1)),
        }
    }
}

#[derive(Default)]
struct ToolBuilder {
    index: u32,
    id: String,
    name: String,
    args: String,
}

/// What one assistant turn has produced so far, across however many connections that took.
#[derive(Default)]
struct TurnState {
    reasoning: String,
    text: String,
    tools: Vec<ToolBuilder>,
    finish: Option<String>,
    usage: Option<Usage>,
    timings: Option<Timings>,
    tokens: u32,
    /// The part of `tokens` spent on tool call fragments, refunded if those get thrown away.
    tool_tokens: u32,
    done_marker: bool,
}

/// Decodes one SSE `data:` payload into state and events.
fn apply_chunk(state: &mut TurnState, id: SessionId, payload: &[u8], sink: &dyn Sink) -> Result<(), NetError> {
    if payload == b"[DONE]" {
        state.done_marker = true;
        return Ok(());
    }
    let chunk: Chunk = serde_json::from_slice(payload).map_err(|e| NetError::new(FailKind::Protocol, format!("bad chunk: {e}")))?;
    if let Some(e) = chunk.error {
        let code = e.code.unwrap_or(0) as u16;
        return Err(NetError::new(FailKind::Server(code), e.message.as_deref().unwrap_or("server error")));
    }
    if let Some(p) = chunk.prompt_progress {
        sink.event(id, Event::Progress { processed: p.processed, total: p.total });
    }
    if chunk.usage.is_some() {
        state.usage = chunk.usage;
    }
    if chunk.timings.is_some() {
        state.timings = chunk.timings;
    }
    let Some(choice) = chunk.choices else { return Ok(()) };
    let delta = choice.delta;
    if let Some(r) = delta.reasoning_content.as_deref().filter(|s| !s.is_empty()) {
        state.reasoning.push_str(r);
        state.tokens += 1;
        sink.event(id, Event::Reasoning(r));
    }
    if let Some(t) = delta.content.as_deref().filter(|s| !s.is_empty()) {
        state.text.push_str(t);
        state.tokens += 1;
        sink.event(id, Event::Text(t));
    }
    if let Some(calls) = delta.tool_calls {
        state.tokens += 1;
        state.tool_tokens += 1;
        for call in calls {
            let position = match state.tools.iter().position(|t| t.index == call.index) {
                Some(p) => p,
                None => {
                    state.tools.push(ToolBuilder { index: call.index, ..Default::default() });
                    state.tools.len() - 1
                }
            };
            let builder = &mut state.tools[position];
            if let Some(id) = call.id {
                builder.id.push_str(&id);
            }
            if let Some(f) = call.function {
                if let Some(name) = f.name {
                    builder.name.push_str(&name);
                    sink.event(id, Event::ToolStart { index: call.index, id: &builder.id, name: &builder.name });
                }
                if let Some(args) = f.arguments.as_deref().filter(|s| !s.is_empty()) {
                    builder.args.push_str(args);
                    sink.event(id, Event::ToolArgs { index: call.index, delta: args });
                }
            }
        }
    }
    if let Some(reason) = choice.finish_reason {
        state.finish = Some(reason.into_owned());
    }
    Ok(())
}

/// Holds the media gate and counts towards the encoding load until the first token, because that is when the
/// encoder has finished with this request.
struct MediaPermit<'a> {
    rt: &'a Runtime,
    permit: Option<OwnedSemaphorePermit>,
    load_ms: u64,
}

impl<'a> MediaPermit<'a> {
    fn none(rt: &'a Runtime) -> Self {
        Self { rt, permit: None, load_ms: 0 }
    }

    fn holding(rt: &'a Runtime, permit: OwnedSemaphorePermit, load_ms: u64) -> Self {
        rt.media_load_ms.fetch_add(load_ms, Ordering::Relaxed);
        Self { rt, permit: Some(permit), load_ms }
    }

    fn release(&mut self) {
        self.permit.take();
        self.rt.media_load_ms.fetch_sub(std::mem::take(&mut self.load_ms), Ordering::Relaxed);
    }
}

impl Drop for MediaPermit<'_> {
    fn drop(&mut self) {
        self.release();
    }
}

const AFTER_FINISH_GRACE: Duration = Duration::from_millis(1500);

/// How far a server may overrun the budget. Tokens and deltas do not line up exactly, so a little is normal.
const BUDGET_SLACK: u32 = 64;
/// A hard stop for one turn, whatever the server claims to be doing.
const MAX_TURN_BYTES: usize = 64 << 20;

/// One attempt at the stream. Returns once the turn is complete, or on the first failure.
#[allow(clippy::too_many_arguments)]
async fn stream_once(
    rt: &Arc<Runtime>,
    id: SessionId,
    params: &ChatParams,
    messages: &[Message],
    state: &mut TurnState,
    media_permit: &mut MediaPermit<'_>,
    patience: u32,
    retry_after: Option<Instant>,
    sink: &dyn Sink,
) -> Result<(), NetError> {
    let prefill = Prefill { reasoning: &state.reasoning, text: &state.text };
    let budget = params.max_tokens.saturating_sub(state.tokens);
    let rope = build_chat_request(params, messages, prefill, budget);
    // Headers come as soon as the server has the request, so the wait is the usual patience plus the time for whatever
    // is still draining from the socket buffer. The rate is pessimistic on purpose: uploads at 20% loss were measured at
    // 4 to 12 KiB/s, and giving up too early throws away the whole upload.
    let draining = Duration::from_secs_f64(rope.len.min(4 << 20) as f64 / (6.0 * 1024.0));
    let header_limit = (rt.stall_limit(patience) + draining).min(rt.transport.config().first_byte_timeout);
    let mut stream = rt.transport.post_stream("/chat/completions", rope, header_limit, retry_after).await?;
    sink.event(id, Event::Connected(stream.timing));
    sink.event(id, Event::Headers { ttfb: stream.ttfb });

    if !stream.status.is_success() {
        let status = stream.status.as_u16();
        let body = stream.read_all(16 << 10, Duration::from_secs(10)).await.unwrap_or_default();
        return Err(NetError::new(FailKind::Http(status), String::from_utf8_lossy(&body).into_owned()));
    }

    let outcome = read_events(rt, id, params, state, &mut stream, media_permit, patience, sink).await;
    match outcome {
        Ok(()) => {
            stream.finish().await;
            Ok(())
        }
        // Once the finish reason is in, the answer is complete and only bookkeeping is still on its way. Whatever
        // goes wrong with that must not reopen the turn and ask the model for more text that nobody wants.
        Err(_) if state.finish.is_some() => Ok(()),
        Err(e) => Err(e),
    }
}

#[allow(clippy::too_many_arguments)]
async fn read_events(
    rt: &Arc<Runtime>,
    id: SessionId,
    params: &ChatParams,
    state: &mut TurnState,
    stream: &mut crate::transport::OpenStream,
    media_permit: &mut MediaPermit<'_>,
    patience: u32,
    sink: &dyn Sink,
) -> Result<(), NetError> {
    let mut parser = SseParser::new();
    let mut first_delta_seen = false;
    let mut finish_deadline = None;
    loop {
        // After the finish reason the wait is measured from that moment. Keep-alive comments from the server must
        // not be able to extend it for ever.
        if state.finish.is_some() {
            finish_deadline.get_or_insert_with(|| Instant::now() + AFTER_FINISH_GRACE);
        }
        let limit = match finish_deadline {
            Some(deadline) => deadline.saturating_duration_since(Instant::now()),
            None => rt.stall_limit(patience),
        };
        let Some(bytes) = stream.next_chunk(limit).await? else { break };
        let before = state.tokens;
        let mut failure = None;
        let fed = parser.feed(&bytes, &mut |event| {
            if failure.is_some() {
                return;
            }
            let outcome = match event {
                SseEvent::Data(payload) => apply_chunk(state, id, payload, sink),
                SseEvent::Error(payload) => Err(server_error(payload)),
            };
            if let Err(e) = outcome {
                failure = Some(e);
            }
        });
        if let Some(e) = failure {
            return Err(e);
        }
        fed.map_err(|_| NetError::new(FailKind::Protocol, "SSE event over 16 MiB"))?;
        if !first_delta_seen && state.tokens > before {
            first_delta_seen = true;
            media_permit.release();
        }
        if state.done_marker {
            break;
        }
        // A server that ignores max_tokens must not be able to fill memory.
        if state.tokens > params.max_tokens.saturating_add(BUDGET_SLACK) || state.text.len() + state.reasoning.len() > MAX_TURN_BYTES {
            state.finish.get_or_insert_with(|| "length".to_owned());
            return Ok(());
        }
    }
    if state.finish.is_none() {
        return Err(NetError::new(FailKind::Truncated, "stream ended without a finish reason"));
    }
    Ok(())
}

fn server_error(payload: &[u8]) -> NetError {
    #[derive(serde::Deserialize)]
    struct Wrapped<'a> {
        #[serde(borrow)]
        error: crate::wire::ApiError<'a>,
    }
    if let Ok(w) = serde_json::from_slice::<Wrapped>(payload) {
        return NetError::new(FailKind::Server(w.error.code.unwrap_or(0) as u16), w.error.message.as_deref().unwrap_or("server error"));
    }
    if let Ok(e) = serde_json::from_slice::<crate::wire::ApiError>(payload) {
        return NetError::new(FailKind::Server(e.code.unwrap_or(0) as u16), e.message.as_deref().unwrap_or("server error"));
    }
    NetError::new(FailKind::Server(0), String::from_utf8_lossy(payload).into_owned())
}

struct TurnContext<'a> {
    rt: &'a Arc<Runtime>,
    id: SessionId,
    step: u32,
    params: &'a ChatParams,
    messages: &'a [Message],
    media_ms: u64,
    sink: &'a dyn Sink,
    cancel: &'a Cancel,
}

/// Streams one assistant turn to completion, reconnecting and resuming as often as the policy allows.
async fn stream_turn(ctx: &TurnContext<'_>, state: &mut TurnState, summary: &mut RunSummary) -> Result<(), NetError> {
    let TurnContext { rt, id, step, params, messages, media_ms, sink, cancel } = *ctx;
    let mut backoff = Backoff::new(rt.config.retry.clone(), ((id as u64) << 16) | step as u64);
    let mut attempt = 0;
    // Attempts in a row that produced nothing. An attempt that delivered text proves that the path works, so a long
    // answer over a link that keeps dropping is not "failing": it is getting there.
    let mut fruitless = 0;
    let mut failed_at = None;

    loop {
        attempt += 1;
        summary.attempts += 1;

        // The media gate comes first, so that a request waiting for it does not sit on a slot that a text
        // request could be using.
        let mut media_permit = MediaPermit::none(rt);
        if media_ms > 0 {
            let gate = tokio::select! {
                p = rt.media_gate.clone().acquire_owned() => p.expect("semaphore is never closed"),
                _ = cancel.cancelled() => return Err(NetError::new(FailKind::Cancelled, "cancelled")),
            };
            media_permit = MediaPermit::holding(rt, gate, media_ms);
        }
        let permit = tokio::select! {
            p = rt.admission.clone().acquire_owned() => p.expect("semaphore is never closed"),
            _ = cancel.cancelled() => return Err(NetError::new(FailKind::Cancelled, "cancelled")),
        };

        let resumed_bytes = state.reasoning.len() + state.text.len();
        if resumed_bytes > 0 {
            summary.resumes += 1;
        }
        sink.event(id, Event::Attempt { step, attempt, resumed_bytes });

        let tokens_before = state.tokens;
        let result = tokio::select! {
            r = stream_once(rt, id, params, messages, state, &mut media_permit, fruitless, failed_at, sink) => r,
            _ = cancel.cancelled() => Err(NetError::new(FailKind::Cancelled, "cancelled")),
        };
        // A slot is not ours while backing off: someone else can use it in the meantime.
        drop(media_permit);
        drop(permit);

        let error = match result {
            Ok(()) => return Ok(()),
            Err(e) => e,
        };
        failed_at = Some(Instant::now());
        summary.count_fail(error.kind);
        // Half a tool call cannot be prefilled. Text and reasoning can, so only the call is redone. It has to be taken
        // back before judging progress: fragments of a call that gets thrown away got the answer no closer.
        if !state.tools.is_empty() {
            state.tools.clear();
            state.tokens -= std::mem::take(&mut state.tool_tokens);
            sink.event(id, Event::ToolRewind);
        }
        // Silence again means asking for more patience, not sending it faster.
        fruitless = if state.tokens > tokens_before { 0 } else { fruitless + 1 };
        if !error.kind.retryable() || fruitless >= rt.config.retry.max_attempts {
            return Err(error);
        }

        state.finish = None;
        state.done_marker = false;
        // If the budget is used up then there is nothing to resume, and the turn ended where it would have.
        if state.tokens >= params.max_tokens {
            state.finish = Some("length".into());
            return Ok(());
        }

        let delay = backoff.next_delay();
        sink.event(id, Event::Retry { attempt, kind: error.kind, detail: &error.detail, delay });
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = cancel.cancelled() => return Err(NetError::new(FailKind::Cancelled, "cancelled")),
        }
    }
}

/// Runs the agent on `conversation` until the model stops calling tools, appending to it as it goes.
#[allow(clippy::result_large_err)] // Failure is the slow path here, and what went right so far is worth returning.
pub async fn run_agent(
    rt: &Arc<Runtime>,
    cfg: &AgentConfig,
    id: SessionId,
    conversation: &mut Vec<Message>,
    sink: &dyn Sink,
    cancel: &Cancel,
) -> Result<RunSummary, (NetError, RunSummary)> {
    let mut summary = RunSummary::default();
    let mut params = ChatParams {
        model: cfg.model.clone(),
        max_tokens: cfg.max_tokens,
        temperature: cfg.temperature,
        seed: cfg.seed,
        thinking: cfg.thinking,
        return_progress: true,
        tools_json: cfg.tools.tools_json(),
    };
    sink.event(id, Event::Queued);

    let last_step = cfg.max_steps.max(1);
    for step in 1..=last_step {
        summary.steps = step;
        // The last step has to end in an answer, and a model with no tools left can only give one. Otherwise a run
        // could spend all its steps on calls and end with nothing to show for them.
        if step == last_step {
            params.tools_json = None;
        }
        let media_ms = conversation.iter().map(Message::media_estimate_ms).sum();
        let ctx = TurnContext { rt, id, step, params: &params, messages: conversation.as_slice(), media_ms, sink, cancel };
        let mut state = TurnState::default();
        if let Err(e) = stream_turn(&ctx, &mut state, &mut summary).await {
            // What the user has already seen stays part of the conversation, or the next question would be asked as
            // if none of it had ever been said.
            summary.tokens += state.tokens;
            if !state.text.is_empty() {
                conversation.push(Message::text(Role::Assistant, state.text.as_str()));
                summary.text = state.text;
            }
            return Err((e, summary));
        }

        let TurnState { reasoning, text, tools, finish, usage, timings, tokens, .. } = state;
        let finish = finish.unwrap_or_else(|| "stop".into());
        sink.event(id, Event::TurnEnd { finish: &finish, usage, timings });
        summary.tokens += tokens;
        summary.usage_completion += usage.map_or(0, |u| u.completion_tokens);
        summary.finish = finish;
        if !reasoning.is_empty() {
            summary.reasoning.push_str(&reasoning);
        }

        let calls: Vec<ToolCall> = tools
            .into_iter()
            .filter(|t| !t.name.is_empty())
            .map(|t| {
                // Some servers leave the id out. The pairing with the result must still be unambiguous.
                let id: Arc<str> = if t.id.is_empty() { format!("call_{step}_{}", t.index).into() } else { t.id.into() };
                ToolCall { id, name: t.name.into(), arguments: t.args.into() }
            })
            .collect();

        conversation.push(Message {
            role: Role::Assistant,
            parts: if text.is_empty() { Vec::new() } else { vec![Part::Text(text.as_str().into())] },
            tool_calls: calls.clone(),
            tool_call_id: None,
        });
        summary.text = text;

        if calls.is_empty() {
            return Ok(summary);
        }
        summary.tool_calls += calls.len() as u32;

        // Tools only ever run on a complete, finished turn, so a retry can never run one twice.
        let mut set = tokio::task::JoinSet::new();
        for (position, call) in calls.iter().enumerate() {
            let tool = cfg.tools.get(&call.name);
            let (name, args) = (call.name.clone(), call.arguments.clone());
            let timeout = cfg.tool_timeout;
            set.spawn(async move {
                let started = Instant::now();
                let outcome = match tool {
                    None => Err(format!("unknown tool {name}")),
                    Some(tool) => match tokio::time::timeout(timeout, tool.call(&args)).await {
                        Ok(r) => r,
                        Err(_) => Err(format!("{name} timed out after {timeout:?}")),
                    },
                };
                (position, outcome, started.elapsed())
            });
        }
        let mut results: Vec<Option<(Result<String, String>, Duration)>> = (0..calls.len()).map(|_| None).collect();
        loop {
            let next = tokio::select! {
                n = set.join_next() => n,
                _ = cancel.cancelled() => {
                    // Every tool call needs a result, or the conversation is invalid for whatever comes next.
                    for call in &calls {
                        conversation.push(Message {
                            role: Role::Tool,
                            parts: vec![Part::Text("error: cancelled".into())],
                            tool_calls: Vec::new(),
                            tool_call_id: Some(call.id.clone()),
                        });
                    }
                    return Err((NetError::new(FailKind::Cancelled, "cancelled"), summary));
                }
            };
            let Some(joined) = next else { break };
            if let Ok((position, outcome, took)) = joined {
                results[position] = Some((outcome, took));
            }
        }
        // Results go back in call order, whichever tool finished first.
        for (call, result) in calls.iter().zip(results) {
            let (outcome, took) = result.unwrap_or_else(|| (Err("tool panicked".to_owned()), Duration::ZERO));
            let is_error = outcome.is_err();
            let output = outcome.unwrap_or_else(|e| format!("error: {e}"));
            sink.event(id, Event::ToolResult { id: &call.id, name: &call.name, output: &output, is_error, took });
            conversation.push(Message {
                role: Role::Tool,
                parts: vec![Part::Text(output.into())],
                tool_calls: Vec::new(),
                tool_call_id: Some(call.id.clone()),
            });
        }
    }
    Err((NetError::new(FailKind::Protocol, format!("no final answer after {} steps", cfg.max_steps)), summary))
}
