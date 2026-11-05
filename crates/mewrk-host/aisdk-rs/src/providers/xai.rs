//! `@ai-sdk/xai`: `createXai` and the streaming path of what `provider(modelId)` returns, the Responses model.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::api::{ErrorShape, VERSION_XAI, combine_headers, generate_id, post_json, with_user_agent_suffix, without_trailing_slash};
use crate::error::AiError;
use crate::http::{BoxFuture, DynFetch, Headers};
use crate::jobj;
use crate::json::{Json, Obj};
use crate::model::{CallOptions, ChunkTransform, DynModel, FinishReason, LanguageModel, ModelStream, ModelUsage, StreamPart, SupportedUrls};
use crate::prompt::{resolve_full_media_type, top_level_media_type};
use crate::stream_text::stream_error_from_payload;

use super::ProviderSettings;
use super::openai_compatible::{http_trim, is_number, nullish, optional, unsupported};

pub fn create(target: ProviderSettings) -> Result<DynModel, AiError> {
    let base_url = without_trailing_slash(target.base_url.as_deref().unwrap_or("https://api.x.ai/v1")).to_owned();
    // `loadApiKey` returns any string as it is, an empty one included, so the header is always there.
    let mut headers = Headers::new();
    headers.set("authorization", http_trim(&format!("Bearer {}", target.api_key)));
    let headers = combine_headers(&headers, &target.headers);
    let headers = with_user_agent_suffix(headers, &[&format!("ai-sdk/xai/{VERSION_XAI}")]);
    Ok(Arc::new(XaiModel { model_id: target.model_id, base_url, headers, fetch: target.fetch }))
}

pub struct XaiModel {
    model_id: String,
    base_url: String,
    headers: Headers,
    fetch: DynFetch,
}

// ------------------------------------------------------------------ Input

fn data_url(part: &Json) -> Result<String, AiError> {
    Ok(format!("data:{};base64,{}", resolve_full_media_type(part)?, part.path(&["data", "data"]).and_then(Json::as_str).unwrap_or("")))
}

/// `resolveProviderReference`.
fn reference_id(part: &Json) -> Result<Json, AiError> {
    let reference = part.path(&["data", "reference"]);
    reference.and_then(|r| r.non_null("xai")).cloned().ok_or_else(|| {
        let available = reference.and_then(Json::as_obj).map_or_else(Vec::new, |r| r.keys().collect::<Vec<_>>()).join(", ");
        AiError::other(format!("No provider reference found for provider 'xai'. Available providers: {available}"))
    })
}

fn user_file_part(block: &Json) -> Result<Json, AiError> {
    let kind = block.path(&["data", "type"]).and_then(Json::as_str).unwrap_or("");
    let url = || block.path(&["data", "url"]).cloned().unwrap_or_default();
    match kind {
        "reference" => Ok(jobj! { "type": "input_file", "file_id": reference_id(block)? }),
        "text" => Err(unsupported("text file parts")),
        _ => {
            let media_type = block.str_at("mediaType").unwrap_or("");
            if top_level_media_type(media_type) == "image" {
                let image_url = if kind == "url" { url() } else { Json::from(data_url(block)?) };
                // `xaiFilePartProviderOptions`: the API silently ignores an invalid value, so it is refused here.
                let detail = match block.path(&["providerOptions", "xai"]).filter(|v| !v.is_null()) {
                    None => None,
                    Some(options) => {
                        let valid = options.is_obj()
                            && optional(options.get("imageDetail"), |detail| matches!(detail.as_str(), Some("low" | "high" | "auto")));
                        if !valid {
                            return Err(AiError::other("invalid xai provider options"));
                        }
                        options.get("imageDetail").cloned()
                    }
                };
                return Ok(jobj! { "type": "input_image", "image_url": image_url, "detail"?: detail });
            }
            if kind == "url" {
                return Ok(jobj! { "type": "input_file", "file_url": url() });
            }
            Err(unsupported(&format!(
                "file part media type {media_type} as inline data (xAI Responses requires a URL or a Files API reference for non-image files)"
            )))
        }
    }
}

fn item_id(part: &Json) -> Option<&Json> {
    part.path(&["providerOptions", "xai", "itemId"]).filter(|id| id.is_str())
}

