//! Dialect differences for Anthropic-compatible endpoints (`anthropic-dialect.ts`).
//!
//! Its behaviors are copied from Claude Code, the reference client for the Messages protocol:
//!
//! - Request, inner layer: the interleaved-thinking beta, prompt-cache breakpoints (the system prompt split at
//!   the host's dynamic boundary, plus the last message whose tail can carry one) and the 400 self-heal chain,
//!   each repair remembered per endpoint and model.
//! - Request, outer layer: adaptive thinking rewritten to an explicit budget on endpoints that are not
//!   Anthropic itself.
//! - Response: a non-streamed Message answering a streaming request is replayed as SSE, and the line rewriter
//!   makes the stream as tolerant as Claude Code's assembler.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

use crate::error::AiError;
use crate::http::{Body, BoxFuture, DynFetch, Fetch, Headers, HttpRequest, HttpResponse, url_hostname, url_without_hash};
use crate::jobj;
use crate::json::{Json, Obj};
use crate::reasoning::js_round;
use crate::sse::{LineRewrite, Rewrite, SseDialectFetch};

fn is_official(base_url: Option<&str>) -> bool {
    match base_url {
        None => true,
        Some(url) if url.is_empty() => true,
        // An unparseable URL counts as official: every accommodation trades correctness on official
        // endpoints for compatibility on relays and must not be applied blindly.
        Some(url) => url_hostname(url).is_none_or(|host| host == "api.anthropic.com"),
    }
}

// ------------------------------------------------------------ Replayed reasoning

/// Provider-options key under which the host tags a replayed reasoning part with the model that made it.
const REPLAY_TAG_KEY: &str = "mewrk";

/// Placeholder Claude Code inserts when a repair empties an assistant message.
const NO_CONTENT_PLACEHOLDER: &str = "(no content)";

/// The model a signature binds to: the id without Claude Code's `[1m]` budget suffix.
fn signing_model(model_id: &str) -> &str {
    let lower = model_id.to_ascii_lowercase();
    if lower.ends_with("[1m]") { &model_id[..model_id.len() - 4] } else { model_id }
}

fn replay_model_of(part: &Json) -> Option<&str> {
    part.path(&["providerOptions", REPLAY_TAG_KEY])?.str_at("model")
}

fn retain_reasoning(messages: &mut [Json], keep: &dyn Fn(&Json) -> bool) {
    for message in messages {
        if message.str_at("role") != Some("assistant") {
            continue;
        }
        let Some(content) = message.get_mut("content").and_then(Json::as_arr_mut) else { continue };
        let before = content.len();
        content.retain(|part| part.str_at("type") != Some("reasoning") || keep(part));
        if content.len() != before && content.is_empty() {
            content.push(jobj! { "type": "text", "text": NO_CONTENT_PLACEHOLDER });
        }
    }
}

/// Drop replayed reasoning parts another model signed: Anthropic binds a signature to its model and rejects
/// it anywhere else. An emptied assistant message keeps a placeholder so the turn structure survives.
pub fn drop_foreign_signed_reasoning(messages: &mut [Json], model_id: &str) {
    let signer = signing_model(model_id).to_owned();
    retain_reasoning(messages, &|part| replay_model_of(part).is_none_or(|model| signing_model(model) == signer));
}

/// Remove the host's replay tag from every part; no provider may see a key it does not own.
pub fn strip_replay_tags(messages: &mut [Json]) {
    for message in messages {
        let Some(content) = message.get_mut("content").and_then(Json::as_arr_mut) else { continue };
        for part in content {
            let Some(object) = part.as_obj_mut() else { continue };
            let Some(Json::Obj(options)) = object.get_mut("providerOptions") else { continue };
            if !options.contains_key(REPLAY_TAG_KEY) {
                continue;
            }
            options.remove(REPLAY_TAG_KEY);
            if options.is_empty() {
                object.remove("providerOptions");
            }
        }
    }
}

/// Only original Anthropic replay payloads can authorize thinking replay; neither a relay URL nor a matching
/// model tag proves unsigned text is replayable. The sentinel older sidecars persisted goes too.
pub fn drop_unsigned_reasoning(messages: &mut [Json]) {
    retain_reasoning(messages, &|part| {
        let Some(anthropic) = part.path(&["providerOptions", "anthropic"]).filter(|a| a.is_obj()) else {
            return false;
        };
        anthropic.str_at("signature").is_some_and(|s| !s.is_empty() && s != "mewrk-unsigned")
            || anthropic.str_at("redactedData").is_some_and(|s| !s.is_empty())
    });
}

// ------------------------------------------------------------ Response tolerance

const ERROR_MARKER: &str = "_tool_result_error";

fn is_tool_result_error(value: &Json) -> bool {
    value.str_at("type").is_some_and(|t| t.ends_with("_tool_result_error"))
}

/// Restores `content: [error]` to `content: error`.
fn unwrap_tool_result_error(frame: &mut Json) -> bool {
    let Some(block) = frame.get_mut("content_block") else { return false };
    let Some(Json::Arr(content)) = block.get("content") else { return false };
    if content.len() != 1 || !is_tool_result_error(&content[0]) {
        return false;
    }
    let only = content[0].clone();
    if let Some(object) = block.as_obj_mut() {
        object.insert("content", only);
    }
    true
}

const MODELED_BLOCK_TYPES: [&str; 16] = [
    "text",
    "thinking",
    "redacted_thinking",
    "tool_use",
    "server_tool_use",
    "mcp_tool_use",
    "mcp_tool_result",
    "compaction",
    "web_search_tool_result",
    "web_fetch_tool_result",
    "code_execution_tool_result",
    "bash_code_execution_tool_result",
    "text_editor_code_execution_tool_result",
    "tool_search_tool_result",
    "advisor_tool_result",
    "fallback",
];

const MODELED_EVENT_TYPES: [&str; 8] =
    ["message_start", "content_block_start", "content_block_delta", "content_block_stop", "message_delta", "message_stop", "ping", "error"];

fn ensure_number(target: &mut Obj, key: &str) -> bool {
    if target.get(key).is_some_and(Json::is_num) {
        return false;
    }
    target.insert(key, 0u64);
    true
}

fn ensure_string(target: &mut Obj, key: &str) -> bool {
    if target.get(key).is_some_and(Json::is_str) {
        return false;
    }
    target.insert(key, "");
    true
}

#[derive(PartialEq)]
enum Tolerated {
    Drop,
    Changed,
    Same,
}

fn changed(flag: bool) -> Tolerated {
    if flag { Tolerated::Changed } else { Tolerated::Same }
}

