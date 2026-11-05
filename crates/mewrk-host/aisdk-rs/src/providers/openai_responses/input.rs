//! `convertToOpenAIResponsesInput`.
//!
//! The branches for shell, local shell, apply-patch, computer, custom and tool-search calls are gone: each needs its
//! provider tool declared on the request, and this crate only ever declares `openai.web_search`
//! (`search.rs`). `programmatic_tool_calling` goes by name alone, so it stays.

use std::collections::{HashMap, HashSet};

use crate::error::AiError;
use crate::jobj;
use crate::json::Json;
use crate::prompt::{resolve_full_media_type, top_level_media_type};

use super::ToolNames;
use super::schema::reasoning_options_schema;
use super::url::normalized_http_url;

pub(super) struct InputOptions<'a> {
    pub names: &'a ToolNames,
    pub system_message_mode: &'a str,
    pub provider_options_name: &'a str,
    pub file_id_prefixes: &'a [&'a str],
    pub pass_through_unsupported_files: bool,
    pub store: bool,
    pub has_conversation: bool,
    pub has_previous_response_id: bool,
    pub output_schema_tool_names: &'a HashSet<String>,
}

fn unsupported(functionality: impl std::fmt::Display) -> AiError {
    AiError::other(format!("'{functionality}' functionality not supported."))
}

/// `part.providerOptions?.[name]`.
fn options_of<'a>(part: &'a Json, name: &str) -> Option<&'a Json> {
    part.path(&["providerOptions", name])
}

/// `getPromptCacheBreakpoint`.
fn prompt_cache_breakpoint(part: &Json, name: &str) -> Option<Json> {
    options_of(part, name)?.non_null("promptCacheBreakpoint").cloned()
}

/// `part.data.url.toString()`. The prompt conversion made it a `URL`, so it comes out normalized.
fn file_url(part: &Json) -> Json {
    let url = part.path(&["data", "url"]);
    url.and_then(Json::as_str).and_then(normalized_http_url).map_or_else(|| url.cloned().unwrap_or_default(), Json::from)
}

/// `isFileId`.
fn is_file_id(data: &str, prefixes: &[&str]) -> bool {
    prefixes.iter().any(|prefix| data.starts_with(prefix))
}

/// `mapToolCaller`.
fn map_tool_caller(caller: Option<&Json>) -> Option<Json> {
    let caller = caller.filter(|c| !c.is_null())?;
    if caller.str_at("type") == Some("program") {
        return Some(jobj! { "type": "program", "caller_id"?: caller.get("callerId").cloned() });
    }
    Some(caller.clone())
}

/// `serializeToolCallArguments`.
fn serialize_arguments(input: Option<&Json>) -> String {
    input.map_or_else(|| "{}".to_owned(), Json::to_text)
}

/// `resolveProviderReference`.
fn resolve_reference(part: &Json, provider: &str) -> Result<Json, AiError> {
    let reference = part.path(&["data", "reference"]);
    reference.and_then(|r| r.non_null(provider)).cloned().ok_or_else(|| {
        let available: Vec<&str> = reference.and_then(Json::as_obj).map_or_else(Vec::new, |r| r.keys().collect());
        AiError::other(format!("No provider reference found for provider '{provider}'. Available providers: {}", available.join(", ")))
    })
}

