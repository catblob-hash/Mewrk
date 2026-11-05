//! Dialect adaptation for Chat Completions-compatible endpoints (`chat-dialect.ts`, `chat-usage.ts`).
//!
//! 1. legacy `function_call` frames are converted to `tool_calls` shape;
//! 2. a missing tool-call id is synthesized only on the name-opening frame;
//! 3. the DeepSeek `reasoning_content` sentinel is backfilled only after same-request dialect evidence;
//! 4. id-to-index accounting is stream-local so interleaved calls resolve to their own indexes.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

use crate::error::AiError;
use crate::http::{Body, BoxFuture, DynFetch, Fetch, HttpRequest, HttpResponse, url_without_hash};
use crate::jobj;
use crate::json::Json;
use crate::model::ModelUsage;
use crate::sse::{LineRewrite, Rewrite, SseDialectFetch};

const FUNCTION_CALL_MARKER: &str = "\"function_call\"";
const TOOL_CALLS_MARKER: &str = "\"tool_calls\"";
const USAGE_MARKER: &str = "\"usage\"";
const DONE_SENTINEL: &str = "[DONE]";

#[derive(Default)]
struct Slot {
    id: Option<String>,
    upstream_id: Option<String>,
    name: Option<String>,
    synthetic_id: bool,
}

/// Per-stream state for synthesized tool-call indexes.
#[derive(Default)]
struct ToolCallIndexState {
    by_id: HashMap<String, u64>,
    by_index: BTreeMap<u64, Slot>,
    next: u64,
}