/// Make one stream frame acceptable to the SDK's schema, the way Claude Code's assembler accepts it.
fn tolerate_frame(frame: &mut Json) -> Tolerated {
    let Some(kind) = frame.str_at("type").filter(|t| MODELED_EVENT_TYPES.contains(t)).map(str::to_owned) else {
        return Tolerated::Drop;
    };
    let Some(object) = frame.as_obj_mut() else { return Tolerated::Drop };
    match kind.as_str() {
        "message_start" => {
            let Some(Json::Obj(message)) = object.get_mut("message") else { return Tolerated::Drop };
            let mut did = false;
            match message.get_mut("usage") {
                Some(Json::Obj(usage)) => did |= ensure_number(usage, "input_tokens"),
                _ => {
                    message.insert("usage", jobj! { "input_tokens": 0u64 });
                    did = true;
                }
            }
            // Only deferred `tool_use` blocks are modeled here; anything else a relay pre-populates is
            // announced again by its own `content_block_start`.
            if let Some(Json::Arr(content)) = message.get("content") {
                if content.iter().any(|block| !block.is_obj() || block.str_at("type") != Some("tool_use")) {
                    message.insert("content", Vec::<Json>::new());
                    did = true;
                }
            }
            changed(did)
        }
        "content_block_start" => {
            if !object.get("index").is_some_and(Json::is_num) {
                return Tolerated::Drop;
            }
            let Some(Json::Obj(block)) = object.get_mut("content_block") else { return Tolerated::Drop };
            let block_type = block.get("type").and_then(Json::as_str).map(str::to_owned);
            let Some(block_type) = block_type.filter(|t| MODELED_BLOCK_TYPES.contains(&t.as_str())) else {
                // Claude Code keeps unknown blocks and never reads them; an empty text block keeps the
                // index alive without producing output.
                object.insert("content_block", jobj! { "type": "text", "text": "" });
                return Tolerated::Changed;
            };
            match block_type.as_str() {
                "text" => changed(ensure_string(block, "text")),
                "thinking" => changed(ensure_string(block, "thinking")),
                "redacted_thinking" => {
                    if block.get("data").is_some_and(Json::is_str) {
                        return Tolerated::Same;
                    }
                    object.insert("content_block", jobj! { "type": "text", "text": "" });
                    Tolerated::Changed
                }
                _ => Tolerated::Same,
            }
        }
        "content_block_delta" => {
            if !object.get("index").is_some_and(Json::is_num) {
                return Tolerated::Drop;
            }
            let Some(delta) = object.get("delta").filter(|d| d.is_obj()) else { return Tolerated::Drop };
            let field = match delta.str_at("type") {
                Some("text_delta") => "text",
                Some("thinking_delta") => "thinking",
                Some("signature_delta") => "signature",
                Some("input_json_delta") => "partial_json",
                Some("compaction_delta") => return Tolerated::Same,
                // Claude Code ignores `citations_delta` and every unknown delta type.
                _ => return Tolerated::Drop,
            };
            if delta.get(field).is_some_and(Json::is_str) { Tolerated::Same } else { Tolerated::Drop }
        }
        "content_block_stop" => {
            if object.get("index").is_some_and(Json::is_num) { Tolerated::Same } else { Tolerated::Drop }
        }
        "message_delta" => {
            let mut did = false;
            if !object.get("delta").is_some_and(Json::is_obj) {
                object.insert("delta", Json::obj());
                did = true;
            }
            match object.get_mut("usage") {
                Some(Json::Obj(usage)) => did |= ensure_number(usage, "output_tokens"),
                _ => {
                    object.insert("usage", jobj! { "output_tokens": 0u64 });
                    did = true;
                }
            }
            changed(did)
        }
        "error" => {
            let Some(Json::Obj(error)) = object.get_mut("error") else { return Tolerated::Drop };
            let a = ensure_string(error, "type");
            let b = ensure_string(error, "message");
            changed(a || b)
        }
        _ => Tolerated::Same,
    }
}

/// Normalizes one SSE line: `None` deletes it, `Some(None)` leaves its bytes alone.
fn normalize_sse_line(line: &str) -> Option<Option<(Json, String)>> {
    if !line.starts_with("data:") {
        return Some(None);
    }
    let payload = line["data:".len()..].trim();
    if payload.is_empty() || payload == "[DONE]" {
        return Some(None);
    }
    // Non-JSON is outside this wrapper's scope; the SDK validates protocol errors.
    let Ok(mut frame) = Json::parse(payload) else { return Some(None) };
    if !frame.is_obj() {
        return Some(None);
    }
    let tolerated = tolerate_frame(&mut frame);
    if tolerated == Tolerated::Drop {
        return None;
    }
    let mut did = tolerated == Tolerated::Changed;
    if line.contains(ERROR_MARKER) && unwrap_tool_result_error(&mut frame) {
        did = true;
    }
    let text = if did { format!("data: {}", frame.to_text()) } else { line.to_owned() };
    Some(Some((frame, text)))
}

#[derive(Default)]
struct ThinkingState {
    signature: String,
    has_signature_delta: bool,
    text: String,
    has_thinking_delta: bool,
}

/// The per-response rewriter. `on_usage` receives `message_start`'s usage so the cache-coverage detector can
/// read the endpoint's own billing counters.
fn make_line_rewriter(on_usage: Arc<dyn Fn(&Json) + Send + Sync>) -> LineRewrite {
    let mut thinking: HashMap<u64, ThinkingState> = HashMap::new();
    Box::new(move |line: &str| {
        let Some(normalized) = normalize_sse_line(line) else { return Rewrite::Drop };
        let Some((frame, text)) = normalized else { return Rewrite::Keep };
        let same = text.len() == line.len() && text == line;
        let keep = |text: String| if same { Rewrite::Keep } else { Rewrite::Replace(text) };
        let kind = frame.str_at("type").unwrap_or("");
        if kind == "message_stop" || kind == "message_start" {
            thinking.clear();
        }
        if kind == "message_start" {
            if let Some(message) = frame.get("message").filter(|m| m.is_obj()) {
                on_usage(message.get("usage").unwrap_or(&Json::Null));
            }
        }
        let Some(index) = frame.get("index").and_then(Json::as_u64) else { return keep(text) };
        if kind == "content_block_start" {
            thinking.remove(&index);
            if let Some(block) = frame.get("content_block").filter(|b| b.str_at("type") == Some("thinking")) {
                thinking.insert(
                    index,
                    ThinkingState {
                        signature: block.str_at("signature").unwrap_or("").to_owned(),
                        has_signature_delta: false,
                        text: block.str_at("thinking").unwrap_or("").to_owned(),
                        has_thinking_delta: false,
                    },
                );
            }
        }
        let Some(state) = thinking.get_mut(&index) else { return keep(text) };
        if kind == "content_block_delta" {
            if let Some(delta) = frame.get("delta").filter(|d| d.is_obj()) {
                if delta.str_at("type") == Some("signature_delta") && delta.str_at("signature").is_some_and(|s| !s.is_empty()) {
                    state.has_signature_delta = true;
                }
                if delta.str_at("type") == Some("thinking_delta") && delta.get("thinking").is_some_and(Json::is_str) {
                    state.has_thinking_delta = true;
                }
            }
        }
        if kind == "content_block_stop" {
            let state = thinking.remove(&index).unwrap_or_default();
            let mut deltas: Vec<Json> = Vec::new();
            if !state.text.is_empty() && !state.has_thinking_delta {
                deltas.push(jobj! { "type": "thinking_delta", "thinking": state.text });
            }
            if !state.signature.is_empty() && !state.has_signature_delta {
                deltas.push(jobj! { "type": "signature_delta", "signature": state.signature });
            }
            if !deltas.is_empty() {
                // Close any upstream event-name line before emitting complete synthetic events.
                let mut out = String::from("\n");
                for delta in deltas {
                    out.push_str("event: content_block_delta\ndata: ");
                    out.push_str(&jobj! { "type": "content_block_delta", "index": index, "delta": delta }.to_text());
                    out.push_str("\n\n");
                }
                out.push_str("event: content_block_stop\n");
                out.push_str(&text);
                return Rewrite::Replace(out);
            }
        }
        keep(text)
    })
}

// ------------------------------------------------------------ Non-streamed Message

