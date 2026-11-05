//! What a provider family and a model can do (lib/modelCapabilities.ts).
//!
//! Upstream mirrors the host's `ProviderFamily` tables across the language boundary. Here they are the same process, so
//! whatever `ProviderFamily` exposes publicly is called directly and only what is private to the host is ported.

use crate::model::{
    ApiProvider, EndpointType, FamilySetting, ModelCapability, ModelProfile, ProviderFamily, ReasoningContent, ReasoningForm,
};
use serde_json::Value;

/// Endpoint-type catalog in the same order as Rust `EndpointType::CATALOG`.
pub const ENDPOINT_TYPES: [EndpointType; 10] = [
    EndpointType::OpenaiChatCompletions,
    EndpointType::OpenaiResponses,
    EndpointType::AnthropicMessages,
    EndpointType::GoogleGenerative,
    EndpointType::AzureOpenai,
    EndpointType::BedrockConverse,
    EndpointType::OpenaiImageGeneration,
    EndpointType::OpenaiImageEdit,
    EndpointType::OpenaiTextToSpeech,
    EndpointType::OpenaiAudioTranscription,
];

/// Adapter family to chat endpoint type.
pub fn chat_endpoint_of(family: ProviderFamily) -> EndpointType {
    match family {
        ProviderFamily::OpenaiResponses | ProviderFamily::OpenaiCodex => EndpointType::OpenaiResponses,
        // xAI and the generic compatibility layer use the `/chat/completions` shape.
        ProviderFamily::OpenaiChat | ProviderFamily::Xai | ProviderFamily::OpenaiCompatible => EndpointType::OpenaiChatCompletions,
        // The Claude Code CLI speaks Messages upstream, so this is a model vocabulary rather than a path the host issues.
        ProviderFamily::Anthropic | ProviderFamily::ClaudeAgent => EndpointType::AnthropicMessages,
        ProviderFamily::Google | ProviderFamily::Vertex => EndpointType::GoogleGenerative,
        ProviderFamily::Azure => EndpointType::AzureOpenai,
        ProviderFamily::Bedrock => EndpointType::BedrockConverse,
    }
}

/// Required identity fields for this family.
pub fn required_family_settings(family: ProviderFamily) -> &'static [FamilySetting] {
    family.required_settings()
}

/// Whether the chat base URL is derived from identity fields, so an empty Base URL remains valid: Vertex and Bedrock from
/// their identity, Codex from the ChatGPT backend, and Claude Agent leaves it to the local CLI.
pub fn derives_base_url(family: ProviderFamily) -> bool {
    family.derives_base_url()
}

/// Whether this provider has a usable chat endpoint.
pub fn has_usable_base_url(provider: &ApiProvider) -> bool {
    !provider.base_url.trim().is_empty() || derives_base_url(provider.family)
}

/// Whether this provider has a model catalog Fetch models can read. Bedrock and Vertex derive a chat endpoint, but nothing
/// there lists models, so with a blank address their model IDs are added by hand.
pub fn has_model_catalog(provider: &ApiProvider) -> bool {
    !provider.base_url.trim().is_empty() || provider.family.lists_models_without_address()
}

/// Identity fields recognized by this family, including optional ones; decides which inputs Settings displays.
pub fn known_family_settings(family: ProviderFamily) -> &'static [FamilySetting] {
    family.known_settings()
}

/// Capability catalog in chip-rendering order, matching Rust `ModelCapability::CATALOG`.
pub const MODEL_CAPABILITIES: [ModelCapability; 5] = [
    ModelCapability::ImageRecognition,
    ModelCapability::ToolAppend,
    ModelCapability::SystemAppend,
    ModelCapability::AsyncTools,
    ModelCapability::NativeCompaction,
];

/// Reasoning-form catalog in selector-rendering order, matching Rust `ReasoningContent::CATALOG`.
pub const REASONING_CONTENTS: [ReasoningContent; 2] = [ReasoningContent::Plaintext, ReasoningContent::Encrypted];

/// Whether this model reads images.
pub fn supports_vision(model: &ModelProfile) -> bool {
    model.has(ModelCapability::ImageRecognition)
}

/// De-duplicated capabilities in catalog order, dropping whatever isn't one (retired slugs an archive still carries).
pub fn normalize_capabilities(values: &[Value]) -> Vec<ModelCapability> {
    let requested: Vec<ModelCapability> =
        values.iter().filter_map(|value| serde_json::from_value(value.clone()).ok()).collect();
    MODEL_CAPABILITIES.into_iter().filter(|capability| requested.contains(capability)).collect()
}

/// Persistence form of reasoning content. An unrecognized or absent value, including the retired `"auto"` in an older
/// document, resolves against the family here rather than staying deferred. `Value::Null` stands for absent.
pub fn normalize_reasoning_content(value: &Value, family: ProviderFamily) -> ReasoningContent {
    match value.as_str() {
        Some("plaintext") => ReasoningContent::Plaintext,
        Some("encrypted") => ReasoningContent::Encrypted,
        _ if reasoning_content_takes_effect(family) => ReasoningContent::Encrypted,
        _ => ReasoningContent::Plaintext,
    }
}

/// Whether this family has a real consumer for [`ReasoningContent`]; for the others the upstream decides and the stored
/// value is descriptive.
pub fn reasoning_content_takes_effect(family: ProviderFamily) -> bool {
    family.reasoning_content_takes_effect()
}

/// The most pages a PDF may have to go to the model as itself. Mirrors Rust `MAX_NATIVE_PDF_PAGES`.
pub const MAX_NATIVE_PDF_PAGES: u32 = 10;

/// Whether this family and model read a PDF as the document itself rather than its extracted text: the families whose wire
/// has a document part, on a model that takes images. Mirrors Rust `aisdk::step::reads_pdf_documents`.
pub fn reads_pdf_documents(family: ProviderFamily, vision: bool) -> bool {
    vision
        && matches!(
            family,
            ProviderFamily::Anthropic
                | ProviderFamily::ClaudeAgent
                | ProviderFamily::OpenaiResponses
                | ProviderFamily::OpenaiCodex
                | ProviderFamily::Azure
                | ProviderFamily::OpenaiChat
                | ProviderFamily::Google
                | ProviderFamily::Vertex
        )
}

