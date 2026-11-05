//! `@ai-sdk/amazon-bedrock`: `createAmazonBedrock` and its Converse language model's streaming path.

pub mod eventstream;
pub mod sigv4;

mod convert;
mod schema;

use std::collections::HashMap;
use std::sync::Arc;

use crate::api::{ErrorShape, VERSION_BEDROCK, allowed_env, post_json, with_user_agent_suffix, without_trailing_slash};
use crate::error::{AiError, StreamProviderError};
use crate::http::{BoxFuture, DynFetch, Headers};
use crate::jobj;
use crate::json::{Json, Obj};
use crate::model::{CallOptions, ChunkTransform, DynModel, FinishReason, LanguageModel, ModelStream, ModelUsage, StreamPart, SupportedUrls};
use crate::reasoning::js_round;

use self::eventstream::EventStreamDecoder;
use self::schema::Schema;
use self::sigv4::{ApiKeyFetch, Clock, CredentialSource, Credentials, SigV4Fetch, encode_uri_component, system_clock};
use super::ProviderSettings;

// ------------------------------------------------------------------ Small JavaScript semantics

/// The characters of JavaScript's `\s` and `String.prototype.trim`, which `char::is_whitespace` is not: U+FEFF is
/// one, U+0085 is not.
fn is_js_whitespace(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

/// `text.trim()`.
fn js_trim(text: &str) -> &str {
    text.trim_matches(is_js_whitespace)
}

/// `!!value`.
fn js_truthy(value: &Json) -> bool {
    match value {
        Json::Null => false,
        Json::Bool(flag) => *flag,
        Json::Num(number) => number.as_f64() != 0.0,
        Json::Str(text) => !text.is_empty(),
        Json::Arr(_) | Json::Obj(_) => true,
    }
}

fn unsupported(functionality: &str) -> AiError {
    AiError::other(format!("'{functionality}' functionality not supported."))
}

/// `parseProviderOptions`: `None` when the key is absent or null, the schema's stripped value when it passes.
fn parse_provider_options(provider: &str, options: Option<&Json>, schema: &'static Schema) -> Result<Option<Json>, AiError> {
    let Some(value) = options.and_then(|o| o.non_null(provider)) else { return Ok(None) };
    schema::validate(schema, value).map(Some).map_err(|_| AiError::other(format!("invalid {provider} provider options")))
}

/// `normalizeToolCallId`: Mistral requires exactly nine alphanumerics.
fn normalize_tool_call_id(tool_call_id: &str, is_mistral: bool) -> String {
    if !is_mistral {
        return tool_call_id.to_owned();
    }
    tool_call_id.chars().filter(char::is_ascii_alphanumeric).take(9).collect()
}

/// `supportsStrictTools`: Bedrock's copy of the Messages schema rejects tool `strict` for the newest Claude models.
fn supports_strict_tools(model_id: &str) -> bool {
    const REJECTING: [&str; 5] = ["claude-opus-4-7", "claude-opus-4-8", "claude-opus-5", "claude-fable-5", "claude-sonnet-5"];
    !REJECTING.iter().any(|model| model_id.contains(model))
}

// ------------------------------------------------------------------ The provider

/// `loadSetting`: only the environment `allowed_env` lets through, so an `AWS_*` name reads as missing here just as it
/// did in the sidecar's cleared environment.
fn load_setting(value: Option<&str>, setting_name: &str, environment_variable: &str, description: &str) -> Result<String, AiError> {
    if let Some(value) = value {
        return Ok(value.to_owned());
    }
    allowed_env(environment_variable).ok_or_else(|| {
        AiError::other(format!(
            "{description} setting is missing. Pass it using the '{setting_name}' parameter or the {environment_variable} environment variable."
        ))
    })
}

fn load_region(region: Option<&str>) -> Result<String, AiError> {
    load_setting(region, "region", "AWS_REGION", "AWS region")
}

/// The body of `createAmazonBedrock`'s credential callback, with the two messages it rewraps a failure into.
fn load_credentials(region: Option<&str>) -> Result<Credentials, AiError> {
    let region = load_region(region)?;
    let access_key_id = load_setting(None, "accessKeyId", "AWS_ACCESS_KEY_ID", "AWS access key ID").map_err(|error| {
        AiError::other(format!(
            "AWS SigV4 authentication requires AWS credentials. Please provide either:\n\
             1. Set AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY environment variables\n\
             2. Provide accessKeyId and secretAccessKey in options\n\
             3. Use a credentialProvider function\n\
             4. Use API key authentication with AWS_BEARER_TOKEN_BEDROCK or apiKey option\n\
             Original error: {}",
            error.message()
        ))
    })?;
    let secret_access_key = load_setting(None, "secretAccessKey", "AWS_SECRET_ACCESS_KEY", "AWS secret access key").map_err(|error| {
        AiError::other(format!(
            "AWS SigV4 authentication requires both AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY. Please ensure both credentials are provided.\nOriginal error: {}",
            error.message()
        ))
    })?;
    Ok(Credentials { region, access_key_id, secret_access_key, session_token: allowed_env("AWS_SESSION_TOKEN") })
}

fn region_setting(target: &ProviderSettings) -> Option<String> {
    target.settings.get("region").filter(|region| !region.is_empty()).cloned()
}

fn build(target: ProviderSettings, region: Option<String>, fetch: DynFetch) -> DynModel {
    let headers = with_user_agent_suffix(target.headers, &[&format!("ai-sdk/amazon-bedrock/{VERSION_BEDROCK}")]);
    Arc::new(BedrockModel { model_id: target.model_id, base_url: target.base_url, region, headers, fetch })
}

/// `createAmazonBedrock({ baseURL, apiKey, headers, region })`, as `providers.ts` calls it.
///
/// The sidecar always passes a string `apiKey`, so `AWS_BEARER_TOKEN_BEDROCK` is never consulted. Empty or blank, it
/// falls back to SigV4, whose credentials come from nowhere in this environment: such a request fails with upstream's
/// message before anything is sent.
pub fn create(target: ProviderSettings) -> Result<DynModel, AiError> {
    let region = region_setting(&target);
    let api_key = js_trim(&target.api_key).to_owned();
    let fetch: DynFetch = if api_key.is_empty() {
        let source = region.clone();
        let credentials: CredentialSource = Box::new(move || load_credentials(source.as_deref()));
        Arc::new(SigV4Fetch { credentials, clock: system_clock(), inner: target.fetch.clone() })
    } else {
        Arc::new(ApiKeyFetch { api_key, inner: target.fetch.clone() })
    };
    Ok(build(target, region, fetch))
}

/// `createAmazonBedrock({ accessKeyId, secretAccessKey, sessionToken, region, ... })` with the signing clock in the
/// caller's hands. No frame reaches this: `providers.ts` never passed credentials, so it exists to exercise the SigV4
/// path whole, in tests.
pub fn create_signed(target: ProviderSettings, credentials: Credentials, clock: Clock) -> Result<DynModel, AiError> {
    let region = Some(credentials.region.clone());
    let source: CredentialSource = Box::new(move || Ok(credentials.clone()));
    let fetch: DynFetch = Arc::new(SigV4Fetch { credentials: source, clock, inner: target.fetch.clone() });
    Ok(build(target, region, fetch))
}

pub struct BedrockModel {
    model_id: String,
    base_url: Option<String>,
    region: Option<String>,
    headers: Headers,
    fetch: DynFetch,
}

/// A JSON number from arithmetic on numbers that were JSON, without a spurious fraction.
fn number(value: f64) -> Json {
    if value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 { Json::from(value as i64) } else { Json::from(value) }
}

/// The `reasoningConfig` of the options, after `resolveAmazonBedrockReasoningConfig`.
#[derive(Clone, Default)]
struct ReasoningConfig {
    kind: Option<String>,
    budget_tokens: Option<f64>,
    max_effort: Option<String>,
    display: Option<String>,
}

impl ReasoningConfig {
    fn from_options(config: Option<&Json>) -> Self {
        let text = |key: &str| config.and_then(|c| c.str_at(key)).map(str::to_owned);
        Self {
            kind: text("type"),
            budget_tokens: config.and_then(|c| c.get("budgetTokens")).and_then(Json::as_f64),
            max_effort: text("maxReasoningEffort"),
            display: text("display"),
        }
    }

    /// `{ ...self, ...caller }`.
    fn overlaid_with(self, caller: &Self) -> Self {
        Self {
            kind: caller.kind.clone().or(self.kind),
            budget_tokens: caller.budget_tokens.or(self.budget_tokens),
            max_effort: caller.max_effort.clone().or(self.max_effort),
            display: caller.display.clone().or(self.display),
        }
    }
}

/// `amazonBedrockReasoningEffortMap`.
fn reasoning_effort(reasoning: &str) -> Option<String> {
    let effort = match reasoning {
        "minimal" | "low" => "low",
        "medium" => "medium",
        "high" => "high",
        "xhigh" => "max",
        _ => return None,
    };
    Some(effort.to_owned())
}

/// `mapReasoningToProviderBudget`, where the ceiling and the most a budget may be are the same number.
fn reasoning_budget(reasoning: &str, max_output_tokens: u64) -> Option<f64> {
    let share = match reasoning {
        "minimal" => 0.02,
        "low" => 0.1,
        "medium" => 0.3,
        "high" => 0.6,
        "xhigh" => 0.9,
        _ => return None,
    };
    Some(max_output_tokens.min(1024u64.max(js_round(max_output_tokens as f64 * share))) as f64)
}

/// `resolveAmazonBedrockReasoningConfig`.
fn resolve_reasoning(reasoning: Option<&str>, caller: ReasoningConfig, is_anthropic: bool, model_id: &str) -> ReasoningConfig {
    let Some(reasoning) = reasoning.filter(|r| *r != "provider-default") else { return caller };
    let mut result = caller.clone();
    if is_anthropic {
        let capabilities = super::anthropic::capabilities(model_id);
        if reasoning == "none" {
            result = ReasoningConfig { kind: Some("disabled".to_owned()), ..Default::default() };
        } else if capabilities.supports_adaptive_thinking {
            let base = ReasoningConfig { kind: Some("adaptive".to_owned()), max_effort: reasoning_effort(reasoning), ..Default::default() };
            result = base.overlaid_with(&caller);
        } else if let Some(budget) = reasoning_budget(reasoning, capabilities.max_output_tokens) {
            let base = ReasoningConfig { kind: Some("enabled".to_owned()), budget_tokens: Some(budget), ..Default::default() };
            result = base.overlaid_with(&caller);
        }
    } else if reasoning != "none" {
        let base = ReasoningConfig { max_effort: reasoning_effort(reasoning), ..Default::default() };
        result = base.overlaid_with(&caller);
    }
    // A caller's `disabled` over a level: no derived effort or budget next to disabled thinking.
    if result.kind.as_deref() == Some("disabled") {
        result.max_effort = None;
        result.budget_tokens = None;
    }
    result
}

/// `/^(?:[^.]+\.)?(openai\..+)$/`, its capture.
fn openai_model_id(model_id: &str) -> Option<&str> {
    let matches = |id: &'_ str| id.strip_prefix("openai.").is_some_and(|rest| !rest.is_empty());
    if let Some((_, rest)) = model_id.split_once('.')
        && matches(rest)
    {
        return Some(rest);
    }
    matches(model_id).then_some(model_id)
}

/// Filters tool content out of a prompt when no tools are available: Bedrock does not take it without active tools.
fn without_tool_content(prompt: &[Json]) -> Vec<Json> {
    let is_tool_part = |part: &Json| matches!(part.str_at("type"), Some("tool-call") | Some("tool-result"));
    let has_tool_content = prompt.iter().any(|message| message.get("content").and_then(Json::as_arr).is_some_and(|parts| parts.iter().any(is_tool_part)));
    if !has_tool_content {
        return prompt.to_vec();
    }
    let mut filtered = Vec::with_capacity(prompt.len());
    for message in prompt {
        if message.str_at("role") == Some("system") {
            filtered.push(message.clone());
            continue;
        }
        let parts = message.get("content").and_then(Json::as_arr).map_or(&[][..], Vec::as_slice);
        let kept: Vec<Json> = parts.iter().filter(|part| !is_tool_part(part)).cloned().collect();
        if kept.is_empty() {
            continue;
        }
        let mut copy = message.as_obj().cloned().unwrap_or_default();
        copy.insert("content", kept);
        filtered.push(Json::Obj(copy));
    }
    filtered
}

/// `{ ...fields[key], [inner]: value }`, under `key`: a non-object there spreads to nothing.
fn merge_nested(fields: &mut Obj, key: &str, inner: &str, value: Json) {
    let mut nested = fields.get(key).and_then(Json::as_obj).cloned().unwrap_or_default();
    nested.insert(inner, value);
    fields.insert(key, nested);
}

impl BedrockModel {
    /// `getArgs`.
    fn get_args(&self, options: &CallOptions) -> Result<Json, AiError> {
        let provider_options = options.provider_options.as_ref();
        let parsed = match parse_provider_options("amazonBedrock", provider_options, &schema::CHAT_OPTIONS)? {
            Some(parsed) => Some(parsed),
            None => parse_provider_options("bedrock", provider_options, &schema::CHAT_OPTIONS)?,
        };
        let anthropic = parse_provider_options("anthropic", provider_options, &schema::ANTHROPIC_OPTIONS)?;
        let disable_parallel = anthropic.as_ref().and_then(|a| a.get("disableParallelToolUse")).and_then(Json::as_bool);

        let model_id = self.model_id.as_str();
        let is_anthropic = model_id.contains("anthropic");
        let openai_id = openai_model_id(model_id);

        let caller_config = ReasoningConfig::from_options(parsed.as_ref().and_then(|p| p.get("reasoningConfig")));
        let config = resolve_reasoning(options.reasoning.as_deref(), caller_config, is_anthropic, model_id);
        let thinking_type = config.kind.as_deref();
        let is_thinking = matches!(thinking_type, Some("enabled") | Some("adaptive"));

        let prepared = convert::prepare_tools(options.tools.as_ref(), &options.tool_choice, model_id, disable_parallel);

        let mut fields: Option<Obj> = parsed.as_ref().and_then(|p| p.get("additionalModelRequestFields")).and_then(Json::as_obj).cloned();
        if let Some(choice) = &prepared.additional_tool_choice {
            fields.get_or_insert_default().insert("tool_choice", choice.clone());
        }
        let caller_betas = parsed.as_ref().and_then(|p| p.get("anthropicBeta")).and_then(Json::as_arr);
        if !prepared.betas.is_empty() || caller_betas.is_some() {
            let mut merged: Vec<Json> = caller_betas.cloned().unwrap_or_default();
            merged.extend(prepared.betas.iter().map(Json::from));
            fields.get_or_insert_default().insert("anthropic_beta", merged);
        }

        let thinking_budget = config.budget_tokens.filter(|_| thinking_type == Some("enabled"));
        let mut max_tokens = options.max_output_tokens.map(|tokens| tokens as f64);
        if is_anthropic && is_thinking {
            if let Some(budget) = thinking_budget {
                // Upstream's default of 4096 visible tokens, when the request names none.
                max_tokens = Some(max_tokens.map_or(budget + 4096.0, |tokens| tokens + budget));
                fields.get_or_insert_default().insert("thinking", jobj! { "type": "enabled", "budget_tokens": number(budget) });
            } else if thinking_type == Some("adaptive") {
                fields.get_or_insert_default().insert("thinking", jobj! { "type": "adaptive", "display"?: config.display.as_deref() });
            }
        }

        if let Some(effort) = config.max_effort.as_deref() {
            let fields = fields.get_or_insert_default();
            if is_anthropic {
                merge_nested(fields, "output_config", "effort", Json::from(effort));
            } else if let Some(openai) = openai_id {
                // gpt-oss models expect a flat value, GPT-5.x a nested object.
                if openai.starts_with("openai.gpt-oss-") {
                    fields.insert("reasoning_effort", effort);
                } else {
                    merge_nested(fields, "reasoning", "effort", Json::from(effort));
                }
            } else {
                // Other models, such as Nova 2.
                fields.insert("reasoningConfig", jobj! {
                    "type"?: thinking_type.filter(|kind| *kind != "adaptive"),
                    "budgetTokens"?: thinking_budget.map(number),
                    "maxReasoningEffort": effort,
                });
            }
        }

        let has_any_tools = !prepared.tools.is_empty() || prepared.additional_tool_choice.is_some();
        let filtered_prompt;
        let prompt: &[Json] = if has_any_tools {
            &options.prompt
        } else {
            filtered_prompt = without_tool_content(&options.prompt);
            &filtered_prompt
        };
        let is_mistral = model_id.contains("mistral.");
        let (system, messages) = convert::convert_messages(prompt, is_mistral)?;

        let mut command = Obj::new();
        command.insert("system", system);
        command.insert("messages", messages);
        if let Some(fields) = fields {
            command.insert("additionalModelRequestFields", fields);
        }
        if is_anthropic {
            command.insert("additionalModelResponseFieldPaths", vec![Json::from("/delta/stop_sequence")]);
        }
        if let Some(tokens) = max_tokens {
            command.insert("inferenceConfig", jobj! { "maxTokens": number(tokens) });
        }
        if let Some(tier) = parsed.as_ref().and_then(|p| p.get("serviceTier")) {
            command.insert("serviceTier", jobj! { "type": tier.clone() });
        }
        // Everything else the caller put in the options goes on the command as it came, `anthropicBeta` included.
        let raw = provider_options.and_then(|o| o.non_null("amazonBedrock").or_else(|| o.non_null("bedrock"))).and_then(Json::as_obj);
        for (key, value) in raw.into_iter().flat_map(Obj::iter) {
            if !matches!(key, "reasoningConfig" | "additionalModelRequestFields" | "serviceTier") {
                command.insert(key, value.clone());
            }
        }
        if !prepared.tools.is_empty() {
            command.insert("toolConfig", jobj! { "tools": prepared.tools, "toolChoice"?: prepared.tool_choice });
        }
        Ok(Json::Obj(command))
    }

    /// `config.baseUrl()`, which is only read once a request is made.
    fn base_url(&self) -> Result<String, AiError> {
        Ok(match &self.base_url {
            Some(url) => without_trailing_slash(url).to_owned(),
            None => format!("https://bedrock-runtime.{}.amazonaws.com", load_region(self.region.as_deref())?),
        })
    }

    async fn stream(&self, options: CallOptions) -> Result<ModelStream, AiError> {
        let args = self.get_args(&options)?;
        let url = format!("{}/model/{}/converse-stream", self.base_url()?, encode_uri_component(&self.model_id));
        let response = post_json(&self.fetch, &url, self.headers.clone(), &args, &options.cancel, ERROR_SHAPE).await?;
        let transform = StreamState {
            request_id: response.headers.get("x-amzn-requestid").map(str::to_owned),
            model_id: self.model_id.clone(),
            is_mistral: self.model_id.contains("mistral."),
            finish_reason: FinishReason::other(),
            usage: None,
            metadata: None,
            stop_sequence: None,
            blocks: HashMap::new(),
        };
        Ok(ModelStream::new(response.body, Box::<EventStreamDecoder>::default(), Box::new(transform)))
    }
}

/// `AmazonBedrockErrorSchema` and `errorToMessage` of `doStream`: `${error.type}: ${error.message}`, with `undefined`
/// for the `type` Bedrock's own errors do not have.
const ERROR_SHAPE: ErrorShape = ErrorShape {
    message: |body| {
        let message = body.str_at("message")?;
        let kind = match body.get("type") {
            None => "undefined",
            Some(Json::Null) => "null",
            Some(Json::Str(kind)) => kind,
            Some(_) => return None,
        };
        Some(format!("{kind}: {message}"))
    },
    is_retryable: None,
};

impl LanguageModel for BedrockModel {
    fn provider(&self) -> &str {
        "amazon-bedrock"
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn supported_urls(&self) -> SupportedUrls {
        vec![("image/*", r"^s3://"), ("video/*", r"^s3://")]
    }

    fn do_stream(&self, options: CallOptions) -> BoxFuture<'_, Result<ModelStream, AiError>> {
        Box::pin(self.stream(options))
    }
}

// ------------------------------------------------------------------ Streaming

/// `convertAmazonBedrockUsage`.
fn convert_usage(usage: Option<&Json>) -> ModelUsage {
    let Some(usage) = usage else { return ModelUsage::default() };
    let count = |key: &str| usage.non_null(key).and_then(Json::as_u64);
    let (input, output) = (count("inputTokens").unwrap_or(0), count("outputTokens").unwrap_or(0));
    let (cache_read, cache_write) = (count("cacheReadInputTokens").unwrap_or(0), count("cacheWriteInputTokens").unwrap_or(0));
    ModelUsage {
        input_total: Some(input + cache_read + cache_write),
        input_no_cache: Some(input),
        input_cache_read: Some(cache_read),
        input_cache_write: Some(cache_write),
        output_total: Some(output),
        output_text: Some(output),
        output_reasoning: None,
        raw: Some(usage.clone()),
    }
}

/// `mapAmazonBedrockFinishReason`, with no JSON response tool in this crate's calls.
fn map_finish_reason(reason: &str) -> &'static str {
    match reason {
        "stop_sequence" | "end_turn" => "stop",
        "max_tokens" => "length",
        "content_filtered" | "guardrail_intervened" => "content-filter",
        "tool_use" => "tool-calls",
        _ => "other",
    }
}