/// The `content` form of a tool output, shared by plain function results.
fn tool_content_items(output: &Json, name: &str) -> Result<Vec<Json>, AiError> {
    let mut items = Vec::new();
    for item in output.get("value").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice) {
        let breakpoint = prompt_cache_breakpoint(item, name);
        match item.str_at("type") {
            Some("text") => items.push(jobj! {
                "type": "input_text",
                "text": item.get("text").cloned().unwrap_or_default(),
                "prompt_cache_breakpoint"?: breakpoint,
            }),
            Some("file") => {
                let media_type = item.str_at("mediaType").unwrap_or("");
                let top_level = top_level_media_type(media_type);
                let detail = options_of(item, name).and_then(|o| o.get("imageDetail")).cloned();
                match item.path(&["data", "type"]).and_then(Json::as_str) {
                    Some("data") => {
                        let full = resolve_full_media_type(item)?;
                        let inline = item.path(&["data", "data"]).and_then(Json::as_str).unwrap_or("");
                        let url = format!("data:{full};base64,{inline}");
                        if top_level == "image" {
                            items.push(jobj! {
                                "type": "input_image",
                                "image_url": url,
                                "detail"?: detail,
                                "prompt_cache_breakpoint"?: breakpoint,
                            });
                        } else {
                            items.push(jobj! {
                                "type": "input_file",
                                "filename": item.get("filename").filter(|f| !f.is_null()).cloned().unwrap_or_else(|| Json::from("data")),
                                "file_data": url,
                                "prompt_cache_breakpoint"?: breakpoint,
                            });
                        }
                    }
                    Some("url") => {
                        let url = file_url(item);
                        if top_level == "image" {
                            items.push(jobj! {
                                "type": "input_image",
                                "image_url": url,
                                "detail"?: detail,
                                "prompt_cache_breakpoint"?: breakpoint,
                            });
                        } else {
                            items.push(jobj! { "type": "input_file", "file_url": url, "prompt_cache_breakpoint"?: breakpoint });
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    Ok(items)
}

/// `convertFunctionToolResultOutput`. `None` is an output type the switch does not list.
fn function_tool_result_output(output: &Json, tool_name: &str, o: &InputOptions<'_>) -> Result<Option<Json>, AiError> {
    // `output` is always a string, but for a function with `output_schema` OpenAI parses that string as JSON.
    let has_output_schema = o.output_schema_tool_names.contains(tool_name);
    let text_like = |text: &Json| if has_output_schema { Json::from(text.to_text()) } else { text.clone() };
    Ok(match output.str_at("type") {
        Some("text") | Some("error-text") => Some(text_like(output.get("value").unwrap_or(&Json::Null))),
        Some("execution-denied") => {
            let reason = output.non_null("reason").cloned().unwrap_or_else(|| Json::from("Tool call execution denied."));
            Some(text_like(&reason))
        }
        Some("json") | Some("error-json") => Some(Json::from(output.get("value").unwrap_or(&Json::Null).to_text())),
        Some("content") => Some(Json::Arr(tool_content_items(output, o.provider_options_name)?)),
        _ => None,
    })
}

/// `ParallelToolCallMetadata`.
struct ParallelCall {
    item_id: String,
    tool_call_id: String,
    tool_name: String,
    input: String,
    index: u64,
    count: u64,
}

/// `getParallelToolCallMetadata`.
fn parallel_metadata(part: &Json, name: &str) -> Option<ParallelCall> {
    let metadata = options_of(part, name)?.get("parallelToolCall")?;
    let index = metadata.get("index")?.as_u64()?;
    let count = metadata.get("count")?.as_u64()?;
    if count <= index {
        return None;
    }
    Some(ParallelCall {
        item_id: metadata.str_at("itemId")?.to_owned(),
        tool_call_id: metadata.str_at("toolCallId")?.to_owned(),
        tool_name: metadata.str_at("toolName")?.to_owned(),
        input: metadata.str_at("input")?.to_owned(),
        index,
        count,
    })
}

/// `hasSameParallelToolCall`.
fn same_parallel_call(first: &ParallelCall, second: &ParallelCall) -> bool {
    first.item_id == second.item_id
        && first.tool_call_id == second.tool_call_id
        && first.tool_name == second.tool_name
        && first.input == second.input
        && first.count == second.count
}

struct ParallelGroup<'a> {
    metadata: ParallelCall,
    results: Vec<&'a Json>,
}

/// `collectCompleteParallelToolResultGroups`.
fn complete_parallel_groups<'a>(prompt: &'a [Json], name: &str) -> HashMap<String, ParallelGroup<'a>> {
    struct Pending<'a> {
        metadata: ParallelCall,
        results: HashMap<u64, &'a Json>,
        invalid: bool,
    }
    let mut pending: HashMap<String, Pending<'a>> = HashMap::new();
    for message in prompt {
        if message.str_at("role") != Some("tool") {
            continue;
        }
        for part in message.get("content").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice) {
            if part.str_at("type") != Some("tool-result") {
                continue;
            }
            let Some(metadata) = parallel_metadata(part, name) else { continue };
            match pending.get_mut(&metadata.tool_call_id) {
                None => {
                    let index = metadata.index;
                    pending.insert(metadata.tool_call_id.clone(), Pending { metadata, results: HashMap::from([(index, part)]), invalid: false });
                }
                Some(existing) => {
                    if !same_parallel_call(&existing.metadata, &metadata) || existing.results.contains_key(&metadata.index) {
                        existing.invalid = true;
                    } else {
                        existing.results.insert(metadata.index, part);
                    }
                }
            }
        }
    }
    let mut complete = HashMap::new();
    for (tool_call_id, group) in pending {
        if group.invalid || group.results.len() as u64 != group.metadata.count {
            continue;
        }
        let results: Option<Vec<&Json>> = (0..group.metadata.count).map(|index| group.results.get(&index).copied()).collect();
        if let Some(results) = results {
            complete.insert(tool_call_id, ParallelGroup { metadata: group.metadata, results });
        }
    }
    complete
}

