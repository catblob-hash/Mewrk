//! One step: `main.ts::runStep`. Runs one `streamText`, translates its parts to host events, sends `done`.
//!
//! Tools have declarations but no `execute`, so the SDK stops at a tool call and the host keeps execution,
//! approval, hooks and next-round decisions.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::error::{AiError, classify, redact_error, redact_secrets, secrets_of};
use crate::http::Cancel;
use crate::jobj;
use crate::json::Json;
use crate::protocol::{MAX_STREAM_TEXT, MAX_TOOL_ARGUMENTS, full_system_prompt};
use crate::providers::resolve_model;
use crate::reasoning::{merge_provider_options, plan_reasoning};
use crate::search::{native_fetch_tool, native_search_tool};
use crate::service::Shared;
use crate::stream_text::{PreparedStep, StepControl, StepRecord, StreamText, StreamTextArgs, TextStreamPart, ToolCallPart, ToolEntry, Usage};
use crate::tool_append::prepare_tool_append;

/// Maximum retained sources per step. More results are a data dump, not citations.
const MAX_SOURCES: usize = 64;

/// Claude Code's default `max_tokens` for the Messages protocol. The SDK would otherwise send the model's
/// whole output ceiling, and Anthropic counts `max_tokens` against the context window.
const ANTHROPIC_DEFAULT_OUTPUT_TOKENS: u64 = 32_000;

fn default_output_tokens(family: &str) -> Option<u64> {
    matches!(family, "anthropic" | "bedrock").then_some(ANTHROPIC_DEFAULT_OUTPUT_TOKENS)
}

/// `value.length`, in JavaScript's UTF-16 units.
pub(crate) fn js_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// `value.slice(0, max)` in UTF-16 units; a pair the cut would split goes whole.
pub(crate) fn js_slice(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut units = 0;
    for (at, c) in text.char_indices() {
        units += c.len_utf16();
        if units > max {
            return text[..at].to_owned();
        }
    }
    text.to_owned()
}

fn usage_json(usage: &Usage) -> Json {
    jobj! {
        "inputTokens"?: usage.input_tokens,
        "outputTokens"?: usage.output_tokens,
        "totalTokens"?: usage.total_tokens,
        "reasoningTokens"?: usage.reasoning_tokens,
        "cacheReadTokens"?: usage.cache_read_tokens,
    }
}

/// Whether provider metadata carries an encrypted reasoning payload. Scanned by value rather than by provider
/// name; an empty string is upstream scaffolding, since Anthropic opens every thinking block with an empty
/// signature before filling it.
fn has_encrypted_reasoning(metadata: Option<&Json>) -> bool {
    let Some(metadata) = metadata.and_then(Json::as_obj) else { return false };
    metadata.iter().any(|(_, entry)| {
        entry.is_obj()
            && ["redactedData", "signature"].iter().any(|key| match entry.get(key) {
                Some(Json::Str(text)) => !text.is_empty(),
                Some(Json::Null) | None => false,
                Some(_) => true,
            })
    })
}

fn has_redacted_data(metadata: Option<&Json>) -> bool {
    metadata.and_then(Json::as_obj).is_some_and(|metadata| {
        metadata.iter().any(|(_, entry)| entry.str_at("redactedData").is_some_and(|data| !data.is_empty()))
    })
}

/// Converts a server-side tool failure into readable bounded text.
fn describe_provider_tool_error(error: &Json) -> String {
    match error {
        Json::Str(text) => js_slice(text, 500),
        Json::Obj(_) => match error.str_at("errorCode") {
            Some(code) => code.to_owned(),
            None => js_slice(&error.to_text(), 500),
        },
        Json::Arr(_) => js_slice(&error.to_text(), 500),
        Json::Null => "null".to_owned(),
        other => js_slice(&other.to_text(), 500),
    }
}