/// Events a streaming endpoint would have sent for one complete Message body.
fn message_as_sse_frames(message: &Json) -> Option<String> {
    let content = message.get("content")?.as_arr()?;
    // A body with no terminal reason is truncated or malformed; replaying it as a clean stream would
    // dress that up as success.
    let stop_reason = message.str_at("stop_reason")?;
    let usage = message.get("usage").filter(|u| u.is_obj()).and_then(Json::as_obj).cloned().unwrap_or_default();
    let mut start_usage = usage.clone();
    start_usage.insert("input_tokens", usage.get("input_tokens").filter(|v| v.is_num()).cloned().unwrap_or(Json::from(0u64)));
    let mut events: Vec<Json> = vec![jobj! {
        "type": "message_start",
        "message": jobj! {
            "id"?: message.get("id").cloned(),
            "type": "message",
            "role": "assistant",
            "model"?: message.get("model").cloned(),
            "content": Vec::<Json>::new(),
            "stop_reason": Json::Null,
            "stop_sequence": Json::Null,
            "usage": start_usage,
        },
    }];
    for (index, block) in content.iter().enumerate() {
        if !block.is_obj() {
            continue;
        }
        let start = |block: Json| jobj! { "type": "content_block_start", "index": index, "content_block": block };
        let delta = |delta: Json| jobj! { "type": "content_block_delta", "index": index, "delta": delta };
        match block.str_at("type").unwrap_or("") {
            "text" => {
                events.push(start(jobj! { "type": "text", "text": "" }));
                if let Some(text) = block.str_at("text").filter(|t| !t.is_empty()) {
                    events.push(delta(jobj! { "type": "text_delta", "text": text }));
                }
            }
            "thinking" => {
                events.push(start(jobj! { "type": "thinking", "thinking": "" }));
                if let Some(text) = block.str_at("thinking").filter(|t| !t.is_empty()) {
                    events.push(delta(jobj! { "type": "thinking_delta", "thinking": text }));
                }
                if let Some(signature) = block.str_at("signature").filter(|t| !t.is_empty()) {
                    events.push(delta(jobj! { "type": "signature_delta", "signature": signature }));
                }
            }
            "tool_use" | "server_tool_use" => {
                let mut head = block.as_obj().cloned().unwrap_or_default();
                let input = head.remove("input").filter(|i| !i.is_null()).unwrap_or_else(Json::obj);
                events.push(start(Json::Obj(head)));
                events.push(delta(jobj! { "type": "input_json_delta", "partial_json": input.to_text() }));
            }
            _ => events.push(start(block.clone())),
        }
        events.push(jobj! { "type": "content_block_stop", "index": index });
    }
    let mut end_usage = usage.clone();
    end_usage.insert("output_tokens", usage.get("output_tokens").filter(|v| v.is_num()).cloned().unwrap_or(Json::from(0u64)));
    events.push(jobj! {
        "type": "message_delta",
        "delta": jobj! { "stop_reason": stop_reason, "stop_sequence": message.non_null("stop_sequence").cloned().unwrap_or(Json::Null) },
        "usage": end_usage,
    });
    events.push(jobj! { "type": "message_stop" });
    Some(events.into_iter().map(|event| format!("data: {}\n\n", event.to_text())).collect())
}

/// Replay a non-streamed Message body as SSE. A relay that answers `stream: true` with one JSON Message would
/// otherwise fail the turn. Only responses to a request that asked to stream are converted.
struct NonStreamingAsSse {
    inner: DynFetch,
}

impl NonStreamingAsSse {
    async fn run(&self, request: HttpRequest) -> Result<HttpResponse, AiError> {
        let body = request.body.clone();
        let mut response = self.inner.fetch(request).await?;
        let Some(body) = body else { return Ok(response) };
        if !response.ok() {
            return Ok(response);
        }
        let content_type = response.headers.get("content-type").unwrap_or("").to_owned();
        if content_type.contains("text/event-stream") || !content_type.contains("json") {
            return Ok(response);
        }
        let asked_to_stream = Json::parse(&body).is_ok_and(|b| b.get("stream").and_then(Json::as_bool) == Some(true));
        if !asked_to_stream {
            return Ok(response);
        }
        let text = response.body.read_text().await?;
        let frames = Json::parse(&text)
            .ok()
            .filter(|payload| payload.str_at("type") == Some("message"))
            .and_then(|payload| message_as_sse_frames(&payload));
        let Some(frames) = frames else {
            return Ok(HttpResponse::with_text(response.status, &response.status_text, response.headers, text));
        };
        let mut headers = response.headers;
        headers.set("content-type", "text/event-stream");
        headers.delete("content-length");
        Ok(HttpResponse { status: response.status, status_text: response.status_text, headers, body: Body::text(frames) })
    }
}