fn user_message(content: &[Json], o: &InputOptions<'_>) -> Result<Json, AiError> {
    let name = o.provider_options_name;
    let mut parts = Vec::with_capacity(content.len());
    for (index, part) in content.iter().enumerate() {
        let breakpoint = prompt_cache_breakpoint(part, name);
        match part.str_at("type") {
            Some("text") => parts.push(jobj! {
                "type": "input_text",
                "text": part.get("text").cloned().unwrap_or_default(),
                "prompt_cache_breakpoint"?: breakpoint,
            }),
            Some("file") => {
                let media_type = part.str_at("mediaType").unwrap_or("");
                let top_level = top_level_media_type(media_type);
                let detail = || options_of(part, name).and_then(|options| options.get("imageDetail")).cloned();
                let kind = part.path(&["data", "type"]).and_then(Json::as_str).unwrap_or("");
                let inline = part.path(&["data", "data"]).and_then(Json::as_str);
                let file_id = inline.filter(|data| is_file_id(data, o.file_id_prefixes));
                match kind {
                    "reference" => {
                        let file_id = resolve_reference(part, name)?;
                        if top_level == "image" {
                            parts.push(jobj! {
                                "type": "input_image",
                                "file_id": file_id,
                                "detail"?: detail(),
                                "prompt_cache_breakpoint"?: breakpoint,
                            });
                        } else {
                            parts.push(jobj! { "type": "input_file", "file_id": file_id, "prompt_cache_breakpoint"?: breakpoint });
                        }
                    }
                    "text" => return Err(unsupported("text file parts")),
                    "url" | "data" if top_level == "image" => {
                        let mut image = crate::json::Obj::new();
                        image.insert("type", "input_image");
                        if kind == "url" {
                            image.insert("image_url", file_url(part));
                        } else if let Some(file_id) = file_id {
                            image.insert("file_id", file_id);
                        } else {
                            let full = resolve_full_media_type(part)?;
                            image.insert("image_url", format!("data:{full};base64,{}", inline.unwrap_or("")));
                        }
                        if let Some(detail) = detail() {
                            image.insert("detail", detail);
                        }
                        if let Some(breakpoint) = breakpoint {
                            image.insert("prompt_cache_breakpoint", breakpoint);
                        }
                        parts.push(Json::Obj(image));
                    }
                    "url" => parts.push(jobj! {
                        "type": "input_file",
                        "file_url": file_url(part),
                        "prompt_cache_breakpoint"?: breakpoint,
                    }),
                    "data" => {
                        let full = resolve_full_media_type(part)?;
                        if full != "application/pdf" && !o.pass_through_unsupported_files {
                            return Err(unsupported(format!("file part media type {full}")));
                        }
                        let mut file = crate::json::Obj::new();
                        file.insert("type", "input_file");
                        if let Some(file_id) = file_id {
                            file.insert("file_id", file_id);
                        } else {
                            let filename = part.get("filename").filter(|f| !f.is_null()).cloned().unwrap_or_else(|| {
                                Json::from(if full == "application/pdf" { format!("part-{index}.pdf") } else { format!("part-{index}") })
                            });
                            file.insert("filename", filename);
                            file.insert("file_data", format!("data:{full};base64,{}", inline.unwrap_or("")));
                        }
                        if let Some(breakpoint) = breakpoint {
                            file.insert("prompt_cache_breakpoint", breakpoint);
                        }
                        parts.push(Json::Obj(file));
                    }
                    // The switch falls through to `undefined`, which `JSON.stringify` writes as `null` in an array.
                    _ => parts.push(Json::Null),
                }
            }
            _ => parts.push(Json::Null),
        }
    }
    Ok(jobj! { "role": "user", "content": parts })
}

/// A reasoning item being built across the parts that share its id: `reasoningMessages[id]`.
struct ReasoningMessage {
    /// Where it sits in `input`. `None` under `store`, where only the first part's reference goes out.
    at: Option<usize>,
    id: String,
    encrypted_content: Option<String>,
    summary: Vec<Json>,
}

