//! Step translation (`StepTranslator`).
//!
//! Raw Anthropic stream events (the SDK's `stream_event` messages) are translated to host events the same way `main.ts`
//! translates the AI SDK stream. A step is the one API message the CLI makes for this round; it ends either when that
//! message stops at `tool_use` (the handlers park) or when the CLI's `result` arrives for a final reply.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::oneshot;

use crate::error::StepError;
use crate::json::{Json, Obj};
use crate::protocol::{MAX_STREAM_TEXT, MAX_TOOL_ARGUMENTS};
use crate::service::Shared;
use crate::step::js_len;
use crate::{jarr, jobj};

use super::log;
use super::native::NativeLedger;
use super::shapes::strip_tool_prefix;

/// Tokens as the host's usage carries them; a JavaScript number, and `undefined` where absent.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct Usage {
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub total: Option<f64>,
    pub reasoning: Option<f64>,
    pub cache_read: Option<f64>,
}

impl Usage {
    pub fn to_json(self) -> Json {
        jobj! {
            "inputTokens"?: self.input,
            "outputTokens"?: self.output,
            "totalTokens"?: self.total,
            "reasoningTokens"?: self.reasoning,
            "cacheReadTokens"?: self.cache_read,
        }
    }
}

fn number(value: Option<&Json>) -> Option<f64> {
    value.and_then(Json::as_f64).filter(|n| n.is_finite())
}

/// `usageOf`: Anthropic `usage` object to host usage, with cache reads and writes counted as input.
pub(super) fn usage_of(usage: &Json) -> Usage {
    let input = number(usage.get("input_tokens"));
    let cache_write = number(usage.get("cache_creation_input_tokens"));
    let cache_read = number(usage.get("cache_read_input_tokens"));
    let output = number(usage.get("output_tokens"));
    let reasoning = number(usage.get("output_tokens_details").filter(|d| d.is_obj()).and_then(|d| d.get("thinking_tokens")));
    let input_total = (input.is_some() || cache_write.is_some() || cache_read.is_some())
        .then(|| input.unwrap_or(0.0) + cache_write.unwrap_or(0.0) + cache_read.unwrap_or(0.0));
    Usage {
        input: input_total,
        output,
        total: (input_total.is_some() || output.is_some()).then(|| input_total.unwrap_or(0.0) + output.unwrap_or(0.0)),
        reasoning,
        cache_read,
    }
}

/// `usageOfModels`: a `result` message's per-model totals to host usage, on the same terms as `usage_of`.
pub(super) fn usage_of_models(models: Option<&Json>) -> Option<Usage> {
    let entries = models?.as_obj()?;
    if entries.is_empty() {
        return None;
    }
    let (mut input, mut output, mut cache_read) = (0.0, 0.0, 0.0);
    let mut reasoning: Option<f64> = None;
    for (_, model) in entries.iter() {
        let read = number(model.get("cacheReadInputTokens")).unwrap_or(0.0);
        input += number(model.get("inputTokens")).unwrap_or(0.0) + read + number(model.get("cacheCreationInputTokens")).unwrap_or(0.0);
        output += number(model.get("outputTokens")).unwrap_or(0.0);
        cache_read += read;
        if let Some(thinking) = number(model.get("thinkingTokens")) {
            reasoning = Some(reasoning.unwrap_or(0.0) + thinking);
        }
    }
    Some(Usage { input: Some(input), output: Some(output), total: Some(input + output), reasoning, cache_read: Some(cache_read) })
}

/// `mergeUsage`: `message_delta` usage restates counters; later values win, absent ones keep the earlier.
pub(super) fn merge_usage(base: Usage, update: Usage) -> Usage {
    let input = update.input.or(base.input);
    let output = update.output.or(base.output);
    Usage {
        input,
        output,
        total: (input.is_some() || output.is_some()).then(|| input.unwrap_or(0.0) + output.unwrap_or(0.0)),
        reasoning: update.reasoning.or(base.reasoning),
        cache_read: update.cache_read.or(base.cache_read),
    }
}

