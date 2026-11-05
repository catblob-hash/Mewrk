//! Message shapes and transcript synthesis (`claude-agent.ts`).
//!
//! Host messages arrive in AI SDK `ModelMessage` shape; the CLI speaks Anthropic Messages blocks. Both directions are
//! handled here so the rest of the family can stay in one vocabulary. The CLI resumes from a Claude Code transcript
//! (JSONL entries), so host history is rewritten into that shape: signed reasoning becomes `thinking`, redacted
//! reasoning `redacted_thinking`, unsigned reasoning is dropped (the CLI would strip it), tool messages become
//! `tool_result` user entries with any following carriers folded into the same entry, and a tool-append marker becomes
//! the pair of entries the CLI itself writes when a tool joins its session.

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::async_tools::is_host_message;
use crate::json::{Json, Obj};
use crate::tool_append::marker_tools;
use crate::{jarr, jobj};

/// Name of the in-process MCP server that publishes the host's tools.
pub(super) const SERVER_NAME: &str = "mewrk";
/// Prefix the CLI would apply without `CLAUDE_AGENT_SDK_MCP_NO_PREFIX`.
const TOOL_PREFIX: &str = "mcp__mewrk__";
/// Text markers that identify carrier user messages riding behind tool results: the image bridge, and host notices
/// from builds that wrote them as user text before they carried the host's mark.
const LEGACY_HOST_NOTICE_MARKER: &str = "[SYSTEM NOTIFICATION - NOT USER INPUT]";
const IMAGE_BRIDGE_MARKER: &str = "[Mewrk tool image]";
/// No-op host tool whose call/result pair the host writes for each host message where the conversation's host
/// messages come in `box`.
const BOX_TOOL_NAME: &str = "box";
/// Placeholder Claude Code writes for an assistant message emptied by repair.
const NO_CONTENT_PLACEHOLDER: &str = "(no content)";

pub(super) fn strip_tool_prefix(name: &str) -> &str {
    name.strip_prefix(TOOL_PREFIX).unwrap_or(name)
}

/// `crypto.randomUUID()`.
pub(super) fn random_uuid() -> String {
    let mut bytes = [0u8; 16];
    fastrand::fill(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: Vec<String> = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        hex[..4].concat(),
        hex[4..6].concat(),
        hex[6..8].concat(),
        hex[8..10].concat(),
        hex[10..].concat()
    )
}

/// `Date.now()`.
pub(super) fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// `new Date(ms).toISOString()`.
pub(super) fn iso_time(ms: u64) -> String {
    let days = (ms / 86_400_000) as i64;
    let in_day = ms % 86_400_000;
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        in_day / 3_600_000,
        in_day / 60_000 % 60,
        in_day / 1000 % 60,
        in_day % 1000
    )
}

/// MCP `CallToolResult` content items this family produces.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum McpContent {
    Text(String),
    Image { data: String, mime_type: String },
}