impl ReasoningMessage {
    fn item(&self) -> Json {
        jobj! {
            "type": "reasoning",
            "id": self.id.as_str(),
            "encrypted_content"?: self.encrypted_content.as_deref(),
            "summary": self.summary.clone(),
        }
    }
}

fn execution_denied(output: &Json) -> bool {
    output.str_at("type") == Some("execution-denied")
        || (output.str_at("type") == Some("json") && output.path(&["value", "type"]).and_then(Json::as_str) == Some("execution-denied"))
}

fn assistant_message(
    content: &[Json],
    groups: &HashMap<String, ParallelGroup<'_>>,
    emitted_calls: &mut HashSet<String>,
    o: &InputOptions<'_>,
    input: &mut Vec<Json>,
) -> Result<(), AiError> {
    let name = o.provider_options_name;
    let mut reasoning: HashMap<String, ReasoningMessage> = HashMap::new();
    for part in content {
        let provider_options = options_of(part, name);
        let item_id = provider_options.and_then(|options| options.non_null("itemId"));
        match part.str_at("type") {
            Some("text") => {
                // Under a conversation an item that already exists there would be a "Duplicate item found" error.
                if o.has_conversation && item_id.is_some() {
                    continue;
                }
                // Item references reduce the payload size.
                if let (true, Some(id)) = (o.store, item_id) {
                    input.push(jobj! { "type": "item_reference", "id": id.clone() });
                    continue;
                }
                input.push(jobj! {
                    "role": "assistant",
                    "content": vec![jobj! { "type": "output_text", "text": part.get("text").cloned().unwrap_or_default() }],
                    "id"?: provider_options.and_then(|options| options.get("itemId")).cloned(),
                    "phase"?: provider_options.and_then(|options| options.non_null("phase")).cloned(),
                });
            }
            Some("tool-call") => {
                let parallel = parallel_metadata(part, name);
                if let Some(metadata) = &parallel {
                    if let Some(group) = groups.get(&metadata.tool_call_id).filter(|group| same_parallel_call(&group.metadata, metadata)) {
                        if emitted_calls.insert(group.metadata.tool_call_id.clone()) && !o.has_conversation {
                            // Conversations already hold the original wrapper; `previousResponseId` chains need plain
                            // client function calls reconstructed in full.
                            input.push(jobj! {
                                "type": "function_call",
                                "call_id": group.metadata.tool_call_id.as_str(),
                                "name": group.metadata.tool_name.as_str(),
                                "arguments": group.metadata.input.as_str(),
                            });
                        }
                        continue;
                    }
                }
                if o.has_conversation && item_id.is_some() {
                    continue;
                }
                let tool_name = part.str_at("toolName").unwrap_or("");
                let resolved = o.names.provider(tool_name);
                let tool_call_id = part.get("toolCallId").cloned().unwrap_or_default();
                if resolved == "programmatic_tool_calling" {
                    if let (true, Some(id)) = (o.store, item_id) {
                        input.push(jobj! { "type": "item_reference", "id": id.clone() });
                        continue;
                    }
                    let parsed = part.get("input").unwrap_or(&Json::Null);
                    let (Some(code), Some(fingerprint)) = (parsed.get("code").filter(|c| c.is_str()), parsed.get("fingerprint").filter(|f| f.is_str())) else {
                        return Err(AiError::validation(parsed, "programmatic_tool_calling input"));
                    };
                    input.push(jobj! {
                        "type": "program",
                        "id": item_id.cloned().unwrap_or_else(|| tool_call_id.clone()),
                        "call_id": tool_call_id,
                        "code": code.clone(),
                        "fingerprint": fingerprint.clone(),
                    });
                    continue;
                }
                if part.get("providerExecuted").and_then(Json::as_bool) == Some(true) {
                    if let (true, Some(id)) = (o.store, item_id) {
                        input.push(jobj! { "type": "item_reference", "id": id.clone() });
                    }
                    continue;
                }
                // Plain client function calls must not be references: the matching `function_call_output` can only
                // name the call by `call_id`, which the API cannot reconcile with an item id.
                input.push(jobj! {
                    "type": "function_call",
                    "call_id": tool_call_id,
                    "name": resolved,
                    "arguments": serialize_arguments(part.get("input")),
                    "namespace"?: provider_options.and_then(|options| options.non_null("namespace")).cloned(),
                    "caller"?: map_tool_caller(provider_options.and_then(|options| options.get("caller"))),
                });
            }
            // Assistant tool results come from provider-executed tools.
            Some("tool-result") => {
                let output = part.get("output").unwrap_or(&Json::Null);
                // Synthetic results of denied approvals have no item in OpenAI's store.
                if execution_denied(output) || o.has_conversation {
                    continue;
                }
                let tool_call_id = part.get("toolCallId").cloned().unwrap_or_default();
                let item = item_id.cloned().unwrap_or_else(|| tool_call_id.clone());
                if o.names.provider(part.str_at("toolName").unwrap_or("")) == "programmatic_tool_calling" {
                    if o.store {
                        input.push(jobj! { "type": "item_reference", "id": item });
                    } else if output.str_at("type") == Some("json") {
                        let parsed = output.get("value").unwrap_or(&Json::Null);
                        let status_ok = matches!(parsed.str_at("status"), Some("completed" | "incomplete"));
                        let (Some(result), true) = (parsed.get("result").filter(|r| r.is_str()), status_ok) else {
                            return Err(AiError::validation(parsed, "programmatic_tool_calling output"));
                        };
                        input.push(jobj! {
                            "type": "program_output",
                            "id": item,
                            "call_id": tool_call_id,
                            "result": result.clone(),
                            "status": parsed.get("status").cloned().unwrap_or_default(),
                        });
                    }
                    continue;
                }
                if o.store {
                    input.push(jobj! { "type": "item_reference", "id": item });
                }
            }
            Some("reasoning") => {
                let parsed = match part.path(&["providerOptions", name]).filter(|options| !options.is_null()) {
                    None => None,
                    Some(options) => Some(
                        reasoning_options_schema()
                            .parse(Some(options))
                            .map_err(|()| AiError::other(format!("invalid {name} provider options")))?
                            .unwrap_or_default(),
                    ),
                };
                let reasoning_id = parsed.as_ref().and_then(|options| options.str_at("itemId")).map(str::to_owned);
                let encrypted = parsed.as_ref().and_then(|options| options.str_at("reasoningEncryptedContent")).map(str::to_owned);
                let text = part.str_at("text").unwrap_or("");
                let summary = if text.is_empty() { Vec::new() } else { vec![jobj! { "type": "summary_text", "text": text }] };
                let Some(reasoning_id) = reasoning_id else {
                    // Without an id the Responses API still takes an item carrying `encrypted_content`, which keeps
                    // multi-turn reasoning working when the id was stripped.
                    if let Some(encrypted) = encrypted {
                        input.push(jobj! { "type": "reasoning", "encrypted_content": encrypted, "summary": summary });
                    }
                    continue;
                };
                if o.has_conversation || o.has_previous_response_id {
                    continue;
                }
                if o.store {
                    if !reasoning.contains_key(&reasoning_id) {
                        input.push(jobj! { "type": "item_reference", "id": reasoning_id.as_str() });
                        reasoning.insert(reasoning_id.clone(), ReasoningMessage { at: None, id: reasoning_id, encrypted_content: None, summary: Vec::new() });
                    }
                    continue;
                }
                match reasoning.get_mut(&reasoning_id) {
                    None => {
                        let message = ReasoningMessage { at: Some(input.len()), id: reasoning_id.clone(), encrypted_content: encrypted, summary };
                        input.push(message.item());
                        reasoning.insert(reasoning_id, message);
                    }
                    Some(message) => {
                        message.summary.extend(summary);
                        // The last summary part carries the encrypted content.
                        if encrypted.is_some() {
                            message.encrypted_content = encrypted;
                        }
                        if let Some(at) = message.at {
                            input[at] = message.item();
                        }
                    }
                }
            }
            Some("custom") if part.str_at("kind") == Some("openai.compaction") => {
                if o.has_conversation && item_id.is_some() {
                    continue;
                }
                let Some(id) = item_id else { continue };
                if o.store {
                    input.push(jobj! { "type": "item_reference", "id": id.clone() });
                    continue;
                }
                input.push(jobj! {
                    "type": "compaction",
                    "id": id.clone(),
                    "encrypted_content"?: provider_options.and_then(|options| options.get("encryptedContent")).cloned(),
                });
            }
            _ => {}
        }
    }
    Ok(())
}

