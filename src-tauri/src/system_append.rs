//! System prompts appended mid-conversation.
//!
//! An instruction that starts to apply partway through a conversation — plan
//! mode turned on or off again, the request to hand off, the notes a
//! continuation was handed — used to have one place to go: the system
//! prompt, which heads every request, so changing it threw away the prompt
//! cache of the whole history behind it. Here it is appended instead, the way a
//! tool that joins is (`tool_append.rs`): the transcript records a system card
//! at the point the instruction applies from, and each protocol that takes a
//! system message in the middle of a conversation receives it right there.
//!
//! - Anthropic Messages: a mid-conversation `role: "system"` message, on the
//!   models Anthropic documents it for at Anthropic's own endpoint.
//! - OpenAI Responses (and Azure, and the Codex backend): a `system` /
//!   `developer` input item in place.
//! - OpenAI Chat at OpenAI's own endpoint: a `system` message in place.
//! - A relay, another Chat endpoint, xAI: in place where the model declares
//!   it takes one — the `SystemAppend` capability, which Mewrk fills in
//!   itself only where it knows ([`known`]) and the user everywhere else.
//! - Everything else: the host does not append one there. The instruction goes
//!   in as a host message instead — a `<system-reminder>` user message, or a
//!   `box` result, as the conversation chose (`host_append::instruction_carrier`)
//!   — which the head of the prompt never notices.
//!
//! A system card the user writes anywhere below the top of the timeline is the
//! same kind of thing: a system prompt that applies from its place
//! ([`applies_from_its_place`]). Only the card at the top is the conversation's
//! system prompt.
//!
//! A card the model at hand cannot take — one written by the user, or recorded
//! before the conversation switched models — goes to it as a host message the
//! same way ([`carry`]). The host's part on the wire is
//! protocol-neutral: project the card as one marker message and say whether
//! this model takes such a message here at all (`StepRequest.system_append`).
//! The sidecar shapes it for the protocol (`aisdk-service/src/system-append.ts`),
//! and lifts it into the tail of the system prompt only where a placement rule
//! leaves it nowhere else to go.

use serde_json::{json, Value};

use crate::model::{ApiProvider, ContextItem, ModelCapability, ModelProfile, ProviderFamily};

/// The `kind` in an appended card's context id:
/// `ctx_system-append_<topic>_<uuid>`.
const CONTEXT_KIND: &str = "system-append";
const CONTEXT_PREFIX: &str = "ctx_system-append_";

/// The key under the marker message's `providerOptions` that names it as an
/// appended system prompt. The sidecar recognises the marker by it and never
/// passes it on.
const MARKER_OPTIONS_KEY: &str = "mewrk";
const MARKER_KEY: &str = "systemAppend";

/// Whether a system prompt appended for this model reaches it as a system
/// message at its point, rather than as a reminder in a user message: the
/// model declares the `SystemAppend` capability — filled in by
/// Mewrk where it knows ([`known`]), by the user everywhere else — and its
/// protocol can carry one. The sidecar keeps the message in place only where
/// this holds (`StepRequest.system_append`); Anthropic's placement rule —
/// right after a user turn — is the caller's to meet.
pub(crate) fn appends_system(provider: &ApiProvider, model: &ModelProfile) -> bool {
    provider.family.system_append_takes_effect() && model.has(ModelCapability::SystemAppend)
}

/// What Mewrk knows about this model taking a system message in the middle
/// of a conversation at this endpoint: `Some` where it knows, `None` where
/// only the user can say.
///
/// Every Responses endpoint takes a `system` / `developer` input item in place
/// — it is the protocol's own, not a newer shape — and so does OpenAI Chat at
/// OpenAI's or Azure's address. Anthropic's Messages API takes one from the
/// models it documents, at its own endpoint. Google's, Bedrock's and Vertex's
/// protocols put every system message at the head, and the CLI writes its own
/// requests. A relay in front of Anthropic or OpenAI Chat, any other Chat
/// endpoint and xAI may or may not keep one in place: those are the user's.
pub(crate) fn known(family: ProviderFamily, base_url: &str, model_id: &str) -> Option<bool> {
    let vendor = crate::host_append::at_vendor_endpoint(family, base_url);
    match family {
        ProviderFamily::OpenaiResponses | ProviderFamily::OpenaiCodex | ProviderFamily::Azure => {
            Some(true)
        }
        ProviderFamily::OpenaiChat => vendor.then_some(true),
        ProviderFamily::Anthropic => vendor
            .then(|| crate::tool_append::anthropic_documents(model_id))
            .flatten(),
        ProviderFamily::OpenaiCompatible | ProviderFamily::Xai => None,
        ProviderFamily::ClaudeAgent
        | ProviderFamily::Google
        | ProviderFamily::Bedrock
        | ProviderFamily::Vertex => Some(false),
    }
}

