//! `@ai-sdk/openai-compatible`: `createOpenAICompatible` and its chat model's streaming path, with the settings
//! `providers.ts` gives it (`includeUsage: true`, `convertUsage: convertChatUsage`, the Chat dialect fetch).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::api::{ErrorShape, VERSION_OPENAI_COMPATIBLE, combine_headers, post_json, with_user_agent_suffix, without_trailing_slash};
use crate::dialect::chat::convert_chat_usage;
use crate::error::AiError;
use crate::http::{BoxFuture, DynFetch, Headers};
use crate::jobj;
use crate::json::{Json, Obj};
use crate::model::{CallOptions, ChunkTransform, DynModel, FinishReason, LanguageModel, ModelStream, StreamPart};
use crate::prompt::{resolve_full_media_type, top_level_media_type};

use super::ProviderSettings;

/// The keys of `openaiCompatibleLanguageModelChatOptions`: everything else under the provider's own name goes
/// onto the request body as it is.
const OPTION_KEYS: [&str; 4] = ["user", "reasoningEffort", "textVerbosity", "strictJsonSchema"];

/// `createOpenAICompatible({ name, baseURL: base ?? "", apiKey, headers, includeUsage: true, convertUsage, fetch }).chatModel(modelId)`.
pub fn create(target: ProviderSettings, name: &str) -> Result<DynModel, AiError> {
    let base_url = without_trailing_slash(target.base_url.as_deref().unwrap_or("")).to_owned();
    let mut headers = Headers::new();
    // An empty key sends no header at all, which lets the upstream answer 401.
    if !target.api_key.is_empty() {
        headers.set("authorization", http_trim(&format!("Bearer {}", target.api_key)));
    }
    let headers = combine_headers(&headers, &target.headers);
    let headers = with_user_agent_suffix(headers, &[&format!("ai-sdk/openai-compatible/{VERSION_OPENAI_COMPATIBLE}")]);
    Ok(Arc::new(ChatModel {
        provider: format!("{name}.chat"),
        name: name.to_owned(),
        model_id: target.model_id,
        base_url,
        headers,
        fetch: target.fetch,
    }))
}

/// What `new Headers()` leaves of a value: surrounding HTTP whitespace goes.
pub(super) fn http_trim(value: &str) -> &str {
    value.trim_matches([' ', '\t', '\r', '\n'])
}

/// `undefined` or the check.
pub(super) fn optional(value: Option<&Json>, check: impl Fn(&Json) -> bool) -> bool {
    value.is_none_or(check)
}

/// `undefined`, `null` or the check.
pub(super) fn nullish(value: Option<&Json>, check: impl Fn(&Json) -> bool) -> bool {
    value.is_none_or(|v| v.is_null() || check(v))
}

/// `z.number()`, which turns away the infinity `JSON.parse` makes of a number past a double.
pub(super) fn is_number(value: &Json) -> bool {
    value.as_f64().is_some_and(f64::is_finite)
}

/// `UnsupportedFunctionalityError`.
pub(super) fn unsupported(functionality: &str) -> AiError {
    AiError::other(format!("'{functionality}' functionality not supported."))
}

/// `new TextDecoder().decode(convertBase64ToUint8Array(data))`: forgiving base64 in either alphabet, lossy UTF-8,
/// and a leading byte-order mark dropped.
fn base64_to_text(data: &str) -> Result<String, AiError> {
    let cleaned: String = data
        .chars()
        .filter(|c| !matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{c}'))
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            other => other,
        })
        .collect();
    let bytes = base64_simd::STANDARD
        .decode_to_vec(cleaned.as_bytes())
        .or_else(|_| base64_simd::STANDARD_NO_PAD.decode_to_vec(cleaned.as_bytes()))
        .map_err(|_| AiError::other("The string to be decoded is not correctly encoded."))?;
    let text = String::from_utf8_lossy(&bytes);
    Ok(text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned())
}

/// `toCamelCase`.
fn to_camel_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut chars = name.chars().peekable();
    while let Some(c) = chars.next() {
        if matches!(c, '_' | '-') && chars.peek().is_some_and(char::is_ascii_lowercase) {
            out.extend(chars.next().map(|next| next.to_ascii_uppercase()));
        } else {
            out.push(c);
        }
    }
    out
}

/// An object literal under construction. A JavaScript property set to `undefined` keeps its position for a later
/// spread or assignment even though `JSON.stringify` leaves it out, which is what these empty slots are.
#[derive(Default)]
struct Slots(Vec<(String, Option<Json>)>);

