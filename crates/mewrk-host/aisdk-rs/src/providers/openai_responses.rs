//! `@ai-sdk/openai`'s Responses language model, streaming path, and `createOpenAI` for the two families on it.
//!
//! `openai-codex` is the same provider with another default base URL; what makes the ChatGPT backend different is its
//! `fetch` dialect (`dialect/codex.rs`). `azure.rs` builds this same model with Azure's URL and header rules.

mod input;
mod schema;
pub(super) mod url;

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use bytes::Bytes;

use crate::api::{ErrorShape, VERSION_OPENAI, combine_headers, generate_id, post_json, with_user_agent_suffix, without_trailing_slash};
use crate::error::{AiError, ApiCallError, StreamProviderError};
use crate::http::{Body, BoxFuture, ChunkMap, DynFetch, Headers};
use crate::json::{Json, Obj};
use crate::model::{CallOptions, ChunkTransform, DynModel, FinishReason, LanguageModel, ModelStream, ModelUsage, StreamPart, SupportedUrls};
use crate::sse::{FrameDecoder, SseJsonDecoder};
use crate::stream_text::secure_json_parse;
use crate::{jarr, jobj};

use self::input::{InputOptions, convert_input};
use self::schema::{Parsed, options_schema, parse_chunk};
use super::ProviderSettings;

const OPENAI_API_URL: &str = "https://api.openai.com/v1";
const CODEX_API_URL: &str = "https://chatgpt.com/backend-api/codex";

pub enum Flavor {
    OpenAi,
    Codex,
}

/// `createOpenAI` as `providers.ts` configures it, `.responses(modelId)`.
pub fn create(target: ProviderSettings, flavor: Flavor) -> Result<DynModel, AiError> {
    let default_url = match flavor {
        Flavor::OpenAi => OPENAI_API_URL,
        Flavor::Codex => CODEX_API_URL,
    };
    let base_url = target.base_url.as_deref().unwrap_or(default_url);
    // `validateBaseURL`.
    if base_url.trim().is_empty() {
        return Err(AiError::other("baseURL must be a non-empty string."));
    }
    let url = format!("{}/responses", without_trailing_slash(base_url));
    let mut headers = Headers::new();
    headers.set("authorization", &format!("Bearer {}", target.api_key));
    let headers = combine_headers(&headers, &target.headers);
    let headers = with_user_agent_suffix(headers, &[&format!("ai-sdk/openai/{VERSION_OPENAI}")]);
    Ok(Arc::new(ResponsesModel {
        provider: "openai.responses",
        provider_options_name: "openai",
        model_id: target.model_id,
        url: Ok(url),
        headers,
        fetch: target.fetch,
        file_id_prefixes: &["file-"],
    }))
}

/// `OpenAIResponsesLanguageModel`.
pub(super) struct ResponsesModel {
    /// `config.provider`: `openai.responses` or `azure.responses`.
    pub(super) provider: &'static str,
    /// `azure` when the provider name has it, `openai` otherwise.
    pub(super) provider_options_name: &'static str,
    pub(super) model_id: String,
    /// `config.url({ path: '/responses' })`. An `Err` is what that throws, which upstream reaches only after the
    /// arguments were built.
    pub(super) url: Result<String, AiError>,
    pub(super) headers: Headers,
    pub(super) fetch: DynFetch,
    pub(super) file_id_prefixes: &'static [&'static str],
}

/// `getOpenAILanguageModelCapabilities`.
struct Capabilities {
    is_reasoning_model: bool,
    system_message_mode: &'static str,
    supports_flex_processing: bool,
    supports_priority_processing: bool,
}

fn capabilities(model_id: &str) -> Capabilities {
    static O_SERIES: OnceLock<regex::Regex> = OnceLock::new();
    static GPT: OnceLock<regex::Regex> = OnceLock::new();
    let o_series = O_SERIES
        .get_or_init(|| regex::Regex::new(r"^o([0-9]+)(?:-|$)").expect("a literal pattern"))
        .captures(model_id)
        .and_then(|found| found[1].parse::<f64>().ok());
    let gpt = GPT.get_or_init(|| regex::Regex::new(r"^gpt-([0-9]+)(?:\.([0-9]+))?(?:-(.+))?$").expect("a literal pattern")).captures(model_id);
    let major = gpt.as_ref().and_then(|found| found[1].parse::<f64>().ok());
    let minor = gpt.as_ref().and_then(|found| found.get(2));
    let variant = gpt.as_ref().and_then(|found| found.get(3)).map(|found| found.as_str());
    let is_chat = minor.is_none() && variant.is_some_and(|v| v.starts_with("chat"));
    let is_nano = variant.is_some_and(|v| v.starts_with("nano"));
    let gpt_at_least_5 = major.is_some_and(|m| m >= 5.0);
    let o_at_least_3 = o_series.is_some_and(|v| v >= 3.0);
    let is_reasoning_model = o_series.is_some() || (gpt_at_least_5 && !is_chat);
    Capabilities {
        is_reasoning_model,
        // Only recognizable OpenAI model families use the developer role; fine-tuned, third-party and custom ids
        // keep the conservative default.
        system_message_mode: if is_reasoning_model { "developer" } else { "system" },
        supports_flex_processing: o_at_least_3 || (gpt_at_least_5 && !is_chat),
        supports_priority_processing: model_id.starts_with("gpt-4") || (gpt_at_least_5 && !is_nano && !is_chat) || o_at_least_3,
    }
}

/// `createToolNameMapping` for the Responses tool ids.
#[derive(Default)]
pub(super) struct ToolNames {
    to_provider: HashMap<String, String>,
    to_custom: HashMap<String, String>,
}

fn provider_tool_name(id: &str) -> Option<&'static str> {
    Some(match id {
        "openai.code_interpreter" => "code_interpreter",
        "openai.computer" => "computer",
        "openai.file_search" => "file_search",
        "openai.image_generation" => "image_generation",
        "openai.local_shell" => "local_shell",
        "openai.shell" => "shell",
        "openai.web_search" => "web_search",
        "openai.web_search_preview" => "web_search_preview",
        "openai.mcp" => "mcp",
        "openai.apply_patch" => "apply_patch",
        "openai.tool_search" => "tool_search",
        "openai.programmatic_tool_calling" => "programmatic_tool_calling",
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

    pub(super) fn provider<'a>(&'a self, custom: &'a str) -> &'a str {
        self.to_provider.get(custom).map_or(custom, String::as_str)
    }

    fn custom<'a>(&'a self, provider: &'a str) -> &'a str {
        self.to_custom.get(provider).map_or(provider, String::as_str)
    }
}

fn unsupported(functionality: impl std::fmt::Display) -> AiError {
    AiError::other(format!("'{functionality}' functionality not supported."))
}

/// `prepareFunctionTool`.
fn prepare_function_tool(tool: &Json, options: Option<&Json>) -> Json {
    jobj! {
        "type": "function",
        "name": tool.get("name").cloned().unwrap_or_default(),
        "description"?: tool.non_null("description").cloned(),
        "parameters": tool.get("inputSchema").cloned().unwrap_or_default(),
        "strict"?: tool.non_null("strict").cloned(),
        "defer_loading"?: options.and_then(|o| o.non_null("deferLoading")).cloned(),
        "allowed_callers"?: options.and_then(|o| o.non_null("allowedCallers")).cloned(),
        "output_schema"?: options.and_then(|o| o.non_null("outputSchema")).cloned(),
    }
}

/// `prepareResponsesTools`. Of the provider tools only `openai.web_search` can reach a model in this crate
/// (`search.rs`), so it is the only one mapped.
fn prepare_tools(
    tools: Option<&Vec<Json>>,
    tool_choice: &Json,
    allowed_tools: Option<&Json>,
    names: &ToolNames,
    output_schema_tool_names: &mut HashSet<String>,
) -> Result<(Option<Vec<Json>>, Option<Json>), AiError> {
    let Some(tools) = tools.filter(|t| !t.is_empty()) else { return Ok((None, None)) };
    let mut openai_tools: Vec<Json> = Vec::new();
    let mut namespaces: HashMap<String, usize> = HashMap::new();
    // What `allowedTools` can name: the entry, or `None` for a tool the API hides from `tool_choice`.
    let mut allow_listable: HashMap<&str, Option<Json>> = HashMap::new();
    for tool in tools {
        let name = tool.str_at("name").unwrap_or("");
        match tool.str_at("type") {
            Some("function") => {
                let options = tool.path(&["providerOptions", "openai"]);
                if options.and_then(|o| o.non_null("outputSchema")).is_some() {
                    output_schema_tool_names.insert(name.to_owned());
                }
                let prepared = prepare_function_tool(tool, options);
                let namespace = options.and_then(|o| o.non_null("namespace"));
                match namespace {
                    None => openai_tools.push(prepared),
                    Some(namespace) => {
                        let namespace_name = namespace.str_at("name").unwrap_or("");
                        let description = namespace.get("description").cloned().unwrap_or_default();
                        let at = match namespaces.get(namespace_name) {
                            None => {
                                namespaces.insert(namespace_name.to_owned(), openai_tools.len());
                                openai_tools.push(jobj! {
                                    "type": "namespace",
                                    "name": namespace_name,
                                    "description": description,
                                    "tools": Vec::<Json>::new(),
                                });
                                openai_tools.len() - 1
                            }
                            Some(&at) => {
                                if openai_tools[at].get("description") != Some(&description) {
                                    return Err(unsupported(format!("conflicting descriptions for OpenAI tool namespace \"{namespace_name}\"")));
                                }
                                at
                            }
                        };
                        if let Some(members) = openai_tools[at].get_mut("tools").and_then(Json::as_arr_mut) {
                            members.push(prepared);
                        }
                    }
                }
                let hidden = namespace.is_some() || options.and_then(|o| o.get("deferLoading")).and_then(Json::as_bool) == Some(true);
                allow_listable.insert(name, (!hidden).then(|| jobj! { "type": "function", "name": name }));
            }
            Some("provider") if tool.str_at("id") == Some("openai.web_search") => {
                let args = tool.get("args");
                let arg = |key: &str| args.and_then(|a| a.get(key)).cloned();
                let filters = args.and_then(|a| a.non_null("filters")).map(|filters| {
                    jobj! {
                        "allowed_domains"?: filters.get("allowedDomains").cloned(),
                        "blocked_domains"?: filters.get("blockedDomains").cloned(),
                    }
                });
                openai_tools.push(jobj! {
                    "type": "web_search",
                    "filters"?: filters,
                    "external_web_access"?: arg("externalWebAccess"),
                    "search_context_size"?: arg("searchContextSize"),
                    "user_location"?: arg("userLocation"),
                });
                allow_listable.insert(name, Some(jobj! { "type": "web_search" }));
            }
            _ => {}
        }
    }
    if let Some(allowed) = allowed_tools {
        let mut entries = Vec::new();
        let mut dropped = Vec::new();
        for name in allowed.get("toolNames").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice).iter().filter_map(Json::as_str) {
            match allow_listable.get(name) {
                // Not part of this request's tools: sent as a function tool.
                None => entries.push(jobj! { "type": "function", "name": names.provider(name) }),
                Some(None) => dropped.push(name),
                Some(Some(entry)) => entries.push(entry.clone()),
            }
        }
        if entries.is_empty() {
            return Err(unsupported(format!("allowedTools with only tools that cannot be allow-listed ({})", dropped.join(", "))));
        }
        let choice = jobj! {
            "type": "allowed_tools",
            "mode": allowed.non_null("mode").cloned().unwrap_or_else(|| Json::from("auto")),
            "tools": entries,
        };
        return Ok((Some(openai_tools), Some(choice)));
    }
    let choice = match tool_choice.str_at("type") {
        None => None,
        Some(kind @ ("auto" | "none" | "required")) => Some(Json::from(kind)),
        Some("tool") => {
            let resolved = names.provider(tool_choice.str_at("toolName").unwrap_or(""));
            Some(match resolved {
                "code_interpreter" | "file_search" | "image_generation" | "web_search_preview" | "web_search" | "mcp" | "apply_patch" | "computer"
                | "programmatic_tool_calling" => jobj! { "type": resolved },
                _ => jobj! { "type": "function", "name": resolved },
            })
        }
        Some(other) => return Err(unsupported(format!("tool choice type: {other}"))),
    };
    Ok((Some(openai_tools), choice))
}