fn build_tools(request: &Json, family: &str, deferred: &HashSet<String>) -> Vec<ToolEntry> {
    let mut tools: Vec<ToolEntry> = Vec::new();
    // A Responses function tool that omits `strict` is put through strict-mode normalization upstream, which
    // makes every optional parameter required. Saying `false` keeps the host schema's optional parameters
    // optional. xAI speaks the same protocol. Chat, Anthropic and the rest are non-strict when absent.
    let non_strict = matches!(family, "openai-responses" | "openai-codex" | "azure" | "xai");
    for spec in request.get("tools").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice) {
        let name = spec.str_at("name").unwrap_or("").to_owned();
        let provider_options = deferred.contains(&name).then(|| jobj! { "anthropic": jobj! { "deferLoading": true } });
        let entry = ToolEntry::Function {
            description: spec.get("description").cloned().unwrap_or_default(),
            input_schema: spec.get("inputSchema").cloned().unwrap_or_default(),
            strict: non_strict.then_some(false),
            provider_options,
            name,
        };
        upsert(&mut tools, entry);
    }
    if let Some(search) = request.get("nativeSearch") {
        let max_uses = search.get("maxUses").and_then(Json::as_u64).unwrap_or(0);
        if let Some((name, tool)) = native_search_tool(family, max_uses, search.str_at("toolType")) {
            upsert(&mut tools, ToolEntry::Provider { name, tool });
        }
    }
    if let Some(fetch) = request.get("nativeFetch") {
        let max_uses = fetch.get("maxUses").and_then(Json::as_u64).unwrap_or(0);
        let max_tokens = fetch.get("maxContentTokens").and_then(Json::as_u64);
        if let Some((name, tool)) = native_fetch_tool(family, max_uses, fetch.str_at("toolType"), max_tokens) {
            upsert(&mut tools, ToolEntry::Provider { name, tool });
        }
    }
    tools
}

/// `Object.assign` semantics: an existing name keeps its position.
fn upsert(tools: &mut Vec<ToolEntry>, entry: ToolEntry) {
    match tools.iter_mut().find(|tool| tool.name() == entry.name()) {
        Some(slot) => *slot = entry,
        None => tools.push(entry),
    }
}

fn is_search_call(tool_name: &str, provider_executed: Option<bool>) -> bool {
    provider_executed == Some(true) && (tool_name == "web_search" || tool_name == "google_search")
}

struct SearchLedger {
    prior: HashSet<String>,
    ids: Vec<String>,
    active: bool,
}

impl SearchLedger {
    fn count(&mut self, call: &ToolCallPart) {
        if self.active
            && is_search_call(&call.tool_name, call.provider_executed)
            && !self.prior.contains(&call.tool_call_id)
            && !self.ids.contains(&call.tool_call_id)
        {
            self.ids.push(call.tool_call_id.clone());
        }
    }
}

struct Control {
    ledger: Arc<Mutex<SearchLedger>>,
    family: String,
    search_tool_type: Option<String>,
    search_limit: u64,
    has_native_search: bool,
    max_steps: usize,
}

impl StepControl for Control {
    fn prepare_step(&mut self, steps: &[StepRecord], tools: &mut Vec<ToolEntry>) -> PreparedStep {
        if !self.has_native_search {
            return PreparedStep::default();
        }
        let mut ledger = self.ledger.lock().unwrap_or_else(|p| p.into_inner());
        for step in steps {
            for call in &step.tool_calls {
                ledger.count(call);
            }
        }
        if self.search_limit == 0 {
            return PreparedStep::default();
        }
        let remaining = self.search_limit.saturating_sub(ledger.ids.len() as u64);
        if remaining == 0 {
            return PreparedStep { active_tools: Some(Vec::new()), tool_choice: Some(jobj! { "type": "none" }) };
        }
        // Replace the provider tool in this request-local list.
        if let Some((name, tool)) = native_search_tool(&self.family, remaining, self.search_tool_type.as_deref()) {
            upsert(tools, ToolEntry::Provider { name, tool });
        }
        PreparedStep::default()
    }

    fn stop_when(&mut self, steps: &[StepRecord]) -> bool {
        let mut ledger = self.ledger.lock().unwrap_or_else(|p| p.into_inner());
        for step in steps {
            for call in &step.tool_calls {
                ledger.count(call);
            }
        }
        // Never begin another request after consuming the finite call budget.
        steps.len() == self.max_steps || (self.search_limit > 0 && ledger.ids.len() as u64 >= self.search_limit)
    }
}

