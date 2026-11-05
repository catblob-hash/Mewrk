//! `@ai-sdk/anthropic`: `createAnthropic` and its language model's streaming path.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::api::{ErrorShape, VERSION_ANTHROPIC, combine_headers, generate_id, post_json, with_user_agent_suffix, without_trailing_slash};
use crate::error::{AiError, ApiCallError};
use crate::http::{BoxFuture, DynFetch, Headers};
use crate::json::{Json, Obj};
use crate::model::{CallOptions, ChunkTransform, DynModel, FinishReason, LanguageModel, ModelStream, ModelUsage, StreamPart, SupportedUrls};
use crate::prompt::{resolve_full_media_type, top_level_media_type};
use crate::reasoning::js_round;
use crate::stream_text::secure_json_parse;
use crate::{jarr, jobj};

use super::ProviderSettings;

const ANTHROPIC_API_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_API_VERSIONED_URL: &str = "https://api.anthropic.com/v1";

pub fn create(target: ProviderSettings) -> Result<DynModel, AiError> {
    let base_url = match target.base_url.as_deref().map(without_trailing_slash) {
        None => ANTHROPIC_API_VERSIONED_URL.to_owned(),
        Some(ANTHROPIC_API_URL) => ANTHROPIC_API_VERSIONED_URL.to_owned(),
        Some(url) => url.to_owned(),
    };
    let mut headers = Headers::new();
    headers.set("anthropic-version", "2023-06-01");
    headers.set("x-api-key", &target.api_key);
    let headers = combine_headers(&headers, &target.headers);
    let headers = with_user_agent_suffix(headers, &[&format!("ai-sdk/anthropic/{VERSION_ANTHROPIC}")]);
    Ok(Arc::new(AnthropicModel { model_id: target.model_id, base_url, headers, fetch: target.fetch }))
}

pub struct AnthropicModel {
    model_id: String,
    base_url: String,
    headers: Headers,
    fetch: DynFetch,
}

/// `getModelCapabilities`.
pub(crate) struct Capabilities {
    pub(crate) max_output_tokens: u64,
    supports_structured_output: bool,
    pub(crate) supports_adaptive_thinking: bool,
    supports_xhigh_effort: bool,
    rejects_thinking_disabled_above_high_effort: bool,
    is_known_model: bool,
}

fn is_legacy_claude(id: &str) -> bool {
    use std::sync::OnceLock;
    static PATTERN: OnceLock<regex::Regex> = OnceLock::new();
    // `claude-(?:instant(?:-|$)|v?2(?=$|[-.:])|3(?=$|[-.]))`, without the lookaheads the crate lacks.
    PATTERN
        .get_or_init(|| regex::Regex::new(r"claude-(?:instant(?:-|$)|v?2(?:$|[-.:])|3(?:$|[-.]))").expect("a literal pattern"))
        .is_match(id)
}

pub(crate) fn capabilities(id: &str) -> Capabilities {
    let caps = |max, structured, adaptive, xhigh, rejects, known| Capabilities {
        max_output_tokens: max,
        supports_structured_output: structured,
        supports_adaptive_thinking: adaptive,
        supports_xhigh_effort: xhigh,
        rejects_thinking_disabled_above_high_effort: rejects,
        is_known_model: known,
    };
    if id.contains("claude-opus-5") {
        caps(128_000, true, true, true, true, true)
    } else if id.contains("claude-opus-4-8") || id.contains("claude-opus-4-7") || id.contains("claude-fable-5") || id.contains("claude-sonnet-5") {
        caps(128_000, true, true, true, false, true)
    } else if id.contains("claude-sonnet-4-6") || id.contains("claude-opus-4-6") {
        caps(128_000, true, true, false, false, true)
    } else if id.contains("claude-sonnet-4-5") || id.contains("claude-opus-4-5") || id.contains("claude-haiku-4-5") {
        caps(64_000, true, false, false, false, true)
    } else if id.contains("claude-opus-4-1") {
        caps(32_000, true, false, false, false, true)
    } else if id.contains("claude-sonnet-4-") {
        caps(64_000, false, false, false, false, true)
    } else if id.contains("claude-opus-4-") {
        caps(32_000, false, false, false, false, true)
    } else if id.contains("claude-3-haiku") {
        caps(4096, false, false, false, false, true)
    } else if is_legacy_claude(id) {
        caps(4096, false, false, false, false, false)
    } else if id.contains("claude-") {
        caps(128_000, true, true, true, true, false)
    } else {
        caps(4096, false, false, false, false, false)
    }
}

/// `createToolNameMapping` for the Messages tool ids.
#[derive(Default)]
struct ToolNames {
    to_provider: HashMap<String, String>,
    to_custom: HashMap<String, String>,
}

fn provider_tool_name(id: &str) -> Option<&'static str> {
    Some(match id {
        "anthropic.code_execution_20250522" | "anthropic.code_execution_20250825" | "anthropic.code_execution_20260120" => "code_execution",
        "anthropic.computer_20241022" | "anthropic.computer_20250124" => "computer",
        "anthropic.text_editor_20241022" | "anthropic.text_editor_20250124" => "str_replace_editor",
        "anthropic.text_editor_20250429" | "anthropic.text_editor_20250728" => "str_replace_based_edit_tool",
        "anthropic.bash_20241022" | "anthropic.bash_20250124" => "bash",
        "anthropic.memory_20250818" => "memory",
        "anthropic.web_search_20250305" | "anthropic.web_search_20260209" => "web_search",
        "anthropic.web_fetch_20250910" | "anthropic.web_fetch_20260209" => "web_fetch",
        "anthropic.tool_search_regex_20251119" => "tool_search_tool_regex",
        "anthropic.tool_search_bm25_20251119" => "tool_search_tool_bm25",
        "anthropic.advisor_20260301" => "advisor",
        _ => return None,
    })
}

impl ToolNames {
    fn new(tools: Option<&Vec<Json>>) -> Self {
        let mut names = Self::default();
        for tool in tools.map_or(&[][..], Vec::as_slice) {
            if tool.str_at("type") != Some("provider") {
                continue;
            }
            let (Some(id), Some(name)) = (tool.str_at("id"), tool.str_at("name")) else { continue };
            if let Some(provider) = provider_tool_name(id) {
                names.to_provider.insert(name.to_owned(), provider.to_owned());
                names.to_custom.insert(provider.to_owned(), name.to_owned());
            }
        }
        names
    }

    fn provider<'a>(&'a self, custom: &'a str) -> &'a str {
        self.to_provider.get(custom).map_or(custom, String::as_str)
    }

    fn custom<'a>(&'a self, provider: &'a str) -> &'a str {
        self.to_custom.get(provider).map_or(provider, String::as_str)
    }
}

/// `CacheControlValidator`: at most four breakpoints the caller asked for. (The dialect's own go in after.)
#[derive(Default)]
struct CacheValidator {
    count: usize,
}

impl CacheValidator {
    fn get(&mut self, options: Option<&Json>, can_cache: bool) -> Option<Json> {
        let anthropic = options?.get("anthropic")?;
        let value = anthropic.non_null("cacheControl").or_else(|| anthropic.non_null("cache_control"))?;
        if !can_cache {
            return None;
        }
        self.count += 1;
        (self.count <= 4).then(|| value.clone())
    }
}

/// An ordered set of beta names.
#[derive(Default, Clone)]
struct Betas(Vec<String>);

impl Betas {
    fn add(&mut self, beta: &str) {
        if !self.0.iter().any(|b| b == beta) {
            self.0.push(beta.to_owned());
        }
    }
}

fn base64_to_text(data: &str) -> String {
    let bytes = base64_simd::STANDARD
        .decode_to_vec(data.as_bytes())
        .or_else(|_| base64_simd::STANDARD_NO_PAD.decode_to_vec(data.as_bytes()))
        .unwrap_or_default();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn document_options(part: &Json) -> (Option<String>, Option<String>, bool) {
    let anthropic = part.path(&["providerOptions", "anthropic"]);
    let title = anthropic.and_then(|a| a.str_at("title")).map(str::to_owned);
    let context = anthropic.and_then(|a| a.str_at("context")).map(str::to_owned);
    let citations = anthropic.and_then(|a| a.path(&["citations", "enabled"])).and_then(Json::as_bool).unwrap_or(false);
    (title, context, citations)
}

fn reference_id(part: &Json) -> Result<String, AiError> {
    let reference = part.path(&["data", "reference"]);
    reference.and_then(|r| r.str_at("anthropic")).map(str::to_owned).ok_or_else(|| {
        let available: Vec<&str> = reference.and_then(Json::as_obj).map_or_else(Vec::new, |r| r.keys().collect());
        AiError::other(format!("No provider reference found for provider 'anthropic'. Available providers: {}", available.join(", ")))
    })
}

fn user_file_block(part: &Json, cache_control: Option<Json>, betas: &mut Betas) -> Result<Json, AiError> {
    let media_type = part.str_at("mediaType").unwrap_or("");
    let data = part.get("data").cloned().unwrap_or_default();
    let top = top_level_media_type(media_type);
    let (title_option, context, citations) = document_options(part);
    let title = title_option.or_else(|| part.str_at("filename").map(str::to_owned));
    let citations_json = || citations.then(|| jobj! { "enabled": true });
    match data.str_at("type").unwrap_or("") {
        "reference" => {
            let file_id = reference_id(part)?;
            betas.add("files-api-2025-04-14");
            let container_upload = part.path(&["providerOptions", "anthropic", "containerUpload"]).and_then(Json::as_bool) == Some(true);
            if container_upload {
                return Ok(jobj! { "type": "container_upload", "file_id": file_id });
            }
            let kind = if top == "image" { "image" } else { "document" };
            Ok(jobj! { "type": kind, "source": jobj! { "type": "file", "file_id": file_id }, "cache_control"?: cache_control })
        }
        "text" => Ok(jobj! {
            "type": "document",
            "source": jobj! { "type": "text", "media_type": "text/plain", "data": data.get("text").cloned().unwrap_or_default() },
            "title"?: title,
            "context"?: context.filter(|c| !c.is_empty()),
            "citations"?: citations_json(),
            "cache_control"?: cache_control,
        }),
        kind @ ("url" | "data") => {
            let is_url = kind == "url";
            let url_source = || jobj! { "type": "url", "url": data.get("url").cloned().unwrap_or_default() };
            let inline = data.str_at("data").unwrap_or("");
            if top == "image" {
                let source = if is_url {
                    url_source()
                } else {
                    jobj! { "type": "base64", "media_type": resolve_full_media_type(part)?, "data": inline }
                };
                return Ok(jobj! { "type": "image", "source": source, "cache_control"?: cache_control });
            }
            let is_pdf = top == "application"
                && if is_url { media_type == "application/pdf" } else { resolve_full_media_type(part)? == "application/pdf" };
            if is_pdf {
                betas.add("pdfs-2024-09-25");
                let source = if is_url {
                    url_source()
                } else {
                    jobj! { "type": "base64", "media_type": "application/pdf", "data": inline }
                };
                return Ok(jobj! {
                    "type": "document",
                    "source": source,
                    "title"?: title,
                    "context"?: context.filter(|c| !c.is_empty()),
                    "citations"?: citations_json(),
                    "cache_control"?: cache_control,
                });
            }
            if media_type == "text/plain" {
                let source = if is_url {
                    url_source()
                } else {
                    jobj! { "type": "text", "media_type": "text/plain", "data": base64_to_text(inline) }
                };
                return Ok(jobj! {
                    "type": "document",
                    "source": source,
                    "title"?: title,
                    "context"?: context.filter(|c| !c.is_empty()),
                    "citations"?: citations_json(),
                    "cache_control"?: cache_control,
                });
            }
            Err(AiError::other(format!("'media type: {media_type}' functionality not supported.")))
        }
        _ => Err(AiError::other("Invalid file part data.")),
    }
}

/// `convertToAnthropicPrompt`'s tool-result content.
fn tool_result_content(output: &Json, betas: &mut Betas) -> Result<Json, AiError> {
    match output.str_at("type").unwrap_or("") {
        "content" => {
            let mut blocks = Vec::new();
            for item in output.get("value").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice) {
                match item.str_at("type").unwrap_or("") {
                    "text" => blocks.push(jobj! { "type": "text", "text": item.get("text").cloned().unwrap_or_default() }),
                    "file" => {
                        let media_type = item.str_at("mediaType").unwrap_or("");
                        let top = top_level_media_type(media_type);
                        match item.path(&["data", "type"]).and_then(Json::as_str) {
                            Some("url") => {
                                let source = jobj! { "type": "url", "url": item.path(&["data", "url"]).cloned().unwrap_or_default() };
                                let kind = if top == "image" { "image" } else { "document" };
                                blocks.push(jobj! { "type": kind, "source": source });
                            }
                            Some("data") => {
                                let inline = item.path(&["data", "data"]).and_then(Json::as_str).unwrap_or("");
                                if top == "image" {
                                    blocks.push(jobj! {
                                        "type": "image",
                                        "source": jobj! { "type": "base64", "media_type": resolve_full_media_type(item)?, "data": inline },
                                    });
                                } else if resolve_full_media_type(item)? == "application/pdf" {
                                    betas.add("pdfs-2024-09-25");
                                    blocks.push(jobj! {
                                        "type": "document",
                                        "source": jobj! { "type": "base64", "media_type": "application/pdf", "data": inline },
                                    });
                                }
                            }
                            _ => {}
                        }
                    }
                    "custom" => {
                        let anthropic = item.path(&["providerOptions", "anthropic"]);
                        if anthropic.and_then(|a| a.str_at("type")) == Some("tool-reference") {
                            blocks.push(jobj! {
                                "type": "tool_reference",
                                "tool_name": anthropic.and_then(|a| a.get("toolName")).cloned().unwrap_or_default(),
                            });
                        }
                    }
                    _ => {}
                }
            }
            Ok(Json::Arr(blocks))
        }
        "text" | "error-text" => Ok(output.get("value").cloned().unwrap_or_default()),
        "execution-denied" => Ok(output.non_null("reason").cloned().unwrap_or_else(|| Json::from("Tool call execution denied."))),
        _ => Ok(Json::from(output.get("value").cloned().unwrap_or_default().to_text())),
    }
}