/// A fresh card id for an appended system prompt about `topic`.
pub(crate) fn new_id(topic: &str) -> String {
    crate::api::new_context_id(&format!("{CONTEXT_KIND}_{topic}"))
}

/// Whether `context` is a system card the user wrote: not a host-local
/// record, a hook's, one the host appended, a tool addition, a skill an older
/// build delivered as a system card, or a continuation's notebook index. At
/// the top of the timeline it is the conversation's system prompt; anywhere
/// else it applies from its place ([`applies_from_its_place`]).
pub(crate) fn is_written(context: &ContextItem) -> bool {
    matches!(
        context,
        ContextItem::System {
            id,
            local_only: false,
            hook_execution: None,
            tools_added,
            ..
        } if tools_added.is_empty()
            && !id.starts_with(CONTEXT_PREFIX)
            && id != crate::handoff::INDEX_CONTEXT_ID
            && !crate::wire_history::is_legacy_skill_card(context)
    )
}

/// Whether `context` is a system prompt that applies from where it stands:
/// one the host appended, or one the user wrote anywhere but the top of the
/// timeline (`first` says whether it is there). Every model receives it at
/// its place ([`carry`]); none is ever dropped.
pub(crate) fn applies_from_its_place(context: &ContextItem, first: bool) -> bool {
    is_appended(context) || (!first && is_written(context))
}

/// Whether `context` is a system prompt the host appended at its point, as
/// opposed to one that belongs in the system prompt itself.
pub(crate) fn is_appended(context: &ContextItem) -> bool {
    matches!(
        context,
        ContextItem::System {
            id,
            local_only: false,
            hook_execution: None,
            tools_added,
            ..
        } if id.starts_with(CONTEXT_PREFIX) && tools_added.is_empty()
    )
}

/// What an appended card is about: the `<topic>` of its id.
pub(crate) fn topic(context: &ContextItem) -> Option<&str> {
    if !is_appended(context) {
        return None;
    }
    let rest = context.id().strip_prefix(CONTEXT_PREFIX)?;
    rest.rsplit_once('_').map(|(topic, _)| topic)
}

/// The transcript record of `content` applying from here on.
pub(crate) fn card(topic: &str, content: String, created_at: String) -> ContextItem {
    ContextItem::System {
        id: new_id(topic),
        content,
        local_only: false,
        hook_execution: None,
        tools_added: Vec::new(),
        native_compaction: None,
        created_at,
    }
}

/// The key beside [`MARKER_KEY`] that carries the card's id, for [`carry`].
const MARKER_CONTEXT_ID_KEY: &str = "contextId";

/// The card as a `ModelMessage`: a system message whose provider options name
/// it as appended. The sidecar keeps it in place as the protocol's own
/// mid-conversation system message, or lifts its text into the system prompt;
/// [`carry`] may first have turned it into a reminder in a user message.
pub(crate) fn marker_message(context_id: &str, content: &str) -> Value {
    json!({
        "role": "system",
        "content": content,
        "providerOptions": {
            MARKER_OPTIONS_KEY: { MARKER_KEY: true, MARKER_CONTEXT_ID_KEY: context_id },
        },
    })
}

/// The card id and text of an appended system prompt's marker message.
fn marker_parts(message: &Value) -> Option<(&str, &str)> {
    let marker = message.get("providerOptions")?.get(MARKER_OPTIONS_KEY)?;
    if message.get("role")?.as_str()? != "system" || marker.get(MARKER_KEY)? != &Value::Bool(true) {
        return None;
    }
    Some((
        marker.get(MARKER_CONTEXT_ID_KEY)?.as_str()?,
        message.get("content")?.as_str()?,
    ))
}