#[derive(Default)]
struct ReasoningItem {
    ordinal: Option<usize>,
    text: String,
    emitted: bool,
    open: bool,
    /// An encrypted payload, a signature or redacted data, arrived for this item.
    encrypted: bool,
    redacted: bool,
    /// A plaintext-channel delta arrived, even an empty one.
    saw_text_frame: bool,
}

/// `reasoning-start` as the evidence gate lets it out.
struct ReasoningBook {
    items: HashMap<String, ReasoningItem>,
    order: Vec<String>,
    ordinals: usize,
    opened_at: Option<Instant>,
    open_items: usize,
    interval_emitted: bool,
    interval_candidate: bool,
    ms: u64,
    candidate_ms: u64,
    seen: bool,
    eager: bool,
}

impl ReasoningBook {
    fn new(eager: bool) -> Self {
        Self {
            items: HashMap::new(),
            order: Vec::new(),
            ordinals: 0,
            opened_at: None,
            open_items: 0,
            interval_emitted: false,
            interval_candidate: false,
            ms: 0,
            candidate_ms: 0,
            seen: false,
            eager,
        }
    }

    fn item(&mut self, key: &str) -> &mut ReasoningItem {
        if !self.items.contains_key(key) {
            self.order.push(key.to_owned());
            self.items.insert(key.to_owned(), ReasoningItem::default());
        }
        self.items.get_mut(key).expect("just inserted")
    }

    /// Duration is the union of open item intervals: it starts when the first item opens and closes only when
    /// the last closes, and counts only if at least one item passed the evidence gate. `candidate` runs the
    /// same clock for items that carry a payload but have not passed the gate.
    fn close_interval(&mut self) {
        let Some(opened) = self.opened_at.take() else { return };
        let elapsed = opened.elapsed().as_millis() as u64;
        if self.interval_emitted {
            self.ms += elapsed;
        }
        if self.interval_candidate {
            self.candidate_ms += elapsed;
        }
        self.interval_emitted = false;
        self.interval_candidate = false;
    }

    fn open_interval(&mut self, key: &str) {
        let (newly_open, emitted, encrypted) = {
            let item = self.item(key);
            let newly = !item.open;
            item.open = true;
            (newly, item.emitted, item.encrypted)
        };
        if newly_open {
            self.open_items += 1;
            if self.opened_at.is_none() {
                self.opened_at = Some(Instant::now());
            }
        }
        if emitted {
            self.interval_emitted = true;
        }
        if emitted || encrypted {
            self.interval_candidate = true;
        }
    }

    /// `(ordinal, form)` for the `reasoning-start` event.
    fn emit_start(&mut self, key: &str) -> (usize, Option<&'static str>) {
        let ordinal = self.ordinals;
        self.ordinals += 1;
        self.seen = true;
        self.interval_emitted = true;
        self.interval_candidate = true;
        let eager = self.eager;
        let item = self.item(key);
        item.emitted = true;
        item.ordinal = Some(ordinal);
        let form = if item.redacted {
            Some("encrypted")
        } else if !item.text.is_empty() && !eager {
            Some("plaintext")
        } else {
            None
        };
        (ordinal, form)
    }
}

fn reasoning_start_event(ordinal: usize, form: Option<&str>) -> Json {
    jobj! { "k": "reasoning-start", "item": ordinal, "form"?: form }
}

struct Sources {
    list: Vec<Json>,
    keys: HashSet<String>,
}

impl Sources {
    fn add(&mut self, id: &str, url: Option<&str>, title: Option<&str>) {
        let id = js_slice(id, 256);
        let url = url.map(|u| js_slice(u, 2048));
        let title = title.map(|t| js_slice(t, 512));
        let key = url.clone().unwrap_or_else(|| format!("document:{id}"));
        if self.list.len() >= MAX_SOURCES || self.keys.contains(&key) {
            return;
        }
        self.keys.insert(key);
        self.list.push(jobj! { "id": id, "url"?: url, "title"?: title });
    }