fn extract_error_code(value: Option<&Json>) -> Option<String> {
    match value? {
        Json::Str(text) => match secure_json_parse(text) {
            Ok(parsed) => parsed.str_at("errorCode").map(str::to_owned),
            Err(_) => Some("unavailable".to_owned()),
        },
        other => other.str_at("errorCode").map(str::to_owned),
    }
}

fn anthropic_caller(options: Option<&Json>) -> Option<Json> {
    let caller = options?.path(&["anthropic", "caller"])?;
    match caller.str_at("type") {
        Some(kind @ ("code_execution_20250825" | "code_execution_20260120")) => {
            caller.str_at("toolId").filter(|id| !id.is_empty()).map(|id| jobj! { "type": kind, "tool_id": id })
        }
        Some("direct") => Some(jobj! { "type": "direct" }),
        _ => None,
    }
}

/// Wrap invalid tool call input because Anthropic requires it to be an object.
fn to_tool_input(input: &Json) -> Json {
    if input.is_obj() { input.clone() } else { jobj! { "rawInvalidInput": input.clone() } }
}

fn without_key(value: &Json, key: &str) -> Json {
    let mut object = value.as_obj().cloned().unwrap_or_default();
    object.remove(key);
    Json::Obj(object)
}

struct PromptContext<'a> {
    names: &'a ToolNames,
    send_reasoning: bool,
    cache: &'a mut CacheValidator,
    betas: &'a mut Betas,
}

fn assistant_tool_call(part: &Json, cache_control: Option<Json>, ctx: &mut PromptContext<'_>, mcp_ids: &mut HashSet<String>) -> Option<Json> {
    let caller = anthropic_caller(part.get("providerOptions"));
    let id = part.get("toolCallId").cloned().unwrap_or_default();
    let tool_name = part.str_at("toolName").unwrap_or("");
    let input = part.get("input").cloned().unwrap_or_default();
    if part.get("providerExecuted").and_then(Json::as_bool) != Some(true) {
        return Some(jobj! {
            "type": "tool_use",
            "id": id,
            "name": tool_name,
            "input": to_tool_input(&input),
            "caller"?: caller,
            "cache_control"?: cache_control,
        });
    }
    let provider_name = ctx.names.provider(tool_name).to_owned();
    let server = |name: &str, input: Json, caller: Option<Json>, cache_control: Option<Json>, id: Json| {
        jobj! { "type": "server_tool_use", "id": id, "name": name, "input": input, "caller"?: caller, "cache_control"?: cache_control }
    };
    if part.path(&["providerOptions", "anthropic", "type"]).and_then(Json::as_str) == Some("mcp-tool-use") {
        if let Some(call_id) = id.as_str() {
            mcp_ids.insert(call_id.to_owned());
        }
        let server_name = part.path(&["providerOptions", "anthropic", "serverName"]).and_then(Json::as_str)?;
        return Some(jobj! {
            "type": "mcp_tool_use",
            "id": id,
            "name": tool_name,
            "input": input,
            "server_name": server_name,
            "cache_control"?: cache_control,
        });
    }
    let input_type = input.str_at("type").map(str::to_owned);
    if provider_name == "code_execution" {
        match input_type.as_deref() {
            Some(sub @ ("bash_code_execution" | "text_editor_code_execution")) => {
                return Some(server(sub, without_key(&input, "type"), caller, cache_control, id));
            }
            Some("programmatic-tool-call") => {
                return Some(server("code_execution", without_key(&input, "type"), caller, cache_control, id));
            }
            _ => {}
        }
    }
    match provider_name.as_str() {
        "code_execution" | "web_fetch" | "web_search" | "tool_search_tool_regex" | "tool_search_tool_bm25" => {
            Some(server(&provider_name, input, caller, cache_control, id))
        }
        // The advisor's `server_tool_use.input` is always `{}`.
        "advisor" => Some(server("advisor", Json::obj(), caller, cache_control, id)),
        _ => None,
    }
}

