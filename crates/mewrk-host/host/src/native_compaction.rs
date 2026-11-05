//! Native compaction: the composer's second auto-compact method.
//!
//! The handoff (`handoff.rs`) has the model write notes and carry on in a new
//! conversation. A model whose protocol compacts natively carries on in a new
//! conversation too, but one that opens on the provider's own compaction of
//! the old one instead of on notes. A conversation uses one or the other
//! ([`method_in_effect`]): the one it chose — a new conversation chooses native
//! where its model compacts natively — unless its model cannot do that one.
//! Past the threshold, at a round boundary — or at the first one of a run the
//! user started with "compact now" — the host sends the request it was about
//! to send anyway — same system prompt, tools, reasoning and history, so the
//! prompt cache holds — with a `compaction_trigger` item last (the AI SDK's
//! `compactionTrigger` option). The provider answers with a single opaque
//! `compaction` item that carries the conversation forward.
//!
//! The continuation differs from a handoff's only in what it opens on: where
//! a handoff's carries the notebook index and the host's opening message, this
//! one carries the compaction card. Everything else is kept as it was — the
//! conversation's own system prompt (and a handoff continuation's notebook
//! index, the system prompt's last section), the settings with the tool lock,
//! the tools the source had appended mid-way, the plan pair once offered, and
//! the prompt cache key — so its requests begin exactly as the compacted
//! conversation's did. An automatic compaction arms the continuation's first
//! run on the card and the work goes on there; one the user asked for leaves
//! it waiting for the user. The source is left as it was and its run ends.
//!
//! The history the card stands for is rebuilt the way Codex rebuilds it
//! (remote compaction v2, `build_v2_compacted_history`): the latest user
//! messages, newest first until the budget is spent — the oldest one kept cut
//! in the middle, anything older dropped — then the compaction item last.
//! Codex keeps 64,000 tokens of them; here the budget is the user's
//! ([`DEFAULT_RETAINED_TOKENS`] to start with), applied as set. Replies,
//! reasoning, tool calls and their results all go: they live on only inside
//! the item. Where Codex re-injects its developer instructions, the system
//! prompts that applied from their place (`system_append.rs`) are kept as
//! they were, outside the budget, and the appended tools are handed over again
//! ahead of the item.
//!
//! The card applies to every model of the provider that produced it — the
//! item is encrypted for that provider's account, and any compacting model
//! there reads it ([`wire_start`]). Another provider, or a model that does not
//! take native compaction, reads only the messages the card kept. Its text is
//! empty: what the card says on screen — who compacted, from how much to how
//! much — is read from its fields by the renderer and never reaches a model.

use std::collections::BTreeSet;
use std::path::Path;

use chrono::Utc;
use serde_json::Value;

use crate::model::{
    ApiProvider, CompactionMethod, ContextItem, Conversation, ModelCapability, ModelProfile,
    NativeCompaction, ProviderFamily, RetainedMessage, RetainedRole, RunModelRequest,
};
use crate::state::AppState;

/// The native threshold a fresh install starts with: 90% of the window, the
/// share Codex compacts at by default.
pub const DEFAULT_THRESHOLD_PERCENT: u32 = 90;
/// Tokens of the latest user messages a compaction keeps to start with, as
/// Codex keeps (`RETAINED_MESSAGE_TOKEN_BUDGET`).
pub const DEFAULT_RETAINED_TOKENS: u32 = 64_000;
/// The most the setting offers.
pub const MAX_RETAINED_TOKENS: u32 = 128_000;

/// The stop reason a run ends with once the continuation exists.
pub const STOP_REASON: &str = "compacted";

/// The AI SDK's content part kind for a Responses `compaction` item.
const COMPACTION_PART_KIND: &str = "openai.compaction";

/// Whether a conversation on this model, at this provider's endpoint, can
/// compact natively: the model declares the `NativeCompaction` capability —
/// filled in by Mewrk where it knows ([`known`]), by the user everywhere
/// else — and its protocol has the interface. Mirrored by TS
/// `takesNativeCompaction` in `src/lib/modelCapabilities.ts`.
pub(crate) fn takes_native_compaction(provider: &ApiProvider, model: &ModelProfile) -> bool {
    provider.family.native_compaction_takes_effect() && model.has(ModelCapability::NativeCompaction)
}