    /// A retrieved page is a source, not a claim that the answer cited it: no invented title. Provider output is
    /// untrusted, so anything that is not an http(s) URL is not a source.
    fn add_search_url(&mut self, value: Option<&Json>) {
        let Some(url) = value.and_then(Json::as_str) else { return };
        if js_len(url) > 2048 {
            return;
        }
        let lower = url.to_ascii_lowercase();
        let rest = lower.strip_prefix("https://").or_else(|| lower.strip_prefix("http://"));
        if rest.is_none_or(|rest| rest.is_empty() || rest.starts_with('/')) {
            return;
        }
        self.add(&format!("search:{url}"), Some(url), None);
    }
}

/// Keeps a provider-executed fetch result whose body is text. A PDF arrives as base64 and is dropped rather than
/// forwarded: the model cannot read it. The URL still reaches the host as a source.
fn add_web_document(documents: &mut Vec<Json>, sources: &mut Sources, value: &Json) {
    if documents.len() >= MAX_SOURCES || !value.is_obj() {
        return;
    }
    let Some(url) = value.str_at("url").filter(|u| !u.is_empty()) else { return };
    sources.add_search_url(value.get("url"));
    let Some(source) = value.path(&["content", "source"]) else { return };
    if source.str_at("type") != Some("text") {
        return;
    }
    let Some(data) = source.str_at("data") else { return };
    let title = value.path(&["content", "title"]).and_then(Json::as_str).filter(|t| !t.is_empty());
    documents.push(jobj! {
        "url": js_slice(url, 2048),
        "title"?: title.map(|t| js_slice(t, 512)),
        "text": js_slice(data, MAX_STREAM_TEXT),
    });
}

/// Runs a step to its terminal frame. Never panics its way out: the caller's `finally` is around this.
pub async fn run_step(shared: Arc<Shared>, id: String, request: Json, cancel: Cancel) {
    let secrets = secrets_of(&request);
    match drive(&shared, &id, request, cancel).await {
        Ok(result) => shared.done(&id, result),
        Err(error) => shared.fail(&id, redact_error(classify(&error), &secrets)),
    }
}