fn assistant_tool_result(part: &Json, cache_control: Option<Json>, ctx: &mut PromptContext<'_>, mcp_ids: &HashSet<String>) -> Result<Option<Json>, AiError> {
    let tool_name = part.str_at("toolName").unwrap_or("");
    let provider_name = ctx.names.provider(tool_name).to_owned();
    let caller = anthropic_caller(part.get("providerOptions"));
    let id = part.get("toolCallId").cloned().unwrap_or_default();
    let output = part.get("output").cloned().unwrap_or_default();
    let kind = output.str_at("type").unwrap_or("");
    let value = output.get("value").cloned().unwrap_or_default();
    if id.as_str().is_some_and(|i| mcp_ids.contains(i)) {
        if kind != "json" && kind != "error-json" {
            return Ok(None);
        }
        return Ok(Some(jobj! {
            "type": "mcp_tool_result",
            "tool_use_id": id,
            "is_error": kind == "error-json",
            "content": value,
            "cache_control"?: cache_control,
        }));
    }
    match provider_name.as_str() {
        "code_execution" => {
            if kind == "error-text" || kind == "error-json" {
                let info = match &value {
                    Json::Str(text) => secure_json_parse(text).unwrap_or_default(),
                    other => other.clone(),
                };
                let code = info.str_at("errorCode").unwrap_or("unknown").to_owned();
                if info.str_at("type") == Some("code_execution_tool_result_error") {
                    return Ok(Some(jobj! {
                        "type": "code_execution_tool_result",
                        "tool_use_id": id,
                        "content": jobj! { "type": "code_execution_tool_result_error", "error_code": code },
                        "cache_control"?: cache_control,
                    }));
                }
                return Ok(Some(jobj! {
                    "type": "bash_code_execution_tool_result",
                    "tool_use_id": id,
                    "cache_control"?: cache_control,
                    "content": jobj! { "type": "bash_code_execution_tool_result_error", "error_code": code },
                }));
            }
            if kind != "json" {
                return Ok(None);
            }
            let Some(result_type) = value.str_at("type").map(str::to_owned) else { return Ok(None) };
            let field = |key: &str| value.get(key).cloned().unwrap_or_default();
            let content_list = || value.non_null("content").cloned().unwrap_or_else(|| Json::Arr(Vec::new()));
            Ok(Some(match result_type.as_str() {
                "code_execution_result" => jobj! {
                    "type": "code_execution_tool_result",
                    "tool_use_id": id,
                    "content": jobj! {
                        "type": result_type.as_str(),
                        "stdout": field("stdout"),
                        "stderr": field("stderr"),
                        "return_code": field("return_code"),
                        "content": content_list(),
                    },
                    "cache_control"?: cache_control,
                },
                "encrypted_code_execution_result" => jobj! {
                    "type": "code_execution_tool_result",
                    "tool_use_id": id,
                    "content": jobj! {
                        "type": result_type.as_str(),
                        "encrypted_stdout": field("encrypted_stdout"),
                        "stderr": field("stderr"),
                        "return_code": field("return_code"),
                        "content": content_list(),
                    },
                    "cache_control"?: cache_control,
                },
                "bash_code_execution_result" => jobj! {
                    "type": "bash_code_execution_tool_result",
                    "tool_use_id": id,
                    "cache_control"?: cache_control,
                    "content": jobj! {
                        "type": result_type.as_str(),
                        "stdout": field("stdout"),
                        "stderr": field("stderr"),
                        "return_code": field("return_code"),
                        "content": field("content"),
                    },
                },
                "bash_code_execution_tool_result_error" => jobj! {
                    "type": "bash_code_execution_tool_result",
                    "tool_use_id": id,
                    "cache_control"?: cache_control,
                    "content": value.clone(),
                },
                _ => jobj! {
                    "type": "text_editor_code_execution_tool_result",
                    "tool_use_id": id,
                    "cache_control"?: cache_control,
                    "content": value.clone(),
                },
            }))
        }
        "web_fetch" => {
            if kind == "error-json" {
                return Ok(Some(jobj! {
                    "type": "web_fetch_tool_result",
                    "tool_use_id": id,
                    "content": jobj! {
                        "type": "web_fetch_tool_result_error",
                        "error_code": extract_error_code(Some(&value)).unwrap_or_else(|| "unavailable".to_owned()),
                    },
                    "caller"?: caller,
                    "cache_control"?: cache_control,
                }));
            }
            if kind != "json" {
                return Ok(None);
            }
            let content = value.get("content").cloned().unwrap_or_default();
            let source = content.get("source").cloned().unwrap_or_default();
            Ok(Some(jobj! {
                "type": "web_fetch_tool_result",
                "tool_use_id": id,
                "content": jobj! {
                    "type": "web_fetch_result",
                    "url": value.get("url").cloned().unwrap_or_default(),
                    "retrieved_at": value.get("retrievedAt").cloned().unwrap_or_default(),
                    "content": jobj! {
                        "type": "document",
                        "title"?: content.get("title").cloned(),
                        "citations"?: content.get("citations").cloned(),
                        "source": jobj! {
                            "type": source.get("type").cloned().unwrap_or_default(),
                            "media_type": source.get("mediaType").cloned().unwrap_or_default(),
                            "data": source.get("data").cloned().unwrap_or_default(),
                        },
                    },
                },
                "caller"?: caller,
                "cache_control"?: cache_control,
            }))
        }
        "web_search" => {
            if kind == "error-json" {
                return Ok(Some(jobj! {
                    "type": "web_search_tool_result",
                    "tool_use_id": id,
                    "content": jobj! {
                        "type": "web_search_tool_result_error",
                        "error_code": extract_error_code(Some(&value)).unwrap_or_else(|| "unavailable".to_owned()),
                    },
                    "caller"?: caller,
                    "cache_control"?: cache_control,
                }));
            }
            if kind != "json" {
                return Ok(None);
            }
            let results: Vec<Json> = value
                .as_arr()
                .map_or(&[][..], Vec::as_slice)
                .iter()
                .map(|result| {
                    jobj! {
                        "url": result.get("url").cloned().unwrap_or_default(),
                        "title": result.get("title").cloned().unwrap_or_default(),
                        "page_age": result.get("pageAge").cloned().unwrap_or_default(),
                        "encrypted_content": result.get("encryptedContent").cloned().unwrap_or_default(),
                        "type": result.get("type").cloned().unwrap_or_default(),
                    }
                })
                .collect();
            Ok(Some(jobj! {
                "type": "web_search_tool_result",
                "tool_use_id": id,
                "content": results,
                "caller"?: caller,
                "cache_control"?: cache_control,
            }))
        }
        "tool_search_tool_regex" | "tool_search_tool_bm25" => {
            if kind != "json" {
                return Ok(None);
            }
            let references: Vec<Json> = value
                .as_arr()
                .map_or(&[][..], Vec::as_slice)
                .iter()
                .map(|r| jobj! { "type": "tool_reference", "tool_name": r.get("toolName").cloned().unwrap_or_default() })
                .collect();
            Ok(Some(jobj! {
                "type": "tool_search_tool_result",
                "tool_use_id": id,
                "content": jobj! { "type": "tool_search_tool_search_result", "tool_references": references },
                "cache_control"?: cache_control,
            }))
        }
        "advisor" => {
            if kind != "json" && kind != "error-json" {
                return Ok(None);
            }
            let content = match value.str_at("type") {
                Some("advisor_result") => jobj! {
                    "type": "advisor_result",
                    "text": value.get("text").cloned().unwrap_or_default(),
                    "stop_reason"?: value.get("stopReason").cloned(),
                },
                Some("advisor_redacted_result") => jobj! {
                    "type": "advisor_redacted_result",
                    "encrypted_content": value.get("encryptedContent").cloned().unwrap_or_default(),
                    "stop_reason"?: value.get("stopReason").cloned(),
                },
                _ => jobj! { "type": "advisor_tool_result_error", "error_code": value.get("errorCode").cloned().unwrap_or_default() },
            };
            Ok(Some(jobj! { "type": "advisor_tool_result", "tool_use_id": id, "content": content, "cache_control"?: cache_control }))
        }
        _ => Ok(None),
    }
}

fn move_tool_use_to_end(content: Vec<Json>) -> Vec<Json> {
    let mut result = Vec::with_capacity(content.len());
    let mut segment: Vec<Json> = Vec::new();
    let flush = |segment: &mut Vec<Json>, result: &mut Vec<Json>| {
        let (tools, others): (Vec<Json>, Vec<Json>) = segment.drain(..).partition(|p| p.str_at("type") == Some("tool_use"));
        result.extend(others);
        result.extend(tools);
    };
    for part in content {
        if matches!(part.str_at("type"), Some("thinking") | Some("redacted_thinking")) {
            flush(&mut segment, &mut result);
            result.push(part);
        } else {
            segment.push(part);
        }
    }
    flush(&mut segment, &mut result);
    result
}