/// `extractApprovalRequestIdToToolCallIdMapping`.
fn approval_ids_from_prompt(prompt: &[Json]) -> HashMap<String, String> {
    let mut mapping = HashMap::new();
    for message in prompt {
        if message.str_at("role") != Some("assistant") {
            continue;
        }
        for part in message.get("content").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice) {
            if part.str_at("type") != Some("tool-call") {
                continue;
            }
            let approval = part.path(&["providerOptions", "openai", "approvalRequestId"]).and_then(Json::as_str);
            if let (Some(approval), Some(call)) = (approval, part.str_at("toolCallId")) {
                mapping.insert(approval.to_owned(), call.to_owned());
            }
        }
    }
    mapping
}

struct Args {
    body: Json,
    web_search_tool_name: Option<String>,
    /// `store`, as the provider options gave it: only `true` concludes a reasoning part early.
    store: bool,
    names: ToolNames,
}

/// `parseProviderOptions` with the Responses schema. `Ok(None)` when the options are absent.
fn parse_options(provider_options: Option<&Json>, provider: &str) -> Result<Option<Json>, AiError> {
    let Some(value) = provider_options.and_then(|options| options.non_null(provider)) else { return Ok(None) };
    options_schema().parse(Some(value)).map_err(|()| AiError::other(format!("invalid {provider} provider options")))
}

impl ResponsesModel {
    /// `getArgs`.
    fn get_args(&self, options: &CallOptions) -> Result<Args, AiError> {
        let capabilities = capabilities(&self.model_id);
        let name = self.provider_options_name;
        let mut parsed = parse_options(options.provider_options.as_ref(), name)?;
        if parsed.is_none() && name != "openai" {
            parsed = parse_options(options.provider_options.as_ref(), "openai")?;
        }
        // `openaiOptions?.key`: `None` is `undefined`, and a `null` the schema took stays one.
        let option = |key: &str| parsed.as_ref().and_then(|o| o.get(key));
        let non_null = |key: &str| option(key).filter(|v| !v.is_null());

        let effort: Option<Json> = non_null("reasoningEffort")
            .cloned()
            .or_else(|| options.reasoning.as_deref().filter(|r| *r != "provider-default").map(Json::from));
        let summary: Option<Json> = match option("reasoningSummary") {
            Some(value) => Some(value.clone()),
            None => effort.as_ref().filter(|e| e.as_str() != Some("none")).map(|_| Json::from("detailed")),
        };
        let is_reasoning_model = option("forceReasoning").and_then(Json::as_bool).unwrap_or(capabilities.is_reasoning_model);

        let names = ToolNames::new(options.tools.as_ref());
        let mut output_schema_tool_names = HashSet::new();
        let (tools, tool_choice) =
            prepare_tools(options.tools.as_ref(), &options.tool_choice, option("allowedTools"), &names, &mut output_schema_tool_names)?;

        let system_message_mode = option("systemMessageMode")
            .and_then(Json::as_str)
            .unwrap_or(if is_reasoning_model { "developer" } else { capabilities.system_message_mode });
        let mut input = convert_input(
            &options.prompt,
            &InputOptions {
                names: &names,
                system_message_mode,
                provider_options_name: name,
                file_id_prefixes: self.file_id_prefixes,
                pass_through_unsupported_files: option("passThroughUnsupportedFiles").and_then(Json::as_bool).unwrap_or(false),
                store: non_null("store").and_then(Json::as_bool).unwrap_or(true),
                has_conversation: non_null("conversation").is_some(),
                has_previous_response_id: non_null("previousResponseId").is_some(),
                output_schema_tool_names: &output_schema_tool_names,
            },
        )?;
        // A compaction trigger is a request control, not history: OpenAI requires it as the final input item.
        if option("compactionTrigger").and_then(Json::as_bool) == Some(true) {
            input.push(jobj! { "type": "compaction_trigger" });
        }

        let mut include: Option<Json> = option("include").cloned();
        let mut add_include = |key: &str| match &mut include {
            Some(Json::Arr(values)) => {
                if !values.iter().any(|value| value.as_str() == Some(key)) {
                    values.push(Json::from(key));
                }
            }
            slot => *slot = Some(jarr![key]),
        };
        let top_logprobs: Option<Json> = match option("logprobs") {
            Some(Json::Bool(true)) => Some(Json::from(20u64)),
            Some(number) if number.is_num() => Some(number.clone()),
            _ => None,
        };
        if top_logprobs.is_some() {
            add_include("message.output_text.logprobs");
        }
        let provider_tool = |ids: &[&str]| {
            options.tools.as_ref().and_then(|tools| {
                tools.iter().find(|tool| tool.str_at("type") == Some("provider") && tool.str_at("id").is_some_and(|id| ids.contains(&id)))
            })
        };
        let web_search_tool_name = provider_tool(&["openai.web_search", "openai.web_search_preview"]).and_then(|tool| tool.str_at("name")).map(str::to_owned);
        if web_search_tool_name.as_deref().is_some_and(|name| !name.is_empty()) {
            add_include("web_search_call.action.sources");
        }
        if provider_tool(&["openai.code_interpreter"]).is_some() {
            add_include("code_interpreter_call.outputs");
        }
        // `store` defaults to true in the Responses API, so only an explicit false asks for the encrypted form.
        let store = option("store").cloned();
        if store.as_ref().and_then(Json::as_bool) == Some(false) && is_reasoning_model {
            add_include("reasoning.encrypted_content");
        }

        let service_tier_value = option("serviceTier").and_then(Json::as_str);
        let drop_service_tier = (service_tier_value == Some("flex") && !capabilities.supports_flex_processing)
            || (matches!(service_tier_value, Some("priority" | "fast")) && !capabilities.supports_priority_processing);

        let reasoning = (is_reasoning_model
            && (effort.is_some() || summary.as_ref().is_some_and(|s| !s.is_null()) || non_null("reasoningMode").is_some() || non_null("reasoningContext").is_some()))
        .then(|| {
            jobj! {
                "effort"?: effort.clone(),
                "summary"?: summary.clone().filter(|s| !s.is_null()),
                "mode"?: non_null("reasoningMode").cloned(),
                "context"?: non_null("reasoningContext").cloned(),
            }
        });
        let context_management = non_null("contextManagement").and_then(Json::as_arr).map(|entries| {
            entries
                .iter()
                .map(|entry| jobj! { "type": entry.get("type").cloned().unwrap_or_default(), "compact_threshold": entry.get("compactThreshold").cloned().unwrap_or_default() })
                .collect::<Vec<_>>()
        });

        let body = jobj! {
            "model": self.model_id.as_str(),
            "input": input,
            "max_output_tokens"?: options.max_output_tokens,
            "text"?: non_null("textVerbosity").map(|verbosity| jobj! { "verbosity": verbosity.clone() }),
            "conversation"?: option("conversation").cloned(),
            "max_tool_calls"?: option("maxToolCalls").cloned(),
            "metadata"?: option("metadata").cloned(),
            "parallel_tool_calls"?: option("parallelToolCalls").cloned(),
            "previous_response_id"?: option("previousResponseId").cloned(),
            "store"?: store.clone(),
            "user"?: option("user").cloned(),
            "instructions"?: option("instructions").cloned(),
            "service_tier"?: option("serviceTier").cloned().filter(|_| !drop_service_tier),
            "include"?: include,
            "prompt_cache_key"?: option("promptCacheKey").cloned(),
            "prompt_cache_options"?: option("promptCacheOptions").cloned(),
            "prompt_cache_retention"?: option("promptCacheRetention").cloned(),
            "safety_identifier"?: option("safetyIdentifier").cloned(),
            "top_logprobs"?: top_logprobs,
            "truncation"?: option("truncation").cloned(),
            "context_management"?: context_management,
            "reasoning"?: reasoning,
            "tools"?: tools,
            "tool_choice"?: tool_choice,
            "stream": true,
        };
        Ok(Args { body, web_search_tool_name, store: store.and_then(|s| s.as_bool()) == Some(true), names })
    }