impl Fetch for NonStreamingAsSse {
    fn fetch(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, AiError>> {
        Box::pin(self.run(request))
    }
}

// ------------------------------------------------------------ Adaptive thinking

fn effort_budget_share(effort: &str) -> Option<f64> {
    match effort {
        "low" => Some(0.1),
        "medium" => Some(0.3),
        "high" => Some(0.6),
        "xhigh" | "max" => Some(0.9),
        _ => None,
    }
}

const MIN_THINKING_BUDGET: u64 = 1024;

/// The explicit-budget thinking form for `body`: an effort's share of `max_tokens`, taken out of the ceiling
/// rather than added on top, or Claude Code's fixed `max_tokens - 1` without one. It stays inside
/// `[1024, max_tokens)`, growing `max_tokens` only when it cannot contain the minimum.
fn enabled_thinking_for(body: &mut Json, require_effort: bool) -> Option<Json> {
    let max_tokens = body.get("max_tokens").and_then(Json::as_u64).filter(|m| *m >= 1)?;
    let share = body.path(&["output_config", "effort"]).and_then(Json::as_str).and_then(effort_budget_share);
    if share.is_none() && require_effort {
        return None;
    }
    let budget = match share {
        None => MIN_THINKING_BUDGET.max(max_tokens - 1),
        Some(share) => MIN_THINKING_BUDGET.max(js_round(max_tokens as f64 * share)),
    };
    if budget >= max_tokens {
        body.as_obj_mut()?.insert("max_tokens", budget + 1);
    }
    Some(jobj! { "type": "enabled", "budget_tokens": budget })
}

/// Relay endpoints answer `200` to `thinking: {type:"adaptive"}` and drop the field, and the turn comes back
/// with no thinking at all. `enabled` with a budget is the form every compatible endpoint implements.
fn rewrite_adaptive_thinking_body(text: &str) -> Option<String> {
    if !text.contains("adaptive") {
        return None;
    }
    let mut body = Json::parse(text).ok().filter(Json::is_obj)?;
    if body.path(&["thinking", "type"]).and_then(Json::as_str) != Some("adaptive") {
        return None;
    }
    let enabled = enabled_thinking_for(&mut body, true)?;
    body.as_obj_mut()?.insert("thinking", enabled);
    Some(body.to_text())
}

// ------------------------------------------------------------ Betas

const INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";

/// Claude Code's `avt`: every model except Claude 3 and Haiku 4.5.
fn supports_interleaved_thinking(model_id: &str) -> bool {
    let id = model_id.to_ascii_lowercase();
    !id.contains("claude-3") && !id.contains("haiku-4-5") && !id.contains("haiku-4.5")
}

const BETA_HEADER: &str = "anthropic-beta";

fn betas_of(headers: &Headers) -> Vec<String> {
    headers.get(BETA_HEADER).unwrap_or("").split(',').map(str::trim).filter(|b| !b.is_empty()).map(str::to_owned).collect()
}

fn set_betas(headers: &mut Headers, betas: &[String]) {
    if betas.is_empty() {
        headers.delete(BETA_HEADER);
    } else {
        headers.set(BETA_HEADER, &betas.join(","));
    }
}

// ------------------------------------------------------------ Prompt-cache breakpoints

/// Per-request prompt-cache directives from the host, for one model.
#[derive(Clone, Default)]
pub struct PromptCacheOptions {
    /// `false` turns every breakpoint off.
    pub enabled: Option<bool>,
    /// The per-step tail of the system prompt, which plays the part of the text after Claude Code's
    /// dynamic boundary: prefix and tail each get a breakpoint.
    pub system_dynamic: Option<String>,
}

fn cache_marker() -> Json {
    jobj! { "type": "ephemeral" }
}

const SYSTEM_SECTION_SEPARATOR: &str = "\n\n";

/// Claude Code's `Akt`: thinking blocks are signed, `fallback` is synthetic, and blank text is rejected.
fn is_stampable(block: &Json) -> bool {
    if !block.is_obj() {
        return false;
    }
    match block.str_at("type") {
        Some("text") => block.str_at("text").is_some_and(|t| !t.trim().is_empty()),
        Some("thinking") | Some("redacted_thinking") | Some("fallback") => false,
        _ => true,
    }
}

/// Claude Code's `kar`: a message whose tail can hold the marker.
fn tail_stampable(message: &Json) -> bool {
    if !message.is_obj() {
        return false;
    }
    // A mid-conversation system message that only hands over tools has no block a breakpoint can sit on.
    if message.str_at("role") == Some("system") {
        let Some(last) = message.get("content").and_then(Json::as_arr).and_then(|c| c.last()) else { return false };
        return last.str_at("type") == Some("text") && is_stampable(last);
    }
    if message.str_at("role") != Some("assistant") {
        return true;
    }
    match message.get("content") {
        Some(Json::Str(_)) => true,
        Some(Json::Arr(content)) => content.last().is_some_and(is_stampable),
        _ => false,
    }
}

fn stamp(block: &mut Json) {
    if let Some(object) = block.as_obj_mut() {
        object.insert("cache_control", cache_marker());
    }
}

/// Claude Code's `Yis`/`Xis`: mark the chosen message.
fn stamp_message(message: &mut Json) -> bool {
    if let Some(text) = message.str_at("content").map(str::to_owned) {
        if text.trim().is_empty() {
            return false;
        }
        if let Some(object) = message.as_obj_mut() {
            object.insert("content", vec![jobj! { "type": "text", "text": text, "cache_control": cache_marker() }]);
        }
        return true;
    }
    let is_user = message.str_at("role") == Some("user");
    let Some(content) = message.get_mut("content").and_then(Json::as_arr_mut).filter(|c| !c.is_empty()) else {
        return false;
    };
    if is_user {
        for block in content.iter_mut().rev() {
            if !block.is_obj() || (block.str_at("type") == Some("text") && !is_stampable(block)) {
                continue;
            }
            stamp(block);
            return true;
        }
        return false;
    }
    let Some(last) = content.last_mut().filter(|b| is_stampable(b)) else { return false };
    stamp(last);
    true
}

/// Claude Code's message-side placement: walk back to the last eligible message and mark it.
fn add_message_breakpoint(body: &mut Json) -> bool {
    let Some(messages) = body.get_mut("messages").and_then(Json::as_arr_mut) else { return false };
    let Some(index) = messages.iter().rposition(tail_stampable) else { return false };
    stamp_message(&mut messages[index])
}

/// Claude Code's `pkt`/`_as`: the system prompt as breakpointed blocks, split at the host's dynamic tail.
fn add_system_breakpoints(body: &mut Json, dynamic: Option<&str>) -> bool {
    let Some(object) = body.as_obj_mut() else { return false };
    if let Some(text) = object.get("system").and_then(Json::as_str).map(str::to_owned) {
        if text.trim().is_empty() {
            return false;
        }
        object.insert("system", vec![jobj! { "type": "text", "text": text }]);
    }
    let Some(system) = object.get_mut("system").and_then(Json::as_arr_mut).filter(|s| !s.is_empty()) else {
        return false;
    };
    if let (1, Some(dynamic), Some("text"), Some(text)) = (
        system.len(),
        dynamic.filter(|d| !d.is_empty()),
        system[0].str_at("type"),
        system[0].str_at("text").map(str::to_owned),
    ) {
        let prefix = if text == dynamic {
            Some(String::new())
        } else {
            text.strip_suffix(&format!("{SYSTEM_SECTION_SEPARATOR}{dynamic}")).map(str::to_owned)
        };
        if let Some(prefix) = prefix {
            let mut blocks = Vec::new();
            if !prefix.trim().is_empty() {
                blocks.push(jobj! { "type": "text", "text": prefix, "cache_control": cache_marker() });
            }
            blocks.push(jobj! { "type": "text", "text": dynamic, "cache_control": cache_marker() });
            object.insert("system", blocks);
            return true;
        }
    }
    for block in system.iter_mut().rev() {
        if is_stampable(block) {
            stamp(block);
            return true;
        }
    }
    false
}

fn add_cache_breakpoints(body: &mut Json, options: &PromptCacheOptions) -> bool {
    let system = add_system_breakpoints(body, options.system_dynamic.as_deref());
    let message = add_message_breakpoint(body);
    system || message
}

/// Keys whose values are data rather than request structure.
fn is_data_key(key: &str) -> bool {
    key == "input" || key == "input_schema"
}

fn strip_cache_control(value: &mut Json) -> bool {
    match value {
        Json::Arr(items) => {
            let mut did = false;
            for item in items {
                did |= strip_cache_control(item);
            }
            did
        }
        Json::Obj(object) => {
            let mut did = object.remove("cache_control").is_some();
            for (key, entry) in object.iter_mut() {
                if !is_data_key(key) {
                    did |= strip_cache_control(entry);
                }
            }
            did
        }
        _ => false,
    }
}

fn carries_cache_control(value: &Json) -> bool {
    match value {
        Json::Arr(items) => items.iter().any(carries_cache_control),
        Json::Obj(object) => {
            object.get("cache_control").is_some_and(|c| !c.is_null())
                || object.iter().any(|(key, entry)| !is_data_key(key) && carries_cache_control(entry))
        }
        _ => false,
    }
}

// ------------------------------------------------------------ Cache-coverage detector

#[derive(Default)]
struct CoverageState {
    consecutive: u32,
    fired: bool,
    ok_emitted: bool,
}

fn coverage_states() -> &'static Mutex<HashMap<String, CoverageState>> {
    static STATES: OnceLock<Mutex<HashMap<String, CoverageState>>> = OnceLock::new();
    STATES.get_or_init(|| Mutex::new(HashMap::new()))
}

const COVERAGE_INPUT_THRESHOLD: f64 = 20_000.0;
const COVERAGE_TURNS: u32 = 3;

/// Claude Code's `OAt`: three consecutive large uncached turns fire once.
fn observe_coverage(key: &str, usage: &Json) {
    let number = |value: Option<&Json>| value.and_then(Json::as_f64).unwrap_or(0.0);
    let input = number(usage.get("input_tokens"));
    let read = number(usage.get("cache_read_input_tokens"));
    let creation_field = number(usage.get("cache_creation_input_tokens"));
    let creation = usage.get("cache_creation").filter(|c| c.is_obj()).map_or(0.0, |c| {
        number(c.get("ephemeral_5m_input_tokens")) + number(c.get("ephemeral_1h_input_tokens"))
    });
    let written = if creation_field > 0.0 { creation_field } else { creation };
    if input + read + written <= 0.0 {
        return;
    }
    let mut states = coverage_states().lock().unwrap_or_else(|p| p.into_inner());
    let state = states.entry(key.to_owned()).or_default();
    if state.fired {
        return;
    }
    if input < COVERAGE_INPUT_THRESHOLD || written >= 0.1 * input {
        state.consecutive = 0;
        if !state.ok_emitted && read + written > 0.0 {
            state.ok_emitted = true;
        }
        return;
    }
    state.consecutive += 1;
    if state.consecutive >= COVERAGE_TURNS {
        state.fired = true;
        eprintln!(
            "[cache-coverage] sustained uncovered input with a cache breakpoint on the wire through a custom endpoint — no cache-billing evidence visible at the client (the endpoint may be silently stripping cache_control) {}",
            jobj! {
                "endpoint": key,
                "consecutive_turns": state.consecutive,
                "input_tokens": input,
                "cache_read_input_tokens": read,
                "cache_creation_input_tokens": written,
            }
            .to_text()
        );
    }
}