/// `getAmazonBedrockStreamErrorMetadata`.
fn stream_error_metadata(kind: &str) -> (u16, bool) {
    match kind {
        "internalServerException" => (500, true),
        "modelStreamErrorException" => (424, true),
        "serviceUnavailableException" => (503, true),
        "throttlingException" => (429, true),
        _ => (400, false),
    }
}

/// `createAmazonBedrockStreamError`.
fn stream_error(kind: &str, error: &Json, raw: &Json) -> AiError {
    let (status, is_retryable) = stream_error_metadata(kind);
    AiError::StreamProvider(Box::new(StreamProviderError {
        message: error.str_at("message").map_or_else(|| format!("Amazon Bedrock stream failed with {kind}"), str::to_owned),
        type_: Some(kind.to_owned()),
        code: None,
        status: Some(status),
        is_retryable,
        data: Some(raw.clone()),
    }))
}

/// `{ amazonBedrock: payload, bedrock: payload }`.
fn bedrock_metadata(payload: Json) -> Json {
    jobj! { "amazonBedrock": payload.clone(), "bedrock": payload }
}

enum Block {
    Text,
    Reasoning { redacted_content: Option<String> },
    ToolCall { tool_call_id: String, tool_name: String, json_text: String },
}

struct StreamState {
    request_id: Option<String>,
    model_id: String,
    is_mistral: bool,
    finish_reason: FinishReason,
    usage: Option<Json>,
    /// `providerMetadata.amazonBedrock`.
    metadata: Option<Json>,
    stop_sequence: Option<String>,
    /// By `String(contentBlockIndex)`.
    blocks: HashMap<String, Block>,
}