    /// `doStream`.
    async fn stream(&self, options: CallOptions) -> Result<ModelStream, AiError> {
        let args = self.get_args(&options)?;
        let url = self.url.clone()?;
        let response = post_json(&self.fetch, &url, self.headers.clone(), &args.body, &options.cancel, ERROR_SHAPE).await?;
        // `createEventSourceResponseHandler`: `fetch` gives these two successes a null body.
        if matches!(response.status, 204 | 205) {
            return Err(AiError::other("Empty response body"));
        }
        let response_headers = response.headers.to_vec();
        let mut decoder: Box<dyn FrameDecoder> = Box::<SseJsonDecoder>::default();
        let (body, buffered) = throw_if_stream_error_before_output(response.body, decoder.as_mut(), &url, &response_headers).await?;
        let function_tools = options
            .tools
            .as_ref()
            .map_or(&[][..], Vec::as_slice)
            .iter()
            .filter(|tool| tool.str_at("type") == Some("function"))
            .filter_map(|tool| tool.str_at("name"))
            .map(str::to_owned)
            .collect();
        let logprobs_requested = options
            .provider_options
            .as_ref()
            .and_then(|o| o.path(&[self.provider_options_name, "logprobs"]))
            .is_some_and(|value| value.as_bool() != Some(false) && !value.is_null() && value.as_f64() != Some(0.0));
        let transform = StreamState {
            name: self.provider_options_name,
            url,
            response_headers,
            names: args.names,
            web_search_tool_name: args.web_search_tool_name,
            store: args.store,
            logprobs_requested,
            function_tools,
            approval_ids_from_prompt: approval_ids_from_prompt(&options.prompt),
            approval_ids_from_stream: HashMap::new(),
            finish_reason: FinishReason::other(),
            usage: None,
            logprobs: Vec::new(),
            response_id: None,
            ongoing_tool_calls: BTreeMap::new(),
            annotations: Vec::new(),
            held_text_ends: Vec::new(),
            active_message_phase: None,
            has_function_call: false,
            active_reasoning: HashMap::new(),
            active_output_item_ids: HashMap::new(),
            service_tier: None,
            reasoning_context: None,
            hosted_tool_search_call_ids: VecDeque::new(),
            encountered_stream_error: false,
            buffered,
        };
        Ok(ModelStream::new(body, decoder, Box::new(transform)))
    }
}