/// Normalize a parsed chunk. `Ok(true)` when it changed; `Err` is a conflict that fails the stream, as the
/// TypeScript's `throw` inside the transform does.
fn normalize_chat_chunk(chunk: &mut Json, indexes: &mut ToolCallIndexState) -> Result<bool, String> {
    let stream_id = chunk.str_at("id").filter(|id| !id.is_empty()).unwrap_or("call").to_owned();
    let Some(choices) = chunk.get_mut("choices").and_then(Json::as_arr_mut) else { return Ok(false) };
    let mut changed = false;
    for choice in choices {
        if !choice.is_obj() {
            continue;
        }
        if let Some(delta) = choice.get_mut("delta").filter(|d| d.is_obj()) {
            let legacy = delta.get("function_call").filter(|l| l.is_obj()).cloned();
            if let Some(legacy) = legacy {
                // Legacy responses carry one call per message. Its id derives from the stream id, so it is
                // deterministic and cannot collide with `call_*`.
                let name = legacy.str_at("name").filter(|n| !n.is_empty());
                let arguments = match legacy.get("arguments") {
                    Some(Json::Str(text)) => Some(Json::from(text.as_str())),
                    // A relay that sends the arguments as an object still sent them.
                    Some(other) if !other.is_null() => Some(Json::from(other.to_text())),
                    _ => None,
                };
                let call = jobj! {
                    "index": 0u64,
                    "id"?: name.map(|_| format!("legacy_{stream_id}")),
                    "type"?: name.map(|_| "function"),
                    "function": jobj! { "name"?: name, "arguments"?: arguments },
                };
                let object = delta.as_obj_mut().expect("checked to be an object");
                object.insert("tool_calls", vec![call]);
                object.remove("function_call");
                changed = true;
            }
            if let Some(entries) = delta.get_mut("tool_calls").and_then(Json::as_arr_mut) {
                for entry in entries {
                    if !entry.is_obj() {
                        continue;
                    }
                    let name = entry.path(&["function", "name"]).and_then(Json::as_str).filter(|n| !n.is_empty()).map(str::to_owned);
                    let starts_call = name.is_some();
                    let entry_id = entry.str_at("id").filter(|id| !id.is_empty()).map(str::to_owned);
                    let explicit_index = entry.get("index").is_some_and(Json::is_num);
                    let slot: u64;
                    if explicit_index {
                        slot = entry.get("index").and_then(Json::as_u64).unwrap_or(0);
                        let owner = indexes.by_index.get(&slot);
                        let known_slot = entry_id.as_ref().and_then(|id| indexes.by_id.get(id).copied());
                        let conflict = known_slot.is_some_and(|known| known != slot)
                            || match (&entry_id, owner.and_then(|o| o.id.as_ref().map(|id| (id, o)))) {
                                (Some(id), Some((owner_id, owner))) if owner_id != id => {
                                    !(owner.synthetic_id
                                        && owner.upstream_id.as_ref().is_none_or(|upstream| upstream == id)
                                        && (!starts_call || owner.name.as_deref() == name.as_deref())
                                        && known_slot.is_none_or(|known| known == slot))
                                }
                                _ => false,
                            };
                        if conflict {
                            return Err(format!("Chat tool-call index conflict at index {slot}"));
                        }
                        // Preserve upstream indexes and advance the synthetic slot counter.
                        indexes.next = indexes.next.max(slot + 1);
                        if let Some(id) = &entry_id {
                            indexes.by_id.insert(id.clone(), slot);
                        }
                    } else {
                        if let Some(known) = entry_id.as_ref().and_then(|id| indexes.by_id.get(id).copied()) {
                            slot = known;
                        } else if starts_call {
                            slot = indexes.next;
                            indexes.next += 1;
                        } else {
                            if entry_id.is_some() || indexes.by_index.len() != 1 {
                                return Err("Chat tool-call identity ambiguity: continuation has no unique owner".to_owned());
                            }
                            slot = *indexes.by_index.keys().next().unwrap_or(&0);
                        }
                        entry.as_obj_mut().expect("checked to be an object").insert("index", slot);
                        if let Some(id) = &entry_id {
                            indexes.by_id.insert(id.clone(), slot);
                        }
                        changed = true;
                    }
                    let owner = indexes.by_index.entry(slot).or_default();
                    if let (Some(id), None) = (&entry_id, &owner.upstream_id) {
                        owner.upstream_id = Some(id.clone());
                    }
                    if let (Some(name), None) = (&name, &owner.name) {
                        owner.name = Some(name.clone());
                    }
                    // Publish one stable identity, even when the upstream supplies its id late, repeats the
                    // name, or changes the enclosing chunk id.
                    if owner.id.is_none() && (starts_call || entry_id.is_some()) {
                        let id = entry_id.clone().unwrap_or_else(|| format!("synth_{stream_id}_{slot}"));
                        owner.synthetic_id = entry_id.is_none();
                        owner.id = Some(id.clone());
                        indexes.by_id.insert(id, slot);
                    }
                    if let Some(id) = &indexes.by_index[&slot].id {
                        if entry.str_at("id") != Some(id.as_str()) {
                            entry.as_obj_mut().expect("checked to be an object").insert("id", id.as_str());
                            changed = true;
                        }
                    }
                }
            }
        }
        if choice.str_at("finish_reason") == Some("function_call") {
            choice.as_obj_mut().expect("checked to be an object").insert("finish_reason", "tool_calls");
            changed = true;
        }
    }
    Ok(changed)
}

/// Give a usage-only terminating chunk the `choices` array the SDK requires.
fn fill_missing_choices(frame: &mut Json) -> bool {
    if frame.get("choices").is_some_and(Json::is_arr) || frame.non_null("usage").is_none() {
        return false;
    }
    frame.as_obj_mut().expect("a frame is an object").insert("choices", Vec::<Json>::new());
    true
}