/// What the request layer learned about one round trip, read by the response layer.
#[derive(Default)]
struct WireContext {
    key: String,
    breakpoints: bool,
}

// ------------------------------------------------------------ 400 self-heal

/// Repairs learned from an endpoint's 400 responses, keyed by request URL and model, for the process.
#[derive(Default)]
struct HealLatch {
    thinking_type: Option<&'static str>,
    strip_thinking: bool,
    drop_effort: bool,
    drop_betas: HashSet<String>,
    no_cache_control: bool,
    max_tokens_cap: Option<u64>,
    no_tool_changes: bool,
    no_system_messages: bool,
}

fn heal_latches() -> &'static Mutex<HashMap<String, HealLatch>> {
    static LATCHES: OnceLock<Mutex<HashMap<String, HealLatch>>> = OnceLock::new();
    LATCHES.get_or_init(|| Mutex::new(HashMap::new()))
}

const MID_CONVERSATION_SYSTEM_BETA: &str = "mid-conversation-system-2026-04-07";
const TOOL_CHANGES_BETA: &str = "mid-conversation-tool-changes-2026-07-01";

/// Lifts the text of every mid-conversation system message into the top-level `system`; tool-change blocks
/// stay where they are. The form every endpoint takes.
fn lift_system_messages(body: &mut Json, headers: &mut Headers) -> bool {
    let Some(messages) = body.get("messages").and_then(Json::as_arr) else { return false };
    let mut lifted: Vec<Json> = Vec::new();
    let mut kept: Vec<Json> = Vec::with_capacity(messages.len());
    for message in messages {
        if message.str_at("role") != Some("system") {
            kept.push(message.clone());
            continue;
        }
        let content: Vec<Json> = match message.get("content") {
            Some(Json::Str(text)) => vec![jobj! { "type": "text", "text": text.as_str() }],
            Some(Json::Arr(content)) => content.clone(),
            _ => Vec::new(),
        };
        let is_text = |block: &Json| block.str_at("type") == Some("text");
        if !content.iter().any(is_text) {
            kept.push(message.clone());
            continue;
        }
        for block in content.iter().filter(|b| is_text(b)) {
            if let Some(text) = block.str_at("text").filter(|t| !t.is_empty()) {
                lifted.push(jobj! { "type": "text", "text": text });
            }
        }
        let rest: Vec<Json> = content.into_iter().filter(|b| !is_text(b)).collect();
        if !rest.is_empty() {
            let mut next = message.as_obj().cloned().unwrap_or_default();
            next.insert("content", rest);
            kept.push(Json::Obj(next));
        }
    }
    if kept.len() == messages.len() && lifted.is_empty() {
        return false;
    }
    let system_left = kept.iter().any(|m| m.str_at("role") == Some("system"));
    let Some(object) = body.as_obj_mut() else { return false };
    object.insert("messages", kept);
    match object.get("system") {
        Some(Json::Str(text)) => {
            let mut blocks = vec![jobj! { "type": "text", "text": text.as_str() }];
            blocks.extend(lifted);
            object.insert("system", blocks);
        }
        Some(Json::Arr(system)) => {
            let mut blocks = system.clone();
            blocks.extend(lifted);
            object.insert("system", blocks);
        }
        _ => {
            if !lifted.is_empty() {
                object.insert("system", lifted);
            }
        }
    }
    if !system_left {
        let betas: Vec<String> = betas_of(headers).into_iter().filter(|b| b != MID_CONVERSATION_SYSTEM_BETA).collect();
        set_betas(headers, &betas);
    }
    true
}

fn is_tool_change(block: &Json) -> bool {
    matches!(block.str_at("type"), Some("tool_addition") | Some("tool_removal"))
}

fn carries_tool_changes(body: &Json) -> bool {
    body.get("messages").and_then(Json::as_arr).is_some_and(|messages| {
        messages.iter().any(|m| {
            m.str_at("role") == Some("system") && m.get("content").and_then(Json::as_arr).is_some_and(|c| c.iter().any(is_tool_change))
        })
    })
}

/// Takes every mid-conversation tool change back out: blocks leave their system messages, each deferred tool
/// is declared outright, and the betas only a change needed are dropped.
fn strip_tool_changes(body: &mut Json, headers: &mut Headers) -> bool {
    if !carries_tool_changes(body) {
        return false;
    }
    let messages = body.get("messages").and_then(Json::as_arr).cloned().unwrap_or_default();
    let mut next: Vec<Json> = Vec::with_capacity(messages.len());
    for message in messages {
        let is_system = message.str_at("role") == Some("system");
        let Some(content) = message.get("content").and_then(Json::as_arr).filter(|_| is_system) else {
            next.push(message);
            continue;
        };
        let kept: Vec<Json> = content.iter().filter(|b| !is_tool_change(b)).cloned().collect();
        if kept.len() == content.len() {
            next.push(message);
        } else if !kept.is_empty() {
            let mut object = message.as_obj().cloned().unwrap_or_default();
            object.insert("content", kept);
            next.push(Json::Obj(object));
        }
    }
    let system_left = next.iter().any(|m| m.str_at("role") == Some("system"));
    let Some(object) = body.as_obj_mut() else { return false };
    object.insert("messages", next);
    if let Some(Json::Arr(tools)) = object.get_mut("tools") {
        for tool in tools {
            if let Some(tool) = tool.as_obj_mut() {
                tool.remove("defer_loading");
            }
        }
    }
    let betas: Vec<String> = betas_of(headers)
        .into_iter()
        .filter(|beta| beta != TOOL_CHANGES_BETA && (system_left || beta != MID_CONVERSATION_SYSTEM_BETA))
        .collect();
    set_betas(headers, &betas);
    true
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum HealClass {
    ToolChanges,
    SystemMessages,
    ThinkingSignature,
    ThinkingType,
    Effort,
    MaxTokensOverflow,
    MaxTokensLimit,
    Beta,
    CacheControl,
}

/// The lower-cased, backtick-free message of a 400 body, or the raw text.
fn error_message_of(text: &str) -> String {
    let mut message = text.to_owned();
    if let Ok(parsed) = Json::parse(text) {
        if parsed.is_obj() {
            if let Some(inner) = parsed.get("error").filter(|e| e.is_obj()).and_then(|e| e.str_at("message")) {
                message = inner.to_owned();
            } else if let Some(inner) = parsed.str_at("message") {
                message = inner.to_owned();
            }
        }
    }
    message.to_lowercase().replace('`', "")
}

/// Claude Code's `EZe`: the server rejected a thinking block's signature.
fn blames_thinking_signature(message: &str) -> bool {
    message.contains("signature in thinking block")
        || (message.contains("thinking.signature") && message.contains("field required"))
        || ((message.contains("thinking block") || message.contains("redacted_thinking"))
            && (message.contains("cannot be modified") || message.contains("invalid signature")))
}

/// Claude Code's `vZe`: which `thinking.type` the server rejected, if any.
fn rejected_thinking_type(message: &str) -> Option<&'static str> {
    static PATTERN: OnceLock<regex::Regex> = OnceLock::new();
    if message.contains("adaptive thinking is not supported") {
        return Some("adaptive");
    }
    let pattern = PATTERN.get_or_init(|| {
        regex::Regex::new(r"thinking\.type[^.]*?(enabled|adaptive)[^.]*?not supported").expect("a literal pattern")
    });
    match pattern.captures(message)?.get(1)?.as_str() {
        "adaptive" => Some("adaptive"),
        _ => Some("enabled"),
    }
}