impl McpContent {
    fn to_json(&self) -> Json {
        match self {
            McpContent::Text(text) => jobj! { "type": "text", "text": text.as_str() },
            McpContent::Image { data, mime_type } => jobj! { "type": "image", "data": data.as_str(), "mimeType": mime_type.as_str() },
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct McpToolResult {
    pub content: Vec<McpContent>,
    pub is_error: bool,
}

impl McpToolResult {
    pub fn to_json(&self) -> Json {
        let content: Vec<Json> = self.content.iter().map(McpContent::to_json).collect();
        jobj! { "content": content, "isError"?: self.is_error.then_some(true) }
    }
}

pub(super) struct ToolResultPart<'a> {
    pub tool_call_id: &'a str,
    pub tool_name: &'a str,
    pub output: Option<&'a Json>,
}

fn is_object(value: &Json) -> bool {
    value.is_obj()
}

/// `toolResultParts`.
pub(super) fn tool_result_parts(message: &Json) -> Vec<ToolResultPart<'_>> {
    if !is_object(message) || message.str_at("role") != Some("tool") {
        return Vec::new();
    }
    let Some(content) = message.get("content").and_then(Json::as_arr) else { return Vec::new() };
    content
        .iter()
        .filter(|part| part.str_at("type") == Some("tool-result"))
        .filter_map(|part| {
            Some(ToolResultPart {
                tool_call_id: part.str_at("toolCallId")?,
                tool_name: part.str_at("toolName").unwrap_or(""),
                output: part.get("output"),
            })
        })
        .collect()
}

/// `/^data:([^;,]+);base64,(.*)$/s`.
fn parse_data_url(value: Option<&Json>) -> Option<(&str, &str)> {
    let text = value?.as_str()?;
    let rest = text.strip_prefix("data:")?;
    let (media_type, data) = rest.split_once(";base64,")?;
    if media_type.is_empty() || media_type.contains([';', ',']) {
        return None;
    }
    Some((media_type, data))
}

/// `jsonText`: bounded JSON text for a value that must become a text block.
fn json_text(value: Option<&Json>) -> String {
    match value {
        Some(Json::Str(text)) => text.clone(),
        Some(other) => other.to_text(),
        None => "null".to_owned(),
    }
}

/// `mcpResultOf`: AI SDK tool-result `output` to MCP result content.
pub(super) fn mcp_result_of(output: Option<&Json>) -> McpToolResult {
    let text = |value: Option<&Json>| McpContent::Text(json_text(value));
    let Some(object) = output.filter(|value| is_object(value)) else {
        return McpToolResult { content: vec![text(output)], is_error: false };
    };
    let value = object.get("value");
    match object.str_at("type") {
        Some("text") | Some("json") => McpToolResult { content: vec![text(value)], is_error: false },
        Some("error-text") | Some("error-json") => McpToolResult { content: vec![text(value)], is_error: true },
        Some("content") => {
            let mut content = Vec::new();
            for item in value.and_then(Json::as_arr).map_or(&[][..], Vec::as_slice) {
                match (item.str_at("type"), item.str_at("text"), item.str_at("data"), item.str_at("mediaType")) {
                    (Some("text"), Some(text), _, _) => content.push(McpContent::Text(text.to_owned())),
                    (Some("media"), _, Some(data), Some(media_type)) => {
                        content.push(McpContent::Image { data: data.to_owned(), mime_type: media_type.to_owned() });
                    }
                    _ => {}
                }
            }
            McpToolResult { content, is_error: false }
        }
        _ => McpToolResult { content: vec![text(output)], is_error: false },
    }
}

/// `mcpContentOfUserMessage`: AI SDK user-message parts to MCP content items, to fold carriers into a tool result.
fn mcp_content_of_user_message(message: &Json) -> Vec<McpContent> {
    if !is_object(message) {
        return Vec::new();
    }
    if let Some(text) = message.str_at("content") {
        return if text.is_empty() { Vec::new() } else { vec![McpContent::Text(text.to_owned())] };
    }
    let mut content = Vec::new();
    for part in message.get("content").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice) {
        match part.str_at("type") {
            Some("text") => {
                if let Some(text) = part.str_at("text") {
                    content.push(McpContent::Text(text.to_owned()));
                }
            }
            Some("image") => {
                if let Some((media_type, data)) = parse_data_url(part.get("image")) {
                    content.push(McpContent::Image { data: data.to_owned(), mime_type: media_type.to_owned() });
                }
            }
            _ => {}
        }
    }
    content
}

/// `anthropicUserBlocks`: Anthropic content blocks of a user message; `None` when it carries nothing.
pub(super) fn anthropic_user_blocks(message: &Json) -> Option<Vec<Json>> {
    if let Some(text) = message.str_at("content") {
        return (!text.is_empty()).then(|| vec![jobj! { "type": "text", "text": text }]);
    }
    let mut blocks = Vec::new();
    for part in message.get("content").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice) {
        match part.str_at("type") {
            Some("text") => {
                if let Some(text) = part.str_at("text") {
                    blocks.push(jobj! { "type": "text", "text": text });
                }
            }
            Some("image") => {
                if let Some((media_type, data)) = parse_data_url(part.get("image")) {
                    blocks.push(jobj! {
                        "type": "image",
                        "source": jobj! { "type": "base64", "media_type": media_type, "data": data },
                    });
                } else if let Some(url) = part.str_at("image") {
                    blocks.push(jobj! { "type": "image", "source": jobj! { "type": "url", "url": url } });
                }
            }
            // A short PDF the host sends as the document itself, as Claude Code's Read hands one to the model; the
            // CLI passes `document` blocks through.
            Some("file") if part.str_at("mediaType") == Some("application/pdf") => {
                if let Some((_, data)) = parse_data_url(part.get("data")) {
                    blocks.push(jobj! {
                        "type": "document",
                        "source": jobj! { "type": "base64", "media_type": "application/pdf", "data": data },
                        "title"?: part.str_at("filename").filter(|name| !name.is_empty()),
                    });
                }
            }
            _ => {}
        }
    }
    (!blocks.is_empty()).then_some(blocks)
}