fn make_chat_line_rewriter() -> LineRewrite {
    let mut indexes = ToolCallIndexState::default();
    Box::new(move |line: &str| {
        if !line.starts_with("data:") {
            return Rewrite::Keep;
        }
        let payload = line["data:".len()..].trim();
        if payload.is_empty() {
            return Rewrite::Keep;
        }
        // Relays vary the spacing around the terminator.
        if payload == DONE_SENTINEL {
            return if line == "data: [DONE]" { Rewrite::Keep } else { Rewrite::Replace("data: [DONE]".to_owned()) };
        }
        if !line.contains(FUNCTION_CALL_MARKER) && !line.contains(TOOL_CALLS_MARKER) && !line.contains(USAGE_MARKER) {
            return Rewrite::Keep;
        }
        let Ok(mut frame) = Json::parse(payload) else { return Rewrite::Keep };
        if !frame.is_obj() {
            return Rewrite::Keep;
        }
        let mut changed = fill_missing_choices(&mut frame);
        match normalize_chat_chunk(&mut frame, &mut indexes) {
            Ok(did) => changed |= did,
            Err(message) => return Rewrite::Fail(message),
        }
        if changed { Rewrite::Replace(format!("data: {}", frame.to_text())) } else { Rewrite::Keep }
    })
}

/// Add missing `reasoning_content` sentinels. A request is adapted only when another message already supplies
/// the field, preserving bytes for every other endpoint dialect.
fn backfill_reasoning_content(text: &str) -> Option<String> {
    if !text.contains("\"reasoning_content\"") || !text.contains(TOOL_CALLS_MARKER) {
        return None;
    }
    let mut body = Json::parse(text).ok()?;
    let messages = body.get_mut("messages")?.as_arr_mut()?;
    let mut changed = false;
    for message in messages {
        let Some(entry) = message.as_obj_mut() else { continue };
        if entry.get("role").and_then(Json::as_str) != Some("assistant")
            || !entry.get("tool_calls").is_some_and(Json::is_arr)
            || entry.contains_key("reasoning_content")
        {
            continue;
        }
        entry.insert("reasoning_content", "");
        changed = true;
    }
    changed.then(|| body.to_text())
}

/// Request URLs already observed to reject `stream_options`, keyed by the full URL: a gateway routes several
/// upstreams under one host.
fn stream_options_rejectors() -> &'static Mutex<HashSet<String>> {
    static REJECTORS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    REJECTORS.get_or_init(|| Mutex::new(HashSet::new()))
}

fn without_stream_options(body: &str) -> Option<String> {
    if !body.contains("\"stream_options\"") {
        return None;
    }
    let mut parsed = Json::parse(body).ok()?;
    parsed.as_obj_mut()?.remove("stream_options")?;
    Some(parsed.to_text())
}

/// Retry once without `stream_options` when the upstream names that field in a rejection. Losing the usage
/// block only degrades token accounting; failing the turn would be worse.
struct RetryWithoutStreamOptions {
    inner: DynFetch,
}

impl RetryWithoutStreamOptions {
    async fn run(&self, mut request: HttpRequest) -> Result<HttpResponse, AiError> {
        let url = url_without_hash(&request.url);
        let body = request.body.clone();
        if let Some(body) = &body {
            let known = stream_options_rejectors().lock().unwrap_or_else(|p| p.into_inner()).contains(&url);
            if known {
                if let Some(stripped) = without_stream_options(body) {
                    request.body = Some(stripped);
                    return self.inner.fetch(request).await;
                }
            }
        }
        let mut retry = request.clone();
        let mut response = self.inner.fetch(request).await?;
        let Some(body) = body else { return Ok(response) };
        if response.ok() || response.status >= 500 {
            return Ok(response);
        }
        let text = response.body.read_text().await?;
        let replay = |response: HttpResponse, text: String| {
            HttpResponse::with_text(response.status, &response.status_text, response.headers, text)
        };
        if !matches!(response.status, 400 | 422) || !text.contains("stream_options") {
            return Ok(replay(response, text));
        }
        let Some(stripped) = without_stream_options(&body) else { return Ok(replay(response, text)) };
        stream_options_rejectors().lock().unwrap_or_else(|p| p.into_inner()).insert(url);
        retry.body = Some(stripped);
        self.inner.fetch(retry).await
    }
}