fn blames_effort(message: &str) -> bool {
    message.contains("does not support the effort parameter")
        || (message.contains("effort") && message.contains("not supported"))
        || (message.contains("output_config")
            && (message.contains("extra inputs are not permitted") || message.contains("requires a model that supports")))
}

/// Claude Code's `Fan`: the exact input + `max_tokens` overflow shape.
fn context_overflow(message: &str) -> Option<(u64, u64)> {
    static PATTERN: OnceLock<regex::Regex> = OnceLock::new();
    let pattern = PATTERN.get_or_init(|| {
        regex::Regex::new(r"input length and max_tokens exceed context limit: (\d+) \+ (\d+) > (\d+)").expect("a literal pattern")
    });
    let captures = pattern.captures(message)?;
    Some((captures.get(1)?.as_str().parse().ok()?, captures.get(3)?.as_str().parse().ok()?))
}

fn max_tokens_limit(message: &str) -> Option<u64> {
    static PATTERN: OnceLock<regex::Regex> = OnceLock::new();
    let pattern = PATTERN.get_or_init(|| regex::Regex::new(r"max_tokens: (\d+) > (\d+)").expect("a literal pattern"));
    pattern.captures(message)?.get(2)?.as_str().parse().ok()
}

fn contains_any(message: &str, words: &[&str]) -> bool {
    words.iter().any(|word| message.contains(word))
}

fn rejected_beta(message: &str, betas: &[String]) -> Option<String> {
    betas
        .iter()
        .find(|beta| {
            message.contains(&beta.to_lowercase()) && contains_any(message, &["not supported", "unsupported", "unknown", "invalid", "unrecognized"])
        })
        .cloned()
}

fn blames_cache_control(message: &str) -> bool {
    message.contains("cache_control") && contains_any(message, &["not supported", "unsupported", "unknown", "invalid", "extra inputs", "unexpected"])
}

/// Strip every thinking block; Claude Code's `xVn`, placeholder included.
fn strip_thinking_blocks(body: &mut Json) -> bool {
    let Some(messages) = body.get_mut("messages").and_then(Json::as_arr_mut) else { return false };
    let mut did = false;
    for message in messages {
        if message.str_at("role") != Some("assistant") {
            continue;
        }
        let Some(content) = message.get_mut("content").and_then(Json::as_arr_mut) else { continue };
        let is_thinking = |block: &Json| matches!(block.str_at("type"), Some("thinking") | Some("redacted_thinking"));
        if !content.iter().any(is_thinking) {
            continue;
        }
        let mut substantive: Vec<Json> = content
            .iter()
            .filter(|b| !is_thinking(b))
            .filter(|b| b.str_at("type") != Some("text") || b.str_at("text").is_some_and(|t| !t.trim().is_empty()))
            .cloned()
            .collect();
        if substantive.is_empty() {
            substantive.push(jobj! { "type": "text", "text": "[Thinking removed]" });
        }
        *content = substantive;
        did = true;
    }
    did
}

fn apply_thinking_type(body: &mut Json, kind: &str) -> bool {
    let current = body.path(&["thinking", "type"]).and_then(Json::as_str).map(str::to_owned);
    let Some(current) = current else { return false };
    if current == kind || (current != "adaptive" && current != "enabled") {
        return false;
    }
    if kind == "adaptive" {
        if let Some(object) = body.as_obj_mut() {
            object.insert("thinking", jobj! { "type": "adaptive", "display": "summarized" });
        }
        return true;
    }
    let Some(enabled) = enabled_thinking_for(body, false) else { return false };
    if let Some(object) = body.as_obj_mut() {
        object.insert("thinking", enabled);
    }
    true
}

fn drop_effort(body: &mut Json) -> bool {
    let Some(object) = body.as_obj_mut() else { return false };
    let Some(Json::Obj(config)) = object.get_mut("output_config") else { return false };
    if config.remove("effort").is_none() {
        return false;
    }
    if config.is_empty() {
        object.remove("output_config");
    }
    true
}

/// Keep `budget_tokens` below a lowered `max_tokens`, as Claude Code's clamp does.
fn clamp_thinking_budget(body: &mut Json) {
    let Some(max_tokens) = body.get("max_tokens").and_then(Json::as_u64) else { return };
    let Some(thinking) = body.get_mut("thinking").and_then(Json::as_obj_mut) else { return };
    let Some(budget) = thinking.get("budget_tokens").and_then(Json::as_u64) else { return };
    if budget < max_tokens {
        return;
    }
    thinking.insert("budget_tokens", MIN_THINKING_BUDGET.max(max_tokens.saturating_sub(1)));
}

/// Apply the endpoint's learned repairs and Claude Code's request-time additions.
fn prepare_request(body: &mut Json, headers: &mut Headers, latch: &HealLatch, cache: &PromptCacheOptions, tool_changes: bool) {
    // A relay may take the deferred declaration and drop the change that would have surfaced it, which loses
    // the tool without a word; only an endpoint that has not refused gets tool changes.
    if !tool_changes || latch.no_tool_changes {
        strip_tool_changes(body, headers);
    }
    if latch.no_system_messages {
        lift_system_messages(body, headers);
    }
    if latch.strip_thinking {
        strip_thinking_blocks(body);
    }
    if let Some(kind) = latch.thinking_type {
        apply_thinking_type(body, kind);
    }
    if latch.drop_effort {
        drop_effort(body);
    }
    if let Some(cap) = latch.max_tokens_cap {
        if body.get("max_tokens").and_then(Json::as_u64).is_some_and(|m| m > cap) {
            if let Some(object) = body.as_obj_mut() {
                object.insert("max_tokens", cap);
            }
            clamp_thinking_budget(body);
        }
    }
    if latch.no_cache_control {
        strip_cache_control(body);
    } else if cache.enabled != Some(false) {
        add_cache_breakpoints(body, cache);
    }
    let model = body.str_at("model").unwrap_or("").to_owned();
    let betas = betas_of(headers);
    let mut wanted = betas.clone();
    if supports_interleaved_thinking(&model) && !wanted.iter().any(|b| b == INTERLEAVED_THINKING_BETA) {
        wanted.push(INTERLEAVED_THINKING_BETA.to_owned());
    }
    let allowed: Vec<String> = wanted.into_iter().filter(|b| !latch.drop_betas.contains(b)).collect();
    if allowed != betas {
        set_betas(headers, &allowed);
    }
}