impl StreamState {
    fn on_metadata(&mut self, metadata: &Json) {
        let usage = metadata.non_null("usage");
        if let Some(usage) = usage {
            self.usage = Some(usage.clone());
        }
        let cache_write = usage.and_then(|u| u.non_null("cacheWriteInputTokens"));
        let cache_details = usage.and_then(|u| u.non_null("cacheDetails"));
        let cache_usage = (cache_write.is_some() || cache_details.is_some()).then(|| {
            jobj! { "cacheWriteInputTokens"?: cache_write.cloned(), "cacheDetails"?: cache_details.cloned() }
        });
        let trace = metadata.get("trace").filter(|trace| js_truthy(trace));
        let performance = metadata.non_null("performanceConfig");
        let tier = metadata.non_null("serviceTier");
        if cache_usage.is_some() || trace.is_some() || performance.is_some() || tier.is_some() {
            self.metadata = Some(jobj! {
                "usage"?: cache_usage,
                "trace"?: trace.cloned(),
                "performanceConfig"?: performance.cloned(),
                "serviceTier"?: tier.cloned(),
            });
        }
    }

    fn open_reasoning(&mut self, id: &str, out: &mut Vec<StreamPart>) {
        if !self.blocks.contains_key(id) {
            self.blocks.insert(id.to_owned(), Block::Reasoning { redacted_content: None });
            out.push(StreamPart::ReasoningStart { id: id.to_owned(), provider_metadata: None });
        }
    }