impl Slots {
    fn set(&mut self, key: &str, value: Option<Json>) {
        match self.0.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = value,
            None => self.0.push((key.to_owned(), value)),
        }
    }

    fn spread(&mut self, object: &Obj) {
        for (key, value) in object.iter() {
            self.set(key, Some(value.clone()));
        }
    }

    fn into_json(self) -> Json {
        let mut out = Obj::new();
        for (key, value) in self.0 {
            if let Some(value) = value {
                out.insert(key, value);
            }
        }
        Json::Obj(out)
    }
}

// ------------------------------------------------------------------ Messages

/// `getOpenAIMetadata`: `providerOptions.openaiCompatible` of a message or part, spread into what it becomes.
fn metadata_of(holder: &Json) -> Obj {
    holder.path(&["providerOptions", "openaiCompatible"]).and_then(Json::as_obj).cloned().unwrap_or_default()
}

fn spread(mut value: Json, metadata: &Obj) -> Json {
    if let Some(object) = value.as_obj_mut() {
        object.extend_from(metadata);
    }
    value
}

/// `String(value)` for a thought signature, which the host persisted as whatever the options held.
fn js_string(value: &Json) -> String {
    match value {
        Json::Str(text) => text.clone(),
        Json::Obj(_) => "[object Object]".to_owned(),
        Json::Arr(items) => items.iter().map(js_string).collect::<Vec<_>>().join(","),
        other => other.to_text(),
    }
}

fn is_truthy(value: &Json) -> bool {
    match value {
        Json::Null => false,
        Json::Bool(flag) => *flag,
        Json::Num(number) => {
            let number = number.as_f64();
            number != 0.0 && !number.is_nan()
        }
        Json::Str(text) => !text.is_empty(),
        Json::Arr(_) | Json::Obj(_) => true,
    }
}

/// A part's `url` field for an image or a video: the URL itself, or the bytes as a data URL.
fn media_url(part: &Json, data: &Json, is_url: bool) -> Result<Json, AiError> {
    if is_url {
        return Ok(data.get("url").cloned().unwrap_or_default());
    }
    Ok(Json::from(format!("data:{};base64,{}", resolve_full_media_type(part)?, data.str_at("data").unwrap_or(""))))
}

fn user_file_part(part: &Json, metadata: &Obj) -> Result<Json, AiError> {
    let data = part.get("data").unwrap_or(&Json::Null);
    let is_url = match data.str_at("type") {
        Some("reference") => return Err(unsupported("file parts with provider references")),
        Some("text") => return Err(unsupported("text file parts")),
        Some("url") => true,
        _ => false,
    };
    let media_type = part.str_at("mediaType").unwrap_or("");
    let inline = data.str_at("data").unwrap_or("");
    let converted = match top_level_media_type(media_type) {
        "image" => jobj! { "type": "image_url", "image_url": jobj! { "url": media_url(part, data, is_url)? } },
        "video" => jobj! { "type": "video_url", "video_url": jobj! { "url": media_url(part, data, is_url)? } },
        "audio" => {
            if is_url {
                return Err(unsupported("audio file parts with URLs"));
            }
            let full = resolve_full_media_type(part)?;
            let format = match full.as_str() {
                "audio/wav" => "wav",
                "audio/mp3" | "audio/mpeg" => "mp3",
                _ => return Err(unsupported(&format!("audio media type {full}"))),
            };
            jobj! { "type": "input_audio", "input_audio": jobj! { "data": inline, "format": format } }
        }
        "application" => {
            if is_url {
                return Err(unsupported("PDF file parts with URLs"));
            }
            let full = resolve_full_media_type(part)?;
            if full != "application/pdf" {
                return Err(unsupported(&format!("file part media type {full}")));
            }
            jobj! {
                "type": "file",
                "file": jobj! {
                    "filename": part.non_null("filename").cloned().unwrap_or_else(|| Json::from("document.pdf")),
                    "file_data": format!("data:application/pdf;base64,{inline}"),
                },
            }
        }
        "text" => {
            let text = if is_url { data.get("url").cloned().unwrap_or_default() } else { Json::from(base64_to_text(inline)?) };
            jobj! { "type": "text", "text": text }
        }
        _ => return Err(unsupported(&format!("file part media type {media_type}"))),
    };
    Ok(spread(converted, metadata))
}