/// Decide the one repair a 400 calls for, apply it, and say which class it was.
fn heal(body: &mut Json, headers: &mut Headers, latch: &mut HealLatch, message: &str) -> Option<HealClass> {
    if blames_thinking_signature(message) {
        if !strip_thinking_blocks(body) {
            return None;
        }
        latch.strip_thinking = true;
        return Some(HealClass::ThinkingSignature);
    }
    if let Some(rejected) = rejected_thinking_type(message) {
        let swapped = if rejected == "enabled" { "adaptive" } else { "enabled" };
        if !apply_thinking_type(body, swapped) {
            return None;
        }
        latch.thinking_type = Some(swapped);
        return Some(HealClass::ThinkingType);
    }
    if blames_effort(message) {
        if !drop_effort(body) {
            return None;
        }
        latch.drop_effort = true;
        return Some(HealClass::Effort);
    }
    if let Some((input, limit)) = context_overflow(message) {
        let lowered = 3000i64.max(limit as i64 - input as i64 - 1000) as u64;
        if body.get("max_tokens").and_then(Json::as_u64).is_none_or(|m| lowered >= m) {
            return None;
        }
        body.as_obj_mut()?.insert("max_tokens", lowered);
        clamp_thinking_budget(body);
        return Some(HealClass::MaxTokensOverflow);
    }
    if let Some(limit) = max_tokens_limit(message) {
        if body.get("max_tokens").and_then(Json::as_u64).is_none_or(|m| limit >= m) || limit < 1 {
            return None;
        }
        body.as_obj_mut()?.insert("max_tokens", limit);
        clamp_thinking_budget(body);
        latch.max_tokens_cap = Some(limit);
        return Some(HealClass::MaxTokensLimit);
    }
    if let Some(beta) = rejected_beta(message, &betas_of(headers)) {
        let betas: Vec<String> = betas_of(headers).into_iter().filter(|b| *b != beta).collect();
        latch.drop_betas.insert(beta);
        set_betas(headers, &betas);
        return Some(HealClass::Beta);
    }
    if blames_cache_control(message) {
        if !strip_cache_control(body) {
            return None;
        }
        latch.no_cache_control = true;
        return Some(HealClass::CacheControl);
    }
    // Last, and for any other 400: a model without tool changes refuses the system message, the deferred
    // declaration or the beta in words that vary, and the request is valid without the change.
    if strip_tool_changes(body, headers) {
        latch.no_tool_changes = true;
        return Some(HealClass::ToolChanges);
    }
    if lift_system_messages(body, headers) {
        latch.no_system_messages = true;
        return Some(HealClass::SystemMessages);
    }
    None
}

/// Claude Code's request-time additions and 400 self-heal chain, so the SDK never sees the rejected attempt.
struct SelfHeal {
    inner: DynFetch,
    cache: PromptCacheOptions,
    context: Arc<Mutex<WireContext>>,
    tool_changes: bool,
}

impl SelfHeal {
    async fn run(&self, request: HttpRequest) -> Result<HttpResponse, AiError> {
        let parsed = request.body.as_deref().and_then(|text| Json::parse(text).ok()).filter(Json::is_obj);
        let Some(mut body) = parsed else { return self.inner.fetch(request).await };
        let url = url_without_hash(&request.url);
        let key = format!("{url}|{}", body.str_at("model").unwrap_or(""));
        let mut headers = request.headers.clone();
        {
            let mut latches = heal_latches().lock().unwrap_or_else(|p| p.into_inner());
            let latch = latches.entry(key.clone()).or_default();
            prepare_request(&mut body, &mut headers, latch, &self.cache, self.tool_changes);
        }
        let mut applied: HashSet<HealClass> = HashSet::new();
        loop {
            {
                let mut context = self.context.lock().unwrap_or_else(|p| p.into_inner());
                context.key = format!("{url}|{}", body.str_at("model").unwrap_or(""));
                context.breakpoints = carries_cache_control(&body);
            }
            let mut attempt = request.clone();
            attempt.headers = headers.clone();
            attempt.body = Some(body.to_text());
            let mut response = self.inner.fetch(attempt).await?;
            if response.status != 400 {
                return Ok(response);
            }
            let error_text = response.body.read_text().await?;
            let message = error_message_of(&error_text);
            let healed = {
                let mut latches = heal_latches().lock().unwrap_or_else(|p| p.into_inner());
                let latch = latches.entry(key.clone()).or_default();
                heal(&mut body, &mut headers, latch, &message)
            };
            let Some(healed) = healed.filter(|class| !applied.contains(class)) else {
                return Ok(HttpResponse::with_text(response.status, &response.status_text, response.headers, error_text));
            };
            applied.insert(healed);
            // Claude Code rebuilds the whole body for every attempt, so a repair that rewrote the messages
            // also moves the message-side marker to the new tail.
            let no_cache_control = {
                let latches = heal_latches().lock().unwrap_or_else(|p| p.into_inner());
                latches.get(&key).is_some_and(|l| l.no_cache_control)
            };
            if healed != HealClass::CacheControl && self.cache.enabled != Some(false) && !no_cache_control {
                if let Some(messages) = body.get_mut("messages") {
                    strip_cache_control(messages);
                }
                add_message_breakpoint(&mut body);
            }
        }
    }
}