/// `convertToAnthropicPrompt`.
fn convert_prompt(prompt: &[Json], ctx: &mut PromptContext<'_>) -> Result<(Option<Vec<Json>>, Vec<Json>), AiError> {
    // `groupIntoBlocks`: runs of one kind, with `tool` counted as `user`.
    let kind_of = |message: &Json| match message.str_at("role") {
        Some("tool") | Some("user") => "user",
        Some("assistant") => "assistant",
        _ => "system",
    };
    let mut blocks: Vec<(&str, Vec<&Json>)> = Vec::new();
    for message in prompt {
        let kind = kind_of(message);
        match blocks.last_mut() {
            Some((current, list)) if *current == kind => list.push(message),
            _ => blocks.push((kind, vec![message])),
        }
    }
    let mut system: Option<Vec<Json>> = None;
    let mut messages: Vec<Json> = Vec::new();
    let block_count = blocks.len();
    for (i, (kind, block)) in blocks.into_iter().enumerate() {
        let is_last_block = i == block_count - 1;
        match kind {
            "system" => {
                let mut content: Vec<Json> = Vec::new();
                let mut tool_change_count = 0;
                for message in &block {
                    let text = message.str_at("content").unwrap_or("");
                    let options = message.get("providerOptions");
                    let changes = options.and_then(|o| o.path(&["anthropic", "toolChanges"])).and_then(Json::as_arr);
                    // A system message that only carries tool changes may have empty text.
                    if !text.is_empty() || changes.is_none_or(|c| c.is_empty()) {
                        content.push(jobj! { "type": "text", "text": text, "cache_control"?: ctx.cache.get(options, true) });
                    }
                    for change in changes.map_or(&[][..], Vec::as_slice) {
                        tool_change_count += 1;
                        let name = ctx.names.provider(change.str_at("toolName").unwrap_or(""));
                        content.push(jobj! {
                            "type": change.get("type").cloned().unwrap_or_default(),
                            "tool": jobj! { "type": "tool_reference", "name": name },
                        });
                    }
                }
                // The first block becomes the top-level system prompt. Later ones go inline: always when they
                // carry tool changes, otherwise only when a top-level prompt already exists.
                if i == 0 || (system.is_none() && tool_change_count == 0) {
                    system = Some(content.into_iter().filter(|part| part.str_at("type") == Some("text")).collect());
                } else {
                    messages.push(jobj! { "role": "system", "content": content });
                    ctx.betas.add("mid-conversation-system-2026-04-07");
                    if tool_change_count > 0 {
                        ctx.betas.add("mid-conversation-tool-changes-2026-07-01");
                    }
                }
            }
            "user" => {
                let mut content: Vec<Json> = Vec::new();
                for message in &block {
                    let parts = message.get("content").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice);
                    let message_options = message.get("providerOptions");
                    for (j, part) in parts.iter().enumerate() {
                        let is_last = j == parts.len() - 1;
                        if message.str_at("role") == Some("user") {
                            let cache = ctx.cache.get(part.get("providerOptions"), true).or_else(|| {
                                if is_last { ctx.cache.get(message_options, true) } else { None }
                            });
                            match part.str_at("type") {
                                Some("text") => content.push(jobj! {
                                    "type": "text",
                                    "text": part.get("text").cloned().unwrap_or_default(),
                                    "cache_control"?: cache,
                                }),
                                Some("file") => content.push(user_file_block(part, cache, ctx.betas)?),
                                _ => {}
                            }
                            continue;
                        }
                        if part.str_at("type") == Some("tool-approval-response") {
                            continue;
                        }
                        let output = part.get("output").cloned().unwrap_or_default();
                        let output_options = output.get("providerOptions").or_else(|| {
                            if output.str_at("type") == Some("content") {
                                output.get("value").and_then(Json::as_arr)?.iter().find_map(|p| p.get("providerOptions"))
                            } else {
                                None
                            }
                        });
                        let cache = ctx
                            .cache
                            .get(part.get("providerOptions"), true)
                            .or_else(|| ctx.cache.get(output_options, true))
                            .or_else(|| if is_last { ctx.cache.get(message_options, true) } else { None });
                        let kind = output.str_at("type").unwrap_or("");
                        content.push(jobj! {
                            "type": "tool_result",
                            "tool_use_id": part.get("toolCallId").cloned().unwrap_or_default(),
                            "content": tool_result_content(&output, ctx.betas)?,
                            "is_error"?: (kind == "error-text" || kind == "error-json").then_some(true),
                            "cache_control"?: cache,
                        });
                    }
                }
                messages.push(jobj! { "role": "user", "content": content });
            }
            _ => {
                let mut content: Vec<Json> = Vec::new();
                let mut mcp_ids: HashSet<String> = HashSet::new();
                let count = block.len();
                for (j, message) in block.iter().enumerate() {
                    let is_last_message = j == count - 1;
                    let parts = message.get("content").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice);
                    let message_options = message.get("providerOptions");
                    for (k, part) in parts.iter().enumerate() {
                        let is_last_part = k == parts.len() - 1;
                        let part_options = part.get("providerOptions");
                        let cache = ctx.cache.get(part_options, true).or_else(|| {
                            if is_last_part { ctx.cache.get(message_options, true) } else { None }
                        });
                        match part.str_at("type") {
                            Some("text") => {
                                let metadata = part_options.and_then(|o| o.get("anthropic"));
                                let text = part.str_at("text").unwrap_or("");
                                if metadata.and_then(|m| m.str_at("type")) == Some("compaction") {
                                    content.push(jobj! { "type": "compaction", "content": text, "cache_control"?: cache });
                                } else {
                                    // Anthropic does not allow trailing whitespace in a pre-filled response.
                                    let text = if is_last_block && is_last_message && is_last_part { text.trim() } else { text };
                                    content.push(jobj! {
                                        "type": "text",
                                        "text": text,
                                        "citations"?: metadata.and_then(|m| m.non_null("citations")).cloned(),
                                        "cache_control"?: cache,
                                    });
                                }
                            }
                            Some("reasoning") => {
                                if !ctx.send_reasoning {
                                    continue;
                                }
                                let metadata = part_options.and_then(|o| o.get("anthropic"));
                                if let Some(signature) = metadata.and_then(|m| m.non_null("signature")) {
                                    content.push(jobj! {
                                        "type": "thinking",
                                        "thinking": part.get("text").cloned().unwrap_or_default(),
                                        "signature": signature.clone(),
                                    });
                                } else if let Some(data) = metadata.and_then(|m| m.non_null("redactedData")) {
                                    content.push(jobj! { "type": "redacted_thinking", "data": data.clone() });
                                }
                            }
                            Some("tool-call") => {
                                if let Some(block) = assistant_tool_call(part, cache, ctx, &mut mcp_ids) {
                                    content.push(block);
                                }
                            }
                            Some("tool-result") => {
                                if let Some(block) = assistant_tool_result(part, cache, ctx, &mcp_ids)? {
                                    content.push(block);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                messages.push(jobj! { "role": "assistant", "content": move_tool_use_to_end(content) });
            }
        }
    }
    Ok((system, messages))
}

/// `prepareTools`.
fn prepare_tools(
    tools: Option<&Vec<Json>>,
    tool_choice: &Json,
    disable_parallel: Option<bool>,
    cache: &mut CacheValidator,
    supports_structured_output: bool,
    eager_input_streaming: bool,
    betas: &mut Betas,
) -> (Option<Vec<Json>>, Option<Json>) {
    let Some(tools) = tools.filter(|t| !t.is_empty()) else { return (None, None) };
    let mut out: Vec<Json> = Vec::with_capacity(tools.len());
    for tool in tools {
        match tool.str_at("type") {
            Some("function") => {
                let options = tool.get("providerOptions");
                let cache_control = cache.get(options, true);
                let anthropic = options.and_then(|o| o.get("anthropic"));
                let eager = anthropic.and_then(|a| a.get("eagerInputStreaming")).and_then(Json::as_bool).unwrap_or(eager_input_streaming);
                let allowed = anthropic.and_then(|a| a.non_null("allowedCallers")).cloned();
                let examples = tool.get("inputExamples").and_then(Json::as_arr);
                out.push(jobj! {
                    "name": tool.get("name").cloned().unwrap_or_default(),
                    "description"?: tool.non_null("description").cloned(),
                    "input_schema": tool.get("inputSchema").cloned().unwrap_or_default(),
                    "cache_control"?: cache_control,
                    "eager_input_streaming"?: eager.then_some(true),
                    "strict"?: if supports_structured_output { tool.get("strict").and_then(Json::as_bool) } else { None },
                    "defer_loading"?: anthropic.and_then(|a| a.non_null("deferLoading")).cloned(),
                    "allowed_callers"?: allowed.clone(),
                    "input_examples"?: examples.map(|list| list.iter().map(|e| e.get("input").cloned().unwrap_or_default()).collect::<Vec<_>>()),
                });
                if supports_structured_output {
                    betas.add("structured-outputs-2025-11-13");
                }
                if examples.is_some() || allowed.is_some() {
                    betas.add("advanced-tool-use-2025-11-20");
                }
            }
            Some("provider") => {
                let args = tool.get("args").cloned().unwrap_or_default();
                let arg = |key: &str| args.get(key).cloned();
                match tool.str_at("id").unwrap_or("") {
                    "anthropic.web_fetch_20250910" | "anthropic.web_fetch_20260209" => {
                        let newer = tool.str_at("id") == Some("anthropic.web_fetch_20260209");
                        betas.add(if newer { "code-execution-web-tools-2026-02-09" } else { "web-fetch-2025-09-10" });
                        out.push(jobj! {
                            "type": if newer { "web_fetch_20260209" } else { "web_fetch_20250910" },
                            "name": "web_fetch",
                            "max_uses"?: arg("maxUses"),
                            "allowed_domains"?: arg("allowedDomains"),
                            "blocked_domains"?: arg("blockedDomains"),
                            "citations"?: arg("citations"),
                            "max_content_tokens"?: arg("maxContentTokens"),
                        });
                    }
                    "anthropic.web_search_20250305" | "anthropic.web_search_20260209" => {
                        let newer = tool.str_at("id") == Some("anthropic.web_search_20260209");
                        if newer {
                            betas.add("code-execution-web-tools-2026-02-09");
                        }
                        out.push(jobj! {
                            "type": if newer { "web_search_20260209" } else { "web_search_20250305" },
                            "name": "web_search",
                            "max_uses"?: arg("maxUses"),
                            "allowed_domains"?: arg("allowedDomains"),
                            "blocked_domains"?: arg("blockedDomains"),
                            "user_location"?: arg("userLocation"),
                        });
                    }
                    // Only the web tools are ever declared by this crate's callers (`search.rs`).
                    _ => {}
                }
            }
            _ => {}
        }
    }
    let choice = match tool_choice.str_at("type").unwrap_or("auto") {
        "required" => Some(jobj! { "type": "any", "disable_parallel_tool_use"?: disable_parallel }),
        // Anthropic has no `none`, so the tools go.
        "none" => return (None, None),
        "tool" => Some(jobj! {
            "type": "tool",
            "name": tool_choice.get("toolName").cloned().unwrap_or_default(),
            "disable_parallel_tool_use"?: disable_parallel,
        }),
        _ => Some(jobj! { "type": "auto", "disable_parallel_tool_use"?: disable_parallel }),
    };
    (Some(out), choice)
}

/// `resolveAnthropicReasoningConfig`: `(thinking, effort)`.
fn resolve_reasoning(reasoning: &str, caps: &Capabilities) -> (Option<Json>, Option<String>) {
    if reasoning == "none" {
        return (Some(jobj! { "type": "disabled" }), None);
    }
    if caps.supports_adaptive_thinking {
        let effort = match reasoning {
            "minimal" | "low" => Some("low"),
            "medium" => Some("medium"),
            "high" => Some("high"),
            "xhigh" => Some(if caps.supports_xhigh_effort { "xhigh" } else { "max" }),
            _ => None,
        };
        return (Some(jobj! { "type": "adaptive", "display": "summarized" }), effort.map(str::to_owned));
    }
    let share = match reasoning {
        "minimal" => 0.02,
        "low" => 0.1,
        "medium" => 0.3,
        "high" => 0.6,
        "xhigh" => 0.9,
        _ => return (None, None),
    };
    let budget = caps.max_output_tokens.min(1024u64.max(js_round(caps.max_output_tokens as f64 * share)));
    (Some(jobj! { "type": "enabled", "budgetTokens": budget }), None)
}

/// The wire form of `providerOptions.anthropic.contextManagement`.
fn context_management(management: &Json) -> Json {
    let mut edits = Vec::new();
    for edit in management.get("edits").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice) {
        let field = |key: &str| edit.get(key).cloned();
        match edit.str_at("type") {
            Some("clear_tool_uses_20250919") => edits.push(jobj! {
                "type": "clear_tool_uses_20250919",
                "trigger"?: field("trigger"),
                "keep"?: field("keep"),
                "clear_at_least"?: field("clearAtLeast"),
                "clear_tool_inputs"?: field("clearToolInputs"),
                "exclude_tools"?: field("excludeTools"),
            }),
            Some("clear_thinking_20251015") => edits.push(jobj! { "type": "clear_thinking_20251015", "keep"?: field("keep") }),
            Some("compact_20260112") => edits.push(jobj! {
                "type": "compact_20260112",
                "trigger"?: field("trigger"),
                "pause_after_compaction"?: field("pauseAfterCompaction"),
                "instructions"?: field("instructions"),
            }),
            _ => {}
        }
    }
    jobj! { "edits": edits }
}

struct Args {
    body: Json,
    betas: Betas,
    names: ToolNames,
    citation_documents: Vec<CitationDocument>,
    mark_code_execution_dynamic: bool,
}

#[derive(Clone)]
struct CitationDocument {
    title: String,
    filename: Option<String>,
    media_type: String,
}

impl AnthropicModel {
    fn get_args(&self, options: &CallOptions) -> Result<Args, AiError> {
        let caps = capabilities(&self.model_id);
        let mut anthropic = options.options_for("anthropic").and_then(Json::as_obj).cloned().unwrap_or_default();
        let names = ToolNames::new(options.tools.as_ref());
        let mut cache = CacheValidator::default();
        let mut betas = Betas::default();
        let send_reasoning = anthropic.get("sendReasoning").and_then(Json::as_bool).unwrap_or(true);
        let (system, messages) = {
            let mut ctx = PromptContext { names: &names, send_reasoning, cache: &mut cache, betas: &mut betas };
            convert_prompt(&options.prompt, &mut ctx)?
        };

        // Provider options always take precedence over the shared `reasoning`.
        if let Some(reasoning) = options.reasoning.as_deref().filter(|r| *r != "provider-default") {
            if anthropic.get("effort").is_none_or(Json::is_null) {
                let (thinking, effort) = resolve_reasoning(reasoning, &caps);
                if let Some(thinking) = thinking {
                    if anthropic.get("thinking").is_none_or(Json::is_null) {
                        anthropic.insert("thinking", thinking);
                    }
                    let disabled = anthropic.get("thinking").and_then(|t| t.str_at("type")) == Some("disabled");
                    if let (Some(effort), false) = (effort, disabled) {
                        anthropic.insert("effort", effort);
                    }
                }
            }
        }
        // Newer models only allow disabling thinking at effort levels up to `high`.
        if caps.rejects_thinking_disabled_above_high_effort
            && anthropic.get("thinking").and_then(|t| t.str_at("type")) == Some("disabled")
            && matches!(anthropic.get("effort").and_then(Json::as_str), Some("xhigh") | Some("max"))
        {
            anthropic.insert("effort", "high");
        }

        let thinking = anthropic.get("thinking").cloned().unwrap_or_default();
        let thinking_type = thinking.str_at("type").map(str::to_owned);
        let is_thinking = matches!(thinking_type.as_deref(), Some("enabled") | Some("adaptive"));
        let send_thinking = is_thinking || thinking_type.as_deref() == Some("disabled");
        let mut thinking_budget = if thinking_type.as_deref() == Some("enabled") { thinking.get("budgetTokens").and_then(Json::as_u64) } else { None };
        let thinking_display = if thinking_type.as_deref() == Some("adaptive") { thinking.non_null("display").cloned() } else { None };
        if thinking_type.as_deref() == Some("enabled") && thinking_budget.is_none() {
            thinking_budget = Some(1024);
        }

        let max_tokens = options.max_output_tokens.unwrap_or(caps.max_output_tokens);
        let mut effective_max = max_tokens;
        if is_thinking {
            effective_max += thinking_budget.unwrap_or(0);
        }
        // Limit to the model's ceiling for known models, which allows switching models without breaking.
        if caps.is_known_model && effective_max > caps.max_output_tokens {
            effective_max = caps.max_output_tokens;
        }

        let mut body = Obj::new();
        body.insert("model", self.model_id.as_str());
        body.insert("max_tokens", effective_max);
        if send_thinking {
            let mut wire = Obj::new();
            wire.insert("type", thinking_type.clone().unwrap_or_default());
            if let Some(budget) = thinking_budget {
                wire.insert("budget_tokens", budget);
            }
            if let Some(display) = thinking_display {
                wire.insert("display", display);
            }
            body.insert("thinking", wire);
        }
        let effort = anthropic.get("effort").and_then(Json::as_str).filter(|e| !e.is_empty()).map(str::to_owned);
        let task_budget = anthropic.non_null("taskBudget").cloned();
        if effort.is_some() || task_budget.is_some() {
            let mut config = Obj::new();
            if let Some(effort) = &effort {
                config.insert("effort", effort.as_str());
            }
            if let Some(budget) = &task_budget {
                config.insert("task_budget", jobj! {
                    "type": budget.get("type").cloned().unwrap_or_default(),
                    "total": budget.get("total").cloned().unwrap_or_default(),
                    "remaining"?: budget.non_null("remaining").cloned(),
                });
            }
            body.insert("output_config", config);
        }
        if let Some(speed) = anthropic.get("speed").and_then(Json::as_str).filter(|s| !s.is_empty()) {
            body.insert("speed", speed);
        }
        if let Some(geo) = anthropic.get("inferenceGeo").and_then(Json::as_str).filter(|s| !s.is_empty()) {
            body.insert("inference_geo", geo);
        }
        let fallbacks = anthropic.non_null("fallbacks").cloned();
        match &fallbacks {
            Some(Json::Str(text)) if text == "default" => body.insert("fallbacks", "default"),
            Some(Json::Arr(list)) if !list.is_empty() => body.insert("fallbacks", list.clone()),
            _ => {}
        }
        if let Some(control) = anthropic.non_null("cacheControl") {
            body.insert("cache_control", control.clone());
        }
        if let Some(user) = anthropic.get("metadata").and_then(|m| m.non_null("userId")) {
            body.insert("metadata", jobj! { "user_id": user.clone() });
        }
        let mcp_servers = anthropic.get("mcpServers").and_then(Json::as_arr).filter(|s| !s.is_empty()).cloned();
        if let Some(servers) = &mcp_servers {
            let wire: Vec<Json> = servers
                .iter()
                .map(|server| {
                    jobj! {
                        "type": server.get("type").cloned().unwrap_or_default(),
                        "name": server.get("name").cloned().unwrap_or_default(),
                        "url": server.get("url").cloned().unwrap_or_default(),
                        "authorization_token"?: server.non_null("authorizationToken").cloned(),
                        "tool_configuration"?: server.non_null("toolConfiguration").map(|c| jobj! {
                            "allowed_tools"?: c.get("allowedTools").cloned(),
                            "enabled"?: c.get("enabled").cloned(),
                        }),
                    }
                })
                .collect();
            body.insert("mcp_servers", wire);
        }
        let container = anthropic.non_null("container").cloned();
        let mut has_skills = false;
        if let Some(container) = &container {
            match container.get("skills").and_then(Json::as_arr).filter(|s| !s.is_empty()) {
                Some(skills) => {
                    has_skills = true;
                    let wire: Vec<Json> = skills
                        .iter()
                        .map(|skill| {
                            let id = if skill.str_at("type") == Some("custom") {
                                skill.path(&["providerReference", "anthropic"]).cloned().unwrap_or_default()
                            } else {
                                skill.get("skillId").cloned().unwrap_or_default()
                            };
                            jobj! {
                                "type": skill.get("type").cloned().unwrap_or_default(),
                                "skill_id": id,
                                "version"?: skill.get("version").cloned(),
                            }
                        })
                        .collect();
                    body.insert("container", jobj! { "id"?: container.get("id").cloned(), "skills": wire });
                }
                None => body.insert("container", container.get("id").cloned().unwrap_or_default()),
            }
        }
        if let Some(system) = system {
            body.insert("system", system);
        }
        body.insert("messages", messages);
        let management = anthropic.non_null("contextManagement").cloned();
        if let Some(management) = &management {
            body.insert("context_management", context_management(management));
        }

        if mcp_servers.is_some() {
            betas.add("mcp-client-2025-04-04");
        }
        if let Some(management) = &management {
            betas.add("context-management-2025-06-27");
            if management.get("edits").and_then(Json::as_arr).is_some_and(|e| e.iter().any(|edit| edit.str_at("type") == Some("compact_20260112"))) {
                betas.add("compact-2026-01-12");
            }
        }
        if has_skills {
            betas.add("code-execution-2025-08-25");
            betas.add("skills-2025-10-02");
            betas.add("files-api-2025-04-14");
        }
        if task_budget.is_some() {
            betas.add("task-budgets-2026-03-13");
        }
        if anthropic.get("speed").and_then(Json::as_str) == Some("fast") {
            betas.add("fast-mode-2026-02-01");
        }
        match &fallbacks {
            Some(Json::Str(text)) if text == "default" => betas.add("server-side-fallback-2026-07-01"),
            Some(Json::Arr(list)) if !list.is_empty() => betas.add("server-side-fallback-2026-06-01"),
            _ => {}
        }

        let eager = anthropic.get("toolStreaming").and_then(Json::as_bool).unwrap_or(true);
        let (tools, tool_choice) = prepare_tools(
            options.tools.as_ref(),
            &options.tool_choice,
            anthropic.get("disableParallelToolUse").and_then(Json::as_bool),
            &mut cache,
            caps.supports_structured_output,
            eager,
            &mut betas,
        );
        let mut mark_dynamic = false;
        if let Some(list) = &tools {
            let web_new = list.iter().any(|t| matches!(t.str_at("type"), Some("web_fetch_20260209") | Some("web_search_20260209")));
            let code = list.iter().any(|t| t.str_at("name") == Some("code_execution"));
            mark_dynamic = web_new && !code;
        }
        if let Some(tools) = tools {
            body.insert("tools", tools);
        }
        if let Some(choice) = tool_choice {
            body.insert("tool_choice", choice);
        }
        body.insert("stream", true);

        // User-supplied betas: config headers first, then the provider option.
        for beta in self.headers.get("anthropic-beta").unwrap_or("").to_lowercase().split(',') {
            let beta = beta.trim();
            if !beta.is_empty() {
                betas.add(beta);
            }
        }
        for beta in anthropic.get("anthropicBeta").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice) {
            if let Some(beta) = beta.as_str() {
                betas.add(beta);
            }
        }

        Ok(Args {
            body: Json::Obj(body),
            betas,
            names,
            citation_documents: citation_documents(&options.prompt),
            mark_code_execution_dynamic: mark_dynamic,
        })
    }

    async fn stream(&self, options: CallOptions) -> Result<ModelStream, AiError> {
        let args = self.get_args(&options)?;
        let url = format!("{}/messages", self.base_url);
        let mut headers = self.headers.clone();
        if !args.betas.0.is_empty() {
            headers.set("anthropic-beta", &args.betas.0.join(","));
        }
        let response = post_json(&self.fetch, &url, headers, &args.body, &options.cancel, ERROR_SHAPE).await?;
        let response_headers = response.headers.to_vec();
        let transform = StreamState {
            usage: WireUsage::default(),
            finish_reason: FinishReason::other(),
            blocks: HashMap::new(),
            mcp_calls: HashMap::new(),
            server_calls: HashMap::new(),
            block_type: None,
            raw_usage: None,
            stop_sequence: None,
            stop_details: None,
            container: None,
            context_management: None,
            message_open: false,
            active_message_id: None,
            invalid_sequence: false,
            names: args.names,
            citation_documents: args.citation_documents,
            mark_code_execution_dynamic: args.mark_code_execution_dynamic,
        };
        let mut stream = ModelStream::sse(response.body, Box::new(transform));
        // The API answers 200 for overloaded errors: an error as the first chunk is a failed call, with the
        // response headers a stream error lacks.
        if let Some(StreamPart::Error(error)) = stream.peek_first().await {
            let error = error.clone();
            return Err(match error {
                AiError::StreamProvider(stream_error) => AiError::api(ApiCallError {
                    message: stream_error.message.clone(),
                    url,
                    status: Some(stream_error.status.unwrap_or(500)),
                    response_headers,
                    response_body: stream_error.data.as_ref().map(Json::to_text),
                    is_retryable: stream_error.is_retryable,
                    data: None,
                }),
                AiError::Abort => AiError::Abort,
                AiError::Network { code, message } => AiError::Network { code, message },
                _ => AiError::other("Expected a normalized Anthropic stream error"),
            });
        }
        Ok(stream)
    }
}

const ERROR_SHAPE: ErrorShape = ErrorShape {
    message: |body| {
        if body.str_at("type") != Some("error") {
            return None;
        }
        let error = body.get("error")?;
        error.str_at("type")?;
        error.str_at("message").map(str::to_owned)
    },
    is_retryable: None,
};

fn citation_documents(prompt: &[Json]) -> Vec<CitationDocument> {
    let mut out = Vec::new();
    for message in prompt {
        if message.str_at("role") != Some("user") {
            continue;
        }
        for part in message.get("content").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice) {
            if part.str_at("type") != Some("file") {
                continue;
            }
            let media = part.str_at("mediaType").unwrap_or("");
            if media != "application/pdf" && media != "text/plain" {
                continue;
            }
            if part.path(&["providerOptions", "anthropic", "citations", "enabled"]).and_then(Json::as_bool) != Some(true) {
                continue;
            }
            out.push(CitationDocument {
                title: part.str_at("filename").unwrap_or("Untitled Document").to_owned(),
                filename: part.str_at("filename").map(str::to_owned),
                media_type: media.to_owned(),
            });
        }
    }
    out
}