fn assistant_message(message: &Json, metadata: &Obj, metadata_key: &str) -> Json {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: Vec<Json> = Vec::new();
    for part in message.get("content").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice) {
        match part.str_at("type").unwrap_or("") {
            "text" => text.push_str(part.str_at("text").unwrap_or("")),
            "reasoning" => reasoning.push_str(part.str_at("text").unwrap_or("")),
            "tool-call" => {
                let options = part.get("providerOptions");
                let signature = options
                    .and_then(|o| o.path(&[metadata_key, "thoughtSignature"]))
                    .filter(|v| !v.is_null())
                    .or_else(|| options.and_then(|o| o.path(&["google", "thoughtSignature"])));
                let mut call = spread(
                    jobj! {
                        "id": part.get("toolCallId").cloned().unwrap_or_default(),
                        "type": "function",
                        "function": jobj! {
                            "name": part.get("toolName").cloned().unwrap_or_default(),
                            "arguments"?: part.get("input").map(Json::to_text),
                        },
                    },
                    &metadata_of(part),
                );
                if let (Some(signature), Some(object)) = (signature.filter(|s| is_truthy(s)), call.as_obj_mut()) {
                    object.insert("extra_content", jobj! { "google": jobj! { "thought_signature": js_string(signature) } });
                }
                tool_calls.push(call);
            }
            _ => {}
        }
    }
    let content = if !tool_calls.is_empty() && text.is_empty() { Json::Null } else { Json::from(text) };
    let mut out = Slots::default();
    out.set("role", Some(Json::from("assistant")));
    out.set("content", Some(content));
    if !reasoning.is_empty() {
        out.set("reasoning_content", Some(Json::from(reasoning)));
    }
    out.set("tool_calls", (!tool_calls.is_empty()).then_some(Json::Arr(tool_calls)));
    out.spread(metadata);
    out.into_json()
}

/// `convertToOpenAICompatibleChatMessages`.
fn convert_messages(prompt: &[Json], metadata_key: &str) -> Result<Vec<Json>, AiError> {
    let mut messages = Vec::new();
    for message in prompt {
        let metadata = metadata_of(message);
        let role = message.str_at("role").unwrap_or("");
        let content = message.get("content").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice);
        match role {
            "system" => messages.push(spread(
                jobj! { "role": "system", "content": message.get("content").cloned().unwrap_or_default() },
                &metadata,
            )),
            "user" => {
                if let [only] = content
                    && only.str_at("type") == Some("text")
                {
                    messages.push(spread(
                        jobj! { "role": "user", "content": only.get("text").cloned().unwrap_or_default() },
                        &metadata_of(only),
                    ));
                    continue;
                }
                let mut parts = Vec::with_capacity(content.len());
                for part in content {
                    let part_metadata = metadata_of(part);
                    parts.push(match part.str_at("type").unwrap_or("") {
                        "text" => spread(jobj! { "type": "text", "text": part.get("text").cloned().unwrap_or_default() }, &part_metadata),
                        _ => user_file_part(part, &part_metadata)?,
                    });
                }
                messages.push(spread(jobj! { "role": "user", "content": parts }, &metadata));
            }
            "assistant" => messages.push(assistant_message(message, &metadata, metadata_key)),
            "tool" => {
                for response in content {
                    if response.str_at("type") == Some("tool-approval-response") {
                        continue;
                    }
                    let output = response.get("output").unwrap_or(&Json::Null);
                    let value = match output.str_at("type").unwrap_or("") {
                        "text" | "error-text" => output.get("value").cloned(),
                        "execution-denied" => Some(output.non_null("reason").cloned().unwrap_or_else(|| Json::from("Tool call execution denied."))),
                        "content" | "json" | "error-json" => output.get("value").map(|value| Json::from(value.to_text())),
                        _ => None,
                    };
                    messages.push(spread(
                        jobj! {
                            "role": "tool",
                            "tool_call_id": response.get("toolCallId").cloned().unwrap_or_default(),
                            "content"?: value,
                        },
                        &metadata_of(response),
                    ));
                }
            }
            other => return Err(AiError::other(format!("Unsupported role: {other}"))),
        }
    }
    Ok(messages)
}

// ------------------------------------------------------------------ Tools

/// `prepareTools`. Provider tools have no Chat form and are dropped.
fn prepare_tools(tools: Option<&Vec<Json>>, tool_choice: &Json) -> Result<(Option<Vec<Json>>, Option<Json>), AiError> {
    let Some(tools) = tools.filter(|tools| !tools.is_empty()) else { return Ok((None, None)) };
    let mut out = Vec::with_capacity(tools.len());
    for tool in tools {
        if tool.str_at("type") == Some("provider") {
            continue;
        }
        out.push(jobj! {
            "type": "function",
            "function": jobj! {
                "name": tool.get("name").cloned().unwrap_or_default(),
                "description"?: tool.non_null("description").cloned(),
                "parameters": tool.get("inputSchema").cloned().unwrap_or_default(),
                "strict"?: tool.non_null("strict").cloned(),
            },
        });
    }
    if tool_choice.is_null() {
        return Ok((Some(out), None));
    }
    let choice = match tool_choice.str_at("type").unwrap_or("") {
        kind @ ("auto" | "none" | "required") => Json::from(kind),
        "tool" => jobj! {
            "type": "function",
            "function": jobj! { "name": tool_choice.get("toolName").cloned().unwrap_or_default() },
        },
        other => return Err(unsupported(&format!("tool choice type: {other}"))),
    };
    Ok((Some(out), Some(choice)))
}