/// Persistence form of the prompt-cache attribute. A boolean has no family-derived default: anything else, including an
/// older document's absent key, resolves to Claude Code's default of enabled.
pub fn normalize_prompt_cache(value: &Value) -> bool {
    value.as_bool().unwrap_or(true)
}

/// Whether the model's `prompt_cache` attribute reaches the wire: only the Messages protocol takes `cache_control` markers.
pub fn prompt_cache_takes_effect(family: ProviderFamily) -> bool {
    family.prompt_cache_takes_effect()
}

/// Models the Messages API takes `tool_addition` from. Mirrors Rust `ANTHROPIC_TOOL_CHANGE_MODELS`.
const ANTHROPIC_TOOL_CHANGE_MODELS: [&str; 5] = ["fable-5", "mythos-5", "opus-5", "opus-4-8", "sonnet-5-5"];
/// Earlier Claude models the same documentation leaves out. Mirrors Rust `ANTHROPIC_EARLIER_MODELS`.
const ANTHROPIC_EARLIER_MODELS: [&str; 7] = ["claude-instant", "claude-2", "claude-3", "opus-4", "sonnet-4", "haiku-4", "haiku-3"];
/// Current models documented without it, as released. Mirrors Rust `ANTHROPIC_RELEASES_WITHOUT`.
const ANTHROPIC_RELEASES_WITHOUT: [&str; 1] = ["sonnet-5"];
/// Models Claude Code appends tools for itself. Mirrors Rust `CLAUDE_CODE_TOOL_CHANGE_MODELS`.
const CLAUDE_CODE_TOOL_CHANGE_MODELS: [&str; 6] = ["fable-5", "mythos-5", "opus-5", "opus-4-8", "sonnet-5-5", "haiku-5-5"];

/// Whether `model_id` names one of `families`, followed by whatever `rest_ok` accepts, the family appearing whole (at the
/// start or after a `-`). Case, `.` for `-` and Claude Code's `[1m]` suffix don't matter. Mirrors Rust
/// `tool_append::names_model_with`.
fn names_model_with(model_id: &str, families: &[&str], rest_ok: impl Fn(&str) -> bool) -> bool {
    let lowered = model_id.to_lowercase().replace('.', "-");
    let id = lowered.strip_suffix("[1m]").unwrap_or(&lowered);
    families.iter().any(|family| {
        let mut from = 0;
        while let Some(found) = id[from..].find(family) {
            let at = from + found;
            if (at == 0 || id.as_bytes()[at - 1] == b'-') && rest_ok(&id[at + family.len()..]) {
                return true;
            }
            // A family starts with an ASCII byte, so one past it is still a character boundary.
            from = at + 1;
        }
        false
    })
}

/// The family followed by the end of the id or another `-` segment. Mirrors Rust `names_model`.
fn names_model(model_id: &str, families: &[&str]) -> bool {
    names_model_with(model_id, families, |rest| rest.is_empty() || rest.starts_with('-'))
}

/// The family as released: the end of the id or a dated snapshot. Mirrors Rust `names_release`.
fn names_release(model_id: &str, families: &[&str]) -> bool {
    names_model_with(model_id, families, |rest| {
        rest.is_empty()
            || (rest.len() == 9 && rest.starts_with('-') && rest[1..].bytes().all(|byte| byte.is_ascii_digit()))
    })
}

/// What `new URL(input).hostname.toLowerCase()` answers, `None` where it throws. The crate has no URL parser and only an
/// exact host can match a vendor, so this keeps the WHATWG rules that decide the host and treats a non-ASCII one (IDNA
/// isn't implemented) as unparseable: a vendor's endpoint is never spelled that way.
fn url_hostname(input: &str) -> Option<String> {
    let cleaned: String =
        input.trim_matches(|c: char| c <= ' ').chars().filter(|c| !matches!(c, '\t' | '\n' | '\r')).collect();
    let (scheme, rest) = cleaned.split_once(':')?;
    if !scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        || !scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return None;
    }
    let scheme = scheme.to_ascii_lowercase();
    let file = scheme == "file";
    let special = matches!(scheme.as_str(), "http" | "https" | "ftp" | "ws" | "wss");
    let is_slash = |c: char| c == '/' || ((special || file) && c == '\\');
    let authority = if special {
        // These schemes ignore how many slashes follow, none included.
        rest.trim_start_matches(is_slash)
    } else {
        let mut chars = rest.chars();
        if !(chars.next().is_some_and(is_slash) && chars.next().is_some_and(is_slash)) {
            // No authority at all (`localhost:8080/v1` is the scheme `localhost`): an empty hostname, not a failure.
            return Some(String::new());
        }
        chars.as_str()
    };
    let end = authority.find(|c: char| is_slash(c) || c == '?' || c == '#').unwrap_or(authority.len());
    let authority = &authority[..end];
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, host_port)| host_port);
    let (host, port) = if host_port.starts_with('[') {
        let close = host_port.find(']')?;
        match host_port[close + 1..].strip_prefix(':') {
            Some(port) => (&host_port[..=close], port),
            None if close + 1 == host_port.len() => (host_port, ""),
            None => return None,
        }
    } else {
        host_port.split_once(':').unwrap_or((host_port, ""))
    };
    if !port.is_empty() && !(port.bytes().all(|byte| byte.is_ascii_digit()) && port.parse::<u16>().is_ok()) {
        return None;
    }
    if host.is_empty() {
        return (!special).then(String::new);
    }
    if host.starts_with('[') {
        // An IPv6 literal is a host but never a vendor's name.
        return Some(host.to_ascii_lowercase());
    }
    let decoded = percent_decode(host)?;
    let forbidden = |c: char| {
        !c.is_ascii()
            || c.is_ascii_control()
            || matches!(c, ' ' | '#' | '%' | '/' | ':' | '<' | '>' | '?' | '@' | '[' | '\\' | ']' | '^' | '|')
    };
    if decoded.chars().any(forbidden) {
        return None;
    }
    Some(decoded.to_ascii_lowercase())
}