/// `anthropicToolResultBlock`.
fn anthropic_tool_result_block(part: &ToolResultPart<'_>) -> Json {
    let result = mcp_result_of(part.output);
    let content: Vec<Json> = result
        .content
        .iter()
        .map(|item| match item {
            McpContent::Text(text) => jobj! { "type": "text", "text": text.as_str() },
            McpContent::Image { data, mime_type } => jobj! {
                "type": "image",
                "source": jobj! { "type": "base64", "media_type": mime_type.as_str(), "data": data.as_str() },
            },
        })
        .collect();
    jobj! {
        "type": "tool_result",
        "tool_use_id": part.tool_call_id,
        "content": content,
        "is_error"?: result.is_error.then_some(true),
    }
}

/// `String.prototype.trimStart`.
fn js_trim_start(text: &str) -> &str {
    text.trim_start_matches(|c: char| (c.is_whitespace() && c != '\u{85}') || c == '\u{FEFF}')
}

/// `isCarrierMessage`: whether a user message is a host carrier, a message the host wrote (a task notification, a
/// reminder) or a tool-image bridge, not a prompt. A host message in `box` is a carrier too: its call is skipped and
/// its result folded the same way.
fn is_carrier_message(message: &Json) -> bool {
    if is_host_message(message) {
        return true;
    }
    if !is_object(message) || message.str_at("role") != Some("user") {
        return false;
    }
    let text = match message.get("content") {
        Some(Json::Str(text)) => Some(text.as_str()),
        Some(Json::Arr(parts)) => parts.iter().find(|part| part.str_at("type") == Some("text")).and_then(|part| part.str_at("text")),
        _ => None,
    };
    let Some(text) = text else { return false };
    let trimmed = js_trim_start(text);
    trimmed.starts_with(LEGACY_HOST_NOTICE_MARKER) || trimmed.starts_with(IMAGE_BRIDGE_MARKER)
}

/// `isBoxCallMessage`: whether an assistant message is the host's fabricated `box` call, not a turn the CLI produced.
fn is_box_call_message(message: &Json) -> bool {
    if message.str_at("role") != Some("assistant") {
        return false;
    }
    let Some(content) = message.get("content").and_then(Json::as_arr) else { return false };
    !content.is_empty()
        && content.iter().all(|part| {
            part.str_at("type") == Some("tool-call")
                && part.str_at("toolName").is_some_and(|name| strip_tool_prefix(name) == BOX_TOOL_NAME)
        })
}

fn is_box_result_part(part: &ToolResultPart<'_>) -> bool {
    strip_tool_prefix(part.tool_name) == BOX_TOOL_NAME
}

/// What the request messages split into: what the CLI must already know, the results owed for a parked round, and the
/// user prompt that starts a new turn.
pub(super) struct SplitMessages {
    pub history: Vec<Json>,
    /// Tool results after the last assistant message, by tool_use id.
    pub results: HashMap<String, McpToolResult>,
    /// Carrier content (bridge images, notices) that rides behind the results.
    pub carriers: Vec<McpContent>,
    /// Genuine trailing user message(s) merged into one prompt.
    pub prompt: Option<Vec<Json>>,
    /// Tool-append markers after the prompt: the tools that joined the conversation with it. `history` stops before the
    /// prompt, so they are not part of it.
    pub prompt_additions: Vec<Vec<String>>,
}