// ------------------------------------------------------------------ The model

pub struct ChatModel {
    provider: String,
    name: String,
    model_id: String,
    base_url: String,
    headers: Headers,
    fetch: DynFetch,
}

/// `parseProviderOptions` against `openaiCompatibleLanguageModelChatOptions`.
fn parse_options(all: Option<&Json>, provider: &str) -> Result<Option<Obj>, AiError> {
    let Some(value) = all.and_then(|all| all.non_null(provider)) else { return Ok(None) };
    let invalid = || AiError::other(format!("invalid {provider} provider options"));
    let object = value.as_obj().ok_or_else(invalid)?;
    let strings = ["user", "reasoningEffort", "textVerbosity"].iter().all(|key| optional(object.get(key), Json::is_str));
    if !strings || !optional(object.get("strictJsonSchema"), |v| v.as_bool().is_some()) {
        return Err(invalid());
    }
    Ok(Some(object.clone()))
}

impl ChatModel {
    /// `getArgs`, and the additions `doStream` makes to them. Also returns `metadataKey`.
    fn get_args(&self, options: &CallOptions) -> Result<(Json, String), AiError> {
        let all = options.provider_options.as_ref();
        let camel = to_camel_case(&self.name);
        let mut merged = Obj::new();
        for key in ["openai-compatible", "openaiCompatible", self.name.as_str(), camel.as_str()] {
            if let Some(parsed) = parse_options(all, key)? {
                merged.extend_from(&parsed);
            }
        }
        let (tools, tool_choice) = prepare_tools(options.tools.as_ref(), &options.tool_choice)?;
        // `resolveProviderOptionsKey`: the camelCase name when the caller used it, the raw name otherwise.
        let metadata_key = if camel != self.name && all.and_then(|all| all.non_null(&camel)).is_some() { camel.clone() } else { self.name.clone() };

        let mut body = Slots::default();
        body.set("model", Some(Json::from(self.model_id.as_str())));
        body.set("user", merged.get("user").cloned());
        body.set("max_tokens", options.max_output_tokens.map(Json::from));
        for unset in ["temperature", "top_p", "frequency_penalty", "presence_penalty", "response_format", "stop", "seed"] {
            body.set(unset, None);
        }
        for source in [self.name.as_str(), camel.as_str()] {
            if let Some(object) = all.and_then(|all| all.get(source)).and_then(Json::as_obj) {
                for (key, value) in object.iter() {
                    if !OPTION_KEYS.contains(&key) {
                        body.set(key, Some(value.clone()));
                    }
                }
            }
        }
        let effort = merged.get("reasoningEffort").cloned().or_else(|| {
            options.reasoning.as_deref().filter(|level| *level != "provider-default").map(Json::from)
        });
        body.set("reasoning_effort", effort);
        body.set("verbosity", merged.get("textVerbosity").cloned());
        body.set("messages", Some(Json::Arr(convert_messages(&options.prompt, &metadata_key)?)));
        body.set("tools", tools.map(Json::Arr));
        body.set("tool_choice", tool_choice);
        body.set("stream", Some(Json::from(true)));
        body.set("stream_options", Some(jobj! { "include_usage": true }));
        Ok((body.into_json(), metadata_key))
    }

    /// `config.url`. It goes through `new URL(...)`, which throws for the relative URL an empty `baseURL` leaves.
    fn url(&self) -> Result<String, AiError> {
        let url = format!("{}/chat/completions", self.base_url);
        let scheme = url.split_once(':').map_or("", |(scheme, _)| scheme);
        let absolute = scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
        if !absolute {
            return Err(AiError::other("Invalid URL"));
        }
        Ok(url)
    }

    async fn stream(&self, options: CallOptions) -> Result<ModelStream, AiError> {
        let (body, metadata_key) = self.get_args(&options)?;
        let url = self.url()?;
        let response = post_json(&self.fetch, &url, self.headers.clone(), &body, &options.cancel, ERROR_SHAPE).await?;
        Ok(ModelStream::sse(response.body, Box::new(ChatStream::new(metadata_key))))
    }
}

/// `defaultOpenAICompatibleErrorStructure`.
const ERROR_SHAPE: ErrorShape = ErrorShape {
    message: |body| valid_error_chunk(body).then(|| body.path(&["error", "message"]).and_then(Json::as_str).map(str::to_owned)).flatten(),
    is_retryable: None,
};

impl LanguageModel for ChatModel {
    fn provider(&self) -> &str {
        &self.provider
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn do_stream(&self, options: CallOptions) -> BoxFuture<'_, Result<ModelStream, AiError>> {
        Box::pin(self.stream(options))
    }
}

// ------------------------------------------------------------------ Chunk schemas