/// The method a conversation auto-compacts by on this model: the one it chose
/// (`None`, a conversation from before the choice, chose the handoff) where
/// the model can do it, otherwise the other where the model can do that —
/// native compaction needs the capability, the handoff a model that takes its
/// tools mid-conversation (`tool_append::appends_tools`) — and `None` where
/// it can do neither. Mirrored by TS `compactionMethodInEffect` in
/// `src/lib/autoCompact.ts`.
pub(crate) fn method_in_effect(
    chosen: Option<CompactionMethod>,
    provider: &ApiProvider,
    model: &ModelProfile,
) -> Option<CompactionMethod> {
    let native = takes_native_compaction(provider, model);
    let handoff = crate::tool_append::appends_tools(provider, model);
    let available = |method: CompactionMethod| match method {
        CompactionMethod::Native => native,
        CompactionMethod::Handoff => handoff,
    };
    let chosen = chosen.unwrap_or_default();
    let other = match chosen {
        CompactionMethod::Native => CompactionMethod::Handoff,
        CompactionMethod::Handoff => CompactionMethod::Native,
    };
    [chosen, other].into_iter().find(|method| available(*method))
}

/// What Mewrk knows about this model compacting natively at this endpoint:
/// `Some` where it knows, `None` where only the user can say.
///
/// Only the Responses protocol has the interface. Codex compacts this way with
/// every model it reaches through OpenAI, whether through the API or the
/// ChatGPT backend; OpenAI documents it for GPT-5 and later
/// ([`openai_documents`]), and the ChatGPT backend took it from GPT-5.5 and
/// GPT-6 alike when measured on 2026-10-05. A relay may or may not pass the
/// item on, and Azure's model names are deployments, so those are the user's.
pub(crate) fn known(family: ProviderFamily, base_url: &str, model_id: &str) -> Option<bool> {
    match family {
        ProviderFamily::OpenaiResponses => crate::host_append::at_vendor_endpoint(family, base_url)
            .then(|| openai_documents(model_id))
            .flatten(),
        ProviderFamily::OpenaiCodex => Some(true),
        ProviderFamily::Azure => None,
        ProviderFamily::Anthropic
        | ProviderFamily::ClaudeAgent
        | ProviderFamily::OpenaiChat
        | ProviderFamily::Google
        | ProviderFamily::Xai
        | ProviderFamily::Bedrock
        | ProviderFamily::Vertex
        | ProviderFamily::OpenaiCompatible => Some(false),
    }
}

/// What OpenAI documents about `model_id` compacting: every GPT from 5 on
/// does. Anything else — an o-series model, an older GPT, a name that is not a
/// GPT version — is the user's to say.
fn openai_documents(model_id: &str) -> Option<bool> {
    let id = model_id.trim().to_ascii_lowercase();
    let version = id.strip_prefix("gpt-")?;
    let major_end = version
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(version.len());
    let major = version[..major_end].parse::<u32>().ok()?;
    (major >= 5).then_some(true)
}

/// The budget the kept user messages are spent against: the setting as the
/// user set it, within what the setting offers.
pub(crate) fn retained_budget(retained_tokens: u32) -> u64 {
    u64::from(retained_tokens.min(MAX_RETAINED_TOKENS))
}

/// The compaction a card carries, if it is one.
pub(crate) fn of(context: &ContextItem) -> Option<&NativeCompaction> {
    match context {
        ContextItem::System {
            native_compaction: Some(compaction),
            ..
        } => Some(compaction),
        _ => None,
    }
}

/// Where a request on this model starts its history: the latest compaction
/// card that applies to it — produced by this provider, for a model that
/// compacts natively — or `None` for the whole transcript.
pub(crate) fn wire_start(
    contexts: &[ContextItem],
    provider: &ApiProvider,
    model: &ModelProfile,
) -> Option<usize> {
    if !takes_native_compaction(provider, model) {
        return None;
    }
    contexts.iter().rposition(|context| {
        of(context).is_some_and(|compaction| compaction.provider_id == provider.id)
    })
}