fn tool_message(
    content: &[Json],
    groups: &HashMap<String, ParallelGroup<'_>>,
    emitted_results: &mut HashSet<String>,
    processed_approvals: &mut HashSet<String>,
    o: &InputOptions<'_>,
    input: &mut Vec<Json>,
) -> Result<(), AiError> {
    let name = o.provider_options_name;
    for part in content {
        if part.str_at("type") == Some("tool-approval-response") {
            let approval_id = part.get("approvalId").cloned().unwrap_or_default();
            if !processed_approvals.insert(approval_id.as_str().unwrap_or("").to_owned()) {
                continue;
            }
            if o.store && !o.has_conversation && !o.has_previous_response_id {
                input.push(jobj! { "type": "item_reference", "id": approval_id.clone() });
            }
            input.push(jobj! {
                "type": "mcp_approval_response",
                "approval_request_id": approval_id,
                "approve": part.get("approved").cloned().unwrap_or_default(),
            });
            continue;
        }
        if let Some(metadata) = parallel_metadata(part, name) {
            if let Some(group) = groups.get(&metadata.tool_call_id).filter(|group| same_parallel_call(&group.metadata, &metadata)) {
                if emitted_results.insert(group.metadata.tool_call_id.clone()) {
                    // The internal wrapper returns one output with the child results in the order of `tool_uses`.
                    let mut outputs = Vec::with_capacity(group.results.len());
                    for result in &group.results {
                        let output = function_tool_result_output(result.get("output").unwrap_or(&Json::Null), result.str_at("toolName").unwrap_or(""), o)?;
                        outputs.push(match output {
                            Some(Json::Str(text)) => text,
                            Some(other) => other.to_text(),
                            None => String::new(),
                        });
                    }
                    input.push(jobj! {
                        "type": "function_call_output",
                        "call_id": group.metadata.tool_call_id.as_str(),
                        "output": outputs.join("\n"),
                    });
                }
                continue;
            }
        }
        let output = part.get("output").unwrap_or(&Json::Null);
        // Already handled through its `tool-approval-response`.
        if output.str_at("type") == Some("execution-denied")
            && output.path(&["providerOptions", "openai", "approvalId"]).and_then(Json::as_str).is_some_and(|id| !id.is_empty())
        {
            continue;
        }
        let value = function_tool_result_output(output, part.str_at("toolName").unwrap_or(""), o)?;
        input.push(jobj! {
            "type": "function_call_output",
            "call_id": part.get("toolCallId").cloned().unwrap_or_default(),
            "output"?: value,
            "caller"?: map_tool_caller(options_of(part, name).and_then(|options| options.get("caller"))),
        });
    }
    Ok(())
}