/// `addUsage`: a step spanning several API messages reports the total.
pub(super) fn add_usage(base: Usage, next: Usage) -> Usage {
    let sum = |left: Option<f64>, right: Option<f64>| (left.is_some() || right.is_some()).then(|| left.unwrap_or(0.0) + right.unwrap_or(0.0));
    Usage {
        input: sum(base.input, next.input),
        output: sum(base.output, next.output),
        total: sum(base.total, next.total),
        reasoning: sum(base.reasoning, next.reasoning),
        cache_read: sum(base.cache_read, next.cache_read),
    }
}

/// `stopReasonOf`.
fn stop_reason_of(stop_reason: Option<&str>) -> &'static str {
    match stop_reason {
        Some("end_turn") | Some("stop_sequence") | None => "stop",
        Some("max_tokens") => "length",
        Some("tool_use") => "tool-calls",
        Some(_) => "other",
    }
}

struct ThinkingBlock {
    text: String,
    signature: String,
    ordinal: usize,
    /// Tokens this block thought without streaming them as text: the API's `estimated_tokens` progress while the
    /// thinking itself is omitted, floored at what the signature's size implies once it is complete.
    hidden_tokens: f64,
    opened_at: Instant,
}

struct ToolUseBlock {
    id: String,
    name: String,
    json: String,
    /// JavaScript length of `json`.
    json_units: usize,
    input: Option<Json>,
    parsed: bool,
    /// Whether the host publishes this tool; other names belong to the CLI's own loop.
    known: bool,
}

enum Block {
    Thinking(ThinkingBlock),
    Redacted { data: String },
    Text { text: String },
    ToolUse(ToolUseBlock),
}

/// The reasoning item a run of consecutive `thinking` blocks shares.
///
/// Under the CLI's default `display: "updates"` a model answers with a thinking block whose text is omitted, directly
/// followed by a second thinking block whose signature the server tagged `narration`: a plain-text summary of the step.
/// The pair is one item, one card whose body is the summary and whose duration covers both. Each block still keeps its
/// own signed part, in order, because both must reach the API again exactly as they came. The item opens with the run's
/// first block rather than its first text: the omitted thinking is most of the step's wall time and streams no text.
struct ThinkingRun {
    ordinal: usize,
    text: String,
    /// `hidden_tokens` of the run's finished blocks.
    hidden_tokens: f64,
}

/// How a step ended, for the task awaiting it.
#[derive(Debug)]
pub(super) enum StepOutcome {
    Done(Json),
    Failed(StepError),
}

pub(super) fn permanent(message: impl Into<String>) -> StepError {
    StepError { kind: "permanent", message: message.into(), status: None, retry_after_ms: None }
}

pub(super) fn cancelled() -> StepError {
    StepError { kind: "cancelled", message: "请求已取消".to_owned(), status: None, retry_after_ms: None }
}

pub(super) struct StepTranslator {
    id: String,
    shared: Arc<Shared>,
    known_tools: HashSet<String>,
    /// Set on a native-search or native-fetch session; its report rides in the result.
    native: Option<Arc<Mutex<NativeLedger>>>,
    text: String,
    /// JavaScript length of `text`: the host's limit is on UTF-16 units.
    text_units: usize,
    reasoning: Vec<String>,
    reasoning_ms: u64,
    reasoning_seen: bool,
    calls: Vec<Json>,
    call_ids: Vec<String>,
    parts: Vec<Json>,
    /// In `Map` order: the first time an index was set.
    blocks: Vec<(f64, Block)>,
    /// Open while the latest block of the message is a thinking block.
    thinking_run: Option<ThinkingRun>,
    /// Index of the thinking block still streaming, if the latest block is one.
    open_thinking: Option<f64>,
    usage: Usage,
    message_usage: Usage,
    model: Option<String>,
    stop_reason: Option<String>,
    message_stopped: bool,
    settled: bool,
    finish: Option<oneshot::Sender<StepOutcome>>,
}

impl StepTranslator {
    pub fn new(id: &str, shared: Arc<Shared>, known_tools: HashSet<String>, native: Option<Arc<Mutex<NativeLedger>>>) -> (Self, oneshot::Receiver<StepOutcome>) {
        let (finish, outcome) = oneshot::channel();
        let translator = Self {
            id: id.to_owned(),
            shared,
            known_tools,
            native,
            text: String::new(),
            text_units: 0,
            reasoning: Vec::new(),
            reasoning_ms: 0,
            reasoning_seen: false,
            calls: Vec::new(),
            call_ids: Vec::new(),
            parts: Vec::new(),
            blocks: Vec::new(),
            thinking_run: None,
            open_thinking: None,
            usage: Usage::default(),
            message_usage: Usage::default(),
            model: None,
            stop_reason: None,
            message_stopped: false,
            settled: false,
            finish: Some(finish),
        };
        (translator, outcome)
    }