impl LanguageModel for ResponsesModel {
    fn provider(&self) -> &str {
        self.provider
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

/// `openaiFailedResponseHandler`. The schema is loose on purpose, for OpenAI-compatible providers.
const ERROR_SHAPE: ErrorShape = ErrorShape {
    message: |body| {
        let error = body.get("error").filter(|e| e.is_obj())?;
        let type_ok = error.non_null("type").is_none_or(Json::is_str);
        let code_ok = error.non_null("code").is_none_or(|code| code.is_str() || code.is_num());
        if !type_ok || !code_ok {
            return None;
        }
        error.str_at("message").map(str::to_owned)
    },
    is_retryable: None,
};

// ------------------------------------------------------------------ Stream errors (`openai-stream-error.ts`)

/// `getHttpStatusCode`.
fn http_status_code(value: Option<&Json>) -> Option<u16> {
    let number = match value? {
        Json::Str(text) if text.len() == 3 && text.bytes().all(|b| b.is_ascii_digit()) => text.parse::<f64>().ok()?,
        Json::Num(number) => number.as_f64(),
        _ => return None,
    };
    (number.fract() == 0.0 && (400.0..=599.0).contains(&number)).then_some(number as u16)
}

/// `getStatusCode`.
fn stream_status_code(code: Option<&Json>, kind: Option<&str>) -> u16 {
    if let Some(status) = http_status_code(code) {
        return status;
    }
    let code_text = match code {
        Some(Json::Str(text)) => Some(text.clone()),
        Some(number @ Json::Num(_)) => Some(number.to_text()),
        _ => None,
    };
    let discriminator = [code_text.as_deref(), kind].into_iter().flatten().collect::<Vec<_>>().join(" ").to_lowercase();
    let has = |term: &str| discriminator.contains(term);
    if has("insufficient_quota") || has("rate_limit") {
        429
    } else if has("authentication") {
        401
    } else if has("permission") {
        403
    } else if has("not_found") {
        404
    } else if has("invalid") || has("bad_request") || has("context_length") {
        400
    } else if has("overload") {
        503
    } else if has("timeout") {
        504
    } else {
        500
    }
}

/// `createOpenAIProviderStreamError`: `{ message, type, code, statusCode, isRetryable, data }`. `None` for a frame
/// that holds no error.
fn provider_stream_error(frame: &Json) -> Option<StreamProviderError> {
    let (message, code, kind): (&str, Option<&Json>, Option<&str>) = if frame.str_at("type") == Some("response.failed") {
        let error = frame.path(&["response", "error"]).filter(|e| e.is_obj())?;
        (error.str_at("message")?, error.get("code"), Some("response.failed"))
    } else {
        let nested = frame.get("error").filter(|e| e.is_obj());
        let error = nested.unwrap_or(frame);
        let message = error.str_at("message")?;
        let qualifies = nested.is_some() || error.str_at("type").is_some() || error.get("code").is_some() || error.get("param").is_some();
        if !qualifies {
            return None;
        }
        (message, error.get("code"), error.str_at("type"))
    };
    let code = code.filter(|c| c.is_str() || c.is_num());
    let status = stream_status_code(code, kind);
    let retryable = code.and_then(Json::as_str) != Some("insufficient_quota") && kind != Some("insufficient_quota") && ApiCallError::default_retryable(Some(status));
    Some(StreamProviderError {
        message: message.to_owned(),
        type_: kind.map(str::to_owned),
        code: code.cloned(),
        status: Some(status),
        is_retryable: retryable,
        data: Some(frame.clone()),
    })
}

/// `getError` in `doStream`: an `error` chunk, or a `response.failed` that names one.
fn is_error_frame(chunk: &Json) -> bool {
    match chunk.str_at("type") {
        Some("error") => true,
        Some("response.failed") => chunk.path(&["response", "error"]).is_some_and(|e| !e.is_null()),
        _ => false,
    }
}

/// `createOpenAIStreamError`: what an error before any output becomes, a failed call with the response's headers.
fn stream_error_before_output(frame: &Json, url: &str, response_headers: &[(String, String)]) -> AiError {
    let stream_error = provider_stream_error(frame);
    let status = stream_error.as_ref().and_then(|e| e.status).unwrap_or(500);
    AiError::api(ApiCallError {
        message: stream_error.as_ref().map_or_else(|| "OpenAI stream failed before any output was generated".to_owned(), |e| e.message.clone()),
        url: url.to_owned(),
        status: Some(status),
        response_headers: response_headers.to_vec(),
        response_body: Some(frame.to_text()),
        is_retryable: stream_error.as_ref().map_or_else(|| ApiCallError::default_retryable(Some(status)), |e| e.is_retryable),
        data: Some(frame.clone()),
    })
}

/// `isResponseOutputChunk`.
fn is_output_chunk(chunk: &Json) -> bool {
    !matches!(chunk.str_at("type"), Some("response.created" | "response.in_progress" | "response.failed" | "error"))
}

type PendingRead = BoxFuture<'static, (Body, Result<Option<Bytes>, crate::error::AiError>)>;

fn read_next(mut body: Body) -> PendingRead {
    Box::pin(async move {
        let result = body.next_chunk().await;
        (body, result)
    })
}

/// The body after the early-error peek. A read the grace period gave up on is still in flight: dropping it could lose
/// bytes, so the first `next_chunk` picks it up.
struct ResumedBody {
    pending: Option<PendingRead>,
    body: Body,
}

impl ChunkMap for ResumedBody {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Bytes>, AiError>> {
        Box::pin(async move {
            if let Some(read) = &mut self.pending {
                let (body, result) = read.await;
                self.pending = None;
                self.body = body;
                return result;
            }
            self.body.next_chunk().await
        })
    }
}

/// How long `throwIfOpenAIStreamErrorBeforeOutput` keeps peeking once `response.in_progress` proved the request was
/// accepted (`acceptedGraceMs`): long enough for an `insufficient_quota` flushed with it, without holding a healthy
/// stream back until its first token.
const ACCEPTED_GRACE: Duration = Duration::from_millis(50);

/// `throwIfOpenAIStreamErrorBeforeOutput`: reads until the stream has shown what it is. An error frame up to there is a
/// failed call rather than a stream error. Returns the body to go on with and the frames read so far, which the
/// transform then sees as if nothing had looked at them.
async fn throw_if_stream_error_before_output(
    body: Body,
    decoder: &mut dyn FrameDecoder,
    url: &str,
    response_headers: &[(String, String)],
) -> Result<(Body, Vec<Result<Json, AiError>>), AiError> {
    let mut frames: Vec<Result<Json, AiError>> = Vec::new();
    let mut inspected = 0;
    let mut accepted = false;
    let mut read = read_next(body);
    let resume = |read: PendingRead| Body::mapped(Box::new(ResumedBody { pending: Some(read), body: Body::empty() }));
    loop {
        while inspected < frames.len() {
            let frame = &frames[inspected];
            inspected += 1;
            let Ok(raw) = frame else { return Ok((resume(read), frames)) };
            match parse_chunk(raw) {
                Parsed::Failed => return Ok((resume(read), frames)),
                Parsed::Unknown => {}
                Parsed::Chunk(chunk) => {
                    if is_error_frame(&chunk) {
                        return Err(stream_error_before_output(&chunk, url, response_headers));
                    }
                    if is_output_chunk(&chunk) {
                        return Ok((resume(read), frames));
                    }
                    if chunk.str_at("type") == Some("response.in_progress") {
                        accepted = true;
                    }
                }
            }
        }
        let (body, result) = if accepted {
            match tokio::time::timeout(ACCEPTED_GRACE, &mut read).await {
                Ok(done) => done,
                Err(_) => return Ok((resume(read), frames)),
            }
        } else {
            (&mut read).await
        };
        match result? {
            Some(chunk) => {
                decoder.feed(&chunk, &mut frames);
                read = read_next(body);
            }
            None => return Ok((body, frames)),
        }
    }
}

// ------------------------------------------------------------------ The stream's transform

/// `isOpenAIChatCompletionChunk`.
fn is_chat_completion_chunk(value: &Json) -> bool {
    value.get("choices").is_some_and(Json::is_arr) && !value.get("type").is_some_and(Json::is_str)
}

/// `escapeJSONDelta`: the delta is embedded in a JSON string.
fn escape_json_delta(delta: &str) -> String {
    let quoted = Json::from(delta).to_text();
    quoted[1..quoted.len() - 1].to_owned()
}

/// `mapOpenAIResponseFinishReason`.
fn map_finish_reason(reason: Option<&str>, has_function_call: bool) -> &'static str {
    match reason {
        None => {
            if has_function_call {
                "tool-calls"
            } else {
                "stop"
            }
        }
        Some("max_output_tokens") => "length",
        Some("content_filter") => "content-filter",
        Some(_) => {
            if has_function_call {
                "tool-calls"
            } else {
                "other"
            }
        }
    }
}

/// `convertOpenAIResponsesUsage`.
fn convert_usage(usage: Option<&Json>) -> ModelUsage {
    let Some(usage) = usage.filter(|u| !u.is_null()) else { return ModelUsage::default() };
    let input = usage.get("input_tokens").and_then(Json::as_u64);
    let output = usage.get("output_tokens").and_then(Json::as_u64);
    let cached = usage.path(&["input_tokens_details", "cached_tokens"]).and_then(Json::as_u64).unwrap_or(0);
    let cache_write = usage.path(&["input_tokens_details", "cache_write_tokens"]).and_then(Json::as_u64);
    let reasoning = usage.path(&["output_tokens_details", "reasoning_tokens"]).and_then(Json::as_u64).unwrap_or(0);
    ModelUsage {
        input_total: input,
        input_no_cache: input.map(|total| total.saturating_sub(cached + cache_write.unwrap_or(0))),
        input_cache_read: Some(cached),
        input_cache_write: cache_write,
        output_total: output,
        output_text: output.map(|total| total.saturating_sub(reasoning)),
        output_reasoning: Some(reasoning),
        raw: Some(usage.clone()),
    }
}

/// `mapWebSearchOutput`.
fn map_web_search_output(action: Option<&Json>) -> Json {
    let Some(action) = action.filter(|a| !a.is_null()) else { return Json::obj() };
    match action.str_at("type") {
        Some("search") => jobj! {
            "action": jobj! {
                "type": "search",
                "query"?: action.non_null("query").cloned(),
                "queries"?: action.non_null("queries").cloned(),
            },
            "sources"?: action.non_null("sources").cloned(),
        },
        Some("open_page") => jobj! { "action": jobj! { "type": "openPage", "url"?: action.get("url").cloned() } },
        Some("find_in_page") => jobj! {
            "action": jobj! { "type": "findInPage", "url"?: action.get("url").cloned(), "pattern"?: action.get("pattern").cloned() },
        },
        _ => Json::obj(),
    }
}

/// `mapComputerAction`.
fn map_computer_action(action: &Json) -> Json {
    let field = |key: &str| action.get(key).cloned().unwrap_or_default();
    let keys = || action.non_null("keys").cloned();
    match action.str_at("type") {
        Some("click") => jobj! { "type": "click", "button": field("button"), "x": field("x"), "y": field("y"), "keys"?: keys() },
        Some("double_click") => jobj! { "type": "double_click", "x": field("x"), "y": field("y"), "keys"?: keys() },
        Some("drag") => jobj! { "type": "drag", "path": field("path"), "keys"?: keys() },
        Some("move") => jobj! { "type": "move", "x": field("x"), "y": field("y"), "keys"?: keys() },
        Some("scroll") => jobj! {
            "type": "scroll",
            "x": field("x"),
            "y": field("y"),
            "scrollX": field("scroll_x"),
            "scrollY": field("scroll_y"),
            "keys"?: keys(),
        },
        _ => action.clone(),
    }
}

/// `mapComputerCallInput`.
fn map_computer_call_input(item: &Json) -> Json {
    let actions: Vec<Json> = match (item.non_null("actions").and_then(Json::as_arr), item.non_null("action")) {
        (Some(actions), _) => actions.iter().map(map_computer_action).collect(),
        (None, Some(action)) => vec![map_computer_action(action)],
        (None, None) => Vec::new(),
    };
    let checks: Vec<Json> = item
        .non_null("pending_safety_checks")
        .and_then(Json::as_arr)
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .map(|check| {
            jobj! {
                "id": check.get("id").cloned().unwrap_or_default(),
                "code"?: check.non_null("code").cloned(),
                "message"?: check.non_null("message").cloned(),
            }
        })
        .collect();
    jobj! { "actions": actions, "pendingSafetyChecks": checks, "status": item.get("status").cloned().unwrap_or_default() }
}

struct ApplyPatchState {
    has_diff: bool,
    end_emitted: bool,
}

/// `ongoingToolCalls[output_index]`.
struct OngoingToolCall {
    tool_name: String,
    tool_call_id: String,
    code_interpreter_container: Option<String>,
    apply_patch: Option<ApplyPatchState>,
    suppress_input_streaming: bool,
    buffered_input_deltas: Vec<String>,
}

impl OngoingToolCall {
    fn new(tool_name: impl Into<String>, tool_call_id: impl Into<String>) -> Self {
        Self {
            tool_name: tool_name.into(),
            tool_call_id: tool_call_id.into(),
            code_interpreter_container: None,
            apply_patch: None,
            suppress_input_streaming: false,
            buffered_input_deltas: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum SummaryPart {
    Active,
    CanConclude,
    Concluded,
}

/// `activeReasoning[itemId]`.
struct ActiveReasoning {
    encrypted_content: Option<String>,
    /// By summary index: `Object.keys` of integer-like keys is ascending.
    summary_parts: BTreeMap<i64, SummaryPart>,
}

/// A `text-end` whose `annotations` is still the transform's one live array.
struct HeldTextEnd {
    id: String,
    phase: Option<Json>,
}

struct StreamState {
    name: &'static str,
    url: String,
    response_headers: Vec<(String, String)>,
    names: ToolNames,
    web_search_tool_name: Option<String>,
    store: bool,
    logprobs_requested: bool,
    function_tools: Vec<String>,
    approval_ids_from_prompt: HashMap<String, String>,
    approval_ids_from_stream: HashMap<String, String>,
    finish_reason: FinishReason,
    usage: Option<Json>,
    logprobs: Vec<Json>,
    response_id: Option<String>,
    /// `Object.values` of integer keys runs in ascending order, which is the order `flush` needs.
    ongoing_tool_calls: BTreeMap<i64, OngoingToolCall>,
    annotations: Vec<Json>,
    held_text_ends: Vec<HeldTextEnd>,
    active_message_phase: Option<Json>,
    has_function_call: bool,
    active_reasoning: HashMap<String, ActiveReasoning>,
    /// OpenAI-compatible providers can rotate opaque item ids between events; `output_index` stays stable.
    active_output_item_ids: HashMap<i64, String>,
    service_tier: Option<String>,
    reasoning_context: Option<Json>,
    hosted_tool_search_call_ids: VecDeque<String>,
    encountered_stream_error: bool,
    /// Frames the early-error peek already read, for `start` to run through `chunk`.
    buffered: Vec<Result<Json, AiError>>,
}

fn output_index(value: &Json) -> Option<i64> {
    value.non_null("output_index").and_then(Json::as_f64).map(|index| index as i64)
}

fn text_of(value: &Json, key: &str) -> String {
    value.str_at(key).unwrap_or("").to_owned()
}

fn is_undeclared_parallel(tool_name: &str, function_tools: &[String]) -> bool {
    tool_name == PARALLEL_TOOL_NAME && !function_tools.iter().any(|name| name == PARALLEL_TOOL_NAME)
}

const PARALLEL_TOOL_NAME: &str = "parallel";
const RECIPIENT_NAME_PREFIX: &str = "functions.";

/// One call out of the internal `parallel` wrapper.
struct ExpandedCall {
    tool_call_id: String,
    tool_name: String,
    input: String,
    metadata: Json,
}

/// `expandParallelToolCall`: the wrapper counts only when every nested recipient is a declared client function.
fn expand_parallel_tool_call(call_id: &str, arguments: &str, item_id: Option<&Json>, function_tools: &[String]) -> Option<Vec<ExpandedCall>> {
    let parsed = secure_json_parse(arguments).ok()?;
    let tool_uses = parsed.get("tool_uses")?.as_arr()?;
    if tool_uses.is_empty() {
        return None;
    }
    let mut expanded = Vec::with_capacity(tool_uses.len());
    for (index, tool_use) in tool_uses.iter().enumerate() {
        let tool_name = tool_use.str_at("recipient_name")?.strip_prefix(RECIPIENT_NAME_PREFIX)?;
        // `isJSONObject` takes an array as well.
        let parameters = tool_use.get("parameters").filter(|p| p.is_obj() || p.is_arr())?;
        if tool_name.is_empty() || !function_tools.iter().any(|name| name == tool_name) {
            return None;
        }
        expanded.push(ExpandedCall {
            tool_call_id: format!("{call_id}_{index}"),
            tool_name: tool_name.to_owned(),
            input: parameters.to_text(),
            metadata: jobj! {
                "parallelToolCall": jobj! {
                    "itemId"?: item_id.cloned(),
                    "toolCallId": call_id,
                    "toolName": PARALLEL_TOOL_NAME,
                    "input": arguments,
                    "index": index,
                    "count": tool_uses.len(),
                },
            },
        });
    }
    Some(expanded)
}

impl StreamState {
    /// `{ [providerOptionsName]: inner }`.
    fn metadata(&self, inner: Json) -> Option<Json> {
        let mut outer = Obj::new();
        outer.insert(self.name, inner);
        Some(Json::Obj(outer))
    }

    /// `{ [providerOptionsName]: { itemId } }`.
    fn item_metadata(&self, item_id: Json) -> Option<Json> {
        self.metadata(jobj! { "itemId": item_id })
    }

    /// `resolveOutputItemId`.
    fn resolve_output_item_id(&self, value: &Json) -> String {
        let item_id = value.str_at("item_id").unwrap_or("");
        output_index(value).and_then(|index| self.active_output_item_ids.get(&index)).map_or_else(|| item_id.to_owned(), String::clone)
    }

    fn custom_name(&self, provider: &str) -> String {
        self.names.custom(provider).to_owned()
    }

    fn web_search_name(&self) -> String {
        self.custom_name(self.web_search_tool_name.as_deref().unwrap_or("web_search"))
    }

    fn fail(&mut self, error: AiError, out: &mut Vec<StreamPart>) {
        self.encountered_stream_error = true;
        self.finish_reason = FinishReason::new("error", None);
        out.push(StreamPart::Error(error));
    }

    fn on_item_added(&mut self, value: &Json, out: &mut Vec<StreamPart>) {
        let Some(item) = value.get("item") else { return };
        let index = output_index(value).unwrap_or(0);
        let id = text_of(item, "id");
        match item.str_at("type").unwrap_or("") {
            "function_call" => {
                let tool_name = text_of(item, "name");
                let call_id = text_of(item, "call_id");
                let suppress = is_undeclared_parallel(&tool_name, &self.function_tools);
                let mut call = OngoingToolCall::new(tool_name.clone(), call_id.clone());
                call.suppress_input_streaming = suppress;
                self.ongoing_tool_calls.insert(index, call);
                if !suppress {
                    out.push(StreamPart::ToolInputStart { id: call_id, tool_name, provider_executed: None, dynamic: None });
                }
            }
            "custom_tool_call" => {
                let tool_name = self.custom_name(item.str_at("name").unwrap_or(""));
                let call_id = text_of(item, "call_id");
                self.ongoing_tool_calls.insert(index, OngoingToolCall::new(tool_name.clone(), call_id.clone()));
                out.push(StreamPart::ToolInputStart { id: call_id, tool_name, provider_executed: None, dynamic: None });
            }
            "web_search_call" => {
                let tool_name = self.web_search_name();
                self.ongoing_tool_calls.insert(index, OngoingToolCall::new(tool_name.clone(), id.clone()));
                out.push(StreamPart::ToolInputStart { id: id.clone(), tool_name: tool_name.clone(), provider_executed: Some(true), dynamic: None });
                out.push(StreamPart::ToolInputEnd { id: id.clone() });
                out.push(StreamPart::ToolCall {
                    tool_call_id: id,
                    tool_name,
                    input: "{}".to_owned(),
                    provider_executed: Some(true),
                    dynamic: None,
                    provider_metadata: None,
                });
            }
            "computer_call" => {
                let tool_call_id = item.non_null("call_id").and_then(Json::as_str).map_or_else(|| id.clone(), str::to_owned);
                let tool_name = self.custom_name("computer");
                self.ongoing_tool_calls.insert(index, OngoingToolCall::new(tool_name.clone(), tool_call_id.clone()));
                out.push(StreamPart::ToolInputStart { id: tool_call_id, tool_name, provider_executed: None, dynamic: None });
            }
            "code_interpreter_call" => {
                let tool_name = self.custom_name("code_interpreter");
                let container = text_of(item, "container_id");
                let mut call = OngoingToolCall::new(tool_name.clone(), id.clone());
                call.code_interpreter_container = Some(container.clone());
                self.ongoing_tool_calls.insert(index, call);
                out.push(StreamPart::ToolInputStart { id: id.clone(), tool_name, provider_executed: Some(true), dynamic: None });
                out.push(StreamPart::ToolInputDelta { id, delta: format!("{{\"containerId\":\"{container}\",\"code\":\"") });
            }
            "file_search_call" => out.push(StreamPart::ToolCall {
                tool_call_id: id,
                tool_name: self.custom_name("file_search"),
                input: "{}".to_owned(),
                provider_executed: Some(true),
                dynamic: None,
                provider_metadata: None,
            }),
            "image_generation_call" => out.push(StreamPart::ToolCall {
                tool_call_id: id,
                tool_name: self.custom_name("image_generation"),
                input: "{}".to_owned(),
                provider_executed: Some(true),
                dynamic: None,
                provider_metadata: None,
            }),
            "tool_search_call" => {
                let tool_name = self.custom_name("tool_search");
                self.ongoing_tool_calls.insert(index, OngoingToolCall::new(tool_name.clone(), id.clone()));
                if item.str_at("execution") == Some("server") {
                    out.push(StreamPart::ToolInputStart { id, tool_name, provider_executed: Some(true), dynamic: None });
                }
            }
            "apply_patch_call" => {
                let call_id = text_of(item, "call_id");
                let operation = item.get("operation").cloned().unwrap_or_default();
                let kind = operation.str_at("type").unwrap_or("").to_owned();
                let is_delete = kind == "delete_file";
                let tool_name = self.custom_name("apply_patch");
                let mut call = OngoingToolCall::new(tool_name.clone(), call_id.clone());
                // A delete has no diff.
                call.apply_patch = Some(ApplyPatchState { has_diff: is_delete, end_emitted: is_delete });
                self.ongoing_tool_calls.insert(index, call);
                out.push(StreamPart::ToolInputStart { id: call_id.clone(), tool_name, provider_executed: None, dynamic: None });
                if is_delete {
                    let input = jobj! { "callId": call_id.as_str(), "operation": operation }.to_text();
                    out.push(StreamPart::ToolInputDelta { id: call_id.clone(), delta: input });
                    out.push(StreamPart::ToolInputEnd { id: call_id });
                } else {
                    let delta = format!(
                        "{{\"callId\":\"{}\",\"operation\":{{\"type\":\"{}\",\"path\":\"{}\",\"diff\":\"",
                        escape_json_delta(&call_id),
                        escape_json_delta(&kind),
                        escape_json_delta(operation.str_at("path").unwrap_or("")),
                    );
                    out.push(StreamPart::ToolInputDelta { id: call_id, delta });
                }
            }
            "shell_call" => {
                let tool_name = self.custom_name("shell");
                self.ongoing_tool_calls.insert(index, OngoingToolCall::new(tool_name, text_of(item, "call_id")));
            }
            "message" => {
                self.active_output_item_ids.insert(index, id.clone());
                self.annotations.clear();
                let phase = item.non_null("phase").cloned();
                self.active_message_phase = phase.clone();
                let metadata = self.metadata(jobj! { "itemId": id.as_str(), "phase"?: phase });
                out.push(StreamPart::TextStart { id, provider_metadata: metadata });
            }
            "reasoning" => {
                self.active_output_item_ids.insert(index, id.clone());
                let encrypted = item.non_null("encrypted_content").and_then(Json::as_str).map(str::to_owned);
                let metadata = self.metadata(jobj! {
                    "itemId": id.as_str(),
                    "reasoningEncryptedContent": item.non_null("encrypted_content").cloned().unwrap_or_default(),
                });
                self.active_reasoning.insert(id.clone(), ActiveReasoning { encrypted_content: encrypted, summary_parts: BTreeMap::from([(0, SummaryPart::Active)]) });
                out.push(StreamPart::ReasoningStart { id: format!("{id}:0"), provider_metadata: metadata });
            }
            // `tool_search_output`, `shell_call_output` and the MCP items are handled when they are done, so MCP ids
            // can be aliased to an approval first.
            _ => {}
        }
    }

    fn on_function_call_done(&mut self, item: &Json, index: i64, out: &mut Vec<StreamPart>) {
        let ongoing = self.ongoing_tool_calls.remove(&index);
        self.has_function_call = true;
        let tool_name = text_of(item, "name");
        let call_id = text_of(item, "call_id");
        let arguments = text_of(item, "arguments");
        let suppress = ongoing.as_ref().map_or_else(|| is_undeclared_parallel(&tool_name, &self.function_tools), |call| call.suppress_input_streaming);
        if suppress {
            if let Some(expanded) = expand_parallel_tool_call(&call_id, &arguments, item.get("id"), &self.function_tools) {
                for call in expanded {
                    out.push(StreamPart::ToolInputStart { id: call.tool_call_id.clone(), tool_name: call.tool_name.clone(), provider_executed: None, dynamic: None });
                    out.push(StreamPart::ToolInputDelta { id: call.tool_call_id.clone(), delta: call.input.clone() });
                    out.push(StreamPart::ToolInputEnd { id: call.tool_call_id.clone() });
                    let metadata = self.metadata(call.metadata);
                    out.push(StreamPart::ToolCall {
                        tool_call_id: call.tool_call_id,
                        tool_name: call.tool_name,
                        input: call.input,
                        provider_executed: None,
                        dynamic: None,
                        provider_metadata: metadata,
                    });
                }
                return;
            }
            out.push(StreamPart::ToolInputStart { id: call_id.clone(), tool_name: tool_name.clone(), provider_executed: None, dynamic: None });
            let deltas = ongoing.map(|call| call.buffered_input_deltas).unwrap_or_default();
            if !deltas.is_empty() {
                for delta in deltas {
                    out.push(StreamPart::ToolInputDelta { id: call_id.clone(), delta });
                }
            } else if !arguments.is_empty() {
                out.push(StreamPart::ToolInputDelta { id: call_id.clone(), delta: arguments.clone() });
            }
        }
        out.push(StreamPart::ToolInputEnd { id: call_id.clone() });
        let caller = item.non_null("caller").map(|caller| {
            if caller.str_at("type") == Some("program") {
                jobj! { "type": "program", "callerId": caller.get("caller_id").cloned().unwrap_or_default() }
            } else {
                caller.clone()
            }
        });
        let metadata = self.metadata(jobj! {
            "itemId": item.get("id").cloned().unwrap_or_default(),
            "namespace"?: item.non_null("namespace").cloned(),
            "caller"?: caller,
        });
        out.push(StreamPart::ToolCall { tool_call_id: call_id, tool_name, input: arguments, provider_executed: None, dynamic: None, provider_metadata: metadata });
    }

    fn on_item_done(&mut self, value: &Json, out: &mut Vec<StreamPart>) {
        let Some(item) = value.get("item") else { return };
        let index = output_index(value).unwrap_or(0);
        let id = item.get("id").cloned().unwrap_or_default();
        let id_text = text_of(item, "id");
        match item.str_at("type").unwrap_or("") {
            "message" => {
                let item_id = output_index(value).and_then(|at| self.active_output_item_ids.get(&at).cloned()).unwrap_or(id_text);
                let phase = item.non_null("phase").cloned().or_else(|| self.active_message_phase.take());
                self.active_message_phase = None;
                // Upstream's metadata holds the live `ongoingAnnotations` array, which the next message empties and
                // refills in place, so what a finished part says is only settled when the step ends. That is what gets
                // persisted, so a part with annotations waits for `flush`. Nothing reads a `text-end` before then.
                if self.annotations.is_empty() {
                    let metadata = self.metadata(jobj! { "itemId": item_id.as_str(), "phase"?: phase });
                    out.push(StreamPart::TextEnd { id: item_id, provider_metadata: metadata });
                } else {
                    self.held_text_ends.push(HeldTextEnd { id: item_id, phase });
                }
                self.active_output_item_ids.remove(&index);
            }
            "function_call" => self.on_function_call_done(item, index, out),
            "program" => out.push(StreamPart::ToolCall {
                tool_call_id: text_of(item, "call_id"),
                tool_name: self.custom_name("programmatic_tool_calling"),
                input: jobj! { "code": item.get("code").cloned().unwrap_or_default(), "fingerprint": item.get("fingerprint").cloned().unwrap_or_default() }.to_text(),
                provider_executed: Some(true),
                dynamic: None,
                provider_metadata: self.item_metadata(id),
            }),
            "program_output" => out.push(StreamPart::ToolResult {
                tool_call_id: text_of(item, "call_id"),
                tool_name: self.custom_name("programmatic_tool_calling"),
                result: jobj! { "result": item.get("result").cloned().unwrap_or_default(), "status": item.get("status").cloned().unwrap_or_default() },
                is_error: None,
                dynamic: None,
                provider_metadata: self.item_metadata(id),
            }),
            "custom_tool_call" => {
                self.ongoing_tool_calls.remove(&index);
                self.has_function_call = true;
                let call_id = text_of(item, "call_id");
                out.push(StreamPart::ToolInputEnd { id: call_id.clone() });
                out.push(StreamPart::ToolCall {
                    tool_call_id: call_id,
                    tool_name: self.custom_name(item.str_at("name").unwrap_or("")),
                    input: item.get("input").cloned().unwrap_or_default().to_text(),
                    provider_executed: None,
                    dynamic: None,
                    provider_metadata: self.item_metadata(id),
                });
            }
            "web_search_call" => {
                self.ongoing_tool_calls.remove(&index);
                out.push(StreamPart::ToolResult {
                    tool_call_id: id_text,
                    tool_name: self.web_search_name(),
                    result: map_web_search_output(item.get("action")),
                    is_error: None,
                    dynamic: None,
                    provider_metadata: None,
                });
            }
            "computer_call" => self.on_computer_call_done(item, index, id, id_text, out),
            "file_search_call" => {
                self.ongoing_tool_calls.remove(&index);
                let results = match item.non_null("results").and_then(Json::as_arr) {
                    Some(results) => Json::Arr(
                        results
                            .iter()
                            .map(|result| {
                                jobj! {
                                    "attributes": result.get("attributes").cloned().unwrap_or_default(),
                                    "fileId": result.get("file_id").cloned().unwrap_or_default(),
                                    "filename": result.get("filename").cloned().unwrap_or_default(),
                                    "score": result.get("score").cloned().unwrap_or_default(),
                                    "text": result.get("text").cloned().unwrap_or_default(),
                                }
                            })
                            .collect(),
                    ),
                    None => Json::Null,
                };
                out.push(StreamPart::ToolResult {
                    tool_call_id: id_text,
                    tool_name: self.custom_name("file_search"),
                    result: jobj! { "queries": item.get("queries").cloned().unwrap_or_default(), "results": results },
                    is_error: None,
                    dynamic: None,
                    provider_metadata: None,
                });
            }
            "code_interpreter_call" => {
                self.ongoing_tool_calls.remove(&index);
                out.push(StreamPart::ToolResult {
                    tool_call_id: id_text,
                    tool_name: self.custom_name("code_interpreter"),
                    result: jobj! { "outputs": item.get("outputs").cloned().unwrap_or_default() },
                    is_error: None,
                    dynamic: None,
                    provider_metadata: None,
                });
            }
            "image_generation_call" => out.push(StreamPart::ToolResult {
                tool_call_id: id_text,
                tool_name: self.custom_name("image_generation"),
                result: jobj! { "result": item.get("result").cloned().unwrap_or_default() },
                is_error: None,
                dynamic: None,
                provider_metadata: None,
            }),
            "tool_search_call" => {
                if let Some(call) = self.ongoing_tool_calls.remove(&index) {
                    let is_hosted = item.str_at("execution") == Some("server");
                    let tool_call_id = if is_hosted { call.tool_call_id } else { item.non_null("call_id").and_then(Json::as_str).map_or_else(|| id_text.clone(), str::to_owned) };
                    if is_hosted {
                        self.hosted_tool_search_call_ids.push_back(tool_call_id.clone());
                    } else {
                        out.push(StreamPart::ToolInputStart { id: tool_call_id.clone(), tool_name: call.tool_name.clone(), provider_executed: None, dynamic: None });
                    }
                    out.push(StreamPart::ToolInputEnd { id: tool_call_id.clone() });
                    let input = jobj! {
                        "arguments"?: item.get("arguments").cloned(),
                        "call_id": if is_hosted { Json::Null } else { Json::from(tool_call_id.as_str()) },
                    };
                    out.push(StreamPart::ToolCall {
                        tool_call_id,
                        tool_name: call.tool_name,
                        input: input.to_text(),
                        provider_executed: is_hosted.then_some(true),
                        dynamic: None,
                        provider_metadata: self.item_metadata(id),
                    });
                }
            }
            "tool_search_output" => {
                let tool_call_id = item
                    .non_null("call_id")
                    .and_then(Json::as_str)
                    .map(str::to_owned)
                    .or_else(|| self.hosted_tool_search_call_ids.pop_front())
                    .unwrap_or(id_text);
                out.push(StreamPart::ToolResult {
                    tool_call_id,
                    tool_name: self.custom_name("tool_search"),
                    result: jobj! { "tools": item.get("tools").cloned().unwrap_or_default() },
                    is_error: None,
                    dynamic: None,
                    provider_metadata: self.item_metadata(id),
                });
            }
            "mcp_call" => {
                self.ongoing_tool_calls.remove(&index);
                // Approved MCP calls are tracked under this crate's own ids, so results have to match those.
                let aliased = item
                    .non_null("approval_request_id")
                    .and_then(Json::as_str)
                    .and_then(|approval| self.approval_ids_from_stream.get(approval).or_else(|| self.approval_ids_from_prompt.get(approval)).cloned())
                    .unwrap_or(id_text);
                let tool_name = format!("mcp.{}", item.str_at("name").unwrap_or(""));
                out.push(StreamPart::ToolCall {
                    tool_call_id: aliased.clone(),
                    tool_name: tool_name.clone(),
                    input: text_of(item, "arguments"),
                    provider_executed: Some(true),
                    dynamic: Some(true),
                    provider_metadata: None,
                });
                out.push(StreamPart::ToolResult {
                    tool_call_id: aliased,
                    tool_name,
                    result: jobj! {
                        "type": "call",
                        "serverLabel": item.get("server_label").cloned().unwrap_or_default(),
                        "name": item.get("name").cloned().unwrap_or_default(),
                        "arguments": item.get("arguments").cloned().unwrap_or_default(),
                        "output"?: item.non_null("output").cloned(),
                        "error"?: item.non_null("error").cloned(),
                    },
                    is_error: None,
                    dynamic: None,
                    provider_metadata: self.item_metadata(id),
                });
            }
            "mcp_list_tools" => {
                self.ongoing_tool_calls.remove(&index);
            }
            "apply_patch_call" => self.on_apply_patch_done(item, index, id, out),
            "mcp_approval_request" => {
                self.ongoing_tool_calls.remove(&index);
                let dummy = generate_id();
                let approval = item.non_null("approval_request_id").and_then(Json::as_str).map_or(id_text, str::to_owned);
                self.approval_ids_from_stream.insert(approval.clone(), dummy.clone());
                out.push(StreamPart::ToolCall {
                    tool_call_id: dummy.clone(),
                    tool_name: format!("mcp.{}", item.str_at("name").unwrap_or("")),
                    input: text_of(item, "arguments"),
                    provider_executed: Some(true),
                    dynamic: Some(true),
                    provider_metadata: None,
                });
                out.push(StreamPart::ToolApprovalRequest { approval_id: approval, tool_call_id: dummy });
            }
            "local_shell_call" => {
                self.ongoing_tool_calls.remove(&index);
                let action = item.get("action");
                let field = |key: &str| action.and_then(|a| a.get(key)).cloned();
                let input = jobj! {
                    "action": jobj! {
                        "type": "exec",
                        "command"?: field("command"),
                        "timeoutMs"?: field("timeout_ms"),
                        "user"?: field("user"),
                        "workingDirectory"?: field("working_directory"),
                        "env"?: field("env"),
                    },
                };
                out.push(StreamPart::ToolCall {
                    tool_call_id: text_of(item, "call_id"),
                    tool_name: self.custom_name("local_shell"),
                    input: input.to_text(),
                    provider_executed: None,
                    dynamic: None,
                    provider_metadata: self.item_metadata(id),
                });
            }
            "shell_call" => {
                self.ongoing_tool_calls.remove(&index);
                let commands = item.path(&["action", "commands"]).cloned().unwrap_or_default();
                out.push(StreamPart::ToolCall {
                    tool_call_id: text_of(item, "call_id"),
                    tool_name: self.custom_name("shell"),
                    input: jobj! { "action": jobj! { "commands": commands } }.to_text(),
                    provider_executed: None,
                    dynamic: None,
                    provider_metadata: self.item_metadata(id),
                });
            }
            "shell_call_output" => {
                let output: Vec<Json> = item
                    .get("output")
                    .and_then(Json::as_arr)
                    .map_or(&[][..], Vec::as_slice)
                    .iter()
                    .map(|entry| {
                        let outcome = if entry.path(&["outcome", "type"]).and_then(Json::as_str) == Some("exit") {
                            jobj! { "type": "exit", "exitCode": entry.path(&["outcome", "exit_code"]).cloned().unwrap_or_default() }
                        } else {
                            jobj! { "type": "timeout" }
                        };
                        jobj! {
                            "stdout": entry.get("stdout").cloned().unwrap_or_default(),
                            "stderr": entry.get("stderr").cloned().unwrap_or_default(),
                            "outcome": outcome,
                        }
                    })
                    .collect();
                out.push(StreamPart::ToolResult {
                    tool_call_id: text_of(item, "call_id"),
                    tool_name: self.custom_name("shell"),
                    result: jobj! { "output": output },
                    is_error: None,
                    dynamic: None,
                    provider_metadata: None,
                });
            }
            "reasoning" => {
                let item_id = output_index(value).and_then(|at| self.active_output_item_ids.get(&at).cloned()).unwrap_or(id_text);
                if let Some(active) = self.active_reasoning.remove(&item_id) {
                    // Concludes what is still open, now with the encrypted content the item only has when it is done.
                    let encrypted = item.non_null("encrypted_content").cloned().unwrap_or_default();
                    for (summary_index, status) in &active.summary_parts {
                        if matches!(status, SummaryPart::Active | SummaryPart::CanConclude) {
                            out.push(StreamPart::ReasoningEnd {
                                id: format!("{item_id}:{summary_index}"),
                                provider_metadata: self.metadata(jobj! { "itemId": item_id.as_str(), "reasoningEncryptedContent": encrypted.clone() }),
                            });
                        }
                    }
                }
                self.active_output_item_ids.remove(&index);
            }
            "compaction" => out.push(StreamPart::Custom {
                kind: "openai.compaction".to_owned(),
                provider_metadata: self.metadata(jobj! {
                    "type": "compaction",
                    "itemId": id,
                    "encryptedContent"?: item.get("encrypted_content").cloned(),
                }),
            }),
            _ => {}
        }
    }

    fn on_computer_call_done(&mut self, item: &Json, index: i64, id: Json, id_text: String, out: &mut Vec<StreamPart>) {
        self.ongoing_tool_calls.remove(&index);
        let Some(call_id) = item.non_null("call_id").and_then(Json::as_str).map(str::to_owned) else {
            out.push(StreamPart::ToolInputEnd { id: id_text.clone() });
            out.push(StreamPart::ToolCall {
                tool_call_id: id_text.clone(),
                tool_name: self.custom_name("computer_use"),
                input: String::new(),
                provider_executed: Some(true),
                dynamic: None,
                provider_metadata: None,
            });
            out.push(StreamPart::ToolResult {
                tool_call_id: id_text,
                tool_name: self.custom_name("computer_use"),
                result: jobj! { "type": "computer_use_tool_result", "status": item.get("status").cloned().unwrap_or_default() },
                is_error: None,
                dynamic: None,
                provider_metadata: None,
            });
            return;
        };
        self.has_function_call = true;
        let input = map_computer_call_input(item).to_text();
        out.push(StreamPart::ToolInputDelta { id: call_id.clone(), delta: input.clone() });
        out.push(StreamPart::ToolInputEnd { id: call_id.clone() });
        out.push(StreamPart::ToolCall {
            tool_call_id: call_id,
            tool_name: self.custom_name("computer"),
            input,
            provider_executed: None,
            dynamic: None,
            provider_metadata: self.item_metadata(id),
        });
    }

    fn on_apply_patch_done(&mut self, item: &Json, index: i64, id: Json, out: &mut Vec<StreamPart>) {
        let operation = item.get("operation").cloned().unwrap_or_default();
        if let Some(call) = self.ongoing_tool_calls.get_mut(&index) {
            let call_id = call.tool_call_id.clone();
            if let Some(patch) = call.apply_patch.as_mut().filter(|p| !p.end_emitted && operation.str_at("type") != Some("delete_file")) {
                if !patch.has_diff {
                    out.push(StreamPart::ToolInputDelta { id: call_id.clone(), delta: escape_json_delta(operation.str_at("diff").unwrap_or("")) });
                }
                out.push(StreamPart::ToolInputDelta { id: call_id.clone(), delta: "\"}}".to_owned() });
                out.push(StreamPart::ToolInputEnd { id: call_id.clone() });
                patch.end_emitted = true;
            }
            // The final tool call, with the complete diff, only when the item completed.
            if item.str_at("status") == Some("completed") {
                let input = jobj! { "callId": item.get("call_id").cloned().unwrap_or_default(), "operation": operation }.to_text();
                let tool_name = self.custom_name("apply_patch");
                out.push(StreamPart::ToolCall {
                    tool_call_id: call_id,
                    tool_name,
                    input,
                    provider_executed: None,
                    dynamic: None,
                    provider_metadata: self.item_metadata(id),
                });
            }
        }
        self.ongoing_tool_calls.remove(&index);
    }

    fn on_annotation(&mut self, value: &Json, out: &mut Vec<StreamPart>) {
        let Some(annotation) = value.get("annotation") else { return };
        self.annotations.push(annotation.clone());
        let field = |key: &str| annotation.get(key).cloned().unwrap_or_default();
        let kind = annotation.str_at("type").unwrap_or("");
        match kind {
            "url_citation" => out.push(StreamPart::Source(jobj! {
                "sourceType": "url",
                "id": generate_id(),
                "url": field("url"),
                "title": field("title"),
            })),
            "file_citation" => out.push(StreamPart::Source(jobj! {
                "sourceType": "document",
                "id": generate_id(),
                "mediaType": "text/plain",
                "title": field("filename"),
                "filename": field("filename"),
                "providerMetadata"?: self.metadata(jobj! { "type": kind, "fileId": field("file_id"), "index": field("index") }),
            })),
            "container_file_citation" => out.push(StreamPart::Source(jobj! {
                "sourceType": "document",
                "id": generate_id(),
                "mediaType": "text/plain",
                "title": field("filename"),
                "filename": field("filename"),
                "providerMetadata"?: self.metadata(jobj! { "type": kind, "fileId": field("file_id"), "containerId": field("container_id") }),
            })),
            "file_path" => out.push(StreamPart::Source(jobj! {
                "sourceType": "document",
                "id": generate_id(),
                "mediaType": "application/octet-stream",
                "title": field("file_id"),
                "filename": field("file_id"),
                "providerMetadata"?: self.metadata(jobj! { "type": kind, "fileId": field("file_id"), "index": field("index") }),
            })),
            _ => {}
        }
    }
}

impl ChunkTransform for StreamState {
    fn start(&mut self, out: &mut Vec<StreamPart>) {
        out.push(StreamPart::StreamStart { warnings: Vec::new() });
        for frame in std::mem::take(&mut self.buffered) {
            self.chunk(frame, out);
        }
    }

    fn chunk(&mut self, chunk: Result<Json, AiError>, out: &mut Vec<StreamPart>) {
        let raw = match chunk {
            Ok(raw) => raw,
            Err(error) => return self.fail(error, out),
        };
        let value = match parse_chunk(&raw) {
            Parsed::Chunk(value) => value,
            Parsed::Unknown => return,
            Parsed::Failed => {
                let error = if is_chat_completion_chunk(&raw) {
                    AiError::api(ApiCallError {
                        message: "Received a Chat Completions stream while using the OpenAI Responses API. The default OpenAI provider model uses the Responses API. If your custom baseURL targets a Chat Completions-compatible endpoint, use openai.chat('model-id') or createOpenAI(...).chat('model-id') instead. You can also use @ai-sdk/openai-compatible for OpenAI-compatible providers.".to_owned(),
                        url: self.url.clone(),
                        status: None,
                        response_headers: self.response_headers.clone(),
                        response_body: Some(raw.to_text()),
                        is_retryable: false,
                        data: Some(raw.clone()),
                    })
                } else {
                    AiError::validation(&raw, "Known response chunk failed schema validation")
                };
                return self.fail(error, out);
            }
        };
        let index = output_index(&value).unwrap_or(0);
        match value.str_at("type").unwrap_or("") {
            "response.output_item.added" => self.on_item_added(&value, out),
            "response.output_item.done" => self.on_item_done(&value, out),
            "response.function_call_arguments.delta" => {
                if let Some(call) = self.ongoing_tool_calls.get_mut(&index) {
                    let delta = text_of(&value, "delta");
                    if call.suppress_input_streaming {
                        call.buffered_input_deltas.push(delta);
                    } else {
                        out.push(StreamPart::ToolInputDelta { id: call.tool_call_id.clone(), delta });
                    }
                }
            }
            "response.custom_tool_call_input.delta" => {
                if let Some(call) = self.ongoing_tool_calls.get(&index) {
                    out.push(StreamPart::ToolInputDelta { id: call.tool_call_id.clone(), delta: text_of(&value, "delta") });
                }
            }
            "response.apply_patch_call_operation_diff.delta" => {
                if let Some(call) = self.ongoing_tool_calls.get_mut(&index) {
                    if let Some(patch) = call.apply_patch.as_mut() {
                        out.push(StreamPart::ToolInputDelta { id: call.tool_call_id.clone(), delta: escape_json_delta(value.str_at("delta").unwrap_or("")) });
                        patch.has_diff = true;
                    }
                }
            }
            "response.apply_patch_call_operation_diff.done" => {
                if let Some(call) = self.ongoing_tool_calls.get_mut(&index) {
                    let call_id = call.tool_call_id.clone();
                    if let Some(patch) = call.apply_patch.as_mut().filter(|p| !p.end_emitted) {
                        if !patch.has_diff {
                            out.push(StreamPart::ToolInputDelta { id: call_id.clone(), delta: escape_json_delta(value.str_at("diff").unwrap_or("")) });
                            patch.has_diff = true;
                        }
                        out.push(StreamPart::ToolInputDelta { id: call_id.clone(), delta: "\"}}".to_owned() });
                        out.push(StreamPart::ToolInputEnd { id: call_id });
                        patch.end_emitted = true;
                    }
                }
            }
            // Preliminary upstream, which nothing here tells apart: it only keeps a deferred call open, and an image
            // tool cannot be declared.
            "response.image_generation_call.partial_image" => out.push(StreamPart::ToolResult {
                tool_call_id: text_of(&value, "item_id"),
                tool_name: self.custom_name("image_generation"),
                result: jobj! { "result": value.get("partial_image_b64").cloned().unwrap_or_default() },
                is_error: None,
                dynamic: None,
                provider_metadata: None,
            }),
            "response.code_interpreter_call_code.delta" => {
                if let Some(call) = self.ongoing_tool_calls.get(&index) {
                    out.push(StreamPart::ToolInputDelta { id: call.tool_call_id.clone(), delta: escape_json_delta(value.str_at("delta").unwrap_or("")) });
                }
            }
            "response.code_interpreter_call_code.done" => {
                if let Some(call) = self.ongoing_tool_calls.get(&index) {
                    let id = call.tool_call_id.clone();
                    let input = jobj! { "code": value.get("code").cloned().unwrap_or_default(), "containerId"?: call.code_interpreter_container.as_deref() }.to_text();
                    out.push(StreamPart::ToolInputDelta { id: id.clone(), delta: "\"}".to_owned() });
                    out.push(StreamPart::ToolInputEnd { id: id.clone() });
                    out.push(StreamPart::ToolCall {
                        tool_call_id: id,
                        tool_name: self.custom_name("code_interpreter"),
                        input,
                        provider_executed: Some(true),
                        dynamic: None,
                        provider_metadata: None,
                    });
                }
            }
            "response.created" => {
                let id = value.path(&["response", "id"]).and_then(Json::as_str).map(str::to_owned);
                self.response_id.clone_from(&id);
                out.push(StreamPart::ResponseMetadata { id, model_id: value.path(&["response", "model"]).and_then(Json::as_str).map(str::to_owned) });
            }
            "response.output_text.delta" => {
                let id = self.resolve_output_item_id(&value);
                out.push(StreamPart::TextDelta { id, delta: text_of(&value, "delta"), provider_metadata: None });
                if self.logprobs_requested {
                    if let Some(logprobs) = value.non_null("logprobs") {
                        self.logprobs.push(logprobs.clone());
                    }
                }
            }
            "response.reasoning_summary_part.added" => {
                let item_id = self.resolve_output_item_id(&value);
                let summary_index = value.get("summary_index").and_then(Json::as_f64).unwrap_or(0.0) as i64;
                // The first start went out with the item.
                if summary_index > 0 {
                    if let Some(active) = self.active_reasoning.get_mut(&item_id) {
                        active.summary_parts.insert(summary_index, SummaryPart::Active);
                        // A new active part lets every part that can conclude do so.
                        let mut concluded = Vec::new();
                        for (at, status) in active.summary_parts.iter_mut() {
                            if *status == SummaryPart::CanConclude {
                                *status = SummaryPart::Concluded;
                                concluded.push(*at);
                            }
                        }
                        let encrypted = active.encrypted_content.clone();
                        for at in concluded {
                            out.push(StreamPart::ReasoningEnd { id: format!("{item_id}:{at}"), provider_metadata: self.metadata(jobj! { "itemId": item_id.as_str() }) });
                        }
                        out.push(StreamPart::ReasoningStart {
                            id: format!("{item_id}:{summary_index}"),
                            provider_metadata: self.metadata(jobj! {
                                "itemId": item_id.as_str(),
                                "reasoningEncryptedContent": encrypted.map_or(Json::Null, Json::from),
                            }),
                        });
                    }
                }
            }
            "response.reasoning_summary_text.delta" => {
                let item_id = self.resolve_output_item_id(&value);
                let summary_index = value.get("summary_index").map_or_else(String::new, Json::to_text);
                out.push(StreamPart::ReasoningDelta {
                    id: format!("{item_id}:{summary_index}"),
                    delta: text_of(&value, "delta"),
                    provider_metadata: self.metadata(jobj! { "itemId": item_id.as_str() }),
                });
            }
            "response.reasoning_summary_part.done" => {
                let item_id = self.resolve_output_item_id(&value);
                let summary_index = value.get("summary_index").and_then(Json::as_f64).unwrap_or(0.0) as i64;
                let store = self.store;
                if let Some(active) = self.active_reasoning.get_mut(&item_id) {
                    // Stored data needs no encrypted content, so the part can end now. Otherwise the last part has to
                    // wait for the item, which is where the encrypted content is.
                    active.summary_parts.insert(summary_index, if store { SummaryPart::Concluded } else { SummaryPart::CanConclude });
                    if store {
                        out.push(StreamPart::ReasoningEnd { id: format!("{item_id}:{summary_index}"), provider_metadata: self.metadata(jobj! { "itemId": item_id.as_str() }) });
                    }
                }
            }
            "response.completed" | "response.incomplete" => {
                let reason = value.path(&["response", "incomplete_details", "reason"]).and_then(Json::as_str);
                if !self.encountered_stream_error {
                    self.finish_reason = FinishReason::new(map_finish_reason(reason, self.has_function_call), reason.map(str::to_owned));
                }
                self.usage = value.path(&["response", "usage"]).cloned();
                if let Some(tier) = value.path(&["response", "service_tier"]).and_then(Json::as_str) {
                    self.service_tier = Some(tier.to_owned());
                }
                if let Some(context) = value.path(&["response", "reasoning", "context"]).filter(|c| !c.is_null()) {
                    self.reasoning_context = Some(context.clone());
                }
            }
            "response.failed" => {
                let reason = value.path(&["response", "incomplete_details", "reason"]).and_then(Json::as_str).filter(|r| !r.is_empty());
                self.finish_reason = FinishReason::new(
                    reason.map_or("error", |reason| map_finish_reason(Some(reason), self.has_function_call)),
                    Some(value.path(&["response", "incomplete_details", "reason"]).and_then(Json::as_str).unwrap_or("error").to_owned()),
                );
                self.usage = value.path(&["response", "usage"]).filter(|u| !u.is_null()).cloned();
                if let Some(context) = value.path(&["response", "reasoning", "context"]).filter(|c| !c.is_null()) {
                    self.reasoning_context = Some(context.clone());
                }
                let error = value.path(&["response", "error"]).filter(|e| !e.is_null());
                if !self.encountered_stream_error && error.is_some() {
                    self.encountered_stream_error = true;
                    let frame = jobj! {
                        "type": "response.failed",
                        "sequence_number": value.get("sequence_number").cloned().unwrap_or_default(),
                        "response": jobj! {
                            "error"?: error.cloned(),
                            "incomplete_details"?: value.path(&["response", "incomplete_details"]).cloned(),
                            "service_tier"?: value.path(&["response", "service_tier"]).cloned(),
                        },
                    };
                    let stream_error = provider_stream_error(&frame).map_or_else(|| crate::stream_text::stream_error_from_payload(&frame), |e| AiError::StreamProvider(Box::new(e)));
                    out.push(StreamPart::Error(stream_error));
                }
            }
            "response.output_text.annotation.added" => self.on_annotation(&value, out),
            "error" => {
                self.encountered_stream_error = true;
                self.finish_reason = FinishReason::new("error", Some("error".to_owned()));
                let stream_error = provider_stream_error(&value).map_or_else(|| crate::stream_text::stream_error_from_payload(&value), |e| AiError::StreamProvider(Box::new(e)));
                out.push(StreamPart::Error(stream_error));
            }
            _ => {}
        }
    }

    fn flush(&mut self, out: &mut Vec<StreamPart>) {
        for held in std::mem::take(&mut self.held_text_ends) {
            let metadata = self.metadata(jobj! { "itemId": held.id.as_str(), "phase"?: held.phase, "annotations": self.annotations.clone() });
            out.push(StreamPart::TextEnd { id: held.id, provider_metadata: metadata });
        }
        for call in self.ongoing_tool_calls.values() {
            if !call.suppress_input_streaming {
                continue;
            }
            out.push(StreamPart::ToolInputStart { id: call.tool_call_id.clone(), tool_name: call.tool_name.clone(), provider_executed: None, dynamic: None });
            for delta in &call.buffered_input_deltas {
                out.push(StreamPart::ToolInputDelta { id: call.tool_call_id.clone(), delta: delta.clone() });
            }
        }
        let provider_metadata = self.metadata(jobj! {
            "responseId": self.response_id.as_deref().map_or(Json::Null, Json::from),
            "logprobs"?: (!self.logprobs.is_empty()).then(|| self.logprobs.clone()),
            "serviceTier"?: self.service_tier.as_deref(),
            "reasoningContext"?: self.reasoning_context.clone(),
        });
        out.push(StreamPart::Finish { finish_reason: self.finish_reason.clone(), usage: convert_usage(self.usage.as_ref()), provider_metadata });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_by_model() {
        let cases = [
            ("gpt-5", true, "developer", true, true),
            ("gpt-5.1-codex", true, "developer", true, true),
            ("gpt-5-chat-latest", false, "system", false, false),
            ("gpt-5.1-chat-x", true, "developer", true, true),
            ("gpt-5-nano", true, "developer", true, false),
            ("gpt-4o", false, "system", false, true),
            ("gpt-4.1-mini", false, "system", false, true),
            ("o1", true, "developer", false, false),
            ("o3-mini", true, "developer", true, true),
            ("o4", true, "developer", true, true),
            ("deepseek-r1", false, "system", false, false),
            ("gpt-5x", false, "system", false, false),
            ("ft:gpt-5:x", false, "system", false, false),
        ];
        for (id, reasoning, mode, flex, priority) in cases {
            let caps = capabilities(id);
            assert_eq!(
                (caps.is_reasoning_model, caps.system_message_mode, caps.supports_flex_processing, caps.supports_priority_processing),
                (reasoning, mode, flex, priority),
                "{id}"
            );
        }
    }

    #[test]
    fn stream_error_status_and_retry_mapping() {
        let run = |frame: &str| {
            let error = provider_stream_error(&Json::parse(frame).unwrap()).unwrap();
            (error.status, error.is_retryable, error.type_.clone())
        };
        assert_eq!(run(r#"{"type":"error","sequence_number":1,"code":"rate_limit_exceeded","message":"m"}"#), (Some(429), true, Some("error".to_owned())));
        assert_eq!(run(r#"{"type":"error","sequence_number":1,"error":{"type":"insufficient_quota","code":"insufficient_quota","message":"m"}}"#), (Some(429), false, Some("insufficient_quota".to_owned())));
        assert_eq!(run(r#"{"type":"response.failed","response":{"error":{"code":"server_error","message":"m"}}}"#), (Some(500), true, Some("response.failed".to_owned())));
        assert_eq!(run(r#"{"type":"error","code":"503","message":"m"}"#), (Some(503), true, Some("error".to_owned())));
        assert_eq!(run(r#"{"type":"error","code":"invalid_prompt","message":"m"}"#), (Some(400), false, Some("error".to_owned())));
        assert_eq!(run(r#"{"type":"error","code":"context_length_exceeded","message":"m"}"#), (Some(400), false, Some("error".to_owned())));
        assert_eq!(run(r#"{"type":"error","code":"permission_denied","message":"m"}"#), (Some(403), false, Some("error".to_owned())));
        assert_eq!(run(r#"{"type":"error","code":"model_not_found","message":"m"}"#), (Some(404), false, Some("error".to_owned())));
        assert_eq!(run(r#"{"type":"error","code":"authentication_error","message":"m"}"#), (Some(401), false, Some("error".to_owned())));
        assert_eq!(run(r#"{"type":"error","code":"server_overloaded","message":"m"}"#), (Some(503), true, Some("error".to_owned())));
        assert_eq!(run(r#"{"type":"error","code":"gateway_timeout","message":"m"}"#), (Some(504), true, Some("error".to_owned())));
    }

    #[test]
    fn finish_reasons() {
        assert_eq!(map_finish_reason(None, false), "stop");
        assert_eq!(map_finish_reason(None, true), "tool-calls");
        assert_eq!(map_finish_reason(Some("max_output_tokens"), true), "length");
        assert_eq!(map_finish_reason(Some("content_filter"), false), "content-filter");
        assert_eq!(map_finish_reason(Some("x"), false), "other");
        assert_eq!(map_finish_reason(Some("x"), true), "tool-calls");
    }

    #[test]
    fn a_parallel_wrapper_counts_only_with_declared_recipients() {
        let tools = vec!["read".to_owned()];
        let good = r#"{"tool_uses":[{"recipient_name":"functions.read","parameters":{"p":1}}]}"#;
        let expanded = expand_parallel_tool_call("c", good, None, &tools).unwrap();
        assert_eq!((expanded[0].tool_call_id.as_str(), expanded[0].input.as_str()), ("c_0", r#"{"p":1}"#));
        assert!(expand_parallel_tool_call("c", r#"{"tool_uses":[{"recipient_name":"functions.other","parameters":{}}]}"#, None, &tools).is_none());
        assert!(expand_parallel_tool_call("c", r#"{"tool_uses":[{"recipient_name":"read","parameters":{}}]}"#, None, &tools).is_none());
        assert!(expand_parallel_tool_call("c", r#"{"tool_uses":[]}"#, None, &tools).is_none());
        assert!(expand_parallel_tool_call("c", "not json", None, &tools).is_none());
    }
}