impl Fetch for SelfHeal {
    fn fetch(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, AiError>> {
        Box::pin(self.run(request))
    }
}

/// Wraps `fetch` to normalize Anthropic request and response dialect differences.
///
/// `base_url` decides whether the adaptive rewrite applies and whether cache coverage is watched: only a custom
/// endpoint can silently drop thinking or strip markers. `tool_changes` is the host's word that this model at
/// this endpoint takes a mid-conversation tool change.
pub fn anthropic_dialect_fetch(base_url: Option<&str>, inner: DynFetch, cache: PromptCacheOptions, tool_changes: bool) -> DynFetch {
    let official = is_official(base_url);
    // One wrapper serves one call, whose fetches run one after another, so the request layer can leave its
    // facts here for the response layer.
    let context = Arc::new(Mutex::new(WireContext::default()));
    let heal = Arc::new(SelfHeal { inner, cache, context: context.clone(), tool_changes });
    let non_streaming: DynFetch = Arc::new(NonStreamingAsSse { inner: heal });
    let on_usage: Arc<dyn Fn(&Json) + Send + Sync> = Arc::new(move |usage: &Json| {
        let context = context.lock().unwrap_or_else(|p| p.into_inner());
        if context.breakpoints && !official && usage.is_obj() {
            observe_coverage(&context.key, usage);
        }
    });
    Arc::new(SseDialectFetch {
        make_rewrite: Box::new(move || make_line_rewriter(on_usage.clone())),
        patch_request: if official { None } else { Some(Box::new(rewrite_adaptive_thinking_body)) },
        inner: non_streaming,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(line: &str) -> Vec<String> {
        let mut rewrite = make_line_rewriter(Arc::new(|_| {}));
        match rewrite(line) {
            Rewrite::Keep => vec![line.to_owned()],
            Rewrite::Drop => vec![],
            Rewrite::Replace(text) => vec![text],
            Rewrite::Fail(message) => vec![format!("FAIL {message}")],
        }
    }

    #[test]
    fn unknown_events_and_deltas_are_dropped() {
        assert!(apply(r#"data: {"type":"telemetry"}"#).is_empty());
        assert!(apply(r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"citations_delta"}}"#).is_empty());
        assert_eq!(apply("event: x"), ["event: x"]);
        assert_eq!(apply("data: [DONE]"), ["data: [DONE]"]);
    }

    #[test]
    fn unknown_blocks_become_empty_text() {
        assert_eq!(
            apply(r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"weird"}}"#),
            [r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#]
        );
    }

    #[test]
    fn scalar_defaults_and_wrapped_tool_errors() {
        assert_eq!(
            apply(r#"data: {"type":"message_start","message":{"id":"m"}}"#),
            [r#"data: {"type":"message_start","message":{"id":"m","usage":{"input_tokens":0}}}"#]
        );
        assert_eq!(
            apply(r#"data: {"type":"content_block_start","index":2,"content_block":{"type":"web_search_tool_result","tool_use_id":"t","content":[{"type":"web_search_tool_result_error","error_code":"max_uses_exceeded"}]}}"#),
            [r#"data: {"type":"content_block_start","index":2,"content_block":{"type":"web_search_tool_result","tool_use_id":"t","content":{"type":"web_search_tool_result_error","error_code":"max_uses_exceeded"}}}"#]
        );
    }

    #[test]
    fn a_thinking_block_carrying_its_text_at_start_gets_deltas() {
        let mut rewrite = make_line_rewriter(Arc::new(|_| {}));
        assert!(matches!(rewrite(r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"T","signature":"S"}}"#), Rewrite::Keep));
        let Rewrite::Replace(text) = rewrite(r#"data: {"type":"content_block_stop","index":0}"#) else { panic!("expected deltas") };
        assert_eq!(text, "\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"T\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"S\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}");
    }

    #[test]
    fn adaptive_thinking_becomes_a_budget_only_with_an_effort() {
        let rewritten = rewrite_adaptive_thinking_body(r#"{"max_tokens":32000,"thinking":{"type":"adaptive","display":"summarized"},"output_config":{"effort":"high"}}"#).unwrap();
        assert_eq!(rewritten, r#"{"max_tokens":32000,"thinking":{"type":"enabled","budget_tokens":19200},"output_config":{"effort":"high"}}"#);
        assert!(rewrite_adaptive_thinking_body(r#"{"max_tokens":100,"thinking":{"type":"adaptive"}}"#).is_none());
        let grown = rewrite_adaptive_thinking_body(r#"{"max_tokens":100,"thinking":{"type":"adaptive"},"output_config":{"effort":"low"}}"#).unwrap();
        assert_eq!(grown, r#"{"max_tokens":1025,"thinking":{"type":"enabled","budget_tokens":1024},"output_config":{"effort":"low"}}"#);
    }

    #[test]
    fn breakpoints_split_the_system_prompt_at_the_dynamic_tail() {
        let mut body = Json::parse(r#"{"system":[{"type":"text","text":"stable\n\ntail"}],"messages":[{"role":"user","content":[{"type":"text","text":"q"},{"type":"text","text":"  "}]},{"role":"assistant","content":[{"type":"thinking","thinking":"t","signature":"s"}]}]}"#).unwrap();
        let cache = PromptCacheOptions { enabled: None, system_dynamic: Some("tail".to_owned()) };
        assert!(add_cache_breakpoints(&mut body, &cache));
        assert_eq!(body.to_text(), r#"{"system":[{"type":"text","text":"stable","cache_control":{"type":"ephemeral"}},{"type":"text","text":"tail","cache_control":{"type":"ephemeral"}}],"messages":[{"role":"user","content":[{"type":"text","text":"q","cache_control":{"type":"ephemeral"}},{"type":"text","text":"  "}]},{"role":"assistant","content":[{"type":"thinking","thinking":"t","signature":"s"}]}]}"#);
    }

    #[test]
    fn cache_control_in_tool_data_is_not_a_breakpoint() {
        let mut body = Json::parse(r#"{"tools":[{"input_schema":{"cache_control":{"type":"string"}}}],"messages":[{"role":"assistant","content":[{"type":"tool_use","input":{"cache_control":1},"cache_control":{"type":"ephemeral"}}]}]}"#).unwrap();
        assert!(carries_cache_control(&body));
        assert!(strip_cache_control(&mut body));
        assert!(!carries_cache_control(&body));
        assert_eq!(body.to_text(), r#"{"tools":[{"input_schema":{"cache_control":{"type":"string"}}}],"messages":[{"role":"assistant","content":[{"type":"tool_use","input":{"cache_control":1}}]}]}"#);
    }

    #[test]
    fn heal_classes_read_the_messages_claude_code_reads() {
        assert!(blames_thinking_signature("invalid `signature` in `thinking` block".replace('`', "").as_str()) || blames_thinking_signature("signature in thinking block"));
        assert_eq!(rejected_thinking_type("adaptive thinking is not supported on this model"), Some("adaptive"));
        assert_eq!(rejected_thinking_type("thinking.type: enabled is not supported"), Some("enabled"));
        assert!(blames_effort("this model does not support the effort parameter"));
        assert_eq!(context_overflow("input length and max_tokens exceed context limit: 100 + 200 > 250"), Some((100, 250)));
        assert_eq!(max_tokens_limit("max_tokens: 64000 > 8192, which is the maximum"), Some(8192));
        assert!(blames_cache_control("cache_control: extra inputs are not permitted"));
        assert_eq!(rejected_beta("unsupported beta header: foo-1", &["Foo-1".to_owned()]).as_deref(), Some("Foo-1"));
    }

    #[test]
    fn stripping_thinking_keeps_a_placeholder() {
        let mut body = Json::parse(r#"{"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"t","signature":"s"},{"type":"text","text":"  "}]}]}"#).unwrap();
        assert!(strip_thinking_blocks(&mut body));
        assert_eq!(body.to_text(), r#"{"messages":[{"role":"assistant","content":[{"type":"text","text":"[Thinking removed]"}]}]}"#);
    }

    #[test]
    fn replayed_reasoning_is_filtered_by_signer_and_signature() {
        let mut messages = vec![Json::parse(r#"{"role":"assistant","content":[{"type":"reasoning","text":"a","providerOptions":{"mewrk":{"model":"claude-opus-5[1m]"},"anthropic":{"signature":"s"}}},{"type":"reasoning","text":"b","providerOptions":{"mewrk":{"model":"other"}}}]}"#).unwrap()];
        drop_foreign_signed_reasoning(&mut messages, "claude-opus-5");
        assert_eq!(messages[0].get("content").unwrap().as_arr().unwrap().len(), 1);
        strip_replay_tags(&mut messages);
        assert!(messages[0].to_text().find("mewrk").is_none());
        drop_unsigned_reasoning(&mut messages);
        assert_eq!(messages[0].get("content").unwrap().as_arr().unwrap().len(), 1);

        let mut unsigned = vec![Json::parse(r#"{"role":"assistant","content":[{"type":"reasoning","text":"x","providerOptions":{"anthropic":{"signature":"mewrk-unsigned"}}}]}"#).unwrap()];
        drop_unsigned_reasoning(&mut unsigned);
        assert_eq!(unsigned[0].get("content").unwrap().to_text(), r#"[{"type":"text","text":"(no content)"}]"#);
    }

    #[test]
    fn a_message_body_replays_as_frames() {
        let message = Json::parse(r#"{"type":"message","id":"m","model":"x","stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":2},"content":[{"type":"thinking","thinking":"t","signature":"s"},{"type":"text","text":"hi"},{"type":"tool_use","id":"c","name":"n","input":{"a":1}}]}"#).unwrap();
        let frames = message_as_sse_frames(&message).unwrap();
        assert!(frames.contains(r#"{"type":"signature_delta","signature":"s"}"#));
        assert!(frames.contains(r#"{"type":"input_json_delta","partial_json":"{\"a\":1}"}"#));
        assert!(frames.ends_with("data: {\"type\":\"message_stop\"}\n\n"));
        assert!(message_as_sse_frames(&Json::parse(r#"{"type":"message","content":[]}"#).unwrap()).is_none());
    }

    #[test]
    fn system_messages_lift_and_tool_changes_strip() {
        let mut headers = Headers::from_pairs([("anthropic-beta", "mid-conversation-system-2026-04-07,mid-conversation-tool-changes-2026-07-01,x")]);
        let mut body = Json::parse(r#"{"system":[{"type":"text","text":"S"}],"tools":[{"name":"t","defer_loading":true}],"messages":[{"role":"user","content":"u"},{"role":"system","content":[{"type":"text","text":"later"},{"type":"tool_addition","tool":{"type":"tool_reference","name":"t"}}]}]}"#).unwrap();
        assert!(strip_tool_changes(&mut body, &mut headers));
        assert_eq!(headers.get("anthropic-beta"), Some("mid-conversation-system-2026-04-07,x"));
        assert!(lift_system_messages(&mut body, &mut headers));
        assert_eq!(headers.get("anthropic-beta"), Some("x"));
        assert_eq!(body.to_text(), r#"{"system":[{"type":"text","text":"S"},{"type":"text","text":"later"}],"tools":[{"name":"t"}],"messages":[{"role":"user","content":"u"}]}"#);
    }
}