impl Fetch for RetryWithoutStreamOptions {
    fn fetch(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, AiError>> {
        Box::pin(self.run(request))
    }
}

/// What an endpoint answered to a `reasoning_effort` it rejected: the value to send instead, or `None`.
fn effort_fallbacks() -> &'static Mutex<HashMap<String, Option<String>>> {
    static FALLBACKS: OnceLock<Mutex<HashMap<String, Option<String>>>> = OnceLock::new();
    FALLBACKS.get_or_init(|| Mutex::new(HashMap::new()))
}

const EFFORT_LADDER: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

fn reasoning_effort_of(body: &str) -> Option<(String, String)> {
    if !body.contains("\"reasoning_effort\"") {
        return None;
    }
    let parsed = Json::parse(body).ok().filter(Json::is_obj)?;
    let effort = parsed.str_at("reasoning_effort")?.to_owned();
    Some((parsed.str_at("model").unwrap_or("").to_owned(), effort))
}

fn with_reasoning_effort(body: &str, effort: Option<&str>) -> String {
    let Ok(mut parsed) = Json::parse(body) else { return body.to_owned() };
    if let Some(object) = parsed.as_obj_mut() {
        match effort {
            Some(effort) => object.insert("reasoning_effort", effort),
            None => {
                object.remove("reasoning_effort");
            }
        }
    }
    parsed.to_text()
}

fn blames_reasoning_effort(status: u16, body: &str) -> bool {
    if status != 400 && status != 422 {
        return false;
    }
    let lower = body.to_ascii_lowercase();
    lower.contains("reasoning_effort") || lower.contains("reasoning effort")
}

/// The value to send after `sent` was rejected with `message`: of the levels it names other than `sent`, the
/// highest not above `sent`, else the lowest. `None` when it names none.
fn effort_named_in(message: &str, sent: &str) -> Option<String> {
    static PATTERN: OnceLock<regex::Regex> = OnceLock::new();
    let pattern = PATTERN.get_or_init(|| regex::Regex::new(r"(?i)\b(?:low|medium|high|xhigh|max)\b").expect("a literal pattern"));
    let mut named: HashSet<String> = HashSet::new();
    for found in pattern.find_iter(message) {
        let level = found.as_str().to_ascii_lowercase();
        if level != sent {
            named.insert(level);
        }
    }
    let ranked: Vec<&str> = EFFORT_LADDER.iter().copied().filter(|level| named.contains(*level)).collect();
    let sent_rank = EFFORT_LADDER.iter().position(|level| *level == sent);
    let below = ranked.iter().filter(|level| {
        let rank = EFFORT_LADDER.iter().position(|l| l == *level);
        match (rank, sent_rank) {
            (Some(rank), Some(sent_rank)) => rank < sent_rank,
            _ => false,
        }
    });
    below.last().or(ranked.first()).map(|level| (*level).to_owned())
}

/// Retry a request whose `reasoning_effort` the endpoint rejected, with a value its rejection names, or without
/// the field. It fires only when the rejection names the field, so any other `400` is surfaced as itself.
struct RetryRejectedEffort {
    inner: DynFetch,
}