/// Whether the model has answered since the compaction a request starts at:
/// `view` is the history from [`wire_start`] on (the whole transcript when
/// nothing applies) and `unsynced` what the run produced and has not yet
/// added to it. A second compaction before then would compact the first one's
/// own window again, for nothing.
pub(crate) fn answered_since(view: &[ContextItem], unsynced: &[ContextItem]) -> bool {
    if !view.first().is_some_and(|context| of(context).is_some()) {
        return true;
    }
    view[1..]
        .iter()
        .chain(unsynced)
        .any(|context| match context {
            ContextItem::Assistant { model_turn_id, .. }
            | ContextItem::Reasoning { model_turn_id, .. }
            | ContextItem::Tool { model_turn_id, .. } => model_turn_id.is_some(),
            _ => false,
        })
}

/// The messages a compaction of `view` keeps ahead of its item, oldest first.
///
/// The candidates are what the view's own opening compaction kept, then every
/// user message after it — not a host delivery, which is a tool card — and
/// every system prompt that applies from its place. User messages are spent
/// newest first against `budget`, as Codex spends them
/// (`truncate_retained_messages`): one that fits is kept whole; the first one
/// that does not is cut in the middle to what is left, and everything older is
/// dropped. System prompts are kept whatever the budget. Only text is kept: an
/// attachment does not survive the copy.
pub(crate) fn retain(view: &[ContextItem], budget: u64) -> Vec<RetainedMessage> {
    let mut candidates: Vec<RetainedMessage> = Vec::new();
    for (index, context) in view.iter().enumerate() {
        if let Some(compaction) = of(context) {
            if index == 0 {
                candidates.extend(compaction.retained.iter().cloned());
            }
            continue;
        }
        match context {
            ContextItem::User { id, content, .. }
                if !id.starts_with(crate::wire_history::HOST_TASK_DELIVERY_CONTEXT_PREFIX)
                    && !content.trim().is_empty() =>
            {
                candidates.push(RetainedMessage {
                    role: RetainedRole::User,
                    source_id: id.clone(),
                    content: content.clone(),
                    truncated: false,
                });
            }
            // At the head of the view a written card is the conversation's
            // system prompt only when the view is the whole transcript, and
            // that one reaches every request through the system surface.
            ContextItem::System { id, content, .. }
                if crate::system_append::applies_from_its_place(context, index == 0) =>
            {
                candidates.push(RetainedMessage {
                    role: RetainedRole::System,
                    source_id: id.clone(),
                    content: content.clone(),
                    truncated: false,
                });
            }
            _ => {}
        }
    }

    let mut remaining = budget;
    let mut exhausted = false;
    let mut kept: Vec<RetainedMessage> = Vec::with_capacity(candidates.len());
    for mut message in candidates.into_iter().rev() {
        if message.role == RetainedRole::System {
            kept.push(message);
            continue;
        }
        if exhausted {
            continue;
        }
        let cost = crate::api::estimate_tokens(&message.content).max(1);
        if cost <= remaining {
            remaining -= cost;
        } else {
            exhausted = true;
            if remaining == 0 {
                continue;
            }
            message.content = truncate_middle(&message.content, remaining);
            message.truncated = true;
            remaining = 0;
        }
        kept.push(message);
    }
    kept.reverse();
    kept
}

/// `text` cut to about `budget` tokens by dropping its middle, the way Codex's
/// `truncate_text` cuts a message at the edge of the budget: the head and the
/// tail survive around a note of how much went.
fn truncate_middle(text: &str, budget: u64) -> String {
    let total = crate::api::estimate_tokens(text);
    if total <= budget {
        return text.to_owned();
    }
    let characters: Vec<char> = text.chars().collect();
    // The share of the characters the budget pays for, split between the two ends.
    let keep = (characters.len() as u128 * u128::from(budget) / u128::from(total.max(1))) as usize;
    let head = keep / 2;
    let tail = keep - head;
    let head_text: String = characters[..head].iter().collect();
    let tail_text: String = characters[characters.len() - tail..].iter().collect();
    let omitted = total.saturating_sub(
        crate::api::estimate_tokens(&head_text) + crate::api::estimate_tokens(&tail_text),
    );
    format!("{head_text}\n…{omitted} tokens truncated…\n{tail_text}")
}