/// `convertToOpenAIResponsesInput`, without the warnings nothing reads.
pub(super) fn convert_input(prompt: &[Json], o: &InputOptions<'_>) -> Result<Vec<Json>, AiError> {
    let name = o.provider_options_name;
    let mut input: Vec<Json> = Vec::new();
    let mut processed_approvals = HashSet::new();
    let groups = if o.has_conversation || o.has_previous_response_id { complete_parallel_groups(prompt, name) } else { HashMap::new() };
    let mut emitted_calls = HashSet::new();
    let mut emitted_results = HashSet::new();
    for message in prompt {
        let content = message.get("content").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice);
        match message.str_at("role") {
            Some("system") => {
                if o.system_message_mode == "remove" {
                    continue;
                }
                let text = message.get("content").cloned().unwrap_or_default();
                let content = match prompt_cache_breakpoint(message, name) {
                    None => text,
                    Some(breakpoint) => Json::Arr(vec![jobj! { "type": "input_text", "text": text, "prompt_cache_breakpoint": breakpoint }]),
                };
                input.push(jobj! { "role": o.system_message_mode, "content": content });
            }
            Some("user") => input.push(user_message(content, o)?),
            Some("assistant") => assistant_message(content, &groups, &mut emitted_calls, o, &mut input)?,
            Some("tool") => tool_message(content, &groups, &mut emitted_results, &mut processed_approvals, o, &mut input)?,
            other => return Err(AiError::other(format!("Unsupported role: {}", other.unwrap_or("undefined")))),
        }
    }
    // Without `store`, a reasoning item the API cannot verify is refused, so those without encrypted content go.
    if !o.store {
        input.retain(|item| item.str_at("type") != Some("reasoning") || item.non_null("encrypted_content").is_some());
    }
    Ok(input)
}