/// `splitMessages`.
pub(super) fn split_messages(messages: &[Json]) -> SplitMessages {
    let last_assistant = messages
        .iter()
        .rposition(|message| message.str_at("role") == Some("assistant") && !is_box_call_message(message));
    let tail_start = last_assistant.map_or(0, |index| index + 1);
    let mut results = HashMap::new();
    let mut carriers = Vec::new();
    let mut prompt_blocks = Vec::new();
    let mut prompt_additions = Vec::new();
    let mut prompt_start: Option<usize> = None;
    for (offset, message) in messages[tail_start..].iter().enumerate() {
        if !is_object(message) {
            continue;
        }
        if let Some(added) = marker_tools(message) {
            if prompt_start.is_some() {
                prompt_additions.push(added);
            }
            continue;
        }
        match message.str_at("role") {
            Some("tool") => {
                for part in tool_result_parts(message) {
                    if is_box_result_part(&part) {
                        carriers.extend(mcp_result_of(part.output).content);
                    } else {
                        results.insert(part.tool_call_id.to_owned(), mcp_result_of(part.output));
                    }
                }
            }
            Some("user") => {
                if prompt_start.is_none() && is_carrier_message(message) {
                    carriers.extend(mcp_content_of_user_message(message));
                    continue;
                }
                if prompt_start.is_none() {
                    prompt_start = Some(offset);
                }
                if let Some(blocks) = anthropic_user_blocks(message) {
                    prompt_blocks.extend(blocks);
                }
            }
            // The `box` call the scan skipped is history, never a prompt.
            _ => {}
        }
    }
    let history = match prompt_start {
        Some(offset) => messages[..tail_start + offset].to_vec(),
        None => messages.to_vec(),
    };
    SplitMessages {
        history,
        results,
        carriers,
        prompt: (!prompt_blocks.is_empty()).then_some(prompt_blocks),
        prompt_additions,
    }
}

/// What a transcript is stamped with.
pub(super) struct TranscriptContext<'a> {
    pub session_id: &'a str,
    pub cwd: &'a str,
    pub cli_version: &'a str,
    /// Model name stamped on assistant entries.
    pub model: &'a str,
    /// The session's tools by name, when the CLI takes tool changes for this model; `None` drops every tool addition,
    /// and the tools it named are then declared like any other.
    pub appendable: Option<HashMap<String, Json>>,
    /// Tool additions that follow the prompt the session starts with.
    pub prompt_additions: &'a [Vec<String>],
}

/// `toolAdditionEntries`: the entries the CLI writes when tools join its session mid-way, a `deferred_tools_delta`
/// naming them and a `deferred_tools_record` with the definitions it declared for them. Resumed, the pair keeps them
/// where they joined, each declared `defer_loading`; a list that changed in place would lose the prompt cache behind
/// it. The shape is the CLI's own, field for field; `rendered` is left out and rendered afresh.
fn tool_addition_entries(tools: &[&Json]) -> [Json; 2] {
    let mut names: Vec<&str> = tools.iter().filter_map(|tool| tool.str_at("name")).collect();
    names.sort_unstable();
    let name_list = || Json::Arr(names.iter().map(|name| Json::from(*name)).collect());
    let entries: Vec<Json> = tools
        .iter()
        .map(|tool| {
            jobj! {
                "name": tool.get("name").cloned().unwrap_or(Json::Null),
                "description"?: tool.get("description").cloned(),
                "input_schema"?: tool.get("inputSchema").cloned(),
                "defer_loading": true,
            }
        })
        .collect();
    [
        jobj! {
            "type": "attachment",
            "attachment": jobj! {
                "type": "deferred_tools_delta",
                "addedNames": name_list(),
                "addedLines": name_list(),
                "removedNames": jarr![],
                "wireHiddenNames": jarr![],
                "readdedNames": jarr![],
                "pendingMcpServers": jarr![],
                "needsAuthMcpServers": jarr![],
                "failedMcpServers": jarr![],
                "surfacedNames": name_list(),
                "toolSearchAbsent": true,
            },
        },
        jobj! {
            "type": "attachment",
            "attachment": jobj! { "type": "deferred_tools_record", "entries": entries },
        },
    ]
}