fn tool_output(output: &Json) -> Result<Json, AiError> {
    Ok(match output.str_at("type").unwrap_or("") {
        "text" | "error-text" => output.get("value").cloned().unwrap_or_default(),
        "execution-denied" => output.non_null("reason").cloned().unwrap_or_else(|| Json::from("tool execution denied")),
        "json" | "error-json" => Json::from(output.get("value").cloned().unwrap_or_default().to_text()),
        "content" => {
            let mut items = Vec::new();
            for item in output.get("value").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice) {
                match item.str_at("type").unwrap_or("") {
                    "text" => items.push(jobj! { "type": "input_text", "text": item.get("text").cloned().unwrap_or_default() }),
                    "file" if top_level_media_type(item.str_at("mediaType").unwrap_or("")) == "image" => {
                        match item.path(&["data", "type"]).and_then(Json::as_str) {
                            Some("url") => items.push(jobj! { "type": "input_image", "image_url": item.path(&["data", "url"]).cloned().unwrap_or_default() }),
                            Some("data") => items.push(jobj! { "type": "input_image", "image_url": data_url(item)? }),
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
            Json::Arr(items)
        }
        _ => Json::from(""),
    })
}

/// `convertToXaiResponsesInput`.
fn convert_input(prompt: &[Json]) -> Result<Vec<Json>, AiError> {
    let mut input = Vec::new();
    for message in prompt {
        let content = message.get("content").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice);
        match message.str_at("role").unwrap_or("") {
            "system" => input.push(jobj! { "role": "system", "content": message.get("content").cloned().unwrap_or_default() }),
            "user" => {
                let mut parts = Vec::with_capacity(content.len());
                for block in content {
                    match block.str_at("type").unwrap_or("") {
                        "text" => parts.push(jobj! { "type": "input_text", "text": block.get("text").cloned().unwrap_or_default() }),
                        "file" => parts.push(user_file_part(block)?),
                        _ => {}
                    }
                }
                input.push(jobj! { "role": "user", "content": parts });
            }
            "assistant" => {
                for part in content {
                    match part.str_at("type").unwrap_or("") {
                        "text" => input.push(jobj! {
                            "role": "assistant",
                            "content": part.get("text").cloned().unwrap_or_default(),
                            "id"?: item_id(part).cloned(),
                        }),
                        "tool-call" => {
                            if part.get("providerExecuted").is_some_and(|flag| flag.as_bool() != Some(false) && !flag.is_null()) {
                                continue;
                            }
                            let call_id = part.get("toolCallId").cloned().unwrap_or_default();
                            input.push(jobj! {
                                "type": "function_call",
                                "id": item_id(part).cloned().unwrap_or_else(|| call_id.clone()),
                                "call_id": call_id,
                                "name": part.get("toolName").cloned().unwrap_or_default(),
                                "arguments": part.get("input").cloned().unwrap_or_default().to_text(),
                                "status": "completed",
                            });
                        }
                        "reasoning" => {
                            let id = item_id(part);
                            let encrypted = part.path(&["providerOptions", "xai", "reasoningEncryptedContent"]).filter(|v| v.is_str());
                            // Anything else cannot be sent back to xAI.
                            if id.is_none() && encrypted.is_none() {
                                continue;
                            }
                            let text = part.str_at("text").unwrap_or("");
                            let summary = if text.is_empty() { Vec::new() } else { vec![jobj! { "type": "summary_text", "text": text }] };
                            input.push(jobj! {
                                "type": "reasoning",
                                "id": id.cloned().unwrap_or_else(|| Json::from("")),
                                "summary": summary,
                                "status": "completed",
                                "encrypted_content"?: encrypted.cloned(),
                            });
                        }
                        _ => {}
                    }
                }
            }
            "tool" => {
                for part in content {
                    if part.str_at("type") == Some("tool-approval-response") {
                        continue;
                    }
                    input.push(jobj! {
                        "type": "function_call_output",
                        "call_id": part.get("toolCallId").cloned().unwrap_or_default(),
                        "output": tool_output(part.get("output").unwrap_or(&Json::Null))?,
                    });
                }
            }
            _ => {}
        }
    }
    Ok(input)
}

// ------------------------------------------------------------------ Tools

/// `removeAdditionalPropertiesFalse`.
fn without_additional_properties_false(value: &Json) -> Json {
    match value {
        Json::Arr(items) => Json::Arr(items.iter().map(without_additional_properties_false).collect()),
        Json::Obj(object) => {
            let mut out = Obj::new();
            for (key, entry) in object.iter() {
                if key == "additionalProperties" && entry.as_bool() == Some(false) {
                    continue;
                }
                out.insert(key, without_additional_properties_false(entry));
            }
            Json::Obj(out)
        }
        other => other.clone(),
    }
}

fn provider_tool(tool: &Json) -> Option<Json> {
    let args = tool.get("args").cloned().unwrap_or_else(Json::obj);
    let arg = |name: &str| args.get(name).cloned();
    Some(match tool.str_at("id")? {
        "xai.web_search" => jobj! {
            "type": "web_search",
            "allowed_domains"?: arg("allowedDomains"),
            "excluded_domains"?: arg("excludedDomains"),
            "enable_image_search"?: arg("enableImageSearch"),
            "enable_image_understanding"?: arg("enableImageUnderstanding"),
        },
        "xai.x_search" => jobj! {
            "type": "x_search",
            "allowed_x_handles"?: arg("allowedXHandles"),
            "excluded_x_handles"?: arg("excludedXHandles"),
            "from_date"?: arg("fromDate"),
            "to_date"?: arg("toDate"),
            "enable_image_understanding"?: arg("enableImageUnderstanding"),
            "enable_video_understanding"?: arg("enableVideoUnderstanding"),
        },
        "xai.code_execution" => jobj! { "type": "code_interpreter" },
        "xai.view_image" => jobj! { "type": "view_image" },
        "xai.view_x_video" => jobj! { "type": "view_x_video" },
        "xai.image_generation" => jobj! { "type": "image_generation", "action"?: arg("action") },
        "xai.file_search" => jobj! {
            "type": "file_search",
            "vector_store_ids"?: arg("vectorStoreIds"),
            "max_num_results"?: arg("maxNumResults"),
        },
        "xai.mcp" => jobj! {
            "type": "mcp",
            "server_url"?: arg("serverUrl"),
            "server_label"?: arg("serverLabel"),
            "server_description"?: arg("serverDescription"),
            "allowed_tools"?: arg("allowedTools"),
            "headers"?: arg("headers"),
            "authorization"?: arg("authorization"),
        },
        _ => return None,
    })
}

/// `prepareResponsesTools`.
fn prepare_tools(tools: Option<&Vec<Json>>, tool_choice: &Json) -> Result<(Vec<Json>, Option<Json>), AiError> {
    let Some(tools) = tools.filter(|tools| !tools.is_empty()) else { return Ok((Vec::new(), None)) };
    let mut out = Vec::with_capacity(tools.len());
    for tool in tools {
        if tool.str_at("type") == Some("provider") {
            out.extend(provider_tool(tool));
        } else {
            out.push(jobj! {
                "type": "function",
                "name": tool.get("name").cloned().unwrap_or_default(),
                "description"?: tool.non_null("description").cloned(),
                "parameters": without_additional_properties_false(tool.get("inputSchema").unwrap_or(&Json::Null)),
                "strict"?: tool.non_null("strict").cloned(),
            });
        }
    }
    if tool_choice.is_null() {
        return Ok((out, None));
    }
    let choice = match tool_choice.str_at("type").unwrap_or("") {
        kind @ ("auto" | "none" | "required") => Some(Json::from(kind)),
        "tool" => {
            let name = tool_choice.str_at("toolName").unwrap_or("");
            // The API cannot force a server-side tool, and an unknown name is no choice at all.
            tools
                .iter()
                .rfind(|tool| tool.str_at("name") == Some(name))
                .filter(|tool| tool.str_at("type") != Some("provider"))
                .map(|_| jobj! { "type": "function", "name": name })
        }
        other => return Err(unsupported(&format!("tool choice type: {other}"))),
    };
    Ok((out, choice))
}

// ------------------------------------------------------------------ The model

/// `xaiLanguageModelResponsesOptions`, as `parseProviderOptions` applies it.
fn parse_options(options: &CallOptions) -> Result<Option<&Json>, AiError> {
    let Some(value) = options.provider_options.as_ref().and_then(|all| all.non_null("xai")) else { return Ok(None) };
    let one_of = |value: &Json, allowed: &[&str]| value.as_str().is_some_and(|text| allowed.contains(&text));
    let valid = value.is_obj()
        && optional(value.get("reasoningEffort"), |v| one_of(v, &["none", "low", "medium", "high", "xhigh"]))
        && optional(value.get("reasoningSummary"), |v| one_of(v, &["auto", "concise", "detailed"]))
        && optional(value.get("logprobs"), |v| v.as_bool().is_some())
        && optional(value.get("topLogprobs"), |v| v.as_f64().is_some_and(|n| n.fract() == 0.0 && (0.0..=8.0).contains(&n)))
        && optional(value.get("serviceTier"), |v| one_of(v, &["default", "priority"]))
        && optional(value.get("store"), |v| v.as_bool().is_some())
        && optional(value.get("previousResponseId"), Json::is_str)
        && nullish(value.get("include"), |v| v.as_arr().is_some_and(|items| items.iter().all(|item| one_of(item, &["file_search_call.results"]))));
    if !valid {
        return Err(AiError::other("invalid xai provider options"));
    }
    Ok(Some(value))
}

/// `supportsReasoningEffort`: `^grok-4\.20(-\d{4})?-(non-)?reasoning$` reject the parameter for every value.
fn supports_reasoning_effort(model_id: &str) -> bool {
    let Some(rest) = model_id.strip_prefix("grok-4.20") else { return true };
    let rest = match rest.strip_prefix('-') {
        Some(tail) if tail.len() >= 4 && tail.as_bytes()[..4].iter().all(u8::is_ascii_digit) && tail[4..].starts_with('-') => &tail[4..],
        _ => rest,
    };
    !matches!(rest, "-reasoning" | "-non-reasoning")
}

/// The names the host declared native tools under, for a stream that reports its own kinds.
#[derive(Default)]
struct ToolNames {
    web_search: Option<String>,
    x_search: Option<String>,
    code_execution: Option<String>,
    mcp: Option<String>,
    file_search: Option<String>,
    image_generation: Option<String>,
}

impl ToolNames {
    fn new(tools: Option<&Vec<Json>>) -> Self {
        let find = |id: &str| {
            tools?
                .iter()
                .find(|tool| tool.str_at("type") == Some("provider") && tool.str_at("id") == Some(id))
                .and_then(|tool| tool.str_at("name"))
                .map(str::to_owned)
        };
        Self {
            web_search: find("xai.web_search"),
            x_search: find("xai.x_search"),
            code_execution: find("xai.code_execution"),
            mcp: find("xai.mcp"),
            file_search: find("xai.file_search"),
            image_generation: find("xai.image_generation"),
        }
    }
}

impl XaiModel {
    /// `getArgs`, and the `stream: true` `doStream` adds.
    fn get_args(&self, options: &CallOptions) -> Result<Json, AiError> {
        let parsed = parse_options(options)?;
        let option = |key: &str| parsed.and_then(|o| o.get(key));
        let input = convert_input(&options.prompt)?;
        let (tools, tool_choice) = prepare_tools(options.tools.as_ref(), &options.tool_choice)?;

        let store_off = option("store").and_then(Json::as_bool) == Some(false);
        // A falsy `include` (`null`) reads as none given.
        let mut include = option("include").and_then(Json::as_arr).cloned();
        if store_off {
            // Encrypted content is what preserves reasoning without server-side storage.
            include.get_or_insert_with(Vec::new).push(Json::from("reasoning.encrypted_content"));
        }

        let effort = option("reasoningEffort").and_then(Json::as_str).map(str::to_owned).or_else(|| {
            let reasoning = options.reasoning.as_deref().filter(|level| *level != "provider-default")?;
            if !supports_reasoning_effort(&self.model_id) {
                return None;
            }
            match reasoning {
                "none" => Some("none"),
                "minimal" | "low" => Some("low"),
                "medium" => Some("medium"),
                "high" => Some("high"),
                "xhigh" if self.model_id == "grok-4.6" => Some("xhigh"),
                "xhigh" => Some("high"),
                _ => None,
            }
            .map(str::to_owned)
        });
        let summary = option("reasoningSummary").cloned();
        let reasoning = (effort.is_some() || summary.is_some()).then(|| jobj! { "effort"?: effort, "summary"?: summary });
        let top_logprobs = option("topLogprobs").cloned();
        let logprobs = (option("logprobs").and_then(Json::as_bool) == Some(true) || top_logprobs.is_some()).then_some(true);

        Ok(jobj! {
            "model": self.model_id.as_str(),
            "input": input,
            "logprobs"?: logprobs,
            "top_logprobs"?: top_logprobs,
            "max_output_tokens"?: options.max_output_tokens,
            "reasoning"?: reasoning,
            "store"?: store_off.then_some(false),
            "include"?: include,
            "previous_response_id"?: option("previousResponseId").cloned(),
            "service_tier"?: option("serviceTier").cloned(),
            "tools"?: (!tools.is_empty()).then_some(tools),
            "tool_choice"?: tool_choice,
            "stream": true,
        })
    }

    async fn stream(&self, options: CallOptions) -> Result<ModelStream, AiError> {
        let body = self.get_args(&options)?;
        let url = format!("{}/responses", self.base_url);
        let response = post_json(&self.fetch, &url, self.headers.clone(), &body, &options.cancel, ERROR_SHAPE).await?;
        Ok(ModelStream::sse(response.body, Box::new(XaiStream::new(ToolNames::new(options.tools.as_ref())))))
    }
}

/// `xaiFailedResponseHandler`: Chat Completions' `{ error: { message } }`, Responses' `{ code, error }` and the
/// bare `{ error }`.
const ERROR_SHAPE: ErrorShape = ErrorShape {
    message: |body| {
        match body.get("error")? {
            Json::Str(error) => match body.str_at("code") {
                Some(code) => Some(format!("{code}: {error}")),
                None => Some(error.clone()),
            },
            error @ Json::Obj(_) => {
                let valid = nullish(error.get("type"), Json::is_str) && nullish(error.get("code"), |code| code.is_str() || is_number(code));
                error.str_at("message").filter(|_| valid).map(str::to_owned)
            }
            _ => None,
        }
    },
    is_retryable: None,
};

impl LanguageModel for XaiModel {
    fn provider(&self) -> &str {
        "xai.responses"
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn supported_urls(&self) -> SupportedUrls {
        vec![("image/*", r"^https?://.*$"), ("application/pdf", r"^https?://.*$"), ("text/*", r"^https?://.*$")]
    }

    fn do_stream(&self, options: CallOptions) -> BoxFuture<'_, Result<ModelStream, AiError>> {
        Box::pin(self.stream(options))
    }
}

// ------------------------------------------------------------------ Chunk schemas

fn has_str(value: &Json, key: &str) -> bool {
    value.get(key).is_some_and(Json::is_str)
}

fn has_num(value: &Json, key: &str) -> bool {
    value.get(key).is_some_and(is_number)
}

fn opt_strs(value: &Json, keys: &[&str]) -> bool {
    keys.iter().all(|key| optional(value.get(key), Json::is_str))
}

/// `annotationSchema`: anything with a string `type`.
fn valid_annotation(annotation: &Json) -> bool {
    has_str(annotation, "type")
}

/// `messageContentPartSchema`.
fn valid_content_part(part: &Json) -> bool {
    has_str(part, "type")
        && optional(part.get("text"), Json::is_str)
        && optional(part.get("logprobs"), Json::is_arr)
        && optional(part.get("annotations"), |list| list.as_arr().is_some_and(|items| items.iter().all(valid_annotation)))
}

fn valid_text_part(part: &Json) -> bool {
    has_str(part, "type") && has_str(part, "text")
}

/// `outputItemSchema`, a discriminated union: an item of another `type` fails the whole chunk.
fn valid_item(item: &Json) -> bool {
    let id_status = has_str(item, "id") && has_str(item, "status");
    match item.str_at("type").unwrap_or("") {
        "web_search_call" | "x_search_call" | "code_interpreter_call" | "code_execution_call" | "view_image_call" | "view_x_video_call"
        | "custom_tool_call" => id_status && opt_strs(item, &["name", "arguments", "input", "call_id"]),
        "file_search_call" => {
            id_status
                && optional(item.get("queries"), |q| q.as_arr().is_some_and(|items| items.iter().all(Json::is_str)))
                && nullish(item.get("results"), |results| {
                    results.as_arr().is_some_and(|items| {
                        items.iter().all(|r| has_str(r, "file_id") && has_str(r, "filename") && has_num(r, "score") && has_str(r, "text"))
                    })
                })
        }
        "image_generation_call" => id_status && nullish(item.get("prompt"), Json::is_str) && nullish(item.get("result"), Json::is_str),
        "mcp_call" => id_status && opt_strs(item, &["name", "arguments", "output", "error", "server_label"]),
        "message" => {
            id_status
                && has_str(item, "role")
                && item.get("content").and_then(Json::as_arr).is_some_and(|parts| parts.iter().all(valid_content_part))
        }
        "function_call" => has_str(item, "name") && has_str(item, "arguments") && has_str(item, "call_id") && has_str(item, "id"),
        "reasoning" => {
            id_status
                && item.get("summary").and_then(Json::as_arr).is_some_and(|parts| parts.iter().all(valid_text_part))
                && nullish(item.get("content"), |content| content.as_arr().is_some_and(|parts| parts.iter().all(valid_text_part)))
                && nullish(item.get("encrypted_content"), Json::is_str)
        }
        _ => false,
    }
}

/// `xaiResponsesUsageSchema`. Everything else is caught by its `catchall`.
fn valid_usage(usage: &Json) -> bool {
    has_num(usage, "input_tokens")
        && has_num(usage, "output_tokens")
        && optional(usage.get("total_tokens"), is_number)
        && optional(usage.get("input_tokens_details"), |d| d.is_obj() && optional(d.get("cached_tokens"), is_number))
        && optional(usage.get("output_tokens_details"), |d| d.is_obj() && optional(d.get("reasoning_tokens"), is_number))
        && optional(usage.get("num_sources_used"), is_number)
        && optional(usage.get("num_server_side_tools_used"), is_number)
        && nullish(usage.get("cost_in_usd_ticks"), is_number)
}

/// `xaiResponsesResponseSchema`. `.partial({ usage: true, status: true })` is `partial`.
fn valid_response(response: &Json, partial: bool) -> bool {
    response.is_obj()
        && nullish(response.get("id"), Json::is_str)
        && nullish(response.get("created_at"), is_number)
        && nullish(response.get("model"), Json::is_str)
        && response.str_at("object") == Some("response")
        && response.get("output").and_then(Json::as_arr).is_some_and(|items| items.iter().all(valid_item))
        && nullish(response.get("usage"), valid_usage)
        && if partial { optional(response.get("status"), Json::is_str) } else { has_str(response, "status") }
        && nullish(response.get("service_tier"), Json::is_str)
}

/// `xaiResponsesChunkSchema`. A type outside the union fails it, and a failed chunk is an `error` part.
fn valid_chunk(event: &Json) -> bool {
    let located = has_str(event, "item_id") && has_num(event, "output_index");
    let content = located && has_num(event, "content_index");
    let summary = located && has_num(event, "summary_index");
    let response = event.get("response");
    match event.str_at("type").unwrap_or("") {
        "response.created" | "response.in_progress" => response.is_some_and(|r| valid_response(r, true)),
        "response.done" | "response.completed" => response.is_some_and(|r| valid_response(r, false)),
        "response.output_item.added" | "response.output_item.done" => event.get("item").is_some_and(valid_item) && has_num(event, "output_index"),
        "response.content_part.added" | "response.content_part.done" => content && event.get("part").is_some_and(valid_content_part),
        "response.output_text.delta" => content && has_str(event, "delta") && optional(event.get("logprobs"), Json::is_arr),
        "response.output_text.done" => {
            content
                && has_str(event, "text")
                && optional(event.get("logprobs"), Json::is_arr)
                && optional(event.get("annotations"), |list| list.as_arr().is_some_and(|items| items.iter().all(valid_annotation)))
        }
        "response.output_text.annotation.added" => {
            content && has_num(event, "annotation_index") && event.get("annotation").is_some_and(valid_annotation)
        }
        "response.reasoning_summary_part.added" | "response.reasoning_summary_part.done" => summary && event.get("part").is_some_and(valid_text_part),
        "response.reasoning_summary_text.delta" => summary && has_str(event, "delta"),
        "response.reasoning_summary_text.done" => summary && has_str(event, "text"),
        "response.reasoning_text.delta" => content && has_str(event, "delta"),
        "response.reasoning_text.done" => content && has_str(event, "text"),
        "response.web_search_call.in_progress"
        | "response.web_search_call.searching"
        | "response.web_search_call.completed"
        | "response.x_search_call.in_progress"
        | "response.x_search_call.searching"
        | "response.x_search_call.completed"
        | "response.file_search_call.in_progress"
        | "response.file_search_call.searching"
        | "response.file_search_call.completed"
        | "response.image_generation_call.in_progress"
        | "response.image_generation_call.generating"
        | "response.image_generation_call.completed"
        | "response.code_execution_call.in_progress"
        | "response.code_execution_call.executing"
        | "response.code_execution_call.completed"
        | "response.code_interpreter_call.in_progress"
        | "response.code_interpreter_call.executing"
        | "response.code_interpreter_call.interpreting"
        | "response.code_interpreter_call.completed"
        | "response.mcp_call.in_progress"
        | "response.mcp_call.executing"
        | "response.mcp_call.completed"
        | "response.mcp_call.failed" => located,
        "response.code_interpreter_call_code.delta"
        | "response.custom_tool_call_input.delta"
        | "response.function_call_arguments.delta"
        | "response.mcp_call_arguments.delta"
        | "response.mcp_call_output.delta" => located && has_str(event, "delta"),
        "response.code_interpreter_call_code.done" => located && has_str(event, "code"),
        "response.custom_tool_call_input.done" => located && has_str(event, "input"),
        "response.function_call_arguments.done" => located && has_str(event, "arguments"),
        "response.mcp_call_arguments.done" => located && optional(event.get("arguments"), Json::is_str),
        "response.mcp_call_output.done" => located && optional(event.get("output"), Json::is_str),
        "response.incomplete" => response.is_some_and(|r| {
            r.is_obj()
                && nullish(r.get("incomplete_details"), |d| has_str(d, "reason"))
                && nullish(r.get("usage"), valid_usage)
                && nullish(r.get("service_tier"), Json::is_str)
        }),
        "response.failed" => response.is_some_and(|r| {
            r.is_obj()
                && nullish(r.get("error"), |e| has_str(e, "message") && nullish(e.get("code"), Json::is_str))
                && nullish(r.get("incomplete_details"), |d| has_str(d, "reason"))
                && nullish(r.get("usage"), valid_usage)
        }),
        "error" => has_str(event, "message") && nullish(event.get("code"), Json::is_str) && nullish(event.get("param"), Json::is_str),
        _ => false,
    }
}

// ------------------------------------------------------------------ Streaming

/// `mapXaiResponsesFinishReason`.
fn map_finish_reason(reason: &str) -> &'static str {
    match reason {
        "stop" | "completed" => "stop",
        "length" | "max_output_tokens" => "length",
        "tool_calls" | "function_call" => "tool-calls",
        "content_filter" => "content-filter",
        _ => "other",
    }
}

/// `convertXaiResponsesUsage`.
fn convert_usage(usage: &Json) -> ModelUsage {
    let count = |path: &[&str]| usage.path(path).and_then(Json::as_u64).unwrap_or(0);
    let (input, output) = (count(&["input_tokens"]), count(&["output_tokens"]));
    let cache_read = count(&["input_tokens_details", "cached_tokens"]);
    let reasoning = count(&["output_tokens_details", "reasoning_tokens"]);
    // Some responses report cached tokens on top of the input count rather than inside it.
    let includes_cached = cache_read <= input;
    ModelUsage {
        input_total: Some(if includes_cached { input } else { input + cache_read }),
        input_no_cache: Some(if includes_cached { input - cache_read } else { input }),
        input_cache_read: Some(cache_read),
        input_cache_write: None,
        output_total: Some(output),
        output_text: Some(output.saturating_sub(reasoning)),
        output_reasoning: Some(reasoning),
        raw: Some(usage.clone()),
    }
}

/// `getXaiResponsesStreamErrorMetadata`.
fn stream_error_metadata(code: Option<&str>) -> (Option<u64>, Option<bool>) {
    match code {
        Some("rate_limit_exceeded" | "rate_limit_error") => (Some(429), Some(true)),
        Some("insufficient_quota") => (Some(429), Some(false)),
        Some("api_error" | "internal_server_error" | "server_error") => (Some(500), Some(true)),
        Some("overloaded_error" | "service_unavailable") => (Some(503), Some(true)),
        Some("timeout" | "timeout_error") => (Some(504), Some(true)),
        Some("authentication_error" | "invalid_api_key") => (Some(401), Some(false)),
        Some("permission_error") => (Some(403), Some(false)),
        Some("not_found_error" | "model_not_found") => (Some(404), Some(false)),
        Some("bad_request" | "context_length_exceeded" | "invalid_request_error") => (Some(400), Some(false)),
        _ => (None, None),
    }
}

/// `createXaiResponsesStreamError`, read the way `normalizeStreamProviderError` reads a `ProviderStreamError`.
fn stream_error(message: &str, code: Option<&str>, event_type: &str) -> AiError {
    let numeric = code.filter(|code| code.len() == 3 && code.bytes().all(|b| b.is_ascii_digit())).and_then(|code| code.parse::<u64>().ok()).filter(|status| (400..=599).contains(status));
    let (status, retryable) = match numeric {
        Some(status) => (Some(status), Some(status == 408 || status == 409 || status == 429 || status >= 500)),
        None => stream_error_metadata(code),
    };
    stream_error_from_payload(&jobj! {
        "message": message,
        "type": event_type,
        "code"?: code,
        "statusCode"?: status,
        "isRetryable"?: retryable,
    })
}

struct XaiStream {
    names: ToolNames,
    finish_reason: FinishReason,
    has_function_call: bool,
    usage: Option<ModelUsage>,
    cost_in_usd_ticks: Option<Json>,
    service_tier: Option<Json>,
    is_first_chunk: bool,
    /// Text block ids, in the order they opened.
    content_blocks: Vec<String>,
    seen_tool_calls: HashSet<String>,
    /// Function calls by `output_index`, so argument deltas can find their call.
    ongoing_tool_calls: HashMap<u64, String>,
    active_reasoning: HashSet<String>,
}

fn item_metadata(id: &str) -> Option<Json> {
    Some(jobj! { "xai": jobj! { "itemId": id } })
}

fn url_source(annotation: &Json) -> Option<StreamPart> {
    if annotation.str_at("type") != Some("url_citation") {
        return None;
    }
    // A `url_citation` whose `title` is not a string falls through to the union's bare `{ type }` member, which has no `url`.
    let url = annotation.str_at("url").filter(|_| optional(annotation.get("title"), Json::is_str))?;
    Some(StreamPart::Source(jobj! {
        "sourceType": "url",
        "id": generate_id(),
        "url": url,
        "title": annotation.str_at("title").unwrap_or(url),
    }))
}

impl XaiStream {
    fn new(names: ToolNames) -> Self {
        Self {
            names,
            finish_reason: FinishReason::other(),
            has_function_call: false,
            usage: None,
            cost_in_usd_ticks: None,
            service_tier: None,
            is_first_chunk: true,
            content_blocks: Vec::new(),
            seen_tool_calls: HashSet::new(),
            ongoing_tool_calls: HashMap::new(),
            active_reasoning: HashSet::new(),
        }
    }

    fn start_reasoning(&mut self, id: &str, out: &mut Vec<StreamPart>) {
        if self.active_reasoning.insert(id.to_owned()) {
            out.push(StreamPart::ReasoningStart { id: format!("reasoning-{id}"), provider_metadata: item_metadata(id) });
        }
    }

    /// What `parseToolCall` keeps of a call's input. `xai.web_search` declares `z.object({})`, which strips every key
    /// of the input `streamText` validates against it, and that stripped value is what the host persists.
    fn declared_call_input(&self, tool_name: &str, input: &str) -> String {
        let stripped = self.names.web_search.as_deref() == Some(tool_name) && Json::parse(input).is_ok_and(|value| value.is_obj());
        if stripped { "{}".to_owned() } else { input.to_owned() }
    }

    /// `tool-input-start`, `tool-input-delta`, `tool-input-end` and `tool-call` of a call the server runs itself.
    fn announce_provider_call(&mut self, id: &str, tool_name: &str, input: &str, call_input: String, out: &mut Vec<StreamPart>) {
        if !self.seen_tool_calls.insert(id.to_owned()) {
            return;
        }
        out.push(StreamPart::ToolInputStart { id: id.to_owned(), tool_name: tool_name.to_owned(), provider_executed: None, dynamic: None });
        out.push(StreamPart::ToolInputDelta { id: id.to_owned(), delta: input.to_owned() });
        out.push(StreamPart::ToolInputEnd { id: id.to_owned() });
        out.push(StreamPart::ToolCall {
            tool_call_id: id.to_owned(),
            tool_name: tool_name.to_owned(),
            input: call_input,
            provider_executed: Some(true),
            dynamic: None,
            provider_metadata: None,
        });
    }

    fn on_output_item(&mut self, event: &Json, part: &Json, done: bool, out: &mut Vec<StreamPart>) {
        let id = part.str_at("id").unwrap_or("");
        let kind = part.str_at("type").unwrap_or("");
        match kind {
            "reasoning" => {
                if done {
                    // Encrypted reasoning may arrive without any summary event before it.
                    self.start_reasoning(id, out);
                    out.push(StreamPart::ReasoningEnd {
                        id: format!("reasoning-{id}"),
                        provider_metadata: Some(jobj! {
                            "xai": jobj! {
                                "reasoningEncryptedContent"?: part.non_null("encrypted_content").and_then(Json::as_str).filter(|c| !c.is_empty()),
                                "itemId"?: Some(id).filter(|id| !id.is_empty()),
                            },
                        }),
                    });
                    self.active_reasoning.remove(id);
                }
            }
            "file_search_call" => {
                let tool_name = self.names.file_search.clone().unwrap_or_else(|| "file_search".to_owned());
                self.announce_provider_call(id, &tool_name, "", String::new(), out);
                if done {
                    let results = part.non_null("results").and_then(Json::as_arr).map(|results| {
                        results
                            .iter()
                            .map(|r| jobj! {
                                "fileId": r.get("file_id").cloned().unwrap_or_default(),
                                "filename": r.get("filename").cloned().unwrap_or_default(),
                                "score": r.get("score").cloned().unwrap_or_default(),
                                "text": r.get("text").cloned().unwrap_or_default(),
                            })
                            .collect::<Vec<_>>()
                    });
                    out.push(StreamPart::ToolResult {
                        tool_call_id: id.to_owned(),
                        tool_name,
                        result: jobj! {
                            "queries": part.get("queries").cloned().unwrap_or_else(|| Json::Arr(Vec::new())),
                            "results": results.map_or(Json::Null, Json::Arr),
                        },
                        is_error: None,
                        dynamic: None,
                        provider_metadata: None,
                    });
                }
            }
            "image_generation_call" => {
                let tool_name = self.names.image_generation.clone().unwrap_or_else(|| "image_generation".to_owned());
                self.announce_provider_call(id, &tool_name, "{}", "{}".to_owned(), out);
                if done {
                    let result = part.non_null("result");
                    out.push(StreamPart::ToolResult {
                        tool_call_id: id.to_owned(),
                        tool_name,
                        is_error: result.is_none().then_some(true),
                        result: match result {
                            Some(result) => jobj! { "result": result, "prompt"?: part.non_null("prompt") },
                            None => Json::from(format!("Image generation failed (status: {}).", part.str_at("status").unwrap_or(""))),
                        },
                        dynamic: None,
                        provider_metadata: None,
                    });
                }
            }
            "web_search_call" | "x_search_call" | "code_interpreter_call" | "code_execution_call" | "view_image_call" | "view_x_video_call"
            | "custom_tool_call" | "mcp_call" => {
                const WEB_SEARCH_SUB_TOOLS: [&str; 3] = ["web_search", "web_search_with_snippets", "browse_page"];
                const X_SEARCH_SUB_TOOLS: [&str; 4] = ["x_user_search", "x_keyword_search", "x_semantic_search", "x_thread_fetch"];
                let name = part.str_at("name");
                let plain = name.unwrap_or("");
                let tool_name = if WEB_SEARCH_SUB_TOOLS.contains(&plain) || kind == "web_search_call" {
                    self.names.web_search.as_deref().unwrap_or("web_search")
                } else if X_SEARCH_SUB_TOOLS.contains(&plain) || kind == "x_search_call" {
                    self.names.x_search.as_deref().unwrap_or("x_search")
                } else if name == Some("code_execution") || matches!(kind, "code_interpreter_call" | "code_execution_call") {
                    self.names.code_execution.as_deref().unwrap_or("code_execution")
                } else if kind == "mcp_call" {
                    self.names.mcp.as_deref().or(name).unwrap_or("mcp")
                } else {
                    plain
                }
                .to_owned();
                let input = part.str_at(if kind == "custom_tool_call" { "input" } else { "arguments" }).unwrap_or("");
                // A custom tool's input is only complete once the item is.
                if kind != "custom_tool_call" || done {
                    let call_input = self.declared_call_input(&tool_name, input);
                    self.announce_provider_call(id, &tool_name, input, call_input, out);
                }
                if done {
                    out.push(StreamPart::ToolResult {
                        tool_call_id: id.to_owned(),
                        tool_name,
                        result: Json::obj(),
                        is_error: None,
                        dynamic: None,
                        provider_metadata: None,
                    });
                }
            }
            "message" => {
                for content in part.get("content").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice) {
                    let text = content.str_at("text").unwrap_or("");
                    if !text.is_empty() {
                        let block = format!("text-{id}");
                        // Only when `output_text.delta` has not already streamed it.
                        if !self.content_blocks.contains(&block) {
                            self.content_blocks.push(block.clone());
                            out.push(StreamPart::TextStart { id: block.clone(), provider_metadata: None });
                            out.push(StreamPart::TextDelta { id: block, delta: text.to_owned(), provider_metadata: None });
                        }
                    }
                    for annotation in content.get("annotations").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice) {
                        out.extend(url_source(annotation));
                    }
                }
            }
            "function_call" => {
                let call_id = part.str_at("call_id").unwrap_or("");
                let tool_name = part.str_at("name").unwrap_or("");
                let index = event.get("output_index").and_then(Json::as_f64).unwrap_or(0.0).to_bits();
                if done {
                    self.has_function_call = true;
                    self.ongoing_tool_calls.remove(&index);
                    out.push(StreamPart::ToolInputEnd { id: call_id.to_owned() });
                    out.push(StreamPart::ToolCall {
                        tool_call_id: call_id.to_owned(),
                        tool_name: tool_name.to_owned(),
                        input: part.str_at("arguments").unwrap_or("").to_owned(),
                        provider_executed: None,
                        dynamic: None,
                        provider_metadata: None,
                    });
                } else {
                    self.ongoing_tool_calls.insert(index, call_id.to_owned());
                    out.push(StreamPart::ToolInputStart { id: call_id.to_owned(), tool_name: tool_name.to_owned(), provider_executed: None, dynamic: None });
                }
            }
            _ => {}
        }
    }

    fn on_event(&mut self, event: &Json, out: &mut Vec<StreamPart>) {
        let item = event.str_at("item_id").unwrap_or("");
        let delta = event.str_at("delta").unwrap_or("");
        let response = event.get("response").unwrap_or(&Json::Null);
        match event.str_at("type").unwrap_or("") {
            "response.created" | "response.in_progress" => {
                if self.is_first_chunk {
                    out.push(StreamPart::ResponseMetadata {
                        id: response.non_null("id").and_then(Json::as_str).map(str::to_owned),
                        model_id: response.non_null("model").and_then(Json::as_str).map(str::to_owned),
                    });
                    self.is_first_chunk = false;
                }
            }
            "response.reasoning_summary_part.added" => self.start_reasoning(item, out),
            "response.reasoning_summary_text.delta" => out.push(StreamPart::ReasoningDelta {
                id: format!("reasoning-{item}"),
                delta: delta.to_owned(),
                provider_metadata: item_metadata(item),
            }),
            "response.reasoning_text.delta" => {
                self.start_reasoning(item, out);
                out.push(StreamPart::ReasoningDelta {
                    id: format!("reasoning-{item}"),
                    delta: delta.to_owned(),
                    provider_metadata: item_metadata(item),
                });
            }
            "response.output_text.delta" => {
                let block = format!("text-{item}");
                if !self.content_blocks.contains(&block) {
                    self.content_blocks.push(block.clone());
                    out.push(StreamPart::TextStart { id: block.clone(), provider_metadata: None });
                }
                out.push(StreamPart::TextDelta { id: block, delta: delta.to_owned(), provider_metadata: None });
            }
            "response.output_text.done" => {
                for annotation in event.get("annotations").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice) {
                    out.extend(url_source(annotation));
                }
            }
            "response.output_text.annotation.added" => {
                out.extend(event.get("annotation").and_then(url_source));
            }
            kind @ ("response.done" | "response.completed" | "response.incomplete") => {
                if let Some(usage) = response.non_null("usage") {
                    self.usage = Some(convert_usage(usage));
                    self.cost_in_usd_ticks = usage.non_null("cost_in_usd_ticks").cloned();
                }
                self.service_tier = response.non_null("service_tier").cloned();
                if kind == "response.incomplete" {
                    let reason = response.path(&["incomplete_details", "reason"]).and_then(Json::as_str);
                    self.finish_reason = FinishReason::new(
                        reason.filter(|r| !r.is_empty()).map_or("other", map_finish_reason),
                        Some(reason.unwrap_or("incomplete").to_owned()),
                    );
                } else if let Some(status) = response.str_at("status").filter(|s| !s.is_empty()) {
                    let unified = if self.has_function_call { "tool-calls" } else { map_finish_reason(status) };
                    self.finish_reason = FinishReason::new(unified, Some(status.to_owned()));
                }
            }
            "response.failed" => {
                let reason = response.path(&["incomplete_details", "reason"]).and_then(Json::as_str);
                self.finish_reason = FinishReason::new(
                    reason.filter(|r| !r.is_empty()).map_or("error", map_finish_reason),
                    Some(reason.unwrap_or("error").to_owned()),
                );
                if let Some(usage) = response.non_null("usage") {
                    self.usage = Some(convert_usage(usage));
                }
                if let Some(error) = response.non_null("error") {
                    out.push(StreamPart::Error(stream_error(error.str_at("message").unwrap_or(""), error.str_at("code"), "response.failed")));
                }
            }
            "error" => out.push(StreamPart::Error(stream_error(event.str_at("message").unwrap_or(""), event.str_at("code"), "error"))),
            "response.function_call_arguments.delta" => {
                let index = event.get("output_index").and_then(Json::as_f64).unwrap_or(0.0).to_bits();
                if let Some(call_id) = self.ongoing_tool_calls.get(&index) {
                    out.push(StreamPart::ToolInputDelta { id: call_id.clone(), delta: delta.to_owned() });
                }
            }
            "response.image_generation_call.in_progress" | "response.image_generation_call.generating" | "response.image_generation_call.completed" => {
                let tool_name = self.names.image_generation.clone().unwrap_or_else(|| "image_generation".to_owned());
                self.announce_provider_call(item, &tool_name, "{}", "{}".to_owned(), out);
            }
            kind @ ("response.output_item.added" | "response.output_item.done") => {
                if let Some(part) = event.get("item") {
                    self.on_output_item(event, part, kind == "response.output_item.done", out);
                }
            }
            _ => {}
        }
    }
}