async fn drive(shared: &Arc<Shared>, id: &str, mut request: Json, cancel: Cancel) -> Result<Json, AiError> {
    let secrets = secrets_of(&request);
    let family = request.str_at("family").unwrap_or("").to_owned();
    let model_id = request.str_at("modelId").unwrap_or("").to_owned();
    let client_tool_names: HashSet<String> = request
        .get("tools")
        .and_then(Json::as_arr)
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .filter_map(|spec| spec.str_at("name"))
        .map(str::to_owned)
        .collect();
    let filter_search_phases =
        request.get("nativeSearch").is_some() && matches!(family.as_str(), "openai-responses" | "openai-codex" | "azure");
    let mut text = String::new();
    let mut text_units = 0usize;
    let mut text_step = 0usize;
    // `{step}:{id}` in first-seen order, with the item's text and phase.
    let mut text_items: Vec<(String, String, Option<String>)> = Vec::new();

    let mut book = ReasoningBook::new(request.str_at("reasoningContent") == Some("encrypted"));
    let mut calls: Vec<Json> = Vec::new();
    let mut sources = Sources { list: Vec::new(), keys: HashSet::new() };
    let mut web_documents: Vec<Json> = Vec::new();
    let mut provider_tool_errors: Vec<Json> = Vec::new();
    let mut announced: HashSet<String> = HashSet::new();
    let mut usage = Usage::default();
    let mut finish_reason: Option<String> = None;
    let mut raw_finish_reason: Option<String> = None;

    let model = resolve_model(&request)?;
    // Tools that joined mid-conversation reach the model through the family's own append interface, where the
    // host says the model takes it; elsewhere they stay in the declared list.
    let messages = request.as_obj_mut().and_then(|o| o.remove("messages")).and_then(|m| match m {
        Json::Arr(messages) => Some(messages),
        _ => None,
    }).unwrap_or_default();
    let tool_append = request.get("toolAppend").and_then(Json::as_bool) == Some(true);
    let appended = prepare_tool_append(&family, messages, &client_tool_names, tool_append);
    let mut messages = appended.messages;
    // History replays the provider's own signed reasoning parts, tagged by the host with the model that made
    // them. Anthropic binds a signature to that model, so a switched conversation drops them; the tag itself
    // never reaches a provider.
    if family == "anthropic" || family == "bedrock" {
        crate::dialect::anthropic::drop_foreign_signed_reasoning(&mut messages, &model_id);
    }
    crate::dialect::anthropic::strip_replay_tags(&mut messages);
    // Replay only original Anthropic payloads; never manufacture a signature. Empty turns stay.
    if family == "anthropic" {
        crate::dialect::anthropic::drop_unsigned_reasoning(&mut messages);
    }
    let tools = build_tools(&request, &family, &appended.deferred);
    let mut prior: HashSet<String> = request
        .path(&["nativeSearch", "previousCallIds"])
        .and_then(Json::as_arr)
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .filter_map(Json::as_str)
        .map(str::to_owned)
        .collect();
    for message in &messages {
        for part in message.get("content").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice) {
            if part.str_at("type") == Some("tool-call")
                && is_search_call(part.str_at("toolName").unwrap_or(""), part.get("providerExecuted").and_then(Json::as_bool))
            {
                if let Some(id) = part.str_at("toolCallId") {
                    prior.insert(id.to_owned());
                }
            }
        }
    }
    let has_native_search = request.get("nativeSearch").is_some();
    let ledger = Arc::new(Mutex::new(SearchLedger { prior, ids: Vec::new(), active: has_native_search }));
    let search_limit = request.path(&["nativeSearch", "maxUses"]).and_then(Json::as_u64).unwrap_or(0);
    let output_ceiling = request.get("maxOutputTokens").and_then(Json::as_u64).or_else(|| default_output_tokens(&family));
    // The level becomes this family's and this model's own controls.
    let plan = plan_reasoning(&family, &model_id, request.str_at("reasoning"), output_ceiling);
    let max_steps = request.get("maxSteps").and_then(Json::as_u64).unwrap_or(1).max(1) as usize;
    let allow_system = appended.system_in_messages || messages.iter().any(|m| m.str_at("role") == Some("system"));
    let provider_options = merge_provider_options(request.get("providerOptions"), plan.provider_options.as_ref());
    let control = Control {
        ledger: ledger.clone(),
        family: family.clone(),
        search_tool_type: request.path(&["nativeSearch", "toolType"]).and_then(Json::as_str).map(str::to_owned),
        search_limit,
        has_native_search,
        max_steps,
    };
    let mut stream = StreamText::new(StreamTextArgs {
        model,
        system: full_system_prompt(&request),
        messages,
        allow_system_in_messages: allow_system,
        tools,
        max_output_tokens: plan.max_output_tokens.or(output_ceiling),
        reasoning: plan.reasoning,
        provider_options,
        cancel: cancel.clone(),
        control: Box::new(control),
    });

    while let Some(part) = stream.next().await {
        match part {
            TextStreamPart::StartStep => text_step += 1,
            TextStreamPart::TextStart { id: part_id, provider_metadata } | TextStreamPart::TextEnd { id: part_id, provider_metadata } => {
                if !filter_search_phases {
                    continue;
                }
                let key = format!("{text_step}:{part_id}");
                let at = match text_items.iter().position(|(k, _, _)| *k == key) {
                    Some(at) => at,
                    None => {
                        text_items.push((key, String::new(), None));
                        text_items.len() - 1
                    }
                };
                // The final item can carry phase even when the start did not. Never decide from deltas or alter
                // the opaque continuation or the live stream.
                let phase = provider_metadata
                    .as_ref()
                    .and_then(|m| m.path(&["openai", "phase"]).filter(|p| !p.is_null()).or_else(|| m.path(&["azure", "phase"])))
                    .and_then(Json::as_str);
                if let Some(phase) = phase {
                    text_items[at].2 = Some(phase.to_owned());
                }
            }
            TextStreamPart::TextDelta { id: part_id, text: delta } => {
                let units = js_len(&delta);
                if text_units + units > MAX_STREAM_TEXT {
                    return Err(AiError::other(format!("单轮可见文本超过 {MAX_STREAM_TEXT} 字节上限")));
                }
                text_units += units;
                text.push_str(&delta);
                if filter_search_phases {
                    let key = format!("{text_step}:{part_id}");
                    match text_items.iter_mut().find(|(k, _, _)| *k == key) {
                        Some(item) => item.1.push_str(&delta),
                        None => text_items.push((key, delta.clone(), None)),
                    }
                }
                shared.emit(id, jobj! { "k": "text-delta", "delta": delta });
            }
            TextStreamPart::ReasoningStart { id: part_id, provider_metadata } => {
                // Timing starts at item start. The evidence gate controls emission, not the measured interval.
                book.open_interval(&part_id);
                // Redacted thinking is an alternative encrypted representation: no delta, but proof.
                if has_encrypted_reasoning(provider_metadata.as_ref()) {
                    let redacted = has_redacted_data(provider_metadata.as_ref());
                    let item = book.item(&part_id);
                    item.redacted = redacted;
                    item.encrypted = true;
                    let emitted = item.emitted;
                    book.interval_candidate = true;
                    if !emitted {
                        let (ordinal, form) = book.emit_start(&part_id);
                        shared.emit(id, reasoning_start_event(ordinal, form));
                    }
                } else if !book.item(&part_id).emitted && book.eager {
                    let (ordinal, form) = book.emit_start(&part_id);
                    shared.emit(id, reasoning_start_event(ordinal, form));
                }
            }
            TextStreamPart::ReasoningDelta { id: part_id, text: delta, provider_metadata } => {
                // Some families emit deltas without starts; the first delta then starts the clock.
                book.open_interval(&part_id);
                let encrypted = has_encrypted_reasoning(provider_metadata.as_ref());
                {
                    let item = book.item(&part_id);
                    item.text.push_str(&delta);
                    if encrypted {
                        item.encrypted = true;
                    } else {
                        item.saw_text_frame = true;
                    }
                }
                if encrypted {
                    book.interval_candidate = true;
                }
                if delta.is_empty() {
                    // An empty delta is not reasoning text, but two different upstreams send one. A relay in
                    // front of Claude strips the text and puts the whole item in the signature: the empty
                    // plaintext frame proves a thinking channel existed, so the signature earns a card. DeepSeek's
                    // endpoint opens a bare thinking block on every tool continuation whose only delta is a
                    // pseudo-signature: nothing was thought. Requiring both signals separates them. A
                    // signature-only item is settled at the end of the stream, by the provider's token count.
                    let item = book.item(&part_id);
                    if !item.emitted && item.encrypted && item.saw_text_frame {
                        let (ordinal, form) = book.emit_start(&part_id);
                        shared.emit(id, reasoning_start_event(ordinal, form));
                    }
                    continue;
                }
                if !book.item(&part_id).emitted {
                    let (ordinal, form) = book.emit_start(&part_id);
                    shared.emit(id, reasoning_start_event(ordinal, form));
                }
                let ordinal = book.item(&part_id).ordinal.unwrap_or(0);
                shared.emit(id, jobj! { "k": "reasoning-delta", "item": ordinal, "delta": delta });
            }
            TextStreamPart::ReasoningEnd { id: part_id } => {
                let was_open = {
                    let item = book.item(&part_id);
                    let open = item.open;
                    item.open = false;
                    open
                };
                if was_open {
                    book.open_items -= 1;
                    // Close only when the last open item ends, so interleaved items keep their tail.
                    if book.open_items == 0 {
                        book.close_interval();
                    }
                }
                let item = book.item(&part_id);
                if item.emitted {
                    let ordinal = item.ordinal.unwrap_or(0);
                    let ms = book.ms;
                    shared.emit(id, jobj! { "k": "reasoning-done", "item": ordinal, "durationMs": ms });
                }
            }
            TextStreamPart::ToolInputStart { id: call_id, tool_name, provider_executed } => {
                // Never report provider-executed tools to the host: it cannot execute them, and a `ToolCall` for
                // one rejects the turn as an unknown tool and breaks `pause_turn`.
                if provider_executed == Some(true) || !client_tool_names.contains(&tool_name) {
                    continue;
                }
                if announced.insert(call_id.clone()) {
                    shared.emit(id, jobj! { "k": "tool-call-announced", "callId": call_id, "toolName": tool_name });
                }
            }
            TextStreamPart::ToolCall(call) => {
                ledger.lock().unwrap_or_else(|p| p.into_inner()).count(&call);
                if call.provider_executed == Some(true) || !client_tool_names.contains(&call.tool_name) {
                    continue;
                }
                let encoded = if call.input.is_null() { "{}".to_owned() } else { call.input.to_text() };
                if js_len(&encoded) > MAX_TOOL_ARGUMENTS {
                    return Err(AiError::other(format!("工具 {} 的参数超过 {MAX_TOOL_ARGUMENTS} 字节上限", call.tool_name)));
                }
                if announced.insert(call.tool_call_id.clone()) {
                    shared.emit(id, jobj! { "k": "tool-call-announced", "callId": call.tool_call_id.as_str(), "toolName": call.tool_name.as_str() });
                }
                calls.push(jobj! { "callId": call.tool_call_id.as_str(), "toolName": call.tool_name.as_str(), "input": call.input.clone() });
                shared.emit(id, jobj! { "k": "tool-call", "callId": call.tool_call_id.as_str(), "input": call.input.clone() });
            }
            TextStreamPart::Source(source) => {
                sources.add(source.str_at("id").unwrap_or(""), source.str_at("url"), source.str_at("title"));
            }
            TextStreamPart::ToolResult { tool_name, output, provider_executed, .. } => {
                if !provider_executed {
                    continue;
                }
                if tool_name == "web_fetch" {
                    add_web_document(&mut web_documents, &mut sources, &output);
                    continue;
                }
                if tool_name != "web_search" || !matches!(family.as_str(), "openai-responses" | "openai-codex" | "azure") {
                    continue;
                }
                match output.path(&["action", "type"]).and_then(Json::as_str) {
                    Some("openPage") | Some("findInPage") => sources.add_search_url(output.path(&["action", "url"])),
                    Some("search") => {
                        if let Some(found) = output.get("sources").and_then(Json::as_arr) {
                            for source in found {
                                if sources.list.len() >= MAX_SOURCES {
                                    break;
                                }
                                if source.is_obj() {
                                    sources.add_search_url(source.get("url"));
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            TextStreamPart::ToolError { tool_name, error, provider_executed } => {
                // A failed provider-executed tool: preserve the failure so native search does not claim
                // completion. Client tools have no `execute`, and their invalid calls are not failures here.
                if !provider_executed {
                    continue;
                }
                provider_tool_errors.push(jobj! {
                    "toolName": tool_name,
                    "message": redact_secrets(&describe_provider_tool_error(&error), &secrets),
                });
            }
            TextStreamPart::FinishStep { usage: step } => {
                usage = step;
                shared.emit(id, jobj! { "k": "usage", "usage": usage_json(&usage) });
            }
            TextStreamPart::Finish { usage: total, finish_reason: reason, raw_finish_reason: raw } => {
                usage = total;
                finish_reason = Some(reason);
                // The upstream value: `pause_turn` normalizes to `stop`, so host continuation inspects this.
                raw_finish_reason = raw;
                shared.emit(id, jobj! { "k": "usage", "usage": usage_json(&usage) });
            }
            TextStreamPart::Error(error) => return Err(error),
        }
    }
    if cancel.is_cancelled() {
        return Err(AiError::Abort);
    }

    // Settle reasoning in ordinal order, because end events can arrive out of order. A provider that omits an
    // end event has its interval closed at stream end; items that did not pass the gate stay absent.
    book.close_interval();
    // Last chance for encrypted-only items: an endpoint that strips the plaintext frames entirely leaves the
    // signature as the stream's only trace, and the billed reasoning-token count is the deciding evidence.
    if !book.seen && usage.reasoning_tokens.unwrap_or(0) > 0 {
        let encrypted: Vec<String> = book.order.iter().filter(|key| book.items[*key].encrypted).cloned().collect();
        if !encrypted.is_empty() {
            book.ms = book.candidate_ms;
            for key in encrypted {
                let (ordinal, form) = book.emit_start(&key);
                shared.emit(id, reasoning_start_event(ordinal, form));
                shared.emit(id, jobj! { "k": "reasoning-done", "item": ordinal, "durationMs": book.ms });
            }
        }
    }
    let mut ordered: Vec<&ReasoningItem> = book.items.values().filter(|item| item.emitted).collect();
    ordered.sort_by_key(|item| item.ordinal.unwrap_or(usize::MAX));
    let reasoning: Vec<Json> = ordered.iter().map(|item| Json::from(item.text.as_str())).collect();

    if filter_search_phases {
        let saw_final = text_items.iter().any(|(_, _, phase)| phase.as_deref() == Some("final_answer"));
        text = text_items
            .iter()
            .filter(|(_, _, phase)| if saw_final { phase.as_deref() == Some("final_answer") } else { phase.as_deref() != Some("commentary") })
            .map(|(_, text, _)| text.as_str())
            .collect();
    }
    let Some(response) = stream.response() else {
        return Err(AiError::other("No output generated. Check the stream for errors."));
    };
    let search_ids = ledger.lock().unwrap_or_else(|p| p.into_inner()).ids.clone();
    Ok(jobj! {
        "text": text,
        "reasoning": reasoning,
        // Absent when no reasoning item exists: the host tells that apart from reasoning under a millisecond.
        "reasoningMs"?: book.seen.then_some(book.ms),
        "calls": calls,
        "usage": usage_json(&usage),
        "model": response.response_model_id.as_str(),
        "finishReason"?: finish_reason,
        "rawFinishReason"?: raw_finish_reason,
        // Opaque continuation blocks keep encrypted content, signatures and reasoning items across turns.
        "responseMessages": response.response_messages.clone(),
        "sources": sources.list,
        "webDocuments"?: (!web_documents.is_empty()).then_some(web_documents),
        "nativeSearchUses"?: has_native_search.then_some(search_ids.len()),
        "nativeSearchCallIds"?: has_native_search.then(|| search_ids.iter().map(Json::from).collect::<Vec<_>>()),
        // Server-side tool failure facts. Absence means none failed.
        "providerToolErrors"?: (!provider_tool_errors.is_empty()).then_some(provider_tool_errors),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_length_counts_utf16() {
        assert_eq!(js_len("a😀"), 3);
        assert_eq!(js_slice("a😀b", 2), "a");
        assert_eq!(js_slice("a😀b", 3), "a😀");
        assert_eq!(js_slice("abc", 10), "abc");
    }

    #[test]
    fn encrypted_evidence_scans_values() {
        let meta = |text: &str| Json::parse(text).unwrap();
        assert!(!has_encrypted_reasoning(None));
        assert!(!has_encrypted_reasoning(Some(&meta(r#"{"anthropic":{"signature":""}}"#))));
        assert!(has_encrypted_reasoning(Some(&meta(r#"{"anthropic":{"signature":"s"}}"#))));
        assert!(has_encrypted_reasoning(Some(&meta(r#"{"other":{"redactedData":"d"}}"#))));
        assert!(has_redacted_data(Some(&meta(r#"{"other":{"redactedData":"d"}}"#))));
        assert!(!has_redacted_data(Some(&meta(r#"{"anthropic":{"signature":"s"}}"#))));
    }

    #[test]
    fn provider_tool_errors_read_short() {
        assert_eq!(describe_provider_tool_error(&Json::parse(r#"{"type":"x","errorCode":"max_uses_exceeded"}"#).unwrap()), "max_uses_exceeded");
        assert_eq!(describe_provider_tool_error(&Json::from("plain")), "plain");
        assert_eq!(describe_provider_tool_error(&Json::parse(r#"{"a":1}"#).unwrap()), r#"{"a":1}"#);
    }
}