fn valid_token_usage(usage: &Json) -> bool {
    let counts = |object: &Json, keys: &[&str]| keys.iter().all(|key| nullish(object.get(key), is_number));
    usage.is_obj()
        && counts(usage, &["prompt_tokens", "completion_tokens", "total_tokens"])
        && nullish(usage.get("prompt_tokens_details"), |d| d.is_obj() && counts(d, &["cached_tokens"]))
        && nullish(usage.get("completion_tokens_details"), |d| {
            d.is_obj() && counts(d, &["reasoning_tokens", "accepted_prediction_tokens", "rejected_prediction_tokens"])
        })
}

fn valid_tool_call(call: &Json) -> bool {
    call.is_obj()
        && nullish(call.get("index"), is_number)
        && nullish(call.get("id"), Json::is_str)
        && call.get("function").is_some_and(|function| {
            function.is_obj() && nullish(function.get("name"), Json::is_str) && nullish(function.get("arguments"), Json::is_str)
        })
        && nullish(call.get("extra_content"), |extra| {
            extra.is_obj()
                && nullish(extra.get("google"), |google| google.is_obj() && nullish(google.get("thought_signature"), Json::is_str))
        })
}

fn valid_delta(delta: &Json) -> bool {
    delta.is_obj()
        && nullish(delta.get("role"), |role| matches!(role.as_str(), Some("assistant" | "")))
        && ["content", "reasoning_content", "reasoning"].iter().all(|key| nullish(delta.get(key), Json::is_str))
        && nullish(delta.get("tool_calls"), |calls| calls.as_arr().is_some_and(|calls| calls.iter().all(valid_tool_call)))
}

/// `chunkBaseSchema`.
fn valid_base_chunk(chunk: &Json) -> bool {
    chunk.is_obj()
        && nullish(chunk.get("id"), Json::is_str)
        && nullish(chunk.get("created"), is_number)
        && nullish(chunk.get("model"), Json::is_str)
        && chunk.get("choices").and_then(Json::as_arr).is_some_and(|choices| {
            choices.iter().all(|choice| {
                choice.is_obj() && nullish(choice.get("delta"), valid_delta) && nullish(choice.get("finish_reason"), Json::is_str)
            })
        })
        && nullish(chunk.get("usage"), valid_token_usage)
}

/// `openaiCompatibleErrorDataSchema`.
fn valid_error_chunk(chunk: &Json) -> bool {
    chunk.get("error").is_some_and(|error| {
        error.is_obj()
            && error.get("message").is_some_and(Json::is_str)
            && nullish(error.get("type"), Json::is_str)
            && nullish(error.get("code"), |code| code.is_str() || is_number(code))
    })
}

/// `{ message, type, param, code }`: the error schema's `z.object` strips everything else, a `status` included,
/// before `normalizeStreamProviderError` looks.
fn stripped_error(error: &Json) -> Json {
    let mut out = Obj::new();
    for key in ["message", "type", "param", "code"] {
        if let Some(value) = error.get(key) {
            out.insert(key, value.clone());
        }
    }
    Json::Obj(out)
}

/// What an `error` part holding `payload` reaches `main.ts` as: `normalizeStreamProviderError` for a well-formed
/// payload, and otherwise the value itself, which `classify` renders with `String(error)`.
fn error_from_chunk(payload: &Json) -> AiError {
    if !payload.is_obj() {
        return AiError::other(js_string(payload));
    }
    let details = payload
        .path(&["response", "error"])
        .filter(|v| v.is_obj())
        .or_else(|| payload.get("error").filter(|v| v.is_obj()))
        .unwrap_or(payload);
    if !details.get("message").is_some_and(Json::is_str) {
        return AiError::other("[object Object]");
    }
    crate::stream_text::stream_error_from_payload(payload)
}

// ------------------------------------------------------------------ Streaming

/// `mapOpenAICompatibleFinishReason`.
fn map_finish_reason(reason: &str) -> &'static str {
    match reason {
        "stop" => "stop",
        "length" => "length",
        "content_filter" => "content-filter",
        "function_call" | "tool_calls" => "tool-calls",
        _ => "other",
    }
}

/// `OpenAICompatibleStreamingToolCallDelta`.
#[derive(Default)]
struct ToolCallDelta {
    /// A plain number: nothing but a `Map` key, and the schema does not require an integer.
    index: Option<f64>,
    id: Option<String>,
    name: Option<String>,
    arguments: Option<String>,
    extra_content: Option<Json>,
}

impl ToolCallDelta {
    fn parse(call: &Json) -> Self {
        Self {
            index: call.non_null("index").and_then(Json::as_f64),
            id: call.non_null("id").and_then(Json::as_str).map(str::to_owned),
            name: call.path(&["function", "name"]).and_then(Json::as_str).map(str::to_owned),
            arguments: call.path(&["function", "arguments"]).and_then(Json::as_str).map(str::to_owned),
            extra_content: call.non_null("extra_content").cloned(),
        }
    }
}