impl LanguageModel for AnthropicModel {
    fn provider(&self) -> &str {
        "anthropic.messages"
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn supported_urls(&self) -> SupportedUrls {
        vec![("image/*", r"^https?://.*$"), ("application/pdf", r"^https?://.*$")]
    }

    fn do_stream(&self, options: CallOptions) -> BoxFuture<'_, Result<ModelStream, AiError>> {
        Box::pin(self.stream(options))
    }
}

// ------------------------------------------------------------------ Streaming

#[derive(Default)]
struct WireUsage {
    input_tokens: u64,
    output_tokens: u64,
    thinking_tokens: Option<u64>,
    cache_creation_input_tokens: u64,
    cache_read_input_tokens: u64,
    iterations: Option<Vec<Json>>,
}

/// `convertAnthropicUsage`.
fn convert_usage(usage: &WireUsage, raw: Option<&Json>) -> ModelUsage {
    let number = |value: Option<&Json>| value.and_then(Json::as_u64).unwrap_or(0);
    let served_by_fallback = usage.iterations.as_ref().is_some_and(|list| list.iter().any(|i| i.str_at("type") == Some("fallback_message")));
    let (mut input, mut output) = (usage.input_tokens, usage.output_tokens);
    if let (Some(iterations), false) = (&usage.iterations, served_by_fallback) {
        let executor: Vec<&Json> = iterations.iter().filter(|i| matches!(i.str_at("type"), Some("compaction") | Some("message"))).collect();
        if !executor.is_empty() {
            input = executor.iter().map(|i| number(i.get("input_tokens"))).sum();
            output = executor.iter().map(|i| number(i.get("output_tokens"))).sum();
        }
    }
    ModelUsage {
        input_total: Some(input + usage.cache_creation_input_tokens + usage.cache_read_input_tokens),
        input_no_cache: Some(input),
        input_cache_read: Some(usage.cache_read_input_tokens),
        input_cache_write: Some(usage.cache_creation_input_tokens),
        output_total: Some(output),
        output_text: usage.thinking_tokens.map(|reasoning| output.saturating_sub(reasoning)),
        output_reasoning: usage.thinking_tokens,
        raw: raw.cloned(),
    }
}

