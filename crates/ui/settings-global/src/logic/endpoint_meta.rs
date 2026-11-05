//! `ProviderSettings/endpointMeta.ts`: labels, examples and request previews for each protocol.

use mewrk_client::model::{EndpointType, FamilySetting, ProviderFamily};
use mewrk_client::{t, tf};

use super::codex_provider::CODEX_DEFAULT_BASE_URL;
use super::model_caps::chat_endpoint_of;

pub struct ApiFormat {
    pub value: ProviderFamily,
    pub label: &'static str,
    pub default_base_url: &'static str,
}

/// Labels are protocol or vendor brands and are not localized. `default_base_url` replaces an unmodified URL when the
/// family changes; empty means no fixed public URL.
pub const API_FORMAT_OPTIONS: &[ApiFormat] = &[
    ApiFormat { value: ProviderFamily::OpenaiResponses, label: "OpenAI Responses", default_base_url: "https://api.openai.com/v1" },
    ApiFormat { value: ProviderFamily::OpenaiChat, label: "OpenAI Chat Completions", default_base_url: "https://api.openai.com/v1" },
    ApiFormat { value: ProviderFamily::Anthropic, label: "Anthropic Messages", default_base_url: "https://api.anthropic.com/v1" },
    ApiFormat { value: ProviderFamily::Google, label: "Google Gemini", default_base_url: "https://generativelanguage.googleapis.com/v1beta" },
    ApiFormat { value: ProviderFamily::Xai, label: "xAI", default_base_url: "https://api.x.ai/v1" },
    ApiFormat { value: ProviderFamily::Azure, label: "Azure OpenAI", default_base_url: "" },
    ApiFormat { value: ProviderFamily::Bedrock, label: "AWS Bedrock", default_base_url: "" },
    ApiFormat { value: ProviderFamily::Vertex, label: "Google Vertex AI", default_base_url: "" },
    ApiFormat { value: ProviderFamily::OpenaiCompatible, label: "OpenAI Compatible", default_base_url: "" },
];

pub fn builtin_family_option(family: ProviderFamily) -> Option<ApiFormat> {
    match family {
        ProviderFamily::OpenaiCodex => Some(ApiFormat { value: family, label: "OpenAI Codex (ChatGPT)", default_base_url: CODEX_DEFAULT_BASE_URL }),
        ProviderFamily::ClaudeAgent => Some(ApiFormat { value: family, label: "Claude Agent (Claude Code)", default_base_url: "" }),
        _ => None,
    }
}

pub fn family_label(family: ProviderFamily) -> &'static str {
    if let Some(b) = builtin_family_option(family) {
        return b.label;
    }
    API_FORMAT_OPTIONS.iter().find(|o| o.value == family).map_or("", |o| o.label)
}

pub fn default_base_url(family: ProviderFamily) -> &'static str {
    API_FORMAT_OPTIONS.iter().find(|o| o.value == family).map_or("", |o| o.default_base_url)
}

/// The example an empty API address field shows. Azure's stops at `/openai`: the AI SDK appends the rest, so `/v1`
/// there would send requests to `/v1/v1/responses`.
pub fn api_address_example(family: ProviderFamily) -> &'static str {
    match family {
        ProviderFamily::Azure => "https://<resource>.openai.azure.com/openai",
        ProviderFamily::OpenaiCompatible => "http://localhost:11434/v1",
        ProviderFamily::Bedrock | ProviderFamily::Vertex => "",
        _ => builtin_family_option(family).map_or_else(|| default_base_url(family), |b| b.default_base_url),
    }
}

/// What Mewrk derives the endpoint from when a Bedrock or Vertex address is blank; `None` where blank is just missing.
pub fn api_address_derivation(family: ProviderFamily) -> Option<&'static str> {
    match family {
        ProviderFamily::Bedrock => Some(t("留空即可，由 AWS 区域推算。", "Leave blank; derived from the AWS region.")),
        ProviderFamily::Vertex => Some(t("留空即可，由 GCP 项目和区域推算。", "Leave blank; derived from the GCP project and location.")),
        _ => None,
    }
}

fn chat_request_path(family: ProviderFamily) -> &'static str {
    match chat_endpoint_of(family) {
        EndpointType::OpenaiResponses => "/responses",
        EndpointType::OpenaiChatCompletions => "/chat/completions",
        EndpointType::AnthropicMessages => "/messages",
        // These depend on the model, deployment, version or region. A guessed URL could make users change a valid setup.
        _ => "",
    }
}

/// Endpoint suffixes the host removes from a pasted Base URL. Mirrors Rust `strip_known_endpoint`; paired forms first.
const PASTED_ENDPOINT_SUFFIXES: &[&[&str]] = &[&["chat", "completions"], &["responses", "compact"], &["models"], &["responses"], &["messages"], &["completions"]];