struct TrackedToolCall {
    id: String,
    name: String,
    arguments: String,
    has_finished: bool,
    metadata: Option<Json>,
}

/// `StreamingToolCallTracker`, with the options the chat model gives it.
struct ToolCallTracker {
    metadata_key: String,
    calls: Vec<TrackedToolCall>,
    by_id: HashMap<String, usize>,
    by_index: HashMap<u64, usize>,
    latest: Option<usize>,
}

impl ToolCallTracker {
    fn new(metadata_key: String) -> Self {
        Self { metadata_key, calls: Vec::new(), by_id: HashMap::new(), by_index: HashMap::new(), latest: None }
    }

    /// `processDelta`. An `Err` is what it throws.
    fn process_delta(&mut self, delta: ToolCallDelta, out: &mut Vec<StreamPart>) -> Result<(), AiError> {
        let found = match (&delta.id, delta.index) {
            (Some(id), _) if !id.is_empty() => self.by_id.get(id).copied(),
            (_, Some(index)) => self.by_index.get(&index.to_bits()).copied(),
            _ => self.latest,
        };
        let index = delta.index;
        let at = match found {
            None => self.process_new(delta, out)?,
            Some(at) => {
                let call = &mut self.calls[at];
                if !call.has_finished
                    && let Some(arguments) = delta.arguments
                {
                    call.arguments.push_str(&arguments);
                    out.push(StreamPart::ToolInputDelta { id: call.id.clone(), delta: arguments });
                }
                at
            }
        };
        if let Some(index) = index {
            self.by_index.insert(index.to_bits(), at);
        }
        self.latest = Some(at);
        Ok(())
    }

    fn process_new(&mut self, delta: ToolCallDelta, out: &mut Vec<StreamPart>) -> Result<usize, AiError> {
        let Some(id) = delta.id else { return Err(AiError::other("Expected 'id' to be a string.")) };
        let Some(name) = delta.name else { return Err(AiError::other("Expected 'function.name' to be a string.")) };
        out.push(StreamPart::ToolInputStart { id: id.clone(), tool_name: name.clone(), provider_executed: None, dynamic: None });
        // `extractMetadata`: Google's thought signature, through its OpenAI-compatible endpoint.
        let signature = delta.extra_content.as_ref().and_then(|extra| extra.path(&["google", "thought_signature"])).and_then(Json::as_str).filter(|s| !s.is_empty());
        let metadata = signature.map(|signature| {
            let mut metadata = Obj::new();
            metadata.insert(self.metadata_key.as_str(), jobj! { "thoughtSignature": signature });
            Json::Obj(metadata)
        });
        let arguments = delta.arguments.unwrap_or_default();
        if !arguments.is_empty() {
            out.push(StreamPart::ToolInputDelta { id: id.clone(), delta: arguments.clone() });
        }
        let at = self.calls.len();
        if !id.is_empty() {
            self.by_id.insert(id.clone(), at);
        }
        self.calls.push(TrackedToolCall { id, name, arguments, has_finished: false, metadata });
        Ok(at)
    }

    /// `flush`: tool calls finish only here, since a parsable argument buffer can still be the prefix of a longer one.
    fn flush(&mut self, out: &mut Vec<StreamPart>) {
        for call in &mut self.calls {
            if call.has_finished {
                continue;
            }
            out.push(StreamPart::ToolInputEnd { id: call.id.clone() });
            out.push(StreamPart::ToolCall {
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
                input: call.arguments.clone(),
                provider_executed: None,
                dynamic: None,
                provider_metadata: call.metadata.clone(),
            });
            call.has_finished = true;
        }
    }
}

#[derive(Default)]
struct PendingToolCall {
    id: Option<String>,
    buffered_arguments: String,
    extra_content: Option<Json>,
}

struct ChatStream {
    metadata_key: String,
    tracker: ToolCallTracker,
    /// Deltas by `index` until `function.name` is known, in the `Map`'s insertion order: some providers send the
    /// first delta without a name, which the tracker rejects on a first chunk.
    pending: Vec<(u64, PendingToolCall)>,
    forwarded: HashSet<u64>,
    finish_reason: Option<FinishReason>,
    usage: Option<Json>,
    is_first_chunk: bool,
    active_reasoning: bool,
    active_text: bool,
    /// The transform threw: the stream has errored, and nothing else comes out of it.
    failed: bool,
}

impl ChatStream {
    fn new(metadata_key: String) -> Self {
        Self {
            tracker: ToolCallTracker::new(metadata_key.clone()),
            metadata_key,
            pending: Vec::new(),
            forwarded: HashSet::new(),
            finish_reason: None,
            usage: None,
            is_first_chunk: true,
            active_reasoning: false,
            active_text: false,
            failed: false,
        }
    }