/// Hands this request's appended system prompts to the model the way it can
/// take them. A model that takes a system message mid-conversation keeps each
/// marker for the sidecar to shape. Any other model receives each as a host
/// message at its place — a `<system-reminder>` in a user message, which a
/// conversation whose host messages come in `box` then turns into a `box`
/// result (`aisdk::project::host_messages_in_box`) — which leaves the head of
/// the prompt alone.
pub(crate) fn carry(request: &crate::model::RunModelRequest, messages: Vec<Value>) -> Vec<Value> {
    if appends_system(&request.provider, &request.model)
        || !messages.iter().any(|message| marker_parts(message).is_some())
    {
        return messages;
    }
    let mut out = Vec::with_capacity(messages.len());
    for message in messages {
        match marker_parts(&message) {
            Some((_, content)) if content.trim().is_empty() => {}
            Some((context_id, content)) => {
                let delivery = crate::wire_history::HostDelivery {
                    local_id: context_id.to_owned(),
                    message: crate::wire_history::system_reminder(content),
                    answers: None,
                };
                crate::aisdk::project::host_delivery_messages(&delivery, &mut out);
            }
            None => out.push(message),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_card_names_its_topic_and_nothing_else_counts_as_appended() {
        let on = card("plan-mode", "Plan.".into(), "2026-10-02T00:00:00Z".into());
        assert!(is_appended(&on));
        assert_eq!(topic(&on), Some("plan-mode"));
        let off = card("plan-mode-exit", "Done.".into(), "2026-10-02T00:00:00Z".into());
        assert_eq!(topic(&off), Some("plan-mode-exit"));

        // The conversation's own system prompt, a host-local note and a
        // tool-append record are system cards too, and none of them is appended.
        let first = ContextItem::System {
            id: "ctx_system_1".into(),
            content: "You are helpful.".into(),
            local_only: false,
            hook_execution: None,
            tools_added: Vec::new(),
            native_compaction: None,
            created_at: String::new(),
        };
        assert!(!is_appended(&first));
        let mut local = on.clone();
        if let ContextItem::System { local_only, .. } = &mut local {
            *local_only = true;
        }
        assert!(!is_appended(&local));
        let tools = crate::tool_append::marker(new_id("plan-mode"), vec!["plan".into()], String::new());
        assert!(!is_appended(&tools));
    }

    /// What Mewrk knows is checked against the fixture in `tool_append.rs`,
    /// which both sides of the TS mirror read; here, how the declaration is
    /// read.
    #[test]
    fn a_message_stays_in_place_where_the_model_declares_it_and_the_protocol_carries_one() {
        let provider = |family, base_url: &str| ApiProvider {
            id: "p".into(),
            name: "P".into(),
            enabled: true,
            family,
            base_url: base_url.into(),
            family_settings: Default::default(),
            notes: String::new(),
            models: Vec::new(),
            active_model_id: None,
        };
        let model = |id: &str, declared: bool| ModelProfile {
            id: id.into(),
            name: String::new(),
            group: String::new(),
            context_window: None,
            max_output_tokens: None,
            capabilities: if declared {
                [ModelCapability::SystemAppend].into()
            } else {
                Default::default()
            },
            reasoning_content: Default::default(),
            prompt_cache: true,
            cache_ttl_minutes: None,
        };
        let relay = provider(ProviderFamily::Anthropic, "https://relay.example.com/v1");
        assert!(!appends_system(&relay, &model("claude-opus-5-5", false)));
        assert!(appends_system(&relay, &model("claude-opus-5-5", true)));
        let deepseek = provider(ProviderFamily::OpenaiCompatible, "https://api.deepseek.com");
        assert!(appends_system(&deepseek, &model("deepseek-v4", true)));
        // Google's protocol and the CLI have no such message to keep.
        assert!(!appends_system(&provider(ProviderFamily::Google, ""), &model("gemini-3-pro", true)));
        assert!(!appends_system(&provider(ProviderFamily::ClaudeAgent, ""), &model("claude-opus-5-5", true)));
    }
}