    /// Tool_use ids of this step's reported calls.
    pub fn call_ids(&self) -> &[String] {
        &self.call_ids
    }

    pub fn is_settled(&self) -> bool {
        self.settled
    }

    fn emit(&self, event: Json) {
        self.shared.emit(&self.id, event);
    }

    pub fn fail(&mut self, error: StepError) {
        if self.settled {
            return;
        }
        self.settled = true;
        if let Some(finish) = self.finish.take() {
            let _ = finish.send(StepOutcome::Failed(error));
        }
    }

    fn block_at(&self, index: f64) -> Option<usize> {
        self.blocks.iter().position(|(at, _)| *at == index)
    }

    /// `Map.set`: an index seen before keeps its place.
    fn set_block(&mut self, index: f64, block: Block) {
        match self.block_at(index) {
            Some(at) => self.blocks[at].1 = block,
            None => self.blocks.push((index, block)),
        }
    }

    /// Supplies a tool input the stream could not parse, from the CLI's own call.
    pub fn supply_tool_input(&mut self, tool_use_id: &str, input: Option<&Json>) {
        for at in 0..self.blocks.len() {
            let Block::ToolUse(block) = &mut self.blocks[at].1 else { continue };
            if block.id == tool_use_id && !block.parsed {
                block.input = Some(input.filter(|value| value.is_obj()).cloned().unwrap_or_else(Json::obj));
                block.parsed = true;
                self.emit_tool_call(at);
            }
        }
        self.maybe_settle_tool_round();
    }