/// `%XX` escapes decoded; a stray `%` stays, and the caller refuses it as a forbidden host character.
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let escape = (bytes[at] == b'%')
            .then(|| text.get(at + 1..at + 3).and_then(|hex| u8::from_str_radix(hex, 16).ok()))
            .flatten();
        match escape {
            Some(byte) => {
                out.push(byte);
                at += 3;
            }
            None => {
                out.push(bytes[at]);
                at += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// Whether `base_url` is the vendor's own endpoint for `family`; empty is the provider's default, which is. Mirrors Rust
/// `host_append::at_vendor_endpoint`.
pub fn at_vendor_endpoint(family: ProviderFamily, base_url: &str) -> bool {
    let trimmed = base_url.trim();
    if trimmed.is_empty() {
        return true;
    }
    let Some(host) = url_hostname(trimmed) else {
        return false;
    };
    match family {
        ProviderFamily::Anthropic => host == "api.anthropic.com",
        ProviderFamily::OpenaiResponses | ProviderFamily::OpenaiCodex | ProviderFamily::Azure | ProviderFamily::OpenaiChat => {
            host == "api.openai.com"
                || host == "chatgpt.com"
                || [".openai.azure.com", ".cognitiveservices.azure.com", ".services.ai.azure.com"]
                    .iter()
                    .any(|suffix| host.ends_with(suffix))
        }
        _ => false,
    }
}

/// What Anthropic documents for this model at its own endpoint. Mirrors Rust `tool_append::anthropic_documents`.
fn anthropic_documents(model_id: &str) -> Option<bool> {
    if names_model(model_id, &ANTHROPIC_TOOL_CHANGE_MODELS) {
        return Some(true);
    }
    if names_model(model_id, &ANTHROPIC_EARLIER_MODELS) || names_release(model_id, &ANTHROPIC_RELEASES_WITHOUT) {
        return Some(false);
    }
    None
}

/// What Mewrk knows about this model taking a tool mid-conversation at this endpoint; `None` is the user's to say (a
/// relay, a model the vendor's documentation doesn't cover). Mirrors Rust `tool_append::known`.
pub fn known_tool_append(family: ProviderFamily, base_url: &str, model_id: &str) -> Option<bool> {
    let vendor = at_vendor_endpoint(family, base_url);
    match family {
        ProviderFamily::OpenaiCodex => Some(true),
        ProviderFamily::OpenaiResponses | ProviderFamily::Azure => vendor.then_some(true),
        ProviderFamily::Anthropic => {
            if vendor {
                anthropic_documents(model_id)
            } else {
                None
            }
        }
        ProviderFamily::ClaudeAgent => Some(names_model(model_id, &CLAUDE_CODE_TOOL_CHANGE_MODELS)),
        _ => Some(false),
    }
}

/// What Mewrk knows about this model taking a system message mid-conversation at this endpoint; `None` is the user's to
/// say. Mirrors Rust `system_append::known`.
pub fn known_system_append(family: ProviderFamily, base_url: &str, model_id: &str) -> Option<bool> {
    let vendor = at_vendor_endpoint(family, base_url);
    match family {
        ProviderFamily::OpenaiResponses | ProviderFamily::OpenaiCodex | ProviderFamily::Azure => Some(true),
        ProviderFamily::OpenaiChat => vendor.then_some(true),
        ProviderFamily::Anthropic => {
            if vendor {
                anthropic_documents(model_id)
            } else {
                None
            }
        }
        ProviderFamily::OpenaiCompatible | ProviderFamily::Xai => None,
        _ => Some(false),
    }
}

/// The OpenAI model generation an id names. Mirrors Rust `async_tools::openai_version`.
enum OpenaiVersion {
    Before6,
    Numbered { major: u64, minor: u64, variant: String },
}

fn openai_version(model_id: &str) -> Option<OpenaiVersion> {
    let id = model_id.trim().to_lowercase();
    if ["o1", "o3", "o4", "chatgpt-", "codex-"].iter().any(|prefix| id.starts_with(prefix)) {
        return Some(OpenaiVersion::Before6);
    }
    let rest = id.strip_prefix("gpt-")?;
    let (major, rest) = leading_number(rest)?;
    // `.` must be followed by a digit to be a minor version; otherwise it is part of the variant.
    let (minor, variant) = rest.strip_prefix('.').and_then(leading_number).unwrap_or((0, rest));
    // Upstream's `(.*)$` doesn't cross a line break, so an id holding one names no generation.
    if variant.contains(['\n', '\r', '\u{2028}', '\u{2029}']) {
        return None;
    }
    if major <= 5 {
        return Some(OpenaiVersion::Before6);
    }
    Some(OpenaiVersion::Numbered { major, minor, variant: variant.to_owned() })
}

/// The run of ASCII digits `text` opens with and what follows; `None` when it opens with none.
fn leading_number(text: &str) -> Option<(u64, &str)> {
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    (digits > 0).then(|| (text[..digits].parse().unwrap_or(u64::MAX), &text[digits..]))
}

/// What OpenAI documents about this model taking asynchronous tool calls: GPT-6 Astra and every later model do, GPT
/// generations before 6 and the o-series don't, and GPT-6 Sol and Luna, older than Astra and named by nothing, are the
/// user's to say. Mirrors Rust `async_tools::openai_documents`.
fn openai_documents_async_tools(model_id: &str) -> Option<bool> {
    match openai_version(model_id)? {
        OpenaiVersion::Before6 => Some(false),
        OpenaiVersion::Numbered { major: 6, minor: 0, variant } => {
            (variant == "-astra" || variant.starts_with("-astra-")).then_some(true)
        }
        OpenaiVersion::Numbered { .. } => Some(true),
    }
}

/// Whether the ChatGPT Codex backend takes asynchronous calls from this model, as measured on 2026-10-05: every GPT-6
/// model does, every GPT-5 model refuses them. Mirrors Rust `async_tools::codex_backend_takes`.
fn codex_backend_takes_async_tools(model_id: &str) -> Option<bool> {
    openai_version(model_id).map(|version| !matches!(version, OpenaiVersion::Before6))
}

/// What Mewrk knows about this model taking asynchronous tool calls at this endpoint; `None` is the user's to say (a relay,
/// Azure's deployment names). Only the Responses protocol has them. Mirrors Rust `async_tools::known`.
pub fn known_async_tools(family: ProviderFamily, base_url: &str, model_id: &str) -> Option<bool> {
    match family {
        ProviderFamily::OpenaiResponses => {
            if at_vendor_endpoint(family, base_url) {
                openai_documents_async_tools(model_id)
            } else {
                None
            }
        }
        ProviderFamily::OpenaiCodex => codex_backend_takes_async_tools(model_id),
        ProviderFamily::Azure => None,
        _ => Some(false),
    }
}

/// What OpenAI documents about this model compacting natively: every GPT from 5 on does; anything else is the user's to
/// say. Mirrors Rust `native_compaction::openai_documents`.
fn openai_documents_native_compaction(model_id: &str) -> Option<bool> {
    let id = model_id.trim().to_lowercase();
    let (major, _) = leading_number(id.strip_prefix("gpt-")?)?;
    (major >= 5).then_some(true)
}

/// What Mewrk knows about this model compacting natively at this endpoint; `None` is the user's to say (a relay, Azure's
/// deployment names). Codex compacts this way with every model its backend serves. Mirrors Rust `native_compaction::known`.
pub fn known_native_compaction(family: ProviderFamily, base_url: &str, model_id: &str) -> Option<bool> {
    match family {
        ProviderFamily::OpenaiResponses => {
            if at_vendor_endpoint(family, base_url) {
                openai_documents_native_compaction(model_id)
            } else {
                None
            }
        }
        ProviderFamily::OpenaiCodex => Some(true),
        ProviderFamily::Azure => None,
        _ => Some(false),
    }
}

/// The protocol capabilities Mewrk declares for a model it fills in itself: those it knows the model has at this endpoint.
/// Takes the two fields because the model drawer asks while a provider is still being edited. Mirrors Rust
/// `model_discovery::known_protocol_capabilities`.
pub fn known_protocol_capabilities(family: ProviderFamily, base_url: &str, model_id: &str) -> Vec<ModelCapability> {
    let known = [
        (known_tool_append(family, base_url, model_id), ModelCapability::ToolAppend),
        (known_system_append(family, base_url, model_id), ModelCapability::SystemAppend),
        (known_async_tools(family, base_url, model_id), ModelCapability::AsyncTools),
        (known_native_compaction(family, base_url, model_id), ModelCapability::NativeCompaction),
    ];
    known.into_iter().filter(|(answer, _)| *answer == Some(true)).map(|(_, capability)| capability).collect()
}

/// Whether this family has a tool-append interface, so the `tool_append` capability is read.
pub fn tool_append_takes_effect(family: ProviderFamily) -> bool {
    family.tool_append_takes_effect()
}

/// Whether this family can carry a system message mid-conversation, so the `system_append` capability is read.
pub fn system_append_takes_effect(family: ProviderFamily) -> bool {
    family.system_append_takes_effect()
}

/// Whether this family can mark a function tool asynchronous, so the `async_tools` capability is read.
pub fn async_tools_take_effect(family: ProviderFamily) -> bool {
    family.async_tools_take_effect()
}

/// Whether this family can compact a conversation's context natively, so the `native_compaction` capability is read.
pub fn native_compaction_takes_effect(family: ProviderFamily) -> bool {
    family.native_compaction_takes_effect()
}

/// Whether a conversation on this model compacts natively: the model declares `native_compaction` and its protocol has the
/// interface. Mirrors Rust `native_compaction::takes_native_compaction`.
pub fn takes_native_compaction(provider: &ApiProvider, model: &ModelProfile) -> bool {
    native_compaction_takes_effect(provider.family) && model.has(ModelCapability::NativeCompaction)
}

/// Whether a conversation on this model can take a tool mid-conversation at all: the model declares `tool_append` and its
/// protocol has an append interface. Where it can't, an added tool is folded back into the declared list, so while the
/// cache is warm the settings draw the whole surface orange, and auto-compact and MCP tool discovery, which both add tools
/// mid-run, are unavailable. Mirrors Rust `tool_append::appends_tools`.
pub fn appends_tools(provider: &ApiProvider, model: &ModelProfile) -> bool {
    tool_append_takes_effect(provider.family) && model.has(ModelCapability::ToolAppend)
}

/// Whether a reasoning card is encrypted and therefore removable but not editable. `form` is authoritative; its absence
/// falls back to an empty body for legacy cards, so all reads go through here to keep that consistent.
pub fn is_encrypted_reasoning(form: Option<ReasoningForm>, content: Option<&str>) -> bool {
    match form {
        None => content.unwrap_or("").is_empty(),
        Some(form) => form == ReasoningForm::Encrypted,
    }
}

/// Display name, falling back to the ID.
pub fn model_display_name(model: &ModelProfile) -> &str {
    let name = model.name.trim();
    if name.is_empty() {
        &model.id
    } else {
        name
    }
}

/// Repairs `active_model_id` to a model that still exists: a removed model can leave the id dangling, which storage
/// validation rejects on save.
pub fn repair_active_model_id(provider: &ApiProvider) -> Option<String> {
    let current = provider.models.iter().find(|model| Some(&model.id) == provider.active_model_id.as_ref());
    current.or(provider.models.first()).map(|model| model.id.clone())
}

/// A model's group, derived when `group` is empty: slash-separated IDs use their first segment, flat IDs their family
/// prefix. Must match the host's `derive_model_group_name`, so models don't move between collapsed groups around discovery.
pub fn model_group(model: &ModelProfile) -> String {
    let explicit = model.group.trim();
    if !explicit.is_empty() {
        return explicit.to_owned();
    }
    let id = model.id.trim();
    if let Some((head, _)) = id.split_once('/') {
        return head.trim().to_owned();
    }
    let family = id.split('-').next().unwrap_or("").trim();
    if family == id {
        String::new()
    } else {
        family.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::{HashMap, HashSet};

    const MODEL_SOURCE: &str = include_str!("../../mewrk-host/host/src/model.rs");
    const TOOL_APPEND_SOURCE: &str = include_str!("../../mewrk-host/host/src/tool_append.rs");

    /// ProviderFamily variant names mapped to values, the catalog itself being `#[cfg(test)]` in the host crate.
    const FAMILIES: [(&str, ProviderFamily); 11] = [
        ("OpenaiResponses", ProviderFamily::OpenaiResponses),
        ("OpenaiCodex", ProviderFamily::OpenaiCodex),
        ("OpenaiChat", ProviderFamily::OpenaiChat),
        ("Anthropic", ProviderFamily::Anthropic),
        ("ClaudeAgent", ProviderFamily::ClaudeAgent),
        ("Google", ProviderFamily::Google),
        ("Xai", ProviderFamily::Xai),
        ("Azure", ProviderFamily::Azure),
        ("Bedrock", ProviderFamily::Bedrock),
        ("Vertex", ProviderFamily::Vertex),
        ("OpenaiCompatible", ProviderFamily::OpenaiCompatible),
    ];

    fn family_named(variant: &str) -> ProviderFamily {
        FAMILIES.iter().find(|(name, _)| *name == variant).unwrap_or_else(|| panic!("{variant} is not in the family table")).1
    }

    fn model(id: &str, group: &str) -> ModelProfile {
        ModelProfile { group: group.to_owned(), ..serde_json::from_value(json!({ "id": id })).unwrap() }
    }

    fn with_capabilities(capabilities: &[ModelCapability]) -> ModelProfile {
        ModelProfile { capabilities: capabilities.iter().copied().collect(), ..model("m", "") }
    }

    fn provider(family: ProviderFamily, base_url: &str) -> ApiProvider {
        serde_json::from_value(json!({ "id": "p", "name": "P", "family": family, "baseUrl": base_url })).unwrap()
    }

    fn provider_with(models: Vec<ModelProfile>, active_model_id: Option<&str>) -> ApiProvider {
        ApiProvider { models, active_model_id: active_model_id.map(str::to_owned), ..provider(ProviderFamily::Anthropic, "") }
    }

    /// The body of an `impl <Name> {` block. Brace counting skips quoted strings because comments may contain braces.
    fn impl_block(source: &'static str, name: &str) -> &'static str {
        let header = format!("impl {name} {{");
        let start = source.find(&header).unwrap_or_else(|| panic!("no impl {name} in model.rs"));
        let bytes = source.as_bytes();
        let (mut depth, mut in_string, mut at) = (0, false, start + header.len() - 1);
        while at < bytes.len() {
            match bytes[at] {
                b'\\' if in_string => at += 1,
                b'"' => in_string = !in_string,
                b'{' if !in_string => depth += 1,
                b'}' if !in_string => {
                    depth -= 1;
                    if depth == 0 {
                        return &source[start..at];
                    }
                }
                _ => {}
            }
            at += 1;
        }
        panic!("impl {name} is never closed");
    }

    fn identifier(text: &str) -> &str {
        &text[..text.find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(text.len())]
    }

    /// Every `Self::Variant` in `text`, in order.
    fn self_variants(text: &'static str) -> impl Iterator<Item = &'static str> {
        text.split("Self::").skip(1).map(identifier)
    }

    /// Mapping of `Self::Variant => "slug",`.
    fn slug_map(block: &'static str) -> HashMap<&'static str, &'static str> {
        block
            .split("Self::")
            .skip(1)
            .filter_map(|piece| {
                let name = identifier(piece);
                let slug = piece[name.len()..].trim_start().strip_prefix("=>")?.trim_start().strip_prefix('"')?;
                Some((name, &slug[..slug.find('"')?]))
            })
            .collect()
    }

    /// Variant order in a `pub const NAME: &'static [Self] = &[ Self::A, Self::B ];`.
    fn const_variants(block: &'static str, name: &str) -> Vec<&'static str> {
        let after = block.split_once(&format!("const {name}:")).unwrap_or_else(|| panic!("no {name}")).1;
        let list = after.split_once("= &[").unwrap().1;
        self_variants(&list[..list.find(']').unwrap()]).collect()
    }

    fn rust_catalog(name: &str) -> Vec<&'static str> {
        let block = impl_block(MODEL_SOURCE, name);
        let slugs = slug_map(block);
        const_variants(block, "CATALOG")
            .into_iter()
            .map(|variant| *slugs.get(variant).unwrap_or_else(|| panic!("{name}::{variant} has no slug")))
            .collect()
    }

    /// Variant names declared by `pub enum <Name> { ... }`, skipping docs and attributes.
    fn enum_variants(name: &str) -> Vec<&'static str> {
        let header = format!("pub enum {name} {{");
        let start = MODEL_SOURCE.find(&header).unwrap_or_else(|| panic!("no pub enum {name} in model.rs")) + header.len();
        let end = start + MODEL_SOURCE[start..].find("\n}").unwrap();
        MODEL_SOURCE[start..end]
            .lines()
            .map(|line| line.trim().trim_end_matches(','))
            .filter(|line| {
                line.starts_with(|c: char| c.is_ascii_uppercase()) && line.chars().all(|c| c.is_alphanumeric() || c == '_')
            })
            .collect()
    }

    /// The families a `fn name(self) -> bool` in `model.rs` names.
    fn rust_true(name: &str) -> HashSet<&'static str> {
        let after = MODEL_SOURCE.split_once(&format!("fn {name}(self) -> bool {{")).unwrap_or_else(|| panic!("no {name} in model.rs")).1;
        self_variants(&after[..after.find("\n    }").unwrap()]).collect()
    }

    fn slug<T: serde::Serialize>(value: T) -> String {
        serde_json::to_value(value).unwrap().as_str().unwrap().to_owned()
    }

    /// `gate` answers yes for exactly `expected` (upstream's table), and so does the host's body of `name`.
    fn assert_gate(name: &str, gate: fn(ProviderFamily) -> bool, expected: &[&str]) {
        assert_eq!(rust_true(name), expected.iter().copied().collect::<HashSet<_>>(), "{name}");
        for (variant, family) in FAMILIES {
            assert_eq!(gate(family), expected.contains(&variant), "{name} ProviderFamily::{variant}");
        }
    }

    mod known_family_settings_table {
        use super::*;

        #[test]
        fn has_no_settings_for_the_bundled_claude_agent() {
            assert!(known_family_settings(ProviderFamily::ClaudeAgent).is_empty());
        }

        #[test]
        fn matches_upstreams_identity_fields_for_every_family() {
            assert_eq!(known_family_settings(ProviderFamily::Bedrock), [FamilySetting::Region]);
            assert_eq!(known_family_settings(ProviderFamily::Vertex), [FamilySetting::Project, FamilySetting::Location]);
            assert_eq!(known_family_settings(ProviderFamily::Azure), [FamilySetting::ApiVersion]);
            assert_eq!(required_family_settings(ProviderFamily::Bedrock), [FamilySetting::Region]);
            assert_eq!(required_family_settings(ProviderFamily::Vertex), [FamilySetting::Project, FamilySetting::Location]);
            assert!(required_family_settings(ProviderFamily::Azure).is_empty());
        }
    }

    mod model_group_derivation {
        use super::*;

        #[test]
        fn prefers_an_explicit_group_over_anything_inferred() {
            assert_eq!(model_group(&model("Qwen/Qwen3-8B", "  我的分组  ")), "我的分组");
        }

        // This table mirrors `model_registry.rs::derive_model_group_name` exactly. Discovery writes `group`; this fallback
        // must use the same rule so models don't move between collapsed groups before and after discovery.
        #[test]
        fn derives_a_group_from_the_id() {
            for (id, expected) in [
                ("openai/gpt-4o", "openai"),
                ("Qwen/Qwen3-8B", "Qwen"),
                ("deepseek-ai/DeepSeek-V3", "deepseek-ai"),
                ("deepseek-v4-pro", "deepseek"),
                ("gpt-4o-mini", "gpt"),
                ("claude-sonnet-4-5", "claude"),
                // An id whose first segment is the whole id has no group; the view puts it in Other.
                ("grok", ""),
                ("o3", ""),
                ("", ""),
            ] {
                assert_eq!(model_group(&model(id, "")), expected, "{id}");
            }
        }
    }

    mod vocabularies_mirror_rust {
        use super::*;

        #[test]
        fn mirrors_endpoint_type_catalog_exactly_including_order() {
            assert_eq!(ENDPOINT_TYPES.map(slug).to_vec(), rust_catalog("EndpointType"));
        }

        #[test]
        fn mirrors_model_capability_catalog_exactly_including_order() {
            assert_eq!(MODEL_CAPABILITIES.map(slug).to_vec(), rust_catalog("ModelCapability"));
        }

        #[test]
        fn mirrors_reasoning_content_catalog_exactly_including_order() {
            assert_eq!(REASONING_CONTENTS.map(slug).to_vec(), rust_catalog("ReasoningContent"));
        }

        /// The mirrors above read `CATALOG`, so a variant declared outside its catalog would pass them while still reaching
        /// the wire through serde. The vocabularies were deliberately shrunk, so pin the declarations themselves too.
        #[test]
        fn leaves_no_rust_variant_outside_its_catalog() {
            for name in ["EndpointType", "ModelCapability", "ReasoningContent"] {
                assert_eq!(enum_variants(name), const_variants(impl_block(MODEL_SOURCE, name), "CATALOG"), "{name}");
            }
        }

        #[test]
        fn mirrors_provider_family_chat_endpoint_for_every_format() {
            let block = impl_block(MODEL_SOURCE, "ProviderFamily");
            let body = block.split_once("fn chat_endpoint(self) -> EndpointType {").unwrap().1;
            let body = &body[..body.find("\n    }").unwrap()];
            let endpoint_slugs = slug_map(impl_block(MODEL_SOURCE, "EndpointType"));
            // Each arm's variants come before its `EndpointType::X`, however the left side is wrapped.
            let mut arms = body.split("EndpointType::");
            let mut pending: Vec<&str> = self_variants(arms.next().unwrap()).collect();
            let mut seen = 0;
            for piece in arms {
                let endpoint = identifier(piece);
                for variant in pending.drain(..) {
                    assert_eq!(slug(chat_endpoint_of(family_named(variant))), endpoint_slugs[endpoint], "{variant}");
                    seen += 1;
                }
                pending = self_variants(&piece[endpoint.len()..]).collect();
            }
            assert_eq!(seen, FAMILIES.len());
        }

        #[test]
        fn normalizes_to_catalog_order_and_drops_unknown_entries() {
            // `rerank`, `function_call` and `reasoning` are retired slugs an archived document still carries; they must
            // normalize away rather than survive.
            assert!(normalize_capabilities(&[json!("rerank"), json!("function_call"), json!("reasoning"), json!("nope"), json!(7)]).is_empty());
            assert_eq!(
                normalize_capabilities(&[json!("image_recognition"), json!("image_recognition")]),
                [ModelCapability::ImageRecognition]
            );
            assert_eq!(
                normalize_capabilities(&[json!("native_compaction"), json!("tool_append")]),
                [ModelCapability::ToolAppend, ModelCapability::NativeCompaction]
            );
        }
    }

    mod reasoning_form {
        use super::*;

        #[test]
        fn mirrors_provider_family_reasoning_content_takes_effect_for_every_family() {
            assert_gate("reasoning_content_takes_effect", reasoning_content_takes_effect, &["OpenaiResponses", "OpenaiCodex", "Azure"]);
        }

        /// The normalizer, not a later resolver, is where an absent, retired or unrecognized value acquires a form. `"auto"`
        /// was the retired third variant and still sits in archives.
        #[test]
        fn resolves_an_absent_retired_or_unrecognized_value_by_family() {
            for (variant, family) in FAMILIES {
                let expected = if reasoning_content_takes_effect(family) { ReasoningContent::Encrypted } else { ReasoningContent::Plaintext };
                assert_eq!(normalize_reasoning_content(&Value::Null, family), expected, "{variant} absent");
                assert_eq!(normalize_reasoning_content(&json!("auto"), family), expected, "{variant} auto");
                assert_eq!(normalize_reasoning_content(&json!("nonsense"), family), expected, "{variant} unknown");
                assert_eq!(normalize_reasoning_content(&json!("plaintext"), family), ReasoningContent::Plaintext, "{variant}");
                assert_eq!(normalize_reasoning_content(&json!("encrypted"), family), ReasoningContent::Encrypted, "{variant}");
            }
            // These two sit on opposite sides of the table, so the loop can't pass with a constant fallback.
            assert_eq!(normalize_reasoning_content(&Value::Null, ProviderFamily::OpenaiResponses), ReasoningContent::Encrypted);
            assert_eq!(normalize_reasoning_content(&Value::Null, ProviderFamily::Anthropic), ReasoningContent::Plaintext);
        }

        /// A card's `form` is authoritative; only an absent form falls back to content. An empty plaintext card remains
        /// editable, unlike an encrypted card.
        #[test]
        fn reads_the_cards_own_form_first_and_only_then_falls_back_to_emptiness() {
            assert!(is_encrypted_reasoning(Some(ReasoningForm::Encrypted), Some("有摘要")));
            assert!(!is_encrypted_reasoning(Some(ReasoningForm::Plaintext), Some("")));
            assert!(!is_encrypted_reasoning(Some(ReasoningForm::Plaintext), None));
            // Cards written before `form` existed fall back to the legacy heuristic.
            assert!(is_encrypted_reasoning(None, Some("")));
            assert!(is_encrypted_reasoning(None, None));
            assert!(!is_encrypted_reasoning(None, Some("想法")));
        }
    }

    mod prompt_cache {
        use super::*;

        #[test]
        fn mirrors_provider_family_prompt_cache_takes_effect_for_every_family() {
            assert_gate("prompt_cache_takes_effect", prompt_cache_takes_effect, &["Anthropic"]);
            assert!(prompt_cache_takes_effect(ProviderFamily::Anthropic));
            assert!(!prompt_cache_takes_effect(ProviderFamily::Bedrock));
        }

        /// An absent key (older documents) or a malformed value acquires Claude Code's default of enabled. Only a real boolean
        /// is the user's own choice.
        #[test]
        fn defaults_anything_but_a_boolean_to_enabled() {
            assert!(normalize_prompt_cache(&Value::Null));
            assert!(normalize_prompt_cache(&json!("false")));
            assert!(normalize_prompt_cache(&json!(0)));
            assert!(normalize_prompt_cache(&json!(true)));
            assert!(!normalize_prompt_cache(&json!(false)));
        }
    }

    mod chat_base_url_readiness {
        use super::*;

        /// A provider whose chat endpoint is derived keeps an empty base URL on purpose.
        #[test]
        fn mirrors_provider_family_derives_base_url_for_every_family() {
            assert_gate("derives_base_url", derives_base_url, &["Vertex", "Bedrock", "OpenaiCodex", "ClaudeAgent"]);
        }

        /// The deriving families accept an empty Base URL; a key-based family has nowhere to derive an endpoint from. Whitespace
        /// counts as empty on both branches.
        #[test]
        fn accepts_an_empty_base_url_only_from_the_deriving_families() {
            for family in [ProviderFamily::Vertex, ProviderFamily::Bedrock, ProviderFamily::OpenaiCodex, ProviderFamily::ClaudeAgent] {
                assert!(has_usable_base_url(&provider(family, "")), "{family:?}");
                assert!(has_usable_base_url(&provider(family, "   ")), "{family:?}");
                // An override still points the derived family at a loopback test double.
                assert!(has_usable_base_url(&provider(family, "http://127.0.0.1:1420")), "{family:?}");
            }
            for family in [ProviderFamily::OpenaiChat, ProviderFamily::OpenaiResponses, ProviderFamily::Anthropic] {
                assert!(!has_usable_base_url(&provider(family, "")), "{family:?}");
                assert!(!has_usable_base_url(&provider(family, "   ")), "{family:?}");
                assert!(has_usable_base_url(&provider(family, "https://api.openai.com/v1")), "{family:?}");
            }
        }

        #[test]
        fn lists_models_from_an_address_or_where_the_family_needs_none() {
            assert!(has_model_catalog(&provider(ProviderFamily::OpenaiCodex, "")));
            assert!(has_model_catalog(&provider(ProviderFamily::ClaudeAgent, "  ")));
            assert!(!has_model_catalog(&provider(ProviderFamily::Bedrock, "")));
            assert!(has_model_catalog(&provider(ProviderFamily::Bedrock, "https://bedrock.example.com")));
        }
    }

    mod active_model_repair {
        use super::*;

        #[test]
        fn keeps_the_active_model_untouched_while_it_is_still_in_the_list() {
            let provider = provider_with(vec![model("a", ""), model("b", "")], Some("b"));
            assert_eq!(repair_active_model_id(&provider).as_deref(), Some("b"));
        }

        #[test]
        fn falls_back_to_the_first_model_once_the_active_one_is_uninstalled() {
            // Two survivors defeat an implementation that reaches for the last entry.
            let provider = provider_with(vec![model("first", ""), model("second", "")], Some("gone"));
            assert_eq!(repair_active_model_id(&provider).as_deref(), Some("first"));
        }

        #[test]
        fn returns_null_only_when_the_provider_has_no_models_left() {
            assert_eq!(repair_active_model_id(&provider_with(Vec::new(), Some("gone"))), None);
            assert_eq!(repair_active_model_id(&provider_with(Vec::new(), None)), None);
        }
    }

    mod append_capabilities {
        use super::*;

        fn answer(text: &str) -> Option<bool> {
            match text {
                "None" => None,
                "Some(true)" => Some(true),
                "Some(false)" => Some(false),
                other => panic!("unexpected fixture answer {other}"),
            }
        }

        /// Mewrk declares a model's capabilities from what it knows of the model at its endpoint, in the host's projection of a
        /// fetch and in the drawer as an ID is typed. Both sides answer the one fixture `tool_append.rs` checks itself against.
        #[test]
        fn knows_what_the_hosts_fixture_says_it_knows_and_leaves_the_rest_to_the_user() {
            let after = TOOL_APPEND_SOURCE.split_once("const APPEND_FIXTURE").unwrap().1;
            let after = after.split_once("&[\n").unwrap().1;
            let block = &after[..after.find("\n        ]").unwrap()];
            let mut cases = 0;
            for line in block.lines() {
                // `(Family, "url", "model", Some(true), None, Some(false), Some(false)),`
                let line = line.trim().strip_prefix('(').and_then(|line| line.strip_suffix("),")).unwrap();
                let fields: Vec<&str> = line.split(", ").collect();
                let [variant, base_url, model_id, tool, system, asynchronous, compacting] = fields[..] else {
                    panic!("unexpected fixture line {line}");
                };
                let family = family_named(variant);
                let (base_url, model_id) = (base_url.trim_matches('"'), model_id.trim_matches('"'));
                assert_eq!(known_tool_append(family, base_url, model_id), answer(tool), "tool: {variant} {base_url} {model_id}");
                assert_eq!(known_system_append(family, base_url, model_id), answer(system), "system: {variant} {base_url} {model_id}");
                assert_eq!(known_async_tools(family, base_url, model_id), answer(asynchronous), "async: {variant} {base_url} {model_id}");
                assert_eq!(known_native_compaction(family, base_url, model_id), answer(compacting), "compaction: {variant} {base_url} {model_id}");
                cases += 1;
            }
            assert!(cases > 20, "{cases}");
        }

        #[test]
        fn mirrors_provider_familys_tool_append_system_append_async_tools_and_native_compaction_gates() {
            let (responses, codex, chat, compatible) = ("OpenaiResponses", "OpenaiCodex", "OpenaiChat", "OpenaiCompatible");
            assert_gate("tool_append_takes_effect", tool_append_takes_effect, &["Anthropic", responses, codex, "Azure", "ClaudeAgent"]);
            assert_gate("system_append_takes_effect", system_append_takes_effect, &["Anthropic", responses, codex, "Azure", chat, compatible, "Xai"]);
            assert_gate("async_tools_take_effect", async_tools_take_effect, &[responses, codex, "Azure"]);
            assert_gate("native_compaction_takes_effect", native_compaction_takes_effect, &[responses, codex, "Azure"]);
        }

        #[test]
        fn reads_the_declared_capability_and_only_where_the_protocol_can_append() {
            let declared = with_capabilities(&[ModelCapability::ToolAppend, ModelCapability::SystemAppend]);
            assert!(appends_tools(&provider(ProviderFamily::Anthropic, ""), &declared));
            assert!(!appends_tools(&provider(ProviderFamily::Anthropic, ""), &with_capabilities(&[])));
            assert!(!appends_tools(&provider(ProviderFamily::OpenaiCompatible, ""), &declared));
        }

        #[test]
        fn declares_what_it_knows_and_nothing_for_a_relay() {
            use ModelCapability::{AsyncTools, NativeCompaction, SystemAppend, ToolAppend};
            let official = "https://api.anthropic.com/v1";
            let relay = "https://relay.example.com/v1";
            assert_eq!(known_protocol_capabilities(ProviderFamily::Anthropic, official, "claude-opus-5-5"), [ToolAppend, SystemAppend]);
            assert!(known_protocol_capabilities(ProviderFamily::Anthropic, official, "claude-sonnet-5").is_empty());
            assert!(known_protocol_capabilities(ProviderFamily::Anthropic, relay, "claude-opus-5-5").is_empty());
            assert_eq!(known_protocol_capabilities(ProviderFamily::OpenaiResponses, relay, "gpt-5.5"), [SystemAppend]);
            assert_eq!(
                known_protocol_capabilities(ProviderFamily::OpenaiResponses, "", "gpt-6-astra"),
                [ToolAppend, SystemAppend, AsyncTools, NativeCompaction]
            );
        }

        #[test]
        fn compacts_natively_only_where_the_model_declares_it_and_the_protocol_has_the_interface() {
            let declared = with_capabilities(&[ModelCapability::NativeCompaction]);
            assert!(takes_native_compaction(&provider(ProviderFamily::OpenaiCodex, ""), &declared));
            assert!(!takes_native_compaction(&provider(ProviderFamily::OpenaiCodex, ""), &with_capabilities(&[])));
            assert!(!takes_native_compaction(&provider(ProviderFamily::Anthropic, ""), &declared));
        }
    }

    /// Not an upstream case: `new URL` has no equivalent in this crate, so its answers are pinned here.
    #[test]
    fn reads_an_endpoints_host_the_way_new_url_does() {
        for (url, host) in [
            ("https://api.openai.com/v1", Some("api.openai.com")),
            ("HTTPS://API.OpenAI.com", Some("api.openai.com")),
            ("https://user:secret@api.openai.com:443/v1?x=1#y", Some("api.openai.com")),
            ("https:api.openai.com", Some("api.openai.com")),
            ("https:\\\\api.openai.com\\v1", Some("api.openai.com")),
            ("https://%61pi.openai.com", Some("api.openai.com")),
            ("foo://Api.OpenAI.com/x", Some("api.openai.com")),
            ("https://[::1]:8080/", Some("[::1]")),
            ("localhost:8080/v1", Some("")),
            ("api.openai.com/v1", None),
            ("https://", None),
            ("https://api.openai.com:notaport/", None),
            ("https://api.openai.com:99999/", None),
            ("https://a b.openai.azure.com/", None),
            ("1https://api.openai.com", None),
        ] {
            assert_eq!(url_hostname(url).as_deref(), host, "{url}");
        }
        assert!(at_vendor_endpoint(ProviderFamily::Azure, "https://mine.services.ai.azure.com/x"));
        assert!(at_vendor_endpoint(ProviderFamily::OpenaiChat, "https://chatgpt.com"));
        assert!(!at_vendor_endpoint(ProviderFamily::Anthropic, "https://api.openai.com"));
        assert!(!at_vendor_endpoint(ProviderFamily::Azure, "https://notopenai.azure.com"));
        assert!(!at_vendor_endpoint(ProviderFamily::Google, "https://generativelanguage.googleapis.com"));
    }

    /// Not an upstream case: pins the parts of the id grammars the host's fixture doesn't reach.
    #[test]
    fn reads_model_ids_the_way_upstreams_patterns_do() {
        assert!(names_model("us.anthropic.claude-opus-4.8-v1", &ANTHROPIC_TOOL_CHANGE_MODELS));
        assert!(names_model("CLAUDE-OPUS-5[1m]", &ANTHROPIC_TOOL_CHANGE_MODELS));
        assert!(!names_model("xopus-5", &ANTHROPIC_TOOL_CHANGE_MODELS));
        assert!(!names_model("opus-55", &ANTHROPIC_TOOL_CHANGE_MODELS));
        assert!(names_release("claude-sonnet-5-20260115", &ANTHROPIC_RELEASES_WITHOUT));
        assert!(!names_release("claude-sonnet-5-2026011", &ANTHROPIC_RELEASES_WITHOUT));
        assert_eq!(openai_documents_async_tools("o3-mini"), Some(false));
        assert_eq!(openai_documents_async_tools("gpt-5.6"), Some(false));
        assert_eq!(openai_documents_async_tools("gpt-6"), None);
        assert_eq!(openai_documents_async_tools("gpt-6-astra-2026"), Some(true));
        assert_eq!(openai_documents_async_tools("gpt-6.x"), None);
        assert_eq!(openai_documents_async_tools("gpt-7"), Some(true));
        assert_eq!(openai_documents_async_tools("gpt-6-astra\nx"), None);
        assert_eq!(openai_documents_async_tools("claude"), None);
        assert_eq!(openai_documents_native_compaction("GPT-5"), Some(true));
        assert_eq!(openai_documents_native_compaction("gpt-4o"), None);
        assert_eq!(model_display_name(&ModelProfile { name: "  Nice  ".into(), ..model("id", "") }), "Nice");
        assert_eq!(model_display_name(&model("id", "")), "id");
        assert!(reads_pdf_documents(ProviderFamily::Anthropic, true));
        assert!(!reads_pdf_documents(ProviderFamily::Anthropic, false));
        assert!(!reads_pdf_documents(ProviderFamily::Bedrock, true));
    }
}