fn map_stop_reason(reason: Option<&str>) -> &'static str {
    match reason {
        Some("pause_turn") | Some("end_turn") | Some("stop_sequence") => "stop",
        Some("refusal") => "content-filter",
        Some("tool_use") => "tool-calls",
        Some("max_tokens") | Some("model_context_window_exceeded") => "length",
        _ => "other",
    }
}

fn stream_error_metadata(kind: &str) -> (Option<u16>, Option<bool>) {
    match kind {
        "api_error" => (Some(500), Some(true)),
        "overloaded_error" => (Some(529), Some(true)),
        "rate_limit_error" => (Some(429), Some(true)),
        "request_too_large" => (Some(413), Some(false)),
        "authentication_error" => (Some(401), Some(false)),
        "permission_error" => (Some(403), Some(false)),
        "not_found_error" => (Some(404), Some(false)),
        "billing_error" | "invalid_request_error" => (Some(400), Some(false)),
        _ => (None, None),
    }
}

/// `createAnthropicStreamError`, normalized as the `ai` package reads it.
fn stream_error(error: &Json) -> AiError {
    let kind = error.str_at("type").unwrap_or("").to_owned();
    let (status, retryable) = stream_error_metadata(&kind);
    let status = error.get("statusCode").and_then(Json::as_u64).map(|s| s as u16).or(status);
    let message = error.str_at("message").unwrap_or("").to_owned();
    let inferred = match message.trim().to_ascii_lowercase().as_str() {
        "overloaded" | "overloaded error" | "model overloaded" | "service unavailable" => Some((503u16, true)),
        "internal server error" => Some((500, true)),
        _ => None,
    };
    let status = status.or(inferred.map(|(s, _)| s));
    let is_retryable = error
        .get("isRetryable")
        .and_then(Json::as_bool)
        .or(retryable)
        .or(inferred.map(|(_, r)| r))
        .unwrap_or_else(|| ApiCallError::default_retryable(status));
    AiError::StreamProvider(Box::new(crate::error::StreamProviderError {
        message,
        type_: Some(kind),
        code: error.get("code").filter(|c| c.is_str() || c.is_num()).cloned(),
        status,
        is_retryable,
        data: Some(error.get("data").cloned().unwrap_or_else(|| error.clone())),
    }))
}

enum Block {
    Text { citations: Vec<Json> },
    Reasoning,
    ToolCall {
        tool_call_id: String,
        tool_name: String,
        input: String,
        provider_executed: bool,
        dynamic: bool,
        first_delta: bool,
        provider_tool_name: Option<String>,
        provider_tool_input_type: Option<String>,
        caller: Option<Json>,
    },
}

struct StreamState {
    usage: WireUsage,
    finish_reason: FinishReason,
    blocks: HashMap<u64, Block>,
    mcp_calls: HashMap<String, (String, Option<Json>)>,
    server_calls: HashMap<String, String>,
    block_type: Option<String>,
    raw_usage: Option<Json>,
    stop_sequence: Option<String>,
    stop_details: Option<Json>,
    container: Option<Json>,
    context_management: Option<Json>,
    message_open: bool,
    active_message_id: Option<Json>,
    invalid_sequence: bool,
    names: ToolNames,
    citation_documents: Vec<CitationDocument>,
    mark_code_execution_dynamic: bool,
}

fn caller_info(part: &Json) -> Option<Json> {
    let caller = part.non_null("caller")?;
    Some(jobj! { "type": caller.get("type").cloned().unwrap_or_default(), "toolId"?: caller.get("tool_id").cloned() })
}

fn caller_metadata(part: &Json) -> Option<Json> {
    caller_info(part).map(|caller| jobj! { "anthropic": jobj! { "caller": caller } })
}

fn merge_raw(raw: &mut Option<Json>, more: &Json) {
    let mut object = raw.take().and_then(|r| match r {
        Json::Obj(o) => Some(o),
        _ => None,
    }).unwrap_or_default();
    if let Some(more) = more.as_obj() {
        object.extend_from(more);
    }
    *raw = Some(Json::Obj(object));
}

fn citation_source(citation: &Json, documents: &[CitationDocument]) -> Option<Json> {
    let kind = citation.str_at("type")?;
    if kind == "web_search_result_location" {
        return Some(jobj! {
            "sourceType": "url",
            "id": generate_id(),
            "url": citation.get("url").cloned().unwrap_or_default(),
            "title"?: citation.get("title").cloned(),
            "providerMetadata": jobj! { "anthropic": jobj! {
                "citedText": citation.get("cited_text").cloned().unwrap_or_default(),
                "encryptedIndex": citation.get("encrypted_index").cloned().unwrap_or_default(),
            } },
        });
    }
    if kind != "page_location" && kind != "char_location" {
        return None;
    }
    let document = documents.get(citation.get("document_index")?.as_u64()? as usize)?;
    let metadata = if kind == "page_location" {
        jobj! {
            "citedText": citation.get("cited_text").cloned().unwrap_or_default(),
            "startPageNumber": citation.get("start_page_number").cloned().unwrap_or_default(),
            "endPageNumber": citation.get("end_page_number").cloned().unwrap_or_default(),
        }
    } else {
        jobj! {
            "citedText": citation.get("cited_text").cloned().unwrap_or_default(),
            "startCharIndex": citation.get("start_char_index").cloned().unwrap_or_default(),
            "endCharIndex": citation.get("end_char_index").cloned().unwrap_or_default(),
        }
    };
    Some(jobj! {
        "sourceType": "document",
        "id": generate_id(),
        "mediaType": document.media_type.as_str(),
        "title": citation.non_null("document_title").cloned().unwrap_or_else(|| Json::from(document.title.as_str())),
        "filename"?: document.filename.as_deref(),
        "providerMetadata": jobj! { "anthropic": metadata },
    })
}

impl StreamState {
    fn tool_result(&self, part: &Json, tool: &str, is_error: bool, result: Json, metadata: Option<Json>) -> StreamPart {
        StreamPart::ToolResult {
            tool_call_id: part.str_at("tool_use_id").unwrap_or("").to_owned(),
            tool_name: self.names.custom(tool).to_owned(),
            result,
            is_error: is_error.then_some(true),
            dynamic: None,
            provider_metadata: metadata,
        }
    }