fn without_pasted_endpoint(base_url: &str) -> String {
    let trimmed = base_url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return String::new();
    }
    let segments: Vec<&str> = trimmed.split('/').collect();
    for suffix in PASTED_ENDPOINT_SUFFIXES {
        if segments.len() <= suffix.len() {
            continue;
        }
        let tail = &segments[segments.len() - suffix.len()..];
        if tail.iter().zip(suffix.iter()).all(|(a, b)| a.eq_ignore_ascii_case(b)) {
            return segments[..segments.len() - suffix.len()].join("/");
        }
    }
    trimmed.to_owned()
}

/// The full conversation request URL. The host strips a pasted suffix first, so the preview does too: showing
/// `.../v1/responses/responses` for a working URL would push users to "fix" a valid configuration.
pub fn chat_request_preview(base_url: &str, family: ProviderFamily) -> String {
    let normalized = without_pasted_endpoint(base_url);
    if normalized.is_empty() {
        return String::new();
    }
    format!("{normalized}{}", chat_request_path(family))
}

pub struct FamilySettingMeta {
    pub label: &'static str,
    pub placeholder: &'static str,
    pub hint: &'static str,
}

pub fn family_setting_meta(setting: FamilySetting) -> FamilySettingMeta {
    match setting {
        FamilySetting::Region => FamilySettingMeta { label: t("AWS 区域", "AWS region"), placeholder: "us-east-1", hint: t("Bedrock 的端点主机名由它决定。", "Bedrock derives its endpoint host from this.") },
        FamilySetting::Project => FamilySettingMeta { label: t("GCP 项目", "GCP project"), placeholder: "my-project-123456", hint: t("Vertex 的模型路径里带着它。", "Vertex puts this in the model path.") },
        FamilySetting::Location => FamilySettingMeta { label: t("GCP 区域", "GCP location"), placeholder: "us-central1", hint: t("Vertex 的端点主机名与模型路径都带着它。", "Vertex puts this in both the endpoint host and the model path.") },
        FamilySetting::ApiVersion => FamilySettingMeta { label: "api-version", placeholder: t("留空 = 用默认版本", "blank = provider default"), hint: t("留空用默认版本，别随意指定。", "Blank uses the default; don't pin an unverified version.") },
    }
}

pub fn no_address_note(family: ProviderFamily) -> String {
    tf("尚未填写 API 地址（{format}）", "No API address yet ({format})", &[("format", &family_label(family))])
}

#[cfg(test)]
mod tests {
    use super::*;
    use ProviderFamily::*;

    // endpointMeta.test.ts
    #[test]
    fn keeps_a_relay_path_segment_the_host_also_keeps() {
        assert_eq!(chat_request_preview("https://relay.example.com/tenant/provider", OpenaiChat), "https://relay.example.com/tenant/provider/chat/completions");
        assert_eq!(chat_request_preview("https://gateway.example.com/v1/acct/gw/openai", OpenaiChat), "https://gateway.example.com/v1/acct/gw/openai/chat/completions");
    }

    #[test]
    fn strips_a_pasted_endpoint_suffix_the_way_the_host_does() {
        assert_eq!(chat_request_preview("https://relay.example.com/v1/chat/completions", OpenaiChat), "https://relay.example.com/v1/chat/completions");
        assert_eq!(chat_request_preview("https://relay.example.com/v1/messages", Anthropic), "https://relay.example.com/v1/messages");
        assert_eq!(chat_request_preview("https://relay.example.com/v1/responses", OpenaiResponses), "https://relay.example.com/v1/responses");
    }

    #[test]
    fn tolerates_trailing_slashes_and_an_empty_base_url() {
        assert_eq!(chat_request_preview("https://relay.example.com/v1//", OpenaiChat), "https://relay.example.com/v1/chat/completions");
        assert_eq!(chat_request_preview("   ", OpenaiChat), "");
    }

    #[test]
    fn appends_no_suffix_where_the_path_depends_on_the_model() {
        assert_eq!(chat_request_preview("https://relay.example.com/v1", Google), "https://relay.example.com/v1");
    }

    #[test]
    fn blank_is_normal_only_on_bedrock_and_vertex() {
        let _language = crate::language_for_test(false);
        assert_eq!(api_address_derivation(Bedrock), Some("留空即可，由 AWS 区域推算。"));
        assert_eq!(api_address_derivation(Vertex), Some("留空即可，由 GCP 项目和区域推算。"));
        assert_eq!(api_address_derivation(OpenaiChat), None);
    }

    #[test]
    fn each_protocol_has_its_own_example() {
        assert_eq!(api_address_example(Azure), "https://<resource>.openai.azure.com/openai");
        assert_eq!(api_address_example(OpenaiCompatible), "http://localhost:11434/v1");
        assert_eq!(api_address_example(Bedrock), "");
        assert_eq!(api_address_example(OpenaiCodex), CODEX_DEFAULT_BASE_URL);
        assert_eq!(api_address_example(Anthropic), "https://api.anthropic.com/v1");
    }
}