    fn end_reasoning(&mut self, out: &mut Vec<StreamPart>) {
        if self.active_reasoning {
            out.push(StreamPart::ReasoningEnd { id: "reasoning-0".to_owned(), provider_metadata: None });
            self.active_reasoning = false;
        }
    }

    /// `processToolCallDelta`.
    fn process_tool_call_delta(&mut self, delta: ToolCallDelta, out: &mut Vec<StreamPart>) -> Result<(), AiError> {
        let Some(index) = delta.index.map(f64::to_bits).filter(|index| !self.forwarded.contains(index)) else {
            return self.tracker.process_delta(delta, out);
        };
        let at = match self.pending.iter().position(|(known, _)| *known == index) {
            Some(at) => {
                let pending = &mut self.pending[at].1;
                if pending.id.is_none() {
                    pending.id.clone_from(&delta.id);
                }
                if pending.extra_content.is_none() {
                    pending.extra_content.clone_from(&delta.extra_content);
                }
                at
            }
            None => {
                self.pending.push((index, PendingToolCall { id: delta.id.clone(), buffered_arguments: String::new(), extra_content: delta.extra_content.clone() }));
                self.pending.len() - 1
            }
        };
        if let Some(arguments) = &delta.arguments {
            self.pending[at].1.buffered_arguments.push_str(arguments);
        }
        if let Some(name) = delta.name {
            let (_, pending) = self.pending.remove(at);
            self.forwarded.insert(index);
            return self.tracker.process_delta(
                ToolCallDelta {
                    index: delta.index,
                    id: pending.id,
                    name: Some(name),
                    arguments: Some(pending.buffered_arguments),
                    extra_content: pending.extra_content,
                },
                out,
            );
        }
        Ok(())
    }

    fn on_chunk(&mut self, value: &Json, out: &mut Vec<StreamPart>) -> Result<(), AiError> {
        if self.is_first_chunk {
            self.is_first_chunk = false;
            out.push(StreamPart::ResponseMetadata {
                id: value.non_null("id").and_then(Json::as_str).map(str::to_owned),
                model_id: value.non_null("model").and_then(Json::as_str).map(str::to_owned),
            });
        }
        if let Some(usage) = value.non_null("usage") {
            self.usage = Some(usage.clone());
        }
        let Some(choice) = value.get("choices").and_then(Json::as_arr).and_then(|choices| choices.first()) else { return Ok(()) };
        if let Some(reason) = choice.non_null("finish_reason").and_then(Json::as_str) {
            self.finish_reason = Some(FinishReason::new(map_finish_reason(reason), Some(reason.to_owned())));
        }
        let Some(delta) = choice.non_null("delta") else { return Ok(()) };

        // Reasoning goes before the text of the same chunk.
        let reasoning = delta.non_null("reasoning_content").or_else(|| delta.non_null("reasoning")).and_then(Json::as_str).unwrap_or("");
        if !reasoning.is_empty() {
            if !self.active_reasoning {
                out.push(StreamPart::ReasoningStart { id: "reasoning-0".to_owned(), provider_metadata: None });
                self.active_reasoning = true;
            }
            out.push(StreamPart::ReasoningDelta { id: "reasoning-0".to_owned(), delta: reasoning.to_owned(), provider_metadata: None });
        }
        let content = delta.non_null("content").and_then(Json::as_str).unwrap_or("");
        if !content.is_empty() {
            self.end_reasoning(out);
            if !self.active_text {
                out.push(StreamPart::TextStart { id: "txt-0".to_owned(), provider_metadata: None });
                self.active_text = true;
            }
            out.push(StreamPart::TextDelta { id: "txt-0".to_owned(), delta: content.to_owned(), provider_metadata: None });
        }
        if let Some(calls) = delta.non_null("tool_calls").and_then(Json::as_arr) {
            self.end_reasoning(out);
            for call in calls {
                self.process_tool_call_delta(ToolCallDelta::parse(call), out)?;
            }
        }
        Ok(())
    }

    fn fail(&mut self, error: AiError, out: &mut Vec<StreamPart>) {
        self.failed = true;
        out.push(StreamPart::Error(error));
    }
}

impl ChunkTransform for ChatStream {
    fn start(&mut self, out: &mut Vec<StreamPart>) {
        out.push(StreamPart::StreamStart { warnings: Vec::new() });
    }