impl ChunkTransform for XaiStream {
    fn start(&mut self, out: &mut Vec<StreamPart>) {
        out.push(StreamPart::StreamStart { warnings: Vec::new() });
    }

    fn chunk(&mut self, chunk: Result<Json, AiError>, out: &mut Vec<StreamPart>) {
        let event = match chunk {
            Ok(event) => event,
            Err(error) => {
                out.push(StreamPart::Error(error));
                return;
            }
        };
        if !valid_chunk(&event) {
            out.push(StreamPart::Error(AiError::validation(&event, "Invalid input")));
            return;
        }
        self.on_event(&event, out);
    }

    fn flush(&mut self, out: &mut Vec<StreamPart>) {
        for block in std::mem::take(&mut self.content_blocks) {
            out.push(StreamPart::TextEnd { id: block, provider_metadata: None });
        }
        let provider_metadata = (self.cost_in_usd_ticks.is_some() || self.service_tier.is_some()).then(|| {
            jobj! { "xai": jobj! { "costInUsdTicks"?: self.cost_in_usd_ticks.take(), "serviceTier"?: self.service_tier.take() } }
        });
        out.push(StreamPart::Finish {
            finish_reason: self.finish_reason.clone(),
            usage: self.usage.take().unwrap_or(ModelUsage {
                input_total: Some(0),
                input_no_cache: Some(0),
                input_cache_read: Some(0),
                input_cache_write: Some(0),
                output_total: Some(0),
                output_text: Some(0),
                output_reasoning: Some(0),
                raw: None,
            }),
            provider_metadata,
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::http::{Cancel, Fetch, HttpRequest, HttpResponse};

    /// Stands in for the network: the default endpoint must not be contacted from a test.
    #[derive(Default)]
    struct Capture(Mutex<Vec<HttpRequest>>);

    impl Fetch for Capture {
        fn fetch(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, AiError>> {
            self.0.lock().unwrap().push(request);
            Box::pin(async { Ok(HttpResponse::with_text(200, "OK", Headers::from_pairs([("content-type", "text/event-stream")]), String::new())) })
        }
    }

    #[test]
    fn the_default_endpoint_is_the_versioned_api() {
        let capture = Arc::new(Capture::default());
        let model = create(ProviderSettings {
            base_url: None,
            api_key: String::new(),
            headers: Headers::new(),
            settings: HashMap::new(),
            model_id: "grok-4.3".to_owned(),
            fetch: capture.clone(),
        })
        .unwrap();
        assert_eq!(model.provider(), "xai.responses");
        let options = CallOptions {
            prompt: vec![jobj! { "role": "user", "content": vec![jobj! { "type": "text", "text": "hi" }] }],
            tools: None,
            tool_choice: jobj! { "type": "auto" },
            max_output_tokens: None,
            reasoning: None,
            provider_options: None,
            cancel: Cancel::new(),
        };
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        assert!(runtime.block_on(model.do_stream(options)).is_ok());
        let requests = capture.0.lock().unwrap();
        assert_eq!(requests[0].url, "https://api.x.ai/v1/responses");
        assert_eq!(requests[0].headers.get("authorization"), Some("Bearer"));
    }

    #[test]
    fn grok_4_20_alone_takes_no_effort() {
        for id in ["grok-4.20-reasoning", "grok-4.20-non-reasoning", "grok-4.20-0309-reasoning", "grok-4.20-0309-non-reasoning"] {
            assert!(!supports_reasoning_effort(id), "{id}");
        }
        for id in ["grok-4.3", "grok-latest", "grok-4.20-multi-agent", "grok-4.20-030-reasoning", "grok-4.20"] {
            assert!(supports_reasoning_effort(id), "{id}");
        }
    }

    #[test]
    fn additional_properties_false_goes_at_any_depth() {
        let schema = Json::parse(r#"{"type":"object","additionalProperties":false,"properties":{"a":{"type":"object","additionalProperties":false},"b":{"additionalProperties":true}},"anyOf":[{"additionalProperties":false,"x":null}]}"#).unwrap();
        assert_eq!(
            without_additional_properties_false(&schema).to_text(),
            r#"{"type":"object","properties":{"a":{"type":"object"},"b":{"additionalProperties":true}},"anyOf":[{"x":null}]}"#
        );
    }

    #[test]
    fn the_chunk_union_rejects_what_it_does_not_name() {
        let parse = |text: &str| Json::parse(text).unwrap();
        assert!(valid_chunk(&parse(r#"{"type":"response.created","response":{"object":"response","output":[]}}"#)));
        assert!(!valid_chunk(&parse(r#"{"type":"response.created","response":{"output":[]}}"#)));
        assert!(!valid_chunk(&parse(r#"{"type":"response.completed","response":{"object":"response","output":[]}}"#)));
        assert!(!valid_chunk(&parse(r#"{"type":"response.brand_new"}"#)));
        assert!(!valid_chunk(&parse(r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"brand_new","id":"x"}}"#)));
        assert!(valid_chunk(&parse(r#"{"type":"error","message":"m","code":null}"#)));
        assert!(!valid_chunk(&parse(r#"{"type":"error","code":"c"}"#)));
    }

    #[test]
    fn usage_counts_cached_tokens_inside_or_on_top_of_the_input() {
        let inside = convert_usage(&Json::parse(r#"{"input_tokens":10,"output_tokens":5,"input_tokens_details":{"cached_tokens":4},"output_tokens_details":{"reasoning_tokens":2}}"#).unwrap());
        assert_eq!((inside.input_total, inside.input_no_cache, inside.input_cache_read), (Some(10), Some(6), Some(4)));
        assert_eq!((inside.output_total, inside.output_text, inside.output_reasoning), (Some(5), Some(3), Some(2)));
        let on_top = convert_usage(&Json::parse(r#"{"input_tokens":2,"output_tokens":1,"input_tokens_details":{"cached_tokens":8}}"#).unwrap());
        assert_eq!((on_top.input_total, on_top.input_no_cache, on_top.input_cache_read), (Some(10), Some(2), Some(8)));
    }

    #[test]
    fn stream_errors_take_their_status_from_the_code() {
        let error = |code| match stream_error("m", code, "error") {
            AiError::StreamProvider(error) => (error.status, error.is_retryable),
            other => panic!("{other:?}"),
        };
        assert_eq!(error(Some("rate_limit_exceeded")), (Some(429), true));
        assert_eq!(error(Some("insufficient_quota")), (Some(429), false));
        assert_eq!(error(Some("503")), (Some(503), true));
        assert_eq!(error(Some("404")), (Some(404), false));
        assert_eq!(error(Some("whatever")), (None, false));
        assert_eq!(error(None), (None, false));
    }
}