    fn on_block_stop(&mut self, id: String, out: &mut Vec<StreamPart>) {
        match self.blocks.remove(&id) {
            Some(Block::Reasoning { redacted_content }) => {
                let provider_metadata = redacted_content.map(|content| bedrock_metadata(jobj! { "redactedContent": content }));
                out.push(StreamPart::ReasoningEnd { id, provider_metadata });
            }
            Some(Block::Text) => out.push(StreamPart::TextEnd { id, provider_metadata: None }),
            Some(Block::ToolCall { tool_call_id, tool_name, json_text }) => {
                out.push(StreamPart::ToolInputEnd { id: tool_call_id.clone() });
                out.push(StreamPart::ToolCall {
                    tool_call_id,
                    tool_name,
                    input: if json_text.is_empty() { "{}".to_owned() } else { json_text },
                    provider_executed: None,
                    dynamic: None,
                    provider_metadata: None,
                });
            }
            None => {}
        }
    }

    fn on_reasoning_delta(&mut self, id: &str, content: &Json, out: &mut Vec<StreamPart>) {
        let signed_delta = |id: &str, payload: Json| StreamPart::ReasoningDelta { id: id.to_owned(), delta: String::new(), provider_metadata: Some(bedrock_metadata(payload)) };
        // The chunk schema's union left exactly one of these keys.
        if let Some(text) = content.str_at("text").filter(|text| !text.is_empty()) {
            self.open_reasoning(id, out);
            out.push(StreamPart::ReasoningDelta { id: id.to_owned(), delta: text.to_owned(), provider_metadata: None });
        } else if let Some(signature) = content.str_at("signature").filter(|text| !text.is_empty()) {
            self.open_reasoning(id, out);
            out.push(signed_delta(id, jobj! { "signature": signature }));
        } else if let Some(data) = content.str_at("data").filter(|text| !text.is_empty()) {
            self.open_reasoning(id, out);
            out.push(signed_delta(id, jobj! { "redactedData": data }));
        } else if let Some(redacted) = content.str_at("redactedContent").filter(|text| !text.is_empty()) {
            self.open_reasoning(id, out);
            // Accumulated and attached once, at the end: a reasoning part's merged provider metadata is
            // last-write-wins, so per-delta metadata would drop the earlier chunks.
            if let Some(Block::Reasoning { redacted_content }) = self.blocks.get_mut(id) {
                redacted_content.get_or_insert_default().push_str(redacted);
            }
        }
    }
}