/// The compaction items in a response's assistant content, as the AI SDK
/// reports them: `custom` parts of kind `openai.compaction`.
pub(crate) fn compaction_parts(continuation: &Value) -> Vec<Value> {
    continuation
        .as_array()
        .into_iter()
        .flatten()
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("assistant"))
        .filter_map(|message| message.get("content").and_then(Value::as_array))
        .flatten()
        .filter(|part| {
            part.get("type").and_then(Value::as_str) == Some("custom")
                && part.get("kind").and_then(Value::as_str) == Some(COMPACTION_PART_KIND)
        })
        .cloned()
        .collect()
}

/// What a compaction item weighs when the provider did not say: Codex's
/// estimate for encrypted content, three quarters of its length less 650.
pub(crate) fn estimated_item_tokens(parts: &[Value]) -> u64 {
    parts
        .iter()
        .filter_map(|part| {
            part.pointer("/providerOptions/openai/encryptedContent")
                .or_else(|| part.pointer("/providerOptions/azure/encryptedContent"))
        })
        .filter_map(Value::as_str)
        .map(|encrypted| (encrypted.len() as u64 * 3 / 4).saturating_sub(650))
        .sum()
}

/// The tools the history in `view` had appended rather than declared
/// (`tool_append.rs`), those its own opening card handed over again included,
/// in name order.
pub(crate) fn appended_tools(view: &[ContextItem]) -> Vec<String> {
    crate::tool_append::appended(view)
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// The prompt cache key a conversation's requests go out under: the one the
/// compaction it opens on carried over, so a chain of continuations keeps the
/// key of the conversation it began as, and the conversation's own id
/// otherwise.
pub(crate) fn cache_key<'a>(contexts: &'a [ContextItem], conversation_id: &'a str) -> &'a str {
    contexts
        .iter()
        .rev()
        .filter_map(of)
        .map(|compaction| compaction.cache_key.as_str())
        .find(|key| !key.trim().is_empty())
        .unwrap_or(conversation_id)
}

/// What the kept messages weigh on a request.
pub(crate) fn retained_tokens(retained: &[RetainedMessage]) -> u64 {
    retained
        .iter()
        .map(|message| crate::api::estimate_tokens(&message.content))
        .sum()
}

/// The card a continuation opens on. Its text is empty: everything it says is
/// in its fields.
pub(crate) fn card(compaction: NativeCompaction, created_at: String) -> ContextItem {
    ContextItem::System {
        id: crate::api::new_context_id("native-compaction"),
        content: String::new(),
        local_only: true,
        hook_execution: None,
        tools_added: Vec::new(),
        native_compaction: Some(Box::new(compaction)),
        created_at,
    }
}

/// The continuation's timeline: the source's own system prompt — kept first,
/// which is what makes it one — and a handoff continuation's notebook index,
/// both as the source's system prompt carried them, then the card the work
/// carries on from.
pub(crate) fn continuation_contexts(source: &[ContextItem], card: ContextItem) -> Vec<ContextItem> {
    let mut contexts = Vec::with_capacity(3);
    if let Some(prompt) = crate::aisdk::step::conversation_system_prompt(source) {
        contexts.push(ContextItem::System {
            id: crate::api::new_context_id("compaction-system"),
            content: prompt.to_owned(),
            local_only: false,
            hook_execution: None,
            tools_added: Vec::new(),
            native_compaction: None,
            created_at: card_created_at(&card),
        });
    }
    if let Some(index) = source
        .iter()
        .find(|context| context.id() == crate::handoff::INDEX_CONTEXT_ID)
    {
        contexts.push(index.clone());
    }
    contexts.push(card);
    contexts
}

fn card_created_at(card: &ContextItem) -> String {
    match card {
        ContextItem::System { created_at, .. } => created_at.clone(),
        _ => Utc::now().to_rfc3339(),
    }
}

/// Opens the continuation of `request`'s conversation on `compaction`.
///
/// `starts_run` arms its first run on the card, for the renderer to start:
/// the work an automatic compaction interrupted goes on there. `offered` is
/// the tool set the compacted request offered, which the continuation's first
/// request is compared against, so a tool enabled since is appended there as
/// it would have been here. The source's handoff notes, if any, are copied,
/// so the continuation keeps the tool that reads them.
pub(crate) fn open_continuation(
    request: &RunModelRequest,
    state: &AppState,
    compaction: NativeCompaction,
    starts_run: bool,
    offered: Option<&BTreeSet<String>>,
) -> Result<Conversation, String> {
    let app_data = Path::new(&request.app_data_path);
    let anchor = app_data.join("document.v1.json");
    let child_id = format!("conv_{}", uuid::Uuid::new_v4());
    let copied_notes = match request.handoff.notebook() {
        Some(notebook) if !notebook.entries().is_empty() => {
            let target = crate::handoff::Notebook::for_conversation(app_data, &child_id)
                .ok_or_else(|| "Could not place the continuation's notebook".to_owned())?;
            notebook.copy_into(&target)?;
            Some(target)
        }
        _ => None,
    };
    let contexts = continuation_contexts(
        &request.contexts,
        card(compaction, Utc::now().to_rfc3339()),
    );
    let child = crate::handoff::create_continuation(
        state,
        &anchor,
        &request.workspace_id,
        &request.conversation_id,
        child_id,
        contexts,
        crate::handoff::Inherits::SettingsAndToolLock,
        starts_run,
    )
    .inspect_err(|_| {
        if let Some(notes) = &copied_notes {
            notes.remove();
        }
    })?;
    if let Some(offered) = offered {
        state.offered_tools.record(&child.id, offered.clone());
    }
    state
        .push_events
        .publish(crate::push_events::AppPushEvent::ConversationHandedOff {
            workspace_id: request.workspace_id.clone(),
            source_conversation_id: request.conversation_id.clone(),
            child_conversation_id: child.id.clone(),
            starts_run,
        });
    Ok(child)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user(id: &str, content: &str) -> ContextItem {
        ContextItem::User {
            id: id.into(),
            content: content.into(),
            images: Vec::new(),
            files: Vec::new(),
            created_at: String::new(),
        }
    }

    fn assistant(id: &str, turn: Option<&str>) -> ContextItem {
        ContextItem::Assistant {
            id: id.into(),
            content: "reply".into(),
            round: None,
            model_turn_id: turn.map(Into::into),
            interrupted: false,
            sources: Vec::new(),
            created_at: String::new(),
        }
    }

    fn compaction(provider: &str, retained: Vec<RetainedMessage>) -> NativeCompaction {
        NativeCompaction {
            provider_id: provider.into(),
            model: "gpt-6-astra".into(),
            parts: vec![json!({
                "type": "custom",
                "kind": "openai.compaction",
                "providerOptions": { "openai": { "type": "compaction", "itemId": "cmp_1", "encryptedContent": "x".repeat(4000) } }
            })],
            retained,
            tokens_before: 200_000,
            tokens_after: 1_000,
            model_name: String::new(),
            appended_tools: Vec::new(),
            plan_tools: false,
            cache_key: String::new(),
        }
    }

    fn provider(id: &str, family: ProviderFamily) -> ApiProvider {
        ApiProvider {
            id: id.into(),
            name: id.into(),
            enabled: true,
            family,
            base_url: String::new(),
            family_settings: Default::default(),
            notes: String::new(),
            models: Vec::new(),
            active_model_id: None,
        }
    }

    fn model(declared: bool) -> ModelProfile {
        ModelProfile {
            id: "gpt-6-astra".into(),
            name: String::new(),
            group: String::new(),
            context_window: Some(272_000),
            max_output_tokens: None,
            capabilities: if declared {
                [ModelCapability::NativeCompaction].into()
            } else {
                Default::default()
            },
            reasoning_content: Default::default(),
            prompt_cache: true,
            cache_ttl_minutes: None,
        }
    }

    #[test]
    fn openai_documents_gpt_5_and_later() {
        for (model, expected) in [
            ("gpt-5", Some(true)),
            ("gpt-5.5", Some(true)),
            ("gpt-5.3-codex", Some(true)),
            ("GPT-6-Astra", Some(true)),
            ("gpt-7", Some(true)),
            ("gpt-4o", None),
            ("gpt-4.1", None),
            ("o3", None),
            ("gpt-reserve", None),
            ("claude-opus-5-5", None),
        ] {
            assert_eq!(openai_documents(model), expected, "{model}");
        }
    }

    #[test]
    fn a_request_starts_at_the_latest_compaction_its_provider_made() {
        let codex = provider("codex", ProviderFamily::OpenaiCodex);
        let contexts = vec![
            user("u1", "first"),
            card(compaction("codex", Vec::new()), String::new()),
            user("u2", "second"),
            card(compaction("other", Vec::new()), String::new()),
            user("u3", "third"),
        ];
        assert_eq!(wire_start(&contexts, &codex, &model(true)), Some(1));
        // A model that does not compact natively reads the whole transcript.
        assert_eq!(wire_start(&contexts, &codex, &model(false)), None);
        // So does one at a family without the interface.
        let anthropic = provider("codex", ProviderFamily::Anthropic);
        assert_eq!(wire_start(&contexts, &anthropic, &model(true)), None);
        assert_eq!(wire_start(&contexts[..1], &codex, &model(true)), None);
    }

    #[test]
    fn a_conversation_compacts_by_its_choice_where_the_model_can() {
        use CompactionMethod::{Handoff, Native};
        let codex = provider("codex", ProviderFamily::OpenaiCodex);
        let mut both = model(true);
        both.capabilities.insert(ModelCapability::ToolAppend);
        assert_eq!(method_in_effect(Some(Native), &codex, &both), Some(Native));
        assert_eq!(method_in_effect(Some(Handoff), &codex, &both), Some(Handoff));
        // A conversation from before the choice hands off.
        assert_eq!(method_in_effect(None, &codex, &both), Some(Handoff));
        // A model that cannot compact natively hands off whatever was chosen…
        let mut appends_only = model(false);
        appends_only.capabilities.insert(ModelCapability::ToolAppend);
        assert_eq!(method_in_effect(Some(Native), &codex, &appends_only), Some(Handoff));
        // …and one that cannot take the handoff tools compacts natively.
        assert_eq!(method_in_effect(Some(Handoff), &codex, &model(true)), Some(Native));
        assert_eq!(method_in_effect(Some(Native), &codex, &model(false)), None);
    }

    #[test]
    fn a_compaction_waits_for_the_model_to_answer_after_the_last_one() {
        let opened = vec![
            card(compaction("codex", Vec::new()), String::new()),
            user("u", "go on"),
        ];
        assert!(!answered_since(&opened, &[]));
        // Host-written cards are not an answer.
        assert!(!answered_since(&opened, &[assistant("a", None)]));
        assert!(answered_since(&opened, &[assistant("a", Some("turn"))]));
        // Without a compaction to start from, nothing holds it back.
        assert!(answered_since(&[user("u", "hi")], &[]));
    }

    #[test]
    fn the_latest_user_messages_are_kept_newest_first_within_the_budget() {
        let view = vec![
            user("u1", &"a".repeat(400)),
            assistant("a1", Some("t1")),
            user("u2", &"b".repeat(400)),
            crate::system_append::card("plan-mode", "Plan first.".into(), String::new()),
            user("u3", &"c".repeat(40)),
        ];
        // 400 ASCII characters are 100 tokens; 40 are 10.
        let kept = retain(&view, 150);
        let ids: Vec<&str> = kept
            .iter()
            .map(|message| message.source_id.as_str())
            .collect();
        assert_eq!(ids.len(), 4);
        assert_eq!(ids[0], "u1");
        assert_eq!(ids[1], "u2");
        assert!(ids[2].starts_with("ctx_system-append_plan-mode_"));
        assert_eq!(ids[3], "u3");
        // u3 and u2 fit whole (110 tokens); u1 is cut to the 40 left.
        assert!(!kept[1].truncated && !kept[3].truncated);
        assert!(kept[0].truncated);
        assert!(kept[0].content.contains("tokens truncated"));
        assert!(crate::api::estimate_tokens(&kept[0].content) < 60);
        assert_eq!(kept[2].role, RetainedRole::System);

        // Once the budget is spent nothing older is kept, however small.
        let kept = retain(&view, 110);
        let ids: Vec<&str> = kept
            .iter()
            .map(|message| message.source_id.as_str())
            .collect();
        assert_eq!(ids.first(), Some(&"u2"));
        assert_eq!(ids.len(), 3);
    }

    #[test]
    fn a_later_compaction_keeps_what_the_earlier_one_kept() {
        let earlier = compaction(
            "codex",
            vec![RetainedMessage {
                role: RetainedRole::User,
                source_id: "u0".into(),
                content: "the original ask".into(),
                truncated: false,
            }],
        );
        let view = vec![card(earlier, String::new()), user("u1", "and then this")];
        let kept = retain(&view, 1_000);
        let ids: Vec<&str> = kept
            .iter()
            .map(|message| message.source_id.as_str())
            .collect();
        assert_eq!(ids, ["u0", "u1"]);
    }

    #[test]
    fn the_budget_is_the_setting_as_set() {
        assert_eq!(retained_budget(64_000), 64_000);
        assert_eq!(retained_budget(20_000), 20_000);
        assert_eq!(retained_budget(500_000), u64::from(MAX_RETAINED_TOKENS));
        assert_eq!(retained_budget(0), 0);
    }

    #[test]
    fn a_continuation_keeps_the_cache_key_and_the_appended_tools_of_its_chain() {
        let mut first = compaction("codex", Vec::new());
        first.cache_key = "conv_origin".into();
        first.appended_tools = vec!["mcp_lookup".into()];
        let view = vec![
            card(first, String::new()),
            user("u1", "go on"),
            crate::tool_append::marker("ctx_tool-append_1".into(), vec!["browser".into()], String::new()),
        ];
        assert_eq!(cache_key(&view, "conv_child"), "conv_origin");
        assert_eq!(cache_key(&view[1..], "conv_child"), "conv_child");
        assert_eq!(appended_tools(&view), ["browser", "mcp_lookup"]);
    }

    #[test]
    fn the_continuation_carries_the_system_prompt_the_index_and_the_card() {
        let system = ContextItem::System {
            id: "ctx_preset".into(),
            content: "You are the reviewer.".into(),
            local_only: false,
            hook_execution: None,
            tools_added: Vec::new(),
            native_compaction: None,
            created_at: String::new(),
        };
        let index = ContextItem::System {
            id: crate::handoff::INDEX_CONTEXT_ID.into(),
            content: "Notes: state".into(),
            local_only: false,
            hook_execution: None,
            tools_added: Vec::new(),
            native_compaction: None,
            created_at: String::new(),
        };
        let source = vec![system, index.clone(), user("u1", "hi"), assistant("a1", Some("t"))];
        let contexts = continuation_contexts(&source, card(compaction("codex", Vec::new()), String::new()));
        assert_eq!(contexts.len(), 3);
        assert_eq!(
            crate::aisdk::step::conversation_system_prompt(&contexts),
            Some("You are the reviewer.")
        );
        assert_eq!(contexts[1], index);
        let ContextItem::System { content, .. } = &contexts[2] else {
            panic!("the card comes last");
        };
        assert!(content.is_empty());
        assert!(of(&contexts[2]).is_some());
        // A source without a system prompt of its own passes none on.
        let bare = continuation_contexts(&source[2..], card(compaction("codex", Vec::new()), String::new()));
        assert_eq!(bare.len(), 1);
    }

    #[test]
    fn the_item_is_read_from_the_sdk_response_messages() {
        let continuation = json!([
            { "role": "assistant", "content": [
                { "type": "text", "text": "ignored" },
                { "type": "custom", "kind": "openai.compaction",
                  "providerOptions": { "openai": { "type": "compaction", "itemId": "cmp_9", "encryptedContent": "e" } } }
            ] }
        ]);
        let parts = compaction_parts(&continuation);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["providerOptions"]["openai"]["itemId"], "cmp_9");
        assert!(compaction_parts(&json!([{ "role": "assistant", "content": "text" }])).is_empty());
    }
}