    fn on_block_start(&mut self, index: u64, part: &Json, out: &mut Vec<StreamPart>) {
        let kind = part.str_at("type").unwrap_or("");
        // Server-side fallback marker: the SDK has no content primitive for a model hop.
        if kind == "fallback" {
            return;
        }
        self.block_type = Some(kind.to_owned());
        let id = index.to_string();
        match kind {
            "text" => {
                self.blocks.insert(index, Block::Text { citations: Vec::new() });
                out.push(StreamPart::TextStart { id, provider_metadata: None });
            }
            "thinking" => {
                self.blocks.insert(index, Block::Reasoning);
                out.push(StreamPart::ReasoningStart { id, provider_metadata: None });
            }
            "redacted_thinking" => {
                self.blocks.insert(index, Block::Reasoning);
                out.push(StreamPart::ReasoningStart {
                    id,
                    provider_metadata: Some(jobj! { "anthropic": jobj! { "redactedData": part.get("data").cloned().unwrap_or_default() } }),
                });
            }
            "compaction" => {
                self.blocks.insert(index, Block::Text { citations: Vec::new() });
                out.push(StreamPart::TextStart { id, provider_metadata: Some(jobj! { "anthropic": jobj! { "type": "compaction" } }) });
            }
            "tool_use" => {
                let initial = part.get("input").filter(|i| i.as_obj().is_some_and(|o| !o.is_empty())).map_or_else(String::new, Json::to_text);
                let tool_call_id = part.str_at("id").unwrap_or("").to_owned();
                let tool_name = part.str_at("name").unwrap_or("").to_owned();
                self.blocks.insert(index, Block::ToolCall {
                    tool_call_id: tool_call_id.clone(),
                    tool_name: tool_name.clone(),
                    first_delta: initial.is_empty(),
                    input: initial,
                    provider_executed: false,
                    dynamic: false,
                    provider_tool_name: None,
                    provider_tool_input_type: None,
                    caller: caller_info(part),
                });
                out.push(StreamPart::ToolInputStart { id: tool_call_id, tool_name, provider_executed: None, dynamic: None });
            }
            "server_tool_use" => {
                let name = part.str_at("name").unwrap_or("").to_owned();
                let tool_call_id = part.str_at("id").unwrap_or("").to_owned();
                let (provider_name, input_type, initial, first_delta) = match name.as_str() {
                    "web_fetch" | "web_search" | "code_execution" | "text_editor_code_execution" | "bash_code_execution" => {
                        let provider = if name == "text_editor_code_execution" || name == "bash_code_execution" { "code_execution".to_owned() } else { name.clone() };
                        let input_type = if name == "text_editor_code_execution" || name == "bash_code_execution" {
                            Some(name.clone())
                        } else if name == "code_execution" {
                            Some("programmatic-tool-call".to_owned())
                        } else {
                            None
                        };
                        // Tools like `web_fetch_20260209` provide input here; others by deltas.
                        let initial = part.get("input").filter(|i| i.as_obj().is_some_and(|o| !o.is_empty())).map_or_else(String::new, Json::to_text);
                        let first = initial.is_empty();
                        (provider, input_type, initial, first)
                    }
                    "tool_search_tool_regex" | "tool_search_tool_bm25" => {
                        self.server_calls.insert(tool_call_id.clone(), name.clone());
                        (name.clone(), None, String::new(), true)
                    }
                    "advisor" => (name.clone(), None, "{}".to_owned(), true),
                    _ => return,
                };
                let custom = self.names.custom(&provider_name).to_owned();
                let dynamic = self.mark_code_execution_dynamic && provider_name == "code_execution";
                self.blocks.insert(index, Block::ToolCall {
                    tool_call_id: tool_call_id.clone(),
                    tool_name: custom.clone(),
                    input: initial,
                    provider_executed: true,
                    dynamic,
                    first_delta,
                    provider_tool_name: Some(provider_name),
                    provider_tool_input_type: input_type,
                    caller: caller_info(part),
                });
                out.push(StreamPart::ToolInputStart { id: tool_call_id, tool_name: custom, provider_executed: Some(true), dynamic: dynamic.then_some(true) });
            }
            "web_fetch_tool_result" => {
                let content = part.get("content").cloned().unwrap_or_default();
                match content.str_at("type") {
                    Some("web_fetch_result") => {
                        let inner = content.get("content").cloned().unwrap_or_default();
                        let source = inner.get("source").cloned().unwrap_or_default();
                        self.citation_documents.push(CitationDocument {
                            title: inner.str_at("title").or_else(|| content.str_at("url")).unwrap_or("").to_owned(),
                            filename: None,
                            media_type: source.str_at("media_type").unwrap_or("").to_owned(),
                        });
                        let result = jobj! {
                            "type": "web_fetch_result",
                            "url": content.get("url").cloned().unwrap_or_default(),
                            "retrievedAt"?: content.get("retrieved_at").cloned(),
                            "content": jobj! {
                                "type": inner.get("type").cloned().unwrap_or_default(),
                                "title"?: inner.get("title").cloned(),
                                "citations"?: inner.get("citations").cloned(),
                                "source": jobj! {
                                    "type": source.get("type").cloned().unwrap_or_default(),
                                    "mediaType": source.get("media_type").cloned().unwrap_or_default(),
                                    "data": source.get("data").cloned().unwrap_or_default(),
                                },
                            },
                        };
                        out.push(self.tool_result(part, "web_fetch", false, result, caller_metadata(part)));
                    }
                    Some("web_fetch_tool_result_error") => {
                        let result = jobj! { "type": "web_fetch_tool_result_error", "errorCode": content.get("error_code").cloned().unwrap_or_default() };
                        out.push(self.tool_result(part, "web_fetch", true, result, caller_metadata(part)));
                    }
                    _ => {}
                }
            }
            "web_search_tool_result" => {
                let content = part.get("content").cloned().unwrap_or_default();
                if let Some(results) = content.as_arr() {
                    let mapped: Vec<Json> = results
                        .iter()
                        .map(|r| {
                            jobj! {
                                "url": r.get("url").cloned().unwrap_or_default(),
                                "title": r.get("title").cloned().unwrap_or_default(),
                                "pageAge": r.non_null("page_age").cloned().unwrap_or(Json::Null),
                                "encryptedContent": r.get("encrypted_content").cloned().unwrap_or_default(),
                                "type": r.get("type").cloned().unwrap_or_default(),
                            }
                        })
                        .collect();
                    out.push(self.tool_result(part, "web_search", false, Json::Arr(mapped), caller_metadata(part)));
                    for r in results {
                        out.push(StreamPart::Source(jobj! {
                            "sourceType": "url",
                            "id": generate_id(),
                            "url": r.get("url").cloned().unwrap_or_default(),
                            "title"?: r.get("title").cloned(),
                            "providerMetadata": jobj! { "anthropic": jobj! { "pageAge": r.non_null("page_age").cloned().unwrap_or(Json::Null) } },
                        }));
                    }
                } else {
                    let result = jobj! { "type": "web_search_tool_result_error", "errorCode": content.get("error_code").cloned().unwrap_or_default() };
                    out.push(self.tool_result(part, "web_search", true, result, caller_metadata(part)));
                }
            }
            "code_execution_tool_result" => {
                let content = part.get("content").cloned().unwrap_or_default();
                let field = |key: &str| content.get(key).cloned().unwrap_or_default();
                let list = || content.non_null("content").cloned().unwrap_or_else(|| jarr![]);
                match content.str_at("type") {
                    Some("code_execution_result") => {
                        let result = jobj! {
                            "type": "code_execution_result",
                            "stdout": field("stdout"),
                            "stderr": field("stderr"),
                            "return_code": field("return_code"),
                            "content": list(),
                        };
                        out.push(self.tool_result(part, "code_execution", false, result, None));
                    }
                    Some("encrypted_code_execution_result") => {
                        let result = jobj! {
                            "type": "encrypted_code_execution_result",
                            "encrypted_stdout": field("encrypted_stdout"),
                            "stderr": field("stderr"),
                            "return_code": field("return_code"),
                            "content": list(),
                        };
                        out.push(self.tool_result(part, "code_execution", false, result, None));
                    }
                    Some("code_execution_tool_result_error") => {
                        let result = jobj! { "type": "code_execution_tool_result_error", "errorCode": field("error_code") };
                        out.push(self.tool_result(part, "code_execution", true, result, None));
                    }
                    _ => {}
                }
            }
            "bash_code_execution_tool_result" | "text_editor_code_execution_tool_result" => {
                out.push(self.tool_result(part, "code_execution", false, part.get("content").cloned().unwrap_or_default(), None));
            }
            "tool_search_tool_result" => {
                let call_id = part.str_at("tool_use_id").unwrap_or("");
                let provider_name = self.server_calls.get(call_id).cloned().unwrap_or_else(|| {
                    if self.names.custom("tool_search_tool_bm25") != "tool_search_tool_bm25" { "tool_search_tool_bm25".to_owned() } else { "tool_search_tool_regex".to_owned() }
                });
                let content = part.get("content").cloned().unwrap_or_default();
                if content.str_at("type") == Some("tool_search_tool_search_result") {
                    let references: Vec<Json> = content
                        .get("tool_references")
                        .and_then(Json::as_arr)
                        .map_or(&[][..], Vec::as_slice)
                        .iter()
                        .map(|r| jobj! { "type": r.get("type").cloned().unwrap_or_default(), "toolName": r.get("tool_name").cloned().unwrap_or_default() })
                        .collect();
                    out.push(self.tool_result(part, &provider_name, false, Json::Arr(references), None));
                } else {
                    let result = jobj! { "type": "tool_search_tool_result_error", "errorCode": content.get("error_code").cloned().unwrap_or_default() };
                    out.push(self.tool_result(part, &provider_name, true, result, None));
                }
            }
            "advisor_tool_result" => {
                let content = part.get("content").cloned().unwrap_or_default();
                match content.str_at("type") {
                    Some("advisor_result") => {
                        let result = jobj! {
                            "type": "advisor_result",
                            "text": content.get("text").cloned().unwrap_or_default(),
                            "stopReason"?: content.non_null("stop_reason").cloned(),
                        };
                        out.push(self.tool_result(part, "advisor", false, result, None));
                    }
                    Some("advisor_redacted_result") => {
                        let result = jobj! {
                            "type": "advisor_redacted_result",
                            "encryptedContent": content.get("encrypted_content").cloned().unwrap_or_default(),
                            "stopReason"?: content.non_null("stop_reason").cloned(),
                        };
                        out.push(self.tool_result(part, "advisor", false, result, None));
                    }
                    _ => {
                        let result = jobj! { "type": "advisor_tool_result_error", "errorCode": content.get("error_code").cloned().unwrap_or_default() };
                        out.push(self.tool_result(part, "advisor", true, result, None));
                    }
                }
            }
            "mcp_tool_use" => {
                let call_id = part.str_at("id").unwrap_or("").to_owned();
                let name = part.str_at("name").unwrap_or("").to_owned();
                let metadata = jobj! { "anthropic": jobj! { "type": "mcp-tool-use", "serverName": part.get("server_name").cloned().unwrap_or_default() } };
                self.mcp_calls.insert(call_id.clone(), (name.clone(), Some(metadata.clone())));
                out.push(StreamPart::ToolCall {
                    tool_call_id: call_id,
                    tool_name: name,
                    input: part.get("input").cloned().unwrap_or_default().to_text(),
                    provider_executed: Some(true),
                    dynamic: Some(true),
                    provider_metadata: Some(metadata),
                });
            }
            "mcp_tool_result" => {
                let call_id = part.str_at("tool_use_id").unwrap_or("").to_owned();
                let (name, metadata) = self.mcp_calls.get(&call_id).cloned().unwrap_or_default();
                out.push(StreamPart::ToolResult {
                    tool_call_id: call_id,
                    tool_name: name,
                    result: part.get("content").cloned().unwrap_or_default(),
                    is_error: part.get("is_error").and_then(Json::as_bool),
                    dynamic: Some(true),
                    provider_metadata: metadata,
                });
            }
            _ => {}
        }
    }

    fn on_block_stop(&mut self, index: u64, out: &mut Vec<StreamPart>) {
        if let Some(block) = self.blocks.remove(&index) {
            match block {
                Block::Text { citations } => out.push(StreamPart::TextEnd {
                    id: index.to_string(),
                    provider_metadata: (!citations.is_empty()).then(|| jobj! { "anthropic": jobj! { "citations": citations } }),
                }),
                Block::Reasoning => out.push(StreamPart::ReasoningEnd { id: index.to_string(), provider_metadata: None }),
                Block::ToolCall { tool_call_id, tool_name, input, provider_executed, dynamic, provider_tool_name, caller, .. } => {
                    out.push(StreamPart::ToolInputEnd { id: tool_call_id.clone() });
                    let mut final_input = if input.is_empty() { "{}".to_owned() } else { input };
                    if provider_tool_name.as_deref() == Some("code_execution") {
                        if let Ok(parsed) = secure_json_parse(&final_input) {
                            if parsed.get("code").is_some() && parsed.get("type").is_none() {
                                let mut object = Obj::new();
                                object.insert("type", "programmatic-tool-call");
                                if let Some(existing) = parsed.as_obj() {
                                    object.extend_from(existing);
                                }
                                final_input = Json::Obj(object).to_text();
                            }
                        }
                    }
                    out.push(StreamPart::ToolCall {
                        tool_call_id,
                        tool_name,
                        input: final_input,
                        provider_executed: provider_executed.then_some(true),
                        dynamic: dynamic.then_some(true),
                        provider_metadata: caller.map(|caller| jobj! { "anthropic": jobj! { "caller": caller } }),
                    });
                }
            }
        }
        self.block_type = None;
    }

    fn on_block_delta(&mut self, index: u64, delta: &Json, out: &mut Vec<StreamPart>) {
        let id = index.to_string();
        match delta.str_at("type").unwrap_or("") {
            "text_delta" => out.push(StreamPart::TextDelta { id, delta: delta.str_at("text").unwrap_or("").to_owned(), provider_metadata: None }),
            "thinking_delta" => out.push(StreamPart::ReasoningDelta { id, delta: delta.str_at("thinking").unwrap_or("").to_owned(), provider_metadata: None }),
            "signature_delta" => {
                // Signatures are only supported on thinking blocks.
                if self.block_type.as_deref() == Some("thinking") {
                    out.push(StreamPart::ReasoningDelta {
                        id,
                        delta: String::new(),
                        provider_metadata: Some(jobj! { "anthropic": jobj! { "signature": delta.get("signature").cloned().unwrap_or_default() } }),
                    });
                }
            }
            "compaction_delta" => {
                if let Some(content) = delta.str_at("content") {
                    out.push(StreamPart::TextDelta { id, delta: content.to_owned(), provider_metadata: None });
                }
            }
            "input_json_delta" => {
                let mut piece = delta.str_at("partial_json").unwrap_or("").to_owned();
                // Empty deltas are skipped so the first character can be replaced below.
                if piece.is_empty() {
                    return;
                }
                let Some(Block::ToolCall { tool_call_id, input, first_delta, provider_tool_input_type, .. }) = self.blocks.get_mut(&index) else {
                    return;
                };
                // The code execution sub-tools need their type added to the first delta.
                if *first_delta {
                    if let Some(kind) = provider_tool_input_type {
                        piece = format!("{{\"type\": \"{kind}\",{}", piece.get(1..).unwrap_or(""));
                    }
                }
                out.push(StreamPart::ToolInputDelta { id: tool_call_id.clone(), delta: piece.clone() });
                input.push_str(&piece);
                *first_delta = false;
            }
            "citations_delta" => {
                let citation = delta.get("citation").cloned().unwrap_or_default();
                if let Some(Block::Text { citations }) = self.blocks.get_mut(&index) {
                    if citation.str_at("type") == Some("web_search_result_location") {
                        citations.push(citation.clone());
                    }
                }
                if let Some(source) = citation_source(&citation, &self.citation_documents) {
                    out.push(StreamPart::Source(source));
                }
            }
            _ => {}
        }
    }