/// `reasoningPayload`: the signature or redacted data of a reasoning part, from whichever provider bucket holds it.
enum ReasoningPayload<'a> {
    Signature(&'a str),
    Redacted(&'a str),
    None,
}

fn reasoning_payload(part: &Json) -> ReasoningPayload<'_> {
    let Some(options) = part.get("providerOptions").and_then(Json::as_obj) else { return ReasoningPayload::None };
    for (_, bucket) in options.iter() {
        if !is_object(bucket) {
            continue;
        }
        if let Some(signature) = bucket.str_at("signature").filter(|value| !value.is_empty()) {
            return ReasoningPayload::Signature(signature);
        }
        if let Some(data) = bucket.str_at("redactedData").filter(|value| !value.is_empty()) {
            return ReasoningPayload::Redacted(data);
        }
    }
    ReasoningPayload::None
}

/// `anthropicAssistantBlocks`.
fn anthropic_assistant_blocks(message: &Json) -> Vec<Json> {
    if let Some(text) = message.str_at("content") {
        return if text.is_empty() { Vec::new() } else { vec![jobj! { "type": "text", "text": text }] };
    }
    let mut blocks = Vec::new();
    for part in message.get("content").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice) {
        match part.str_at("type") {
            Some("reasoning") => match reasoning_payload(part) {
                ReasoningPayload::Signature(signature) => blocks.push(jobj! {
                    "type": "thinking",
                    "thinking": part.str_at("text").unwrap_or(""),
                    "signature": signature,
                }),
                ReasoningPayload::Redacted(data) => blocks.push(jobj! { "type": "redacted_thinking", "data": data }),
                ReasoningPayload::None => {}
            },
            Some("text") => {
                if let Some(text) = part.str_at("text").filter(|text| !text.is_empty()) {
                    blocks.push(jobj! { "type": "text", "text": text });
                }
            }
            Some("tool-call") => {
                if let (Some(id), Some(name)) = (part.str_at("toolCallId"), part.str_at("toolName")) {
                    blocks.push(jobj! {
                        "type": "tool_use",
                        "id": id,
                        "name": strip_tool_prefix(name),
                        "input": part.get("input").filter(|input| is_object(input)).cloned().unwrap_or_else(Json::obj),
                    });
                }
            }
            _ => {}
        }
    }
    blocks
}

fn is_thinking_block(block: &Json) -> bool {
    matches!(block.str_at("type"), Some("thinking") | Some("redacted_thinking"))
}

/// `synthesizeTranscript`.
struct Transcript<'a> {
    context: &'a TranscriptContext<'a>,
    entries: Vec<Json>,
    parent_uuid: Option<String>,
    clock: u64,
    /// Consecutive user-side messages are laid out the way the CLI writes them: one entry per tool result, then one
    /// entry for the remaining blocks.
    pending_blocks: Vec<Json>,
    assistant_count: usize,
    /// Consecutive assistant messages become one entry, as the AI SDK merges them for the API. The host projects a
    /// round's reasoning card after that round's tool exchange, as an assistant message of its own; left alone, the CLI
    /// drops a thinking-only message and the signed reasoning is lost.
    pending_assistant: Option<Vec<Json>>,
    /// Each tool joins once; a marker naming it again, or naming a tool the session no longer lists, adds nothing.
    announced: HashSet<String>,
}