impl RetryRejectedEffort {
    async fn run(&self, mut request: HttpRequest) -> Result<HttpResponse, AiError> {
        let url = url_without_hash(&request.url);
        let key = |sent: &(String, String)| format!("{url}\n{}\n{}", sent.0, sent.1);
        let mut body = request.body.clone();
        // Start from what this endpoint already answered, through its chain of step-downs (bounded: an
        // endpoint that names levels in a circle stops).
        let mut sent = body.as_deref().and_then(reasoning_effort_of);
        for _ in 0..EFFORT_LADDER.len() {
            let (Some(text), Some(current)) = (&body, &sent) else { break };
            let instead = {
                let fallbacks = effort_fallbacks().lock().unwrap_or_else(|p| p.into_inner());
                let Some(instead) = fallbacks.get(&key(current)) else { break };
                instead.clone()
            };
            let rewritten = with_reasoning_effort(text, instead.as_deref());
            sent = instead.map(|effort| (current.0.clone(), effort));
            body = Some(rewritten);
        }
        let mut attempt = 0;
        loop {
            request.body = body.clone();
            let mut response = self.inner.fetch(request.clone()).await?;
            let sent = body.as_deref().and_then(reasoning_effort_of);
            let Some(sent) = sent else { return Ok(response) };
            if response.ok() || response.status >= 500 || attempt >= EFFORT_LADDER.len() {
                return Ok(response);
            }
            let text = response.body.read_text().await?;
            if !blames_reasoning_effort(response.status, &text) {
                return Ok(HttpResponse::with_text(response.status, &response.status_text, response.headers, text));
            }
            let instead = effort_named_in(&text, &sent.1);
            effort_fallbacks().lock().unwrap_or_else(|p| p.into_inner()).insert(key(&sent), instead.clone());
            body = body.map(|b| with_reasoning_effort(&b, instead.as_deref()));
            attempt += 1;
        }
    }
}

impl Fetch for RetryRejectedEffort {
    fn fetch(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, AiError>> {
        Box::pin(self.run(request))
    }
}

/// SSE frame carrying one choice, derived from a whole message. `finish_reason` is copied, never invented.
fn chunk_from_message(source: &Json, choice: &Json) -> String {
    let message = choice.get("message").cloned().unwrap_or_else(Json::obj);
    let delta = jobj! {
        "role": "assistant",
        "content"?: message.get("content").filter(|v| v.is_str()).cloned(),
        "reasoning_content"?: message.get("reasoning_content").filter(|v| v.is_str()).cloned(),
        "reasoning"?: message.get("reasoning").filter(|v| v.is_str()).cloned(),
        "tool_calls"?: message.get("tool_calls").filter(|v| v.is_arr()).cloned(),
    };
    jobj! {
        "id"?: source.get("id").cloned(),
        "object": "chat.completion.chunk",
        "created"?: source.get("created").cloned(),
        "model"?: source.get("model").cloned(),
        "choices": vec![jobj! { "index": 0u64, "delta": delta, "finish_reason": choice.get("finish_reason").cloned().unwrap_or_default() }],
        "usage"?: source.non_null("usage").cloned(),
    }
    .to_text()
}

/// Replay a non-streamed Chat Completions body as SSE: relays fronting a batch backend answer `stream: true`
/// with one ordinary JSON object. Only the first choice, matching what the SDK reads from a real stream.
struct NonStreamingBodyAsSse {
    inner: DynFetch,
}

impl NonStreamingBodyAsSse {
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
        if !Json::parse(&body).is_ok_and(|b| b.get("stream").and_then(Json::as_bool) == Some(true)) {
            return Ok(response);
        }
        let text = response.body.read_text().await?;
        let frame = Json::parse(&text).ok().filter(Json::is_obj).and_then(|source| {
            let choice = source.get("choices")?.as_arr()?.first()?.clone();
            // A body with no terminal reason is malformed: replaying it unchanged lets the SDK say so.
            choice.get("finish_reason").filter(|f| f.is_str())?;
            Some(chunk_from_message(&source, &choice))
        });
        let Some(frame) = frame else {
            return Ok(HttpResponse::with_text(response.status, &response.status_text, response.headers, text));
        };
        let mut headers = response.headers;
        headers.set("content-type", "text/event-stream");
        headers.delete("content-length");
        Ok(HttpResponse {
            status: response.status,
            status_text: response.status_text,
            headers,
            body: Body::text(format!("data: {frame}\n\ndata: {DONE_SENTINEL}\n\n")),
        })
    }
}