    fn on_message_start(&mut self, value: &Json, out: &mut Vec<StreamPart>) {
        let message = value.get("message").cloned().unwrap_or_default();
        let message_id = message.get("id").cloned().filter(|id| !id.is_null());
        if self.message_open {
            if self.active_message_id == message_id {
                return;
            }
            self.invalid_sequence = true;
            let shown = |id: &Option<Json>| id.as_ref().map_or_else(|| "undefined".to_owned(), Json::to_text);
            out.push(StreamPart::Error(AiError::other(format!(
                "Received message_start for message {} while message {} is still open.",
                shown(&message_id),
                shown(&self.active_message_id)
            ))));
            return;
        }
        self.message_open = true;
        self.active_message_id = message_id.clone();
        let usage = message.get("usage").cloned().unwrap_or_default();
        let count = |key: &str| usage.get(key).and_then(Json::as_u64).unwrap_or(0);
        self.usage.input_tokens = count("input_tokens");
        self.usage.cache_read_input_tokens = count("cache_read_input_tokens");
        self.usage.cache_creation_input_tokens = count("cache_creation_input_tokens");
        self.raw_usage = Some(usage.clone());
        if let Some(container) = message.non_null("container") {
            self.container = Some(jobj! {
                "expiresAt": container.get("expires_at").cloned().unwrap_or_default(),
                "id": container.get("id").cloned().unwrap_or_default(),
                "skills": Json::Null,
            });
        }
        if let Some(reason) = message.str_at("stop_reason") {
            self.finish_reason = FinishReason::new(map_stop_reason(Some(reason)), Some(reason.to_owned()));
        }
        out.push(StreamPart::ResponseMetadata {
            id: message_id.as_ref().and_then(Json::as_str).map(str::to_owned),
            model_id: message.str_at("model").map(str::to_owned),
        });
        // Programmatic tool calling: deferred calls may be pre-populated in `message_start`.
        for part in message.get("content").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice) {
            if part.str_at("type") != Some("tool_use") {
                continue;
            }
            let call_id = part.str_at("id").unwrap_or("").to_owned();
            let name = part.str_at("name").unwrap_or("").to_owned();
            let input = part.non_null("input").map_or_else(|| "{}".to_owned(), Json::to_text);
            out.push(StreamPart::ToolInputStart { id: call_id.clone(), tool_name: name.clone(), provider_executed: None, dynamic: None });
            out.push(StreamPart::ToolInputDelta { id: call_id.clone(), delta: input.clone() });
            out.push(StreamPart::ToolInputEnd { id: call_id.clone() });
            out.push(StreamPart::ToolCall {
                tool_call_id: call_id,
                tool_name: name,
                input,
                provider_executed: None,
                dynamic: None,
                provider_metadata: caller_info(part).map(|caller| jobj! { "anthropic": jobj! { "caller": caller } }),
            });
        }
    }

    fn on_message_delta(&mut self, value: &Json) {
        let usage = value.get("usage").cloned().unwrap_or_default();
        if let Some(input) = usage.non_null("input_tokens").and_then(Json::as_u64) {
            self.usage.input_tokens = input;
        }
        self.usage.output_tokens = usage.get("output_tokens").and_then(Json::as_u64).unwrap_or(0);
        if let Some(details) = usage.non_null("output_tokens_details") {
            self.usage.thinking_tokens = details.non_null("thinking_tokens").and_then(Json::as_u64);
        }
        if let Some(read) = usage.non_null("cache_read_input_tokens").and_then(Json::as_u64) {
            self.usage.cache_read_input_tokens = read;
        }
        if let Some(created) = usage.non_null("cache_creation_input_tokens").and_then(Json::as_u64) {
            self.usage.cache_creation_input_tokens = created;
        }
        if let Some(iterations) = usage.non_null("iterations").and_then(Json::as_arr) {
            self.usage.iterations = Some(iterations.clone());
        }
        let delta = value.get("delta").cloned().unwrap_or_default();
        let reason = delta.str_at("stop_reason");
        self.finish_reason = FinishReason::new(map_stop_reason(reason), reason.map(str::to_owned));
        self.stop_sequence = delta.str_at("stop_sequence").map(str::to_owned);
        self.stop_details = delta.non_null("stop_details").map(|details| {
            jobj! {
                "type": details.get("type").cloned().unwrap_or_default(),
                "category"?: details.non_null("category").cloned(),
                "explanation"?: details.non_null("explanation").cloned(),
                "recommendedModel"?: details.non_null("recommended_model").cloned(),
            }
        });
        self.container = delta.non_null("container").map(|container| {
            let skills = container.get("skills").and_then(Json::as_arr).map(|list| {
                list.iter()
                    .map(|skill| {
                        jobj! {
                            "type": skill.get("type").cloned().unwrap_or_default(),
                            "skillId": skill.get("skill_id").cloned().unwrap_or_default(),
                            "version": skill.get("version").cloned().unwrap_or_default(),
                        }
                    })
                    .collect::<Vec<_>>()
            });
            jobj! {
                "expiresAt": container.get("expires_at").cloned().unwrap_or_default(),
                "id": container.get("id").cloned().unwrap_or_default(),
                "skills": skills.map_or(Json::Null, Json::Arr),
            }
        });
        if let Some(management) = value.non_null("context_management") {
            self.context_management = Some(management.clone());
        }
        merge_raw(&mut self.raw_usage, &usage);
    }

    fn on_message_stop(&mut self, out: &mut Vec<StreamPart>) {
        self.message_open = false;
        self.active_message_id = None;
        let metadata = jobj! {
            "usage": self.raw_usage.clone().unwrap_or(Json::Null),
            "stopSequence": self.stop_sequence.clone().map_or(Json::Null, Json::from),
            "stopDetails"?: self.stop_details.clone(),
            "iterations": Json::Null,
            "container": self.container.clone().unwrap_or(Json::Null),
            "contextManagement": self.context_management.clone().unwrap_or(Json::Null),
        };
        out.push(StreamPart::Finish {
            finish_reason: self.finish_reason.clone(),
            usage: convert_usage(&self.usage, self.raw_usage.as_ref()),
            provider_metadata: Some(jobj! { "anthropic": metadata }),
        });
    }
}

impl ChunkTransform for StreamState {
    fn start(&mut self, out: &mut Vec<StreamPart>) {
        out.push(StreamPart::StreamStart { warnings: Vec::new() });
    }

    fn chunk(&mut self, chunk: Result<Json, AiError>, out: &mut Vec<StreamPart>) {
        if self.invalid_sequence {
            return;
        }
        let value = match chunk {
            Ok(value) => value,
            Err(error) => {
                out.push(StreamPart::Error(error));
                return;
            }
        };
        let index = value.get("index").and_then(Json::as_u64);
        match value.str_at("type") {
            Some("ping") => {}
            Some("content_block_start") => {
                if let (Some(index), Some(block)) = (index, value.get("content_block")) {
                    self.on_block_start(index, block, out);
                }
            }
            Some("content_block_stop") => {
                if let Some(index) = index {
                    self.on_block_stop(index, out);
                }
            }
            Some("content_block_delta") => {
                if let (Some(index), Some(delta)) = (index, value.get("delta")) {
                    self.on_block_delta(index, delta, out);
                }
            }
            Some("message_start") => self.on_message_start(&value, out),
            Some("message_delta") => self.on_message_delta(&value),
            Some("message_stop") => self.on_message_stop(out),
            Some("error") => out.push(StreamPart::Error(stream_error(value.get("error").unwrap_or(&Json::Null)))),
            _ => out.push(StreamPart::Error(AiError::validation(&value, "unknown chunk type"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_follow_the_sdk_table() {
        assert_eq!(capabilities("claude-opus-5").max_output_tokens, 128_000);
        assert!(capabilities("claude-opus-5").rejects_thinking_disabled_above_high_effort);
        assert!(!capabilities("claude-sonnet-4-6").supports_xhigh_effort);
        assert!(!capabilities("claude-sonnet-4-5").supports_adaptive_thinking);
        assert_eq!(capabilities("claude-3-opus").max_output_tokens, 4096);
        assert!(!capabilities("claude-3-opus").is_known_model);
        assert!(!capabilities("claude-future").is_known_model);
        assert_eq!(capabilities("deepseek").max_output_tokens, 4096);
    }

    #[test]
    fn stop_reasons_map() {
        assert_eq!(map_stop_reason(Some("pause_turn")), "stop");
        assert_eq!(map_stop_reason(Some("tool_use")), "tool-calls");
        assert_eq!(map_stop_reason(Some("refusal")), "content-filter");
        assert_eq!(map_stop_reason(Some("model_context_window_exceeded")), "length");
        assert_eq!(map_stop_reason(Some("compaction")), "other");
        assert_eq!(map_stop_reason(None), "other");
    }

    #[test]
    fn usage_counts_cache_and_compaction_iterations() {
        let usage = WireUsage {
            input_tokens: 1,
            output_tokens: 2,
            thinking_tokens: Some(1),
            cache_creation_input_tokens: 10,
            cache_read_input_tokens: 20,
            iterations: Some(vec![
                Json::parse(r#"{"type":"compaction","input_tokens":100,"output_tokens":50}"#).unwrap(),
                Json::parse(r#"{"type":"message","input_tokens":5,"output_tokens":6}"#).unwrap(),
                Json::parse(r#"{"type":"advisor_message","input_tokens":999,"output_tokens":999}"#).unwrap(),
            ]),
        };
        let converted = convert_usage(&usage, None);
        assert_eq!((converted.input_total, converted.input_no_cache), (Some(135), Some(105)));
        assert_eq!((converted.output_total, converted.output_text, converted.output_reasoning), (Some(56), Some(55), Some(1)));
    }

    #[test]
    fn tool_use_moves_behind_text_within_a_thinking_segment() {
        let block = |kind: &str| jobj! { "type": kind };
        let moved = move_tool_use_to_end(vec![block("tool_use"), block("text"), block("thinking"), block("tool_use"), block("text")]);
        let kinds: Vec<&str> = moved.iter().map(|b| b.str_at("type").unwrap()).collect();
        assert_eq!(kinds, ["text", "tool_use", "thinking", "text", "tool_use"]);
    }
}