impl Transcript<'_> {
    fn push(&mut self, entry: Json) {
        let uuid = random_uuid();
        let mut object = Obj::new();
        object.insert("parentUuid", self.parent_uuid.as_deref().map_or(Json::Null, Json::from));
        object.insert("isSidechain", false);
        object.insert("userType", "external");
        object.insert("cwd", self.context.cwd);
        object.insert("sessionId", self.context.session_id);
        object.insert("version", self.context.cli_version);
        object.insert("gitBranch", "HEAD");
        if let Json::Obj(entry) = &entry {
            object.extend_from(entry);
        }
        object.insert("uuid", uuid.as_str());
        object.insert("timestamp", iso_time(self.clock));
        self.entries.push(Json::Obj(object));
        self.parent_uuid = Some(uuid);
        self.clock += 1000;
    }

    fn flush_user(&mut self) {
        if !self.pending_blocks.is_empty() {
            let content = std::mem::take(&mut self.pending_blocks);
            self.push(jobj! { "type": "user", "message": jobj! { "role": "user", "content": content } });
        }
    }

    /// Thinking blocks lead the merged message because the API requires it.
    fn flush_assistant(&mut self) {
        let Some(pending) = self.pending_assistant.take() else { return };
        let (mut blocks, rest): (Vec<Json>, Vec<Json>) = pending.into_iter().partition(is_thinking_block);
        blocks.extend(rest);
        if blocks.is_empty() {
            blocks.push(jobj! { "type": "text", "text": NO_CONTENT_PLACEHOLDER });
        }
        self.assistant_count += 1;
        let stop_reason = if blocks.iter().any(|block| block.str_at("type") == Some("tool_use")) { "tool_use" } else { "end_turn" };
        let message = jobj! {
            "id": format!("msg_mewrk_{}", self.assistant_count),
            "type": "message",
            "role": "assistant",
            "model": self.context.model,
            "content": blocks,
            "stop_reason": stop_reason,
            "stop_sequence": Json::Null,
            "usage": jobj! { "input_tokens": 0u64, "output_tokens": 0u64 },
        };
        self.push(jobj! { "type": "assistant", "message": message });
    }

    fn add_tools(&mut self, names: &[String]) {
        let context = self.context;
        let Some(appendable) = &context.appendable else { return };
        let mut tools: Vec<&Json> = Vec::new();
        for name in names {
            let Some(tool) = appendable.get(name) else { continue };
            if self.announced.insert(name.clone()) {
                tools.push(tool);
            }
        }
        if !tools.is_empty() {
            for entry in tool_addition_entries(&tools) {
                self.push(entry);
            }
        }
    }
}