/// `String(contentBlockIndex)`.
fn block_key(block: &Json) -> Option<String> {
    block.non_null("contentBlockIndex").map(Json::to_text)
}

impl ChunkTransform for StreamState {
    fn start(&mut self, out: &mut Vec<StreamPart>) {
        out.push(StreamPart::StreamStart { warnings: Vec::new() });
        out.push(StreamPart::ResponseMetadata { id: self.request_id.clone(), model_id: Some(self.model_id.clone()) });
    }

    fn chunk(&mut self, chunk: Result<Json, AiError>, out: &mut Vec<StreamPart>) {
        let failed = |state: &mut Self, error: AiError, out: &mut Vec<StreamPart>| {
            state.finish_reason = FinishReason::new("error", None);
            out.push(StreamPart::Error(error));
        };
        let raw = match chunk {
            Ok(raw) => raw,
            Err(error) => return failed(self, error, out),
        };
        let value = match schema::validate(&schema::STREAM_CHUNK, &raw) {
            Ok(value) => value,
            Err(detail) => return failed(self, AiError::validation(&raw, &detail), out),
        };

        for kind in ["internalServerException", "modelStreamErrorException", "serviceUnavailableException", "throttlingException", "validationException"] {
            if let Some(error) = value.non_null(kind) {
                return failed(self, stream_error(kind, error, &raw), out);
            }
        }

        if let Some(stop) = value.non_null("messageStop") {
            let reason = stop.str_at("stopReason").unwrap_or("");
            self.finish_reason = FinishReason::new(map_finish_reason(reason), Some(reason.to_owned()));
            self.stop_sequence = stop.path(&["additionalModelResponseFields", "delta", "stop_sequence"]).and_then(Json::as_str).map(str::to_owned);
        }

        if let Some(metadata) = value.non_null("metadata") {
            self.on_metadata(metadata);
        }

        let start = value.non_null("contentBlockStart");
        if let Some(start) = start {
            let has_tool_use = start.path(&["start", "toolUse"]).is_some_and(|tool| !tool.is_null());
            if let (Some(id), false) = (block_key(start), has_tool_use) {
                self.blocks.insert(id.clone(), Block::Text);
                out.push(StreamPart::TextStart { id, provider_metadata: None });
            }
        }

        let block_delta = value.non_null("contentBlockDelta");
        let delta = block_delta.and_then(|d| d.non_null("delta"));
        // `contentBlockIndex || 0`: the one falsy number it could hold is the same zero.
        let delta_id = block_delta.and_then(block_key).unwrap_or_else(|| "0".to_owned());

        if let Some(text) = delta.and_then(|d| d.str_at("text")).filter(|text| !text.is_empty()) {
            if !self.blocks.contains_key(&delta_id) {
                self.blocks.insert(delta_id.clone(), Block::Text);
                out.push(StreamPart::TextStart { id: delta_id.clone(), provider_metadata: None });
            }
            out.push(StreamPart::TextDelta { id: delta_id.clone(), delta: text.to_owned(), provider_metadata: None });
        }

        if let Some(id) = value.non_null("contentBlockStop").and_then(block_key) {
            self.on_block_stop(id, out);
        }

        if let Some(content) = delta.and_then(|d| d.non_null("reasoningContent")) {
            self.on_reasoning_delta(&delta_id, content, out);
        }

        if let Some(start) = start
            && let (Some(tool), Some(id)) = (start.path(&["start", "toolUse"]).filter(|tool| !tool.is_null()), block_key(start))
        {
            let tool_call_id = normalize_tool_call_id(tool.str_at("toolUseId").unwrap_or(""), self.is_mistral);
            let tool_name = tool.str_at("name").unwrap_or("").to_owned();
            out.push(StreamPart::ToolInputStart { id: tool_call_id.clone(), tool_name: tool_name.clone(), provider_executed: None, dynamic: None });
            self.blocks.insert(id, Block::ToolCall { tool_call_id, tool_name, json_text: String::new() });
        }

        if let Some(input) = delta.and_then(|d| d.path(&["toolUse", "input"])).and_then(Json::as_str)
            && let Some(Block::ToolCall { tool_call_id, json_text, .. }) = self.blocks.get_mut(&delta_id)
        {
            out.push(StreamPart::ToolInputDelta { id: tool_call_id.clone(), delta: input.to_owned() });
            json_text.push_str(input);
        }
    }