    /// One message off the CLI. `Err` is what the TypeScript threw out of `handle`.
    pub fn handle(&mut self, message: &Json) -> Result<(), String> {
        if self.settled {
            return Ok(());
        }
        let top_level = message.get("parent_tool_use_id").is_some_and(Json::is_null);
        match message.str_at("type") {
            Some("stream_event") if top_level => {
                if let Some(event) = message.get("event") {
                    self.handle_event(event)?;
                }
            }
            Some("assistant") if top_level => {
                let content = message.path(&["message", "content"]);
                let blocks = content.and_then(Json::as_arr).map_or(&[][..], Vec::as_slice);
                for block in blocks {
                    if block.str_at("type") == Some("tool_use") {
                        if let Some(id) = block.str_at("id") {
                            self.supply_tool_input(id, block.get("input"));
                        }
                    }
                }
                if let Some(error) = message.get("error").filter(|error| is_truthy(error)) {
                    let text: String = blocks
                        .iter()
                        .filter(|block| block.str_at("type") == Some("text"))
                        .map(|block| match block.get("text") {
                            None | Some(Json::Null) => String::new(),
                            Some(Json::Str(text)) => text.clone(),
                            Some(other) => other.to_text(),
                        })
                        .collect();
                    let shown = error.as_str().map_or_else(|| error.to_text(), str::to_owned);
                    self.fail(permanent(if text.is_empty() { format!("Claude Code 报告错误：{shown}") } else { text }));
                }
            }
            Some("user") if top_level => {
                if let Some(native) = &self.native {
                    native.lock().unwrap_or_else(|p| p.into_inner()).observe(message);
                }
            }
            Some("result") => {
                let subtype = message.str_at("subtype");
                let is_error = message.get("is_error").is_some_and(is_truthy);
                if is_error || subtype != Some("success") {
                    let detail = if subtype == Some("success") {
                        message.str_at("result").unwrap_or("").to_owned()
                    } else {
                        let mut parts: Vec<String> = vec![subtype.map_or_else(|| "undefined".to_owned(), str::to_owned)];
                        for error in message.get("errors").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice) {
                            parts.push(error.as_str().map_or_else(|| error.to_text(), str::to_owned));
                        }
                        parts.join(": ")
                    };
                    self.fail(permanent(if detail.is_empty() { "Claude Code 回合失败".to_owned() } else { detail }));
                    return Ok(());
                }
                // The CLI's own search request, like the model call that renders a fetched page, streams nothing here,
                // so only the turn's per-model totals count it; a native session is one fresh turn, which makes those
                // totals exactly this step's.
                if self.native.is_some() {
                    if let Some(total) = usage_of_models(message.get("modelUsage")) {
                        self.usage = total;
                        self.emit(jobj! { "k": "usage", "usage": self.usage.to_json() });
                    }
                }
                self.settle(false);
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_event(&mut self, event: &Json) -> Result<(), String> {
        let index = event.get("index").and_then(Json::as_f64);
        match event.str_at("type") {
            Some("message_start") => {
                let empty = Json::obj();
                let message = event.get("message").filter(|value| value.is_obj()).unwrap_or(&empty);
                if let Some(model) = message.str_at("model") {
                    self.model = Some(model.to_owned());
                }
                // A native step spans the turn's messages; a search step's text is the host's findings, read as one
                // report, so each message's text starts a paragraph. (A fetch step's text is not read at all.)
                if self.native.is_some() && !self.text.is_empty() && !self.text.ends_with("\n\n") {
                    self.text.push_str("\n\n");
                    self.text_units += 2;
                }
                self.close_thinking_run();
                self.blocks.clear();
                self.message_stopped = false;
                self.stop_reason = None;
                self.message_usage = usage_of(message.get("usage").filter(|usage| usage.is_obj()).unwrap_or(&empty));
            }
            Some("content_block_start") => {
                let index = index.unwrap_or(self.blocks.len() as f64);
                let empty = Json::obj();
                let block = event.get("content_block").filter(|block| block.is_obj()).unwrap_or(&empty);
                self.start_block(index, block);
            }
            Some("content_block_delta") => {
                let empty = Json::obj();
                let delta = event.get("delta").filter(|delta| delta.is_obj()).unwrap_or(&empty);
                self.apply_delta(index.unwrap_or(-1.0), delta)?;
            }
            Some("content_block_stop") => self.stop_block(index.unwrap_or(-1.0)),
            Some("message_delta") => {
                if let Some(stop_reason) = event.path(&["delta", "stop_reason"]).and_then(Json::as_str) {
                    self.stop_reason = Some(stop_reason.to_owned());
                }
                if let Some(usage) = event.get("usage").filter(|usage| usage.is_obj()) {
                    self.message_usage = merge_usage(self.message_usage, usage_of(usage));
                }
            }
            Some("message_stop") => {
                self.close_thinking_run();
                self.message_stopped = true;
                self.usage = add_usage(self.usage, self.message_usage);
                self.emit(jobj! { "k": "usage", "usage": self.usage.to_json() });
                self.maybe_settle_tool_round();
            }
            _ => {}
        }
        Ok(())
    }

    fn start_block(&mut self, index: f64, block: &Json) {
        self.open_thinking = None;
        let kind = block.str_at("type");
        if kind != Some("thinking") {
            self.close_thinking_run();
        }
        match kind {
            Some("text") => self.set_block(index, Block::Text { text: String::new() }),
            Some("thinking") => {
                if self.thinking_run.is_none() {
                    let ordinal = self.open_reasoning(None);
                    self.thinking_run = Some(ThinkingRun { ordinal, text: String::new(), hidden_tokens: 0.0 });
                }
                let ordinal = self.thinking_run.as_ref().map_or(0, |run| run.ordinal);
                self.set_block(
                    index,
                    Block::Thinking(ThinkingBlock {
                        text: String::new(),
                        signature: block.str_at("signature").unwrap_or("").to_owned(),
                        ordinal,
                        hidden_tokens: 0.0,
                        opened_at: Instant::now(),
                    }),
                );
                self.open_thinking = Some(index);
            }
            Some("redacted_thinking") => {
                // A whole encrypted item: its start is the only evidence, so the card opens and closes at once.
                let Some(data) = block.str_at("data").filter(|data| !data.is_empty()) else { return };
                let ordinal = self.open_reasoning(Some("encrypted"));
                self.set_block(index, Block::Redacted { data: data.to_owned() });
                self.emit(jobj! { "k": "reasoning-done", "item": ordinal, "durationMs": self.reasoning_ms });
            }
            Some("tool_use") => {
                let id = block.str_at("id").unwrap_or("").to_owned();
                let name = strip_tool_prefix(block.str_at("name").unwrap_or("")).to_owned();
                let known = self.known_tools.contains(&name);
                if known {
                    self.emit(jobj! { "k": "tool-call-announced", "callId": id.as_str(), "toolName": name.as_str() });
                } else {
                    log(&format!("模型调用了宿主未发布的工具 {name}（{id}），交由 Claude Code 自行回绝"));
                }
                self.set_block(index, Block::ToolUse(ToolUseBlock { id, name, json: String::new(), json_units: 0, input: None, parsed: false, known }));
            }
            _ => {}
        }
    }

    fn open_reasoning(&mut self, form: Option<&str>) -> usize {
        let ordinal = self.reasoning.len();
        self.reasoning_seen = true;
        self.reasoning.push(String::new());
        self.emit(jobj! { "k": "reasoning-start", "item": ordinal, "form"?: form });
        ordinal
    }

    /// Ends the current thinking run, closing its item.
    fn close_thinking_run(&mut self) {
        let run = self.thinking_run.take();
        self.open_thinking = None;
        if let Some(run) = run {
            self.emit(jobj! { "k": "reasoning-done", "item": run.ordinal, "durationMs": self.reasoning_ms });
        }
    }

    /// Reports how much the open run has thought that its text does not show. Text the run streams is left out: the
    /// host can count that itself, and counting it here too would make every delta a second frame.
    fn emit_thinking_progress(&self) {
        let Some(run) = &self.thinking_run else { return };
        let open = self.open_thinking.and_then(|index| self.block_at(index)).map_or(0.0, |at| match &self.blocks[at].1 {
            Block::Thinking(block) => block.hidden_tokens,
            _ => 0.0,
        });
        self.emit(jobj! { "k": "reasoning-progress", "item": run.ordinal, "estimatedTokens": run.hidden_tokens + open });
    }

    fn apply_delta(&mut self, index: f64, delta: &Json) -> Result<(), String> {
        let Some(at) = self.block_at(index) else { return Ok(()) };
        match delta.str_at("type") {
            Some("text_delta") => {
                let Some(piece) = delta.str_at("text") else { return Ok(()) };
                let Block::Text { text } = &mut self.blocks[at].1 else { return Ok(()) };
                let units = js_len(piece);
                if self.text_units + units > MAX_STREAM_TEXT {
                    return Err(format!("单轮可见文本超过 {MAX_STREAM_TEXT} 字节上限"));
                }
                text.push_str(piece);
                self.text.push_str(piece);
                self.text_units += units;
                self.emit(jobj! { "k": "text-delta", "delta": piece });
            }
            Some("thinking_delta") => {
                let Block::Thinking(block) = &mut self.blocks[at].1 else { return Ok(()) };
                // Omitted thinking streams progress instead of text: an increment of the tokens thought so far, which is
                // all there is to show until it ends. (The SDK also digests these into `system`/`thinking_tokens`
                // messages, interleaved with the raw events; reading both would count twice.)
                if let Some(estimated) = delta.get("estimated_tokens").and_then(Json::as_f64).filter(|n| *n > 0.0) {
                    block.hidden_tokens += estimated;
                    self.emit_thinking_progress();
                }
                let Some(piece) = delta.str_at("thinking").filter(|piece| !piece.is_empty()) else { return Ok(()) };
                if let Some(run) = &mut self.thinking_run {
                    run.text.push_str(piece);
                }
                let Block::Thinking(block) = &mut self.blocks[at].1 else { return Ok(()) };
                block.text.push_str(piece);
                let ordinal = block.ordinal;
                self.emit(jobj! { "k": "reasoning-delta", "item": ordinal, "delta": piece });
            }
            Some("signature_delta") => {
                if let (Block::Thinking(block), Some(signature)) = (&mut self.blocks[at].1, delta.str_at("signature")) {
                    block.signature.push_str(signature);
                }
            }
            Some("input_json_delta") => {
                let Some(piece) = delta.str_at("partial_json") else { return Ok(()) };
                let Block::ToolUse(block) = &mut self.blocks[at].1 else { return Ok(()) };
                let units = js_len(piece);
                if block.json_units + units > MAX_TOOL_ARGUMENTS {
                    return Err(format!("工具 {} 的参数超过 {MAX_TOOL_ARGUMENTS} 字节上限", block.name));
                }
                block.json.push_str(piece);
                block.json_units += units;
            }
            _ => {}
        }
        Ok(())
    }

    fn stop_block(&mut self, index: f64) {
        let Some(at) = self.block_at(index) else { return };
        match &mut self.blocks[at].1 {
            Block::Text { text } => {
                if !text.is_empty() {
                    self.parts.push(jobj! { "type": "text", "text": text.as_str() });
                }
            }
            Block::Thinking(block) => {
                // The item stays open until the run ends: a narration block may follow.
                self.reasoning_ms += block.opened_at.elapsed().as_millis() as u64;
                self.reasoning[block.ordinal] = self.thinking_run.as_ref().map_or_else(|| block.text.clone(), |run| run.text.clone());
                if self.open_thinking == Some(index) {
                    self.open_thinking = None;
                }
                if block.text.is_empty() {
                    // The signature carries the omitted thinking, so its size bounds how much there was: the same floor
                    // Claude Code puts under its estimate.
                    let signature_bytes = (js_len(&block.signature) as f64 * 0.75 + 0.5).floor();
                    block.hidden_tokens = block.hidden_tokens.max((signature_bytes / 4.0).ceil());
                }
                let hidden = block.hidden_tokens;
                let part = if !block.signature.is_empty() {
                    Some(jobj! {
                        "type": "reasoning",
                        "text": block.text.as_str(),
                        "providerOptions": jobj! { "anthropic": jobj! { "signature": block.signature.as_str() } },
                    })
                } else if !block.text.is_empty() {
                    Some(jobj! { "type": "reasoning", "text": block.text.as_str() })
                } else {
                    None
                };
                if let Some(run) = self.thinking_run.as_mut().filter(|_| hidden > 0.0) {
                    run.hidden_tokens += hidden;
                    self.emit_thinking_progress();
                }
                if let Some(part) = part {
                    self.parts.push(part);
                }
            }
            Block::Redacted { data } => {
                let part = jobj! {
                    "type": "reasoning",
                    "text": "",
                    "providerOptions": jobj! { "anthropic": jobj! { "redactedData": data.as_str() } },
                };
                self.parts.push(part);
            }
            Block::ToolUse(block) => {
                let json = super::native::js_trim(&block.json);
                if json.is_empty() {
                    block.input = Some(Json::obj());
                    block.parsed = true;
                } else if let Ok(parsed) = Json::parse(json) {
                    block.input = Some(if parsed.is_obj() { parsed } else { Json::obj() });
                    block.parsed = true;
                } else {
                    // The CLI's own `assistant` message or its tools/call carries the full input.
                    block.parsed = false;
                }
                if block.parsed {
                    self.emit_tool_call(at);
                }
            }
        }
    }

    fn emit_tool_call(&mut self, at: usize) {
        let Block::ToolUse(block) = &self.blocks[at].1 else { return };
        if !block.known || self.call_ids.contains(&block.id) {
            return;
        }
        let input = block.input.clone().unwrap_or(Json::Null);
        let (id, name) = (block.id.clone(), block.name.clone());
        self.calls.push(jobj! { "callId": id.as_str(), "toolName": name.as_str(), "input": input.clone() });
        self.parts.push(jobj! { "type": "tool-call", "toolCallId": id.as_str(), "toolName": name.as_str(), "input": input.clone() });
        self.emit(jobj! { "k": "tool-call", "callId": id.as_str(), "input": input });
        self.call_ids.push(id);
    }

    /// Ends the step once the message is complete and every known call has its input.
    ///
    /// The stop reason is deliberately not consulted: the CLI invokes the handler of every complete `tool_use` block even
    /// when the message was cut off by `max_tokens`, so waiting for `tool_use` there would park the handler against a
    /// host round that never starts. The raw stop reason still travels in the result.
    fn maybe_settle_tool_round(&mut self) {
        if !self.message_stopped || self.settled {
            return;
        }
        let tools = || self.blocks.iter().filter_map(|(_, block)| if let Block::ToolUse(tool) = block { Some(tool) } else { None });
        if tools().any(|tool| tool.known && !tool.parsed) {
            return;
        }
        // A message whose only calls target tools the host does not publish is the CLI's own business: it answers them
        // itself and keeps streaming into this step.
        if !tools().any(|tool| tool.known) {
            return;
        }
        self.settle(true);
    }

    fn settle(&mut self, tool_calls: bool) {
        if self.settled {
            return;
        }
        self.close_thinking_run();
        self.settled = true;
        let mut result = Obj::new();
        result.insert("text", self.text.as_str());
        result.insert("reasoning", Json::Arr(self.reasoning.iter().map(Json::from).collect()));
        if self.reasoning_seen {
            result.insert("reasoningMs", self.reasoning_ms);
        }
        result.insert("calls", Json::Arr(self.calls.clone()));
        result.insert("usage", self.usage.to_json());
        if let Some(model) = &self.model {
            result.insert("model", model.as_str());
        }
        result.insert("finishReason", if tool_calls { "tool-calls" } else { stop_reason_of(self.stop_reason.as_deref()) });
        if let Some(stop_reason) = self.stop_reason.as_deref().filter(|reason| !reason.is_empty()) {
            result.insert("rawFinishReason", stop_reason);
        }
        let response_messages = if self.parts.is_empty() {
            jarr![]
        } else {
            jarr![jobj! { "role": "assistant", "content": self.parts.clone() }]
        };
        result.insert("responseMessages", response_messages);
        result.insert("sources", jarr![]);
        if let Some(native) = &self.native {
            for (key, value) in native.lock().unwrap_or_else(|p| p.into_inner()).report() {
                result.insert(key, value);
            }
        }
        if let Some(finish) = self.finish.take() {
            let _ = finish.send(StepOutcome::Done(Json::Obj(result)));
        }
    }
}

fn is_truthy(value: &Json) -> bool {
    match value {
        Json::Null => false,
        Json::Bool(flag) => *flag,
        Json::Num(number) => number.as_f64() != 0.0 && !number.as_f64().is_nan(),
        Json::Str(text) => !text.is_empty(),
        Json::Arr(_) | Json::Obj(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(text: &str) -> Usage {
        usage_of(&Json::parse(text).unwrap())
    }

    #[test]
    fn usage_counts_cache_as_input() {
        let start = usage(r#"{"input_tokens":123,"output_tokens":1,"cache_creation_input_tokens":7,"cache_read_input_tokens":20}"#);
        assert_eq!(start.to_json().to_text(), r#"{"inputTokens":150,"outputTokens":1,"totalTokens":151,"cacheReadTokens":20}"#);
        let merged = merge_usage(start, usage(r#"{"output_tokens":42}"#));
        assert_eq!(merged.to_json().to_text(), r#"{"inputTokens":150,"outputTokens":42,"totalTokens":192,"cacheReadTokens":20}"#);
        let summed = add_usage(merged, merged);
        assert_eq!(summed.to_json().to_text(), r#"{"inputTokens":300,"outputTokens":84,"totalTokens":384,"cacheReadTokens":40}"#);
        assert_eq!(usage("{}").to_json().to_text(), "{}");
        assert_eq!(
            usage(r#"{"output_tokens":5,"output_tokens_details":{"thinking_tokens":3}}"#).to_json().to_text(),
            r#"{"outputTokens":5,"totalTokens":5,"reasoningTokens":3}"#
        );
    }

    #[test]
    fn per_model_totals_read_on_the_same_terms() {
        let models = Json::parse(
            r#"{"a":{"inputTokens":10,"outputTokens":2,"cacheReadInputTokens":3,"cacheCreationInputTokens":4},
                "b":{"inputTokens":1,"outputTokens":1,"thinkingTokens":9}}"#,
        )
        .unwrap();
        assert_eq!(
            usage_of_models(Some(&models)).unwrap().to_json().to_text(),
            r#"{"inputTokens":18,"outputTokens":3,"totalTokens":21,"reasoningTokens":9,"cacheReadTokens":3}"#
        );
        assert_eq!(usage_of_models(Some(&Json::obj())), None);
        assert_eq!(usage_of_models(None), None);
    }

    #[test]
    fn stop_reasons_map() {
        assert_eq!(stop_reason_of(None), "stop");
        assert_eq!(stop_reason_of(Some("end_turn")), "stop");
        assert_eq!(stop_reason_of(Some("max_tokens")), "length");
        assert_eq!(stop_reason_of(Some("tool_use")), "tool-calls");
        assert_eq!(stop_reason_of(Some("pause_turn")), "other");
    }
}