impl Fetch for NonStreamingBodyAsSse {
    fn fetch(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, AiError>> {
        Box::pin(self.run(request))
    }
}

/// Shared dialect fetch for `openai-chat` and `openai-compatible`.
pub fn chat_dialect_fetch(inner: DynFetch) -> DynFetch {
    // Innermost first: retry the request, then reshape a non-streamed body into a stream, and only then let
    // the line rewriter see ordinary SSE.
    let effort: DynFetch = Arc::new(RetryRejectedEffort { inner });
    let stream_options: DynFetch = Arc::new(RetryWithoutStreamOptions { inner: effort });
    let non_streaming: DynFetch = Arc::new(NonStreamingBodyAsSse { inner: stream_options });
    Arc::new(SseDialectFetch {
        make_rewrite: Box::new(make_chat_line_rewriter),
        patch_request: Some(Box::new(backfill_reasoning_content)),
        inner: non_streaming,
    })
}

/// `convertChatUsage`: SDK accounting, accepting validated legacy DeepSeek cache evidence.
pub fn convert_chat_usage(usage: Option<&Json>) -> ModelUsage {
    let Some(usage) = usage.filter(|u| !u.is_null()) else { return ModelUsage::default() };
    let count = |value: Option<&Json>| value.and_then(Json::as_u64);
    let prompt = count(usage.non_null("prompt_tokens")).unwrap_or(0);
    let completion = count(usage.non_null("completion_tokens")).unwrap_or(0);
    let hit = count(usage.get("prompt_cache_hit_tokens"));
    let miss_field = usage.get("prompt_cache_miss_tokens");
    let legacy = match (count(usage.get("prompt_tokens")), hit) {
        (Some(_), Some(hit)) if hit <= prompt => {
            let miss_ok = match miss_field {
                None => true,
                Some(value) => value.as_u64().is_some_and(|miss| hit + miss == prompt),
            };
            if miss_ok { hit } else { 0 }
        }
        _ => 0,
    };
    let cache_read = count(usage.path(&["prompt_tokens_details", "cached_tokens"]).filter(|v| !v.is_null())).unwrap_or(legacy);
    let reasoning = count(usage.path(&["completion_tokens_details", "reasoning_tokens"]).filter(|v| !v.is_null())).unwrap_or(0);
    ModelUsage {
        input_total: Some(prompt),
        input_no_cache: Some(prompt.saturating_sub(cache_read)),
        input_cache_read: Some(cache_read),
        input_cache_write: None,
        output_total: Some(completion),
        output_text: Some(completion.saturating_sub(reasoning)),
        output_reasoning: Some(reasoning),
        raw: Some(usage.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(lines: &[&str]) -> Vec<String> {
        let mut rewrite = make_chat_line_rewriter();
        lines
            .iter()
            .map(|line| match rewrite(line) {
                Rewrite::Keep => (*line).to_owned(),
                Rewrite::Drop => String::new(),
                Rewrite::Replace(text) => text,
                Rewrite::Fail(message) => format!("FAIL {message}"),
            })
            .collect()
    }

    #[test]
    fn legacy_function_call_frames_become_tool_calls() {
        let out = run(&[
            r#"data: {"id":"s","choices":[{"delta":{"function_call":{"name":"f","arguments":"{"}}}]}"#,
            r#"data: {"id":"s","choices":[{"delta":{"function_call":{"arguments":{"a":1}}},"finish_reason":"function_call"}]}"#,
        ]);
        assert_eq!(out[0], r#"data: {"id":"s","choices":[{"delta":{"tool_calls":[{"index":0,"id":"legacy_s","type":"function","function":{"name":"f","arguments":"{"}}]}}]}"#);
        assert_eq!(out[1], r#"data: {"id":"s","choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"a\":1}"},"id":"legacy_s"}]},"finish_reason":"tool_calls"}]}"#);
    }

    #[test]
    fn missing_ids_and_indexes_are_synthesized() {
        let out = run(&[
            r#"data: {"id":"s","choices":[{"delta":{"tool_calls":[{"function":{"name":"a","arguments":""}}]}}]}"#,
            r#"data: {"id":"s","choices":[{"delta":{"tool_calls":[{"function":{"arguments":"{}"}}]}}]}"#,
        ]);
        assert!(out[0].contains(r#""id":"synth_s_0""#) && out[0].contains(r#""index":0"#), "{}", out[0]);
        assert!(out[1].contains(r#""index":0"#) && out[1].contains(r#""id":"synth_s_0""#), "{}", out[1]);
    }

    #[test]
    fn an_index_conflict_is_a_stream_failure() {
        let out = run(&[
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"x"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"b","function":{"name":"y"}}]}}]}"#,
        ]);
        assert!(out[1].starts_with("FAIL Chat tool-call index conflict"), "{}", out[1]);
    }

    #[test]
    fn usage_only_chunks_get_choices_and_done_normalizes() {
        let out = run(&[r#"data: {"usage":{"prompt_tokens":1}}"#, "data:[DONE]", "data: [DONE]"]);
        assert_eq!(out[0], r#"data: {"usage":{"prompt_tokens":1},"choices":[]}"#);
        assert_eq!(out[1], "data: [DONE]");
        assert_eq!(out[2], "data: [DONE]");
    }

    #[test]
    fn reasoning_content_backfill_needs_evidence() {
        assert!(backfill_reasoning_content(r#"{"messages":[{"role":"assistant","tool_calls":[]}]}"#).is_none());
        let body = r#"{"messages":[{"role":"assistant","reasoning_content":"x"},{"role":"assistant","tool_calls":[]}]}"#;
        assert_eq!(backfill_reasoning_content(body).unwrap(), r#"{"messages":[{"role":"assistant","reasoning_content":"x"},{"role":"assistant","tool_calls":[],"reasoning_content":""}]}"#);
    }

    #[test]
    fn effort_steps_down_to_a_named_level() {
        assert_eq!(effort_named_in("Supported values are: 'low', 'medium' and 'high'", "xhigh").as_deref(), Some("high"));
        assert_eq!(effort_named_in("Expected 'xhigh' | 'high'", "low").as_deref(), Some("high"));
        assert_eq!(effort_named_in("reasoning_effort not supported with this model", "high"), None);
        assert!(blames_reasoning_effort(400, "bad reasoning effort"));
        assert!(!blames_reasoning_effort(404, "reasoning_effort"));
    }

    #[test]
    fn usage_reads_cached_and_legacy_counters() {
        let modern = convert_chat_usage(Some(&Json::parse(r#"{"prompt_tokens":10,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":4},"completion_tokens_details":{"reasoning_tokens":2}}"#).unwrap()));
        assert_eq!((modern.input_total, modern.input_cache_read, modern.input_no_cache), (Some(10), Some(4), Some(6)));
        assert_eq!((modern.output_total, modern.output_reasoning, modern.output_text), (Some(5), Some(2), Some(3)));
        let legacy = convert_chat_usage(Some(&Json::parse(r#"{"prompt_tokens":10,"completion_tokens":0,"prompt_cache_hit_tokens":7,"prompt_cache_miss_tokens":3}"#).unwrap()));
        assert_eq!(legacy.input_cache_read, Some(7));
        let inconsistent = convert_chat_usage(Some(&Json::parse(r#"{"prompt_tokens":10,"prompt_cache_hit_tokens":7,"prompt_cache_miss_tokens":9}"#).unwrap()));
        assert_eq!(inconsistent.input_cache_read, Some(0));
        assert_eq!(convert_chat_usage(None), ModelUsage::default());
    }

    #[test]
    fn a_whole_body_replays_only_with_a_finish_reason() {
        let source = Json::parse(r#"{"id":"s","created":1,"model":"m","usage":{"prompt_tokens":1}}"#).unwrap();
        let choice = Json::parse(r#"{"message":{"content":"hi","reasoning_content":"r"},"finish_reason":"stop"}"#).unwrap();
        assert_eq!(chunk_from_message(&source, &choice), r#"{"id":"s","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"role":"assistant","content":"hi","reasoning_content":"r"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1}}"#);
    }
}