/// `synthesizeTranscript`: the JSONL entries of a session whose past is `history`.
pub(super) fn synthesize_transcript(history: &[Json], context: &TranscriptContext<'_>) -> Vec<Json> {
    // Timestamps only need to be monotonic; place them in the past so the CLI never sees a future clock.
    let clock = now_ms().saturating_sub(history.len() as u64 * 2000);
    let mut transcript = Transcript {
        context,
        entries: Vec::new(),
        parent_uuid: None,
        clock,
        pending_blocks: Vec::new(),
        assistant_count: 0,
        pending_assistant: None,
        announced: HashSet::new(),
    };
    // The role of the last message that was not a marker. A tool addition goes right after a user turn (a tool result
    // is one), as the Messages API requires; the host places its markers there, and one anywhere else is dropped
    // rather than moved.
    let mut last_role: Option<&str> = None;
    for message in history {
        if !is_object(message) {
            continue;
        }
        if let Some(added) = marker_tools(message) {
            if matches!(last_role, Some("user") | Some("tool")) {
                transcript.flush_user();
                transcript.add_tools(&added);
            }
            continue;
        }
        last_role = message.str_at("role");
        match last_role {
            Some("user") => {
                transcript.flush_assistant();
                if let Some(blocks) = anthropic_user_blocks(message) {
                    transcript.pending_blocks.extend(blocks);
                }
            }
            Some("tool") => {
                transcript.flush_assistant();
                transcript.flush_user();
                for part in tool_result_parts(message) {
                    let block = anthropic_tool_result_block(&part);
                    let tool_use_result = block.get("content").cloned().unwrap_or(Json::Null);
                    transcript.push(jobj! {
                        "type": "user",
                        "message": jobj! { "role": "user", "content": jarr![block] },
                        "toolUseResult": tool_use_result,
                    });
                }
            }
            Some("assistant") => {
                transcript.flush_user();
                let blocks = anthropic_assistant_blocks(message);
                transcript.pending_assistant.get_or_insert_with(Vec::new).extend(blocks);
            }
            _ => {}
        }
    }
    transcript.flush_assistant();
    transcript.flush_user();
    // After the history, before the prompt the session starts with; the CLI hands the tools over behind that prompt,
    // where the host's markers stand.
    for added in context.prompt_additions {
        transcript.add_tools(added);
    }
    transcript.entries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Json {
        Json::parse(text).unwrap()
    }

    #[test]
    fn dates_print_like_to_iso_string() {
        assert_eq!(iso_time(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_time(1_709_210_096_789), "2024-02-29T12:34:56.789Z");
        assert_eq!(iso_time(1_798_761_599_999), "2026-12-31T23:59:59.999Z");
    }

    #[test]
    fn uuids_are_v4() {
        let id = random_uuid();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
        assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"));
    }

    #[test]
    fn outputs_become_mcp_results() {
        let of = |text: &str| mcp_result_of(Some(&parse(text)));
        assert_eq!(of(r#""plain""#).to_json().to_text(), r#"{"content":[{"type":"text","text":"plain"}]}"#);
        assert_eq!(of(r#"{"type":"text","value":"t"}"#).to_json().to_text(), r#"{"content":[{"type":"text","text":"t"}]}"#);
        assert_eq!(
            of(r#"{"type":"error-text","value":"bad"}"#).to_json().to_text(),
            r#"{"content":[{"type":"text","text":"bad"}],"isError":true}"#
        );
        assert_eq!(of(r#"{"type":"json","value":{"a":1}}"#).to_json().to_text(), r#"{"content":[{"type":"text","text":"{\"a\":1}"}]}"#);
        assert_eq!(
            of(r#"{"type":"content","value":[{"type":"text","text":"x"},{"type":"media","data":"AA","mediaType":"image/png"},{"type":"odd"}]}"#)
                .to_json()
                .to_text(),
            r#"{"content":[{"type":"text","text":"x"},{"type":"image","data":"AA","mimeType":"image/png"}]}"#
        );
        assert_eq!(of(r#"{"type":"unknown"}"#).to_json().to_text(), r#"{"content":[{"type":"text","text":"{\"type\":\"unknown\"}"}]}"#);
        assert_eq!(mcp_result_of(None).to_json().to_text(), r#"{"content":[{"type":"text","text":"null"}]}"#);
    }

    #[test]
    fn a_split_finds_results_carriers_and_the_prompt() {
        let messages = parse(
            r#"[
            {"role":"user","content":"first"},
            {"role":"assistant","content":[{"type":"text","text":"a"}]},
            {"role":"tool","content":[{"type":"tool-result","toolCallId":"t1","toolName":"read","output":{"type":"text","value":"r"}}]},
            {"role":"user","content":"[Mewrk tool image] x"},
            {"role":"user","content":"next"},
            {"role":"system","content":"","providerOptions":{"mewrk":{"toolAddition":["late"]}}}
        ]"#,
        );
        let split = split_messages(messages.as_arr().unwrap());
        assert_eq!(split.history.len(), 4);
        assert_eq!(split.results["t1"].content, vec![McpContent::Text("r".to_owned())]);
        assert_eq!(split.carriers, vec![McpContent::Text("[Mewrk tool image] x".to_owned())]);
        assert_eq!(split.prompt.unwrap()[0].str_at("text"), Some("next"));
        assert_eq!(split.prompt_additions, vec![vec!["late".to_owned()]]);
    }

    #[test]
    fn a_box_pair_is_a_carrier_and_not_a_turn() {
        let messages = parse(
            r#"[
            {"role":"user","content":"go"},
            {"role":"assistant","content":[{"type":"text","text":"started"}]},
            {"role":"assistant","content":[{"type":"tool-call","toolCallId":"b","toolName":"box","input":{"none":[]}}]},
            {"role":"tool","content":[{"type":"tool-result","toolCallId":"b","toolName":"box","output":{"type":"text","value":"note"}}]}
        ]"#,
        );
        let split = split_messages(messages.as_arr().unwrap());
        assert!(split.prompt.is_none() && split.results.is_empty());
        assert_eq!(split.carriers, vec![McpContent::Text("note".to_owned())]);
        assert_eq!(split.history.len(), 4);
    }

    #[test]
    fn a_transcript_merges_assistants_and_keeps_signed_thinking_first() {
        let history = parse(
            r#"[
            {"role":"user","content":"hi"},
            {"role":"assistant","content":[{"type":"text","text":"call"},{"type":"tool-call","toolCallId":"t1","toolName":"mcp__mewrk__read","input":{"p":1}}]},
            {"role":"tool","content":[{"type":"tool-result","toolCallId":"t1","toolName":"read","output":{"type":"error-text","value":"no"}}]},
            {"role":"assistant","content":[{"type":"reasoning","text":"thought","providerOptions":{"anthropic":{"signature":"sig"}}},{"type":"reasoning","text":"unsigned"}]},
            {"role":"assistant","content":[{"type":"text","text":"answer"}]},
            {"role":"assistant","content":[{"type":"reasoning","text":"","providerOptions":{"anthropic":{"redactedData":"enc"}}}]}
        ]"#,
        );
        let context = TranscriptContext {
            session_id: "s",
            cwd: "/c",
            cli_version: "9.9.9",
            model: "m",
            appendable: None,
            prompt_additions: &[],
        };
        let entries = synthesize_transcript(history.as_arr().unwrap(), &context);
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].get("parentUuid"), Some(&Json::Null));
        assert_eq!(entries[1].path(&["message", "stop_reason"]).and_then(Json::as_str), Some("tool_use"));
        assert_eq!(entries[1].path(&["message", "content"]).unwrap().as_arr().unwrap()[1].str_at("name"), Some("read"));
        let result = &entries[2];
        assert_eq!(result.get("toolUseResult").unwrap().to_text(), r#"[{"type":"text","text":"no"}]"#);
        assert_eq!(result.path(&["message", "content"]).unwrap().as_arr().unwrap()[0].get("is_error"), Some(&Json::Bool(true)));
        let merged = entries[3].path(&["message", "content"]).unwrap().as_arr().unwrap();
        let kinds: Vec<_> = merged.iter().filter_map(|b| b.str_at("type")).collect();
        assert_eq!(kinds, ["thinking", "redacted_thinking", "text"]);
        assert_eq!(entries[3].path(&["message", "stop_reason"]).and_then(Json::as_str), Some("end_turn"));
        assert_eq!(entries[3].get("parentUuid"), entries[2].get("uuid"));
        let keys: Vec<_> = entries[0].as_obj().unwrap().keys().collect();
        assert_eq!(
            keys,
            ["parentUuid", "isSidechain", "userType", "cwd", "sessionId", "version", "gitBranch", "type", "message", "uuid", "timestamp"]
        );
    }

    #[test]
    fn tool_additions_are_the_clis_own_record_and_only_join_once() {
        let late = parse(r#"{"name":"late","description":"d","inputSchema":{"type":"object"}}"#);
        let appendable: HashMap<String, Json> = [("late".to_owned(), late)].into_iter().collect();
        let history = parse(
            r#"[
            {"role":"user","content":"hi"},
            {"role":"system","content":"","providerOptions":{"mewrk":{"toolAddition":["late","gone"]}}},
            {"role":"assistant","content":[{"type":"text","text":"a"}]},
            {"role":"system","content":"","providerOptions":{"mewrk":{"toolAddition":["late"]}}}
        ]"#,
        );
        let context = TranscriptContext {
            session_id: "s",
            cwd: "/c",
            cli_version: "9",
            model: "m",
            appendable: Some(appendable),
            prompt_additions: &[],
        };
        let entries = synthesize_transcript(history.as_arr().unwrap(), &context);
        let kinds: Vec<_> = entries.iter().map(|e| e.str_at("type").unwrap()).collect();
        assert_eq!(kinds, ["user", "attachment", "attachment", "assistant"]);
        assert_eq!(
            entries[2].path(&["attachment", "entries"]).unwrap().to_text(),
            r#"[{"name":"late","description":"d","input_schema":{"type":"object"},"defer_loading":true}]"#
        );
    }

    #[test]
    fn user_blocks_cover_images_and_pdfs() {
        let message = parse(
            r#"{"role":"user","content":[
            {"type":"text","text":"t"},
            {"type":"image","image":"data:image/png;base64,AAAA"},
            {"type":"image","image":"https://x/y.png"},
            {"type":"file","mediaType":"application/pdf","data":"data:application/pdf;base64,BBBB","filename":"r.pdf"},
            {"type":"file","mediaType":"text/plain","data":"data:text/plain;base64,CCCC"}
        ]}"#,
        );
        let blocks = anthropic_user_blocks(&message).unwrap();
        assert_eq!(blocks.len(), 4);
        assert_eq!(blocks[1].path(&["source", "media_type"]).and_then(Json::as_str), Some("image/png"));
        assert_eq!(blocks[2].path(&["source", "url"]).and_then(Json::as_str), Some("https://x/y.png"));
        assert_eq!(blocks[3].str_at("title"), Some("r.pdf"));
        assert!(anthropic_user_blocks(&parse(r#"{"role":"user","content":""}"#)).is_none());
    }
}
