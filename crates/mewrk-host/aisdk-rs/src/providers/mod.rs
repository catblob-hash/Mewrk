//! Provider-family to language-model mapping (`providers.ts`): the only family dispatch point.
//!
//! `baseURL` has already passed host URL validation. API keys come only from the frame: nothing here reads the
//! environment, which could silently send an unrelated development key to a user-configured relay. An explicit
//! empty string lets the upstream answer 401.

pub mod anthropic;
pub mod azure;
pub mod bedrock;
pub mod google;
pub mod openai_compatible;
pub mod openai_responses;
pub mod vertex;
pub mod xai;

use std::collections::HashMap;

use crate::api::headers_from_json;
use crate::async_tools::async_tools_responses_fetch;
use crate::dialect::anthropic::{PromptCacheOptions, anthropic_dialect_fetch};
use crate::dialect::chat::chat_dialect_fetch;
use crate::dialect::codex::codex_dialect_fetch;
use crate::dialect::responses::plaintext_reasoning_fetch;
use crate::error::AiError;
use crate::http::{DynFetch, Headers, NetFetch};
use crate::json::Json;
use crate::model::DynModel;
use crate::tool_append::tool_append_responses_fetch;

/// What every family's constructor takes: `createX({ baseURL, apiKey, headers, fetch })`.
pub struct ProviderSettings {
    /// `None` selects the provider default. An empty string in the frame also reads as `None`.
    pub base_url: Option<String>,
    pub api_key: String,
    pub headers: Headers,
    /// `settings`: `region`, `project`, `location`, `apiVersion`.
    pub settings: HashMap<String, String>,
    pub model_id: String,
    /// The HTTP layer with this family's dialect wrappers.
    pub fetch: DynFetch,
}

pub fn resolve_model(request: &Json) -> Result<DynModel, AiError> {
    let family = request.str_at("family").unwrap_or("");
    let base_url = request.str_at("baseURL").filter(|url| !url.is_empty()).map(str::to_owned);
    let settings: HashMap<String, String> = request
        .get("settings")
        .and_then(Json::as_obj)
        .map(|object| object.iter().filter_map(|(k, v)| Some((k.to_owned(), v.as_str()?.to_owned()))).collect())
        .unwrap_or_default();
    let reasoning_plaintext = request.str_at("reasoningContent") == Some("plaintext");
    let tool_append = request.get("toolAppend").and_then(Json::as_bool) == Some(true);
    let async_tools: Vec<String> = request
        .get("asyncTools")
        .and_then(Json::as_arr)
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .filter_map(Json::as_str)
        .map(str::to_owned)
        .collect();
    let net = NetFetch::shared();
    let responses_fetch = || {
        let inner = if reasoning_plaintext { plaintext_reasoning_fetch(net.clone()) } else { net.clone() };
        async_tools_responses_fetch(tool_append_responses_fetch(inner, tool_append), async_tools.clone())
    };
    let fetch: DynFetch = match family {
        "openai-responses" | "azure" => responses_fetch(),
        // ChatGPT subscription Codex requires streamed Responses requests.
        "openai-codex" => async_tools_responses_fetch(tool_append_responses_fetch(codex_dialect_fetch(net.clone()), tool_append), async_tools.clone()),
        "openai-chat" | "openai-compatible" => chat_dialect_fetch(net.clone()),
        "anthropic" => anthropic_dialect_fetch(
            base_url.as_deref(),
            net.clone(),
            PromptCacheOptions {
                enabled: request.get("promptCache").and_then(Json::as_bool),
                system_dynamic: request.str_at("systemDynamic").map(str::to_owned),
            },
            tool_append,
        ),
        "claude-agent" => return Err(AiError::other("claude-agent 家族不经 AI SDK 模型解析")),
        "google" | "xai" | "bedrock" | "vertex" => net.clone(),
        other => return Err(AiError::other(format!("未知的适配家族：{other}"))),
    };
    let target = ProviderSettings {
        base_url,
        api_key: request.str_at("apiKey").unwrap_or("").to_owned(),
        headers: headers_from_json(request.get("headers")),
        settings,
        model_id: request.str_at("modelId").unwrap_or("").to_owned(),
        fetch,
    };
    match family {
        "openai-responses" => openai_responses::create(target, openai_responses::Flavor::OpenAi),
        "openai-codex" => openai_responses::create(target, openai_responses::Flavor::Codex),
        "openai-chat" => openai_compatible::create(target, "openai-chat"),
        "openai-compatible" => openai_compatible::create(target, "openai-compatible"),
        "anthropic" => anthropic::create(target),
        "google" => google::create(target),
        "xai" => xai::create(target),
        "azure" => azure::create(target),
        "bedrock" => bedrock::create(target),
        "vertex" => vertex::create(target),
        other => Err(AiError::other(format!("未知的适配家族：{other}"))),
    }
}