    fn chunk(&mut self, chunk: Result<Json, AiError>, out: &mut Vec<StreamPart>) {
        if self.failed {
            return;
        }
        let value = match chunk {
            Ok(value) => value,
            Err(error) => {
                self.finish_reason = Some(FinishReason::new("error", None));
                out.push(StreamPart::Error(error));
                return;
            }
        };
        // `z.union([chunkBaseSchema, errorSchema])`, first match wins. The base schema is loose, so an `error` it
        // lets through arrives unchanged, whatever it holds.
        let base = valid_base_chunk(&value);
        if !base && !valid_error_chunk(&value) {
            self.finish_reason = Some(FinishReason::new("error", None));
            out.push(StreamPart::Error(AiError::validation(&value, "Invalid input")));
            return;
        }
        if let Some(error) = value.get("error") {
            self.finish_reason = Some(FinishReason::new("error", None));
            let payload = if base { error.clone() } else { stripped_error(error) };
            out.push(StreamPart::Error(error_from_chunk(&payload)));
            return;
        }
        if let Err(error) = self.on_chunk(&value, out) {
            self.fail(error, out);
        }
    }

    fn flush(&mut self, out: &mut Vec<StreamPart>) {
        if self.failed {
            return;
        }
        self.end_reasoning(out);
        if self.active_text {
            out.push(StreamPart::TextEnd { id: "txt-0".to_owned(), provider_metadata: None });
        }
        // A delta that never received a name: the tracker throws, keeping the invalid-response semantics.
        for (index, pending) in std::mem::take(&mut self.pending) {
            let delta = ToolCallDelta {
                index: Some(f64::from_bits(index)),
                id: pending.id,
                name: None,
                arguments: Some(pending.buffered_arguments),
                extra_content: None,
            };
            if let Err(error) = self.tracker.process_delta(delta, out) {
                self.fail(error, out);
                return;
            }
        }
        self.tracker.flush(out);
        let finish_reason = self.finish_reason.take().unwrap_or_else(|| {
            out.push(StreamPart::Error(AiError::other("Response stream ended without a finish reason.")));
            FinishReason::new("error", None)
        });
        let details = self.usage.as_ref().and_then(|usage| usage.non_null("completion_tokens_details"));
        let inner = jobj! {
            "acceptedPredictionTokens"?: details.and_then(|d| d.non_null("accepted_prediction_tokens")).cloned(),
            "rejectedPredictionTokens"?: details.and_then(|d| d.non_null("rejected_prediction_tokens")).cloned(),
        };
        let mut provider_metadata = Obj::new();
        provider_metadata.insert(self.metadata_key.as_str(), inner);
        out.push(StreamPart::Finish {
            finish_reason,
            usage: convert_chat_usage(self.usage.as_ref()),
            provider_metadata: Some(Json::Obj(provider_metadata)),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camel_case_follows_the_sdk() {
        assert_eq!(to_camel_case("openai-compatible"), "openaiCompatible");
        assert_eq!(to_camel_case("openai-chat"), "openaiChat");
        assert_eq!(to_camel_case("my_provider-X"), "myProvider-X");
        assert_eq!(to_camel_case("plain"), "plain");
    }

    #[test]
    fn slots_keep_the_position_of_an_undefined_property() {
        let mut body = Slots::default();
        body.set("model", Some(Json::from("m")));
        body.set("temperature", None);
        body.set("seed", None);
        body.spread(jobj! { "extra": 1u64, "temperature": 0.5f64 }.as_obj().unwrap());
        body.set("seed", None);
        assert_eq!(body.into_json().to_text(), r#"{"model":"m","temperature":0.5,"extra":1}"#);
    }

    #[test]
    fn text_files_decode_as_the_sdk_decodes_them() {
        assert_eq!(base64_to_text("aGk=").unwrap(), "hi");
        assert_eq!(base64_to_text("aGk").unwrap(), "hi");
        assert_eq!(base64_to_text("77u/aGk=").unwrap(), "hi");
        assert!(base64_to_text("***").is_err());
    }

    #[test]
    fn a_chunk_with_a_bad_field_fails_the_base_schema() {
        let parse = |text: &str| Json::parse(text).unwrap();
        assert!(valid_base_chunk(&parse(r#"{"choices":[]}"#)));
        assert!(!valid_base_chunk(&parse(r#"{"id":"x"}"#)));
        assert!(!valid_base_chunk(&parse(r#"{"choices":[{"delta":{"role":"tool"}}]}"#)));
        assert!(!valid_base_chunk(&parse(r#"{"choices":[{"delta":{"content":[]}}]}"#)));
        assert!(!valid_base_chunk(&parse(r#"{"choices":[{"delta":{"tool_calls":[{"index":0}]}}]}"#)));
        assert!(!valid_base_chunk(&parse(r#"{"choices":[],"usage":{"prompt_tokens":"1"}}"#)));
        assert!(valid_error_chunk(&parse(r#"{"error":{"message":"m","code":429}}"#)));
        assert!(!valid_error_chunk(&parse(r#"{"error":{"message":1}}"#)));
    }
}