    fn flush(&mut self, out: &mut Vec<StreamPart>) {
        if let Some(stop_sequence) = &self.stop_sequence {
            let mut payload = self.metadata.as_ref().and_then(Json::as_obj).cloned().unwrap_or_default();
            payload.insert("stopSequence", stop_sequence.as_str());
            self.metadata = Some(Json::Obj(payload));
        }
        out.push(StreamPart::Finish {
            finish_reason: self.finish_reason.clone(),
            usage: convert_usage(self.usage.as_ref()),
            provider_metadata: self.metadata.clone().map(bedrock_metadata),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream_state(model: &str) -> StreamState {
        StreamState {
            request_id: None,
            model_id: model.to_owned(),
            is_mistral: model.contains("mistral."),
            finish_reason: FinishReason::other(),
            usage: None,
            metadata: None,
            stop_sequence: None,
            blocks: HashMap::new(),
        }
    }

    fn feed(state: &mut StreamState, chunks: &[&str]) -> Vec<StreamPart> {
        let mut out = Vec::new();
        for chunk in chunks {
            state.chunk(Ok(Json::parse(chunk).unwrap()), &mut out);
        }
        out
    }

    #[test]
    fn finish_reasons_map_as_upstream_maps_them() {
        for (raw, unified) in [
            ("stop_sequence", "stop"),
            ("end_turn", "stop"),
            ("max_tokens", "length"),
            ("content_filtered", "content-filter"),
            ("guardrail_intervened", "content-filter"),
            ("tool_use", "tool-calls"),
            ("stop", "other"),
            ("something_new", "other"),
        ] {
            assert_eq!(map_finish_reason(raw), unified, "{raw}");
        }
    }

    #[test]
    fn usage_counts_cache_into_the_input_total() {
        let usage = Json::parse(r#"{"inputTokens":3,"outputTokens":5,"cacheReadInputTokens":2,"cacheWriteInputTokens":7}"#).unwrap();
        let converted = convert_usage(Some(&usage));
        assert_eq!(converted.input_total, Some(12));
        assert_eq!(converted.input_no_cache, Some(3));
        assert_eq!(converted.input_cache_read, Some(2));
        assert_eq!(converted.input_cache_write, Some(7));
        assert_eq!((converted.output_total, converted.output_text, converted.output_reasoning), (Some(5), Some(5), None));
        assert_eq!(convert_usage(None), ModelUsage::default());
    }

    #[test]
    fn stream_error_table() {
        for (kind, status, retryable) in [
            ("internalServerException", 500, true),
            ("modelStreamErrorException", 424, true),
            ("serviceUnavailableException", 503, true),
            ("throttlingException", 429, true),
            ("validationException", 400, false),
        ] {
            assert_eq!(stream_error_metadata(kind), (status, retryable), "{kind}");
        }
    }

    #[test]
    fn finish_metadata_merges_trace_tier_and_stop_sequence() {
        let mut state = stream_state("anthropic.claude-sonnet-4-5");
        feed(&mut state, &[
            r#"{"messageStop":{"stopReason":"stop_sequence","additionalModelResponseFields":{"delta":{"stop_sequence":"END"}}}}"#,
            r#"{"metadata":{"usage":{"inputTokens":1,"outputTokens":2,"cacheWriteInputTokens":3,"cacheDetails":[{"inputTokens":3,"ttl":"5m"}]},"trace":{"guardrail":{}},"performanceConfig":{"latency":"optimized","x":1},"serviceTier":{"type":"priority"}}}"#,
        ]);
        let mut out = Vec::new();
        state.flush(&mut out);
        let [StreamPart::Finish { finish_reason, provider_metadata, usage }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!(finish_reason, &FinishReason::new("stop", Some("stop_sequence".to_owned())));
        assert_eq!(usage.input_cache_write, Some(3));
        let payload = r#"{"usage":{"cacheWriteInputTokens":3,"cacheDetails":[{"inputTokens":3,"ttl":"5m"}]},"trace":{"guardrail":{}},"performanceConfig":{"latency":"optimized"},"serviceTier":{"type":"priority"},"stopSequence":"END"}"#;
        assert_eq!(provider_metadata.as_ref().unwrap().to_text(), format!(r#"{{"amazonBedrock":{payload},"bedrock":{payload}}}"#));
    }

    #[test]
    fn redacted_content_accumulates_and_lands_on_the_end() {
        let mut state = stream_state("us.openai.gpt-5.6-luna");
        let out = feed(&mut state, &[
            r#"{"contentBlockDelta":{"contentBlockIndex":0,"delta":{"reasoningContent":{"redactedContent":"AB"}}}}"#,
            r#"{"contentBlockDelta":{"contentBlockIndex":0,"delta":{"reasoningContent":{"redactedContent":"CD"}}}}"#,
            r#"{"contentBlockStop":{"contentBlockIndex":0}}"#,
        ]);
        let [StreamPart::ReasoningStart { .. }, StreamPart::ReasoningEnd { provider_metadata: Some(metadata), .. }] = out.as_slice() else { panic!("{out:?}") };
        assert_eq!(metadata.to_text(), r#"{"amazonBedrock":{"redactedContent":"ABCD"},"bedrock":{"redactedContent":"ABCD"}}"#);
    }

    #[test]
    fn mistral_ids_are_nine_alphanumerics() {
        assert_eq!(normalize_tool_call_id("tooluse_bpe71yCfRu2b5i-nKGDr5g", true), "toolusebp");
        assert_eq!(normalize_tool_call_id("tooluse_bpe71yCfRu2b5i-nKGDr5g", false), "tooluse_bpe71yCfRu2b5i-nKGDr5g");
    }

    #[test]
    fn openai_ids_with_and_without_a_region() {
        assert_eq!(openai_model_id("openai.gpt-oss-120b-1:0"), Some("openai.gpt-oss-120b-1:0"));
        assert_eq!(openai_model_id("us.openai.gpt-5.6-luna"), Some("openai.gpt-5.6-luna"));
        assert_eq!(openai_model_id("openai.openai.x"), Some("openai.x"));
        assert_eq!(openai_model_id("openai."), None);
        assert_eq!(openai_model_id("us.anthropic.claude"), None);
    }

    #[test]
    fn javascript_trim_is_not_rust_trim() {
        assert_eq!(js_trim("\u{feff} a\u{a0}\u{3000}"), "a");
        assert_eq!(js_trim("\u{85}a\u{85}"), "\u{85}a\u{85}");
    }

    #[test]
    fn the_aws_environment_is_out_of_reach() {
        for name in ["AWS_REGION", "AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN", "AWS_BEARER_TOKEN_BEDROCK"] {
            assert_eq!(allowed_env(name), None, "{name}");
        }
    }
}
