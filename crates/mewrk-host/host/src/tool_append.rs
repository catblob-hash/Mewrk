//! Tools that join a conversation after its first request.
//!
//! Every request declares a tool list, and every protocol puts that list at
//! the head of the prompt, ahead of the system prompt and the history. A tool
//! that joins mid-conversation — the user ticks it in the settings, the run
//! arms the handoff tools, `tool_search` hands out an MCP schema — used to be
//! written into that list, which rewrote the head of the prompt and threw the
//! whole prompt cache away. Here it is appended instead: the transcript records
//! the point it joined at, and each protocol that has a tool-append interface
//! hands the tool over there, at the end of the transcript as it stood.
//!
//! - Anthropic Messages: a mid-conversation `role: "system"` message carrying
//!   `tool_addition` blocks, the tool itself declared with `defer_loading`.
//! - OpenAI Responses (and Azure, and the Codex backend): an `additional_tools`
//!   input item with the tool's definition.
//! - Every other protocol has no such interface; the sidecar folds the tool
//!   back into the declared list, which is what happened before.
//!
//! Whether a model takes the interface at its endpoint is a capability the
//! model declares, as it declares vision: Mewrk fills it in where it knows
//! ([`known`]), the user where it does not — a relay that may or may not pass
//! the interface on. The sidecar appends only where it is declared.
//!
//! The host's part is protocol-neutral: decide which tools are new, record the
//! point, and project it as one marker message. The sidecar turns the marker
//! into the protocol's own shape (`aisdk-service/src/tool-append.ts`). No text
//! goes with it — the tool simply becomes available.
//!
//! A tool the conversation turns off goes the other way. Its calls and results
//! stay in the transcript, where the user still sees them, and every request
//! leaves them out ([`WireMask`]). Turned back on, what happens depends on
//! whether the request's view of the transcript still holds such a call: if it
//! does, leaving them out simply stops, which puts the prefix back exactly as
//! it was, and the tool is declared where it was declared. If it does not, the
//! tool is appended like any other newcomer, and whatever registered it earlier
//! is taken out of the transcript ([`take_registrations`]), so the prefix sent
//! since it went off is left alone.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::model::{
    ApiProvider, ContextItem, ConversationSettings, FetchProviderSelection, ModelCapability,
    ModelProfile, ProviderFamily, RunModelRequest, SearchProviderSelection,
};

/// Whether a conversation on this model, at this provider's endpoint, can take
/// a tool mid-conversation at all: the model declares the `ToolAppend`
/// capability — filled in by Mewrk where it knows ([`known`]), by the user
/// everywhere else — and its protocol has an append interface to read it for.
///
/// Where it cannot, the conversation's tool surface is fixed from its first
/// request on — the renderer locks it (`src/lib/toolLock.ts`), auto-compact
/// never arms (its four tools would join mid-run) and MCP tool discovery is off
/// (a fetched schema would join mid-run). The sidecar hands an appended tool
/// over through the protocol's interface only where this holds
/// (`StepRequest.tool_append`). Mirrored by TS `appendsTools` in
/// `src/lib/modelCapabilities.ts`.
pub(crate) fn appends_tools(provider: &ApiProvider, model: &ModelProfile) -> bool {
    provider.family.tool_append_takes_effect() && model.has(ModelCapability::ToolAppend)
}

/// What Mewrk knows about this model taking a tool mid-conversation at this
/// endpoint: `Some` where it knows, `None` where only the user can say. A
/// model is declared `ToolAppend` from this once, when it is fetched or
/// seeded (and once for the models a schema-3 document already held); the
/// declaration is the model's from then on, for the user to change.
///
/// It knows a protocol with no append interface (`false`), the Codex backend
/// (`true`: its address is ChatGPT's or a local stand-in for it), Claude
/// Code's own catalogue for the CLI, and at the vendor's own endpoint
/// ([`crate::host_append::at_vendor_endpoint`]) what the vendor documents:
/// Responses and Azure take `additional_tools` from every model, Anthropic's
/// Messages API takes `tool_addition` from the models in its list. A relay in
/// front of either may or may not pass the interface on, so there — and for a
/// model the vendor's documentation does not cover — it does not know.
pub(crate) fn known(family: ProviderFamily, base_url: &str, model_id: &str) -> Option<bool> {
    let vendor = crate::host_append::at_vendor_endpoint(family, base_url);
    match family {
        ProviderFamily::OpenaiCodex => Some(true),
        ProviderFamily::OpenaiResponses | ProviderFamily::Azure => vendor.then_some(true),
        ProviderFamily::Anthropic => vendor.then(|| anthropic_documents(model_id)).flatten(),
        ProviderFamily::ClaudeAgent => Some(names_model(model_id, CLAUDE_CODE_TOOL_CHANGE_MODELS)),
        ProviderFamily::OpenaiChat
        | ProviderFamily::Google
        | ProviderFamily::Xai
        | ProviderFamily::Bedrock
        | ProviderFamily::Vertex
        | ProviderFamily::OpenaiCompatible => Some(false),
    }
}

/// Models the Messages API takes `tool_addition` from (Anthropic's
/// mid-conversation tool-change documentation).
const ANTHROPIC_TOOL_CHANGE_MODELS: &[&str] = &[
    "fable-5", "mythos-5", "opus-5", "opus-4-8", "sonnet-5-5",
];
/// Earlier Claude models, which the same documentation leaves out: every
/// generation before 4.8, matched whole (no later model will join them).
const ANTHROPIC_EARLIER_MODELS: &[&str] = &[
    "claude-instant", "claude-2", "claude-3", "opus-4", "sonnet-4", "haiku-4", "haiku-3",
];
/// Current models the documentation names as without it, matched as released
/// (a dated snapshot, never a later minor version).
const ANTHROPIC_RELEASES_WITHOUT: &[&str] = &["sonnet-5"];

/// What Anthropic documents about this model taking a system message — and
/// so a tool change, which rides in one — mid-conversation at its own
/// endpoint: `Some(true)` for the models in its list, `Some(false)` for the
/// Claude models it leaves out, `None` for a model it says nothing about.
/// Anthropic documents system messages and tool changes for the same models,
/// so `system_append::known` asks this too.
pub(crate) fn anthropic_documents(model_id: &str) -> Option<bool> {
    if names_model(model_id, ANTHROPIC_TOOL_CHANGE_MODELS) {
        Some(true)
    } else if names_model(model_id, ANTHROPIC_EARLIER_MODELS)
        || names_release(model_id, ANTHROPIC_RELEASES_WITHOUT)
    {
        Some(false)
    } else {
        None
    }
}

/// Models Claude Code itself appends tools for (`mid_conv_tool_change` in the
/// CLI's model catalogue), as Claude Code 2.1.309 has them. The CLI is the
/// user's own and its catalogue moves with its releases; a real-CLI test in
/// `aisdk-rs/tests/claude_agent.rs` pins this table, and a stale entry costs
/// prompt cache on a rebuilt session, never a tool.
const CLAUDE_CODE_TOOL_CHANGE_MODELS: &[&str] =
    &["fable-5", "mythos-5", "opus-5", "opus-4-8", "sonnet-5-5", "haiku-5-5"];

/// The id as [`names_model`] reads it: case, `.` for `-` and Claude Code's
/// `[1m]` suffix do not matter.
fn normalized_model_id(model_id: &str) -> String {
    model_id
        .to_ascii_lowercase()
        .replace('.', "-")
        .trim_end_matches("[1m]")
        .to_owned()
}

/// Whether `model_id` names one of `families`, followed by whatever `rest_ok`
/// accepts, the family appearing whole (at the start or after a `-`).
fn names_model_with(model_id: &str, families: &[&str], rest_ok: impl Fn(&str) -> bool) -> bool {
    let id = normalized_model_id(model_id);
    families.iter().any(|family| {
        id.match_indices(family).any(|(at, _)| {
            let before_ok = at == 0 || id.as_bytes()[at - 1] == b'-';
            before_ok && rest_ok(&id[at + family.len()..])
        })
    })
}

/// Whether `model_id` names one of `families`: the family appears whole,
/// followed by the end of the id or another `-` segment (a minor version, a
/// date).
fn names_model(model_id: &str, families: &[&str]) -> bool {
    names_model_with(model_id, families, |rest| rest.is_empty() || rest.starts_with('-'))
}

/// Whether `model_id` names one of `families` as released: followed by the
/// end of the id or a dated snapshot (`-YYYYMMDD`), never a minor version.
fn names_release(model_id: &str, families: &[&str]) -> bool {
    names_model_with(model_id, families, |rest| {
        rest.is_empty()
            || rest
                .strip_prefix('-')
                .is_some_and(|date| date.len() == 8 && date.bytes().all(|byte| byte.is_ascii_digit()))
    })
}

/// The `kind` in the marker's context id (`ctx_tools-added_<uuid>`).
pub(crate) const CONTEXT_KIND: &str = "tools-added";

/// The key under the marker message's `providerOptions` that names it as one.
/// The sidecar recognises the marker by it and never passes it on.
const MARKER_OPTIONS_KEY: &str = "mewrk";
const MARKER_TOOLS_KEY: &str = "toolAddition";

/// The tools a context records as having joined at its point, if it is a
/// marker.
pub(crate) fn added_tools(context: &ContextItem) -> &[String] {
    match context {
        // A native compaction's card hands the tools its conversation had
        // appended over again (`native_compaction.rs`).
        ContextItem::System {
            native_compaction: Some(compaction),
            ..
        } => &compaction.appended_tools,
        ContextItem::System { tools_added, .. } => tools_added,
        _ => &[],
    }
}

/// Every tool the transcript has already appended somewhere.
pub(crate) fn appended<'a>(contexts: impl IntoIterator<Item = &'a ContextItem>) -> HashSet<String> {
    contexts
        .into_iter()
        .flat_map(|context| added_tools(context).iter().cloned())
        .collect()
}

/// The tool `context` records the model calling. A card somebody wrote by hand
/// or a template laid in has no model turn, so it says nothing about the tool
/// ever having been offered, and a host delivery is not a call at all.
fn called_tool(context: &ContextItem) -> Option<&str> {
    match context {
        ContextItem::Tool {
            tool_name,
            model_turn_id: Some(_),
            ..
        } => Some(tool_name),
        _ => None,
    }
}

/// Every tool the transcript holds the model's calls of. A registration is not
/// a call: a tool that was appended and never used has no history here.
pub(crate) fn called<'a>(contexts: impl IntoIterator<Item = &'a ContextItem>) -> HashSet<String> {
    contexts
        .into_iter()
        .filter_map(called_tool)
        .map(str::to_owned)
        .collect()
}

/// The tools this request offers that have to be appended.
///
/// `previous` is the set the previous request of this transcript offered.
/// Without it — the first request, or the first after a restart — nothing
/// counts as new: whatever is offered goes into the declared list, which is
/// exactly what the previous request would have declared had it been known. A
/// tool coming back to a transcript that still holds its calls (`history`) is
/// not new either: the calls return to the prefix, and the tool to where it
/// was declared or appended before.
pub(crate) fn newly_offered(
    offered: &[String],
    previous: Option<&BTreeSet<String>>,
    history: &HashSet<String>,
) -> Vec<String> {
    let Some(previous) = previous else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    offered
        .iter()
        .filter(|name| !previous.contains(*name) && !history.contains(*name))
        .filter(|name| seen.insert(name.as_str()))
        .cloned()
        .collect()
}

/// Whether the settings leave `name` on, whether or not this run can offer it.
/// A shell tool on a machine without that shell, native fetch on a family
/// without it, tool discovery on a model that cannot append: the user turned
/// none of those off, and moving back brings them straight back. The tools the
/// host derives from a switch of their own are on exactly when the run's list
/// says so, and a stale name in the persisted list says nothing about them.
fn left_on_by_settings(name: &str, settings: &ConversationSettings) -> bool {
    match name {
        crate::api::WEB_SEARCH_TOOL => {
            settings.web_search_enabled
                && !matches!(settings.web_search.provider, SearchProviderSelection::Disabled)
        }
        crate::api::WEB_FETCH_TOOL => {
            settings.web_search_enabled
                && !matches!(settings.web_search.fetch_provider, FetchProviderSelection::Disabled)
        }
        crate::capabilities::TOOL_SEARCH_TOOL => settings.mcp_tool_discovery_enabled,
        crate::capabilities::SKILL_TOOL => false,
        _ if crate::mewrk_memory::is_memory_tool(name)
            || crate::agents::is_task_runtime_tool_name(name)
            || crate::agents::is_subagent_messaging_tool_name(name) =>
        {
            false
        }
        _ => settings.enabled_tools.iter().any(|tool| tool == name),
    }
}

/// The tools the conversation has turned off that its transcript holds the
/// model's calls of: what a request leaves out ([`WireMask`]).
///
/// Only a turned-off tool the host can recognise counts. The plan pair stays
/// once offered and the handoff tools are the run's own; an MCP tool is off
/// when no selected server serves it, not when a selected one failed to answer
/// this time; and a name the catalogue does not know, a tool an older build
/// had, is left as it always was. `request` has its final derived tool list and
/// its MCP servers, but no bindings yet.
pub(crate) fn closed_tools(
    request: &RunModelRequest,
    settings: &ConversationSettings,
) -> BTreeSet<String> {
    called(&request.contexts)
        .into_iter()
        .filter(|name| {
            if request.enabled_tools.iter().any(|tool| tool == name)
                || crate::plan_mode::is_plan_mode_tool_name(name)
                || crate::handoff::is_handoff_tool_name(name)
                || left_on_by_settings(name, settings)
            {
                return false;
            }
            if name.starts_with("mcp__") {
                return !request
                    .mcp_servers
                    .iter()
                    .any(|server| crate::mcp::serves_tool(server, name));
            }
            request.tools.iter().any(|tool| &tool.name == name)
        })
        .collect()
}

/// What one request leaves out of the transcript it projects. The transcript
/// itself is not touched: this is the difference between what the user keeps
/// and what the model is shown, so it can stop applying at any time and the
/// prefix comes back byte for byte.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct WireMask {
    /// The tools whose calls and results are left out.
    closed: BTreeSet<String>,
    /// The provider ids of those calls, for a background result that answers one.
    calls: BTreeSet<String>,
    /// `(card id, tool)` for a registration a later one replaced that the
    /// transcript still holds: one the same run wrote, or one a failed write or
    /// a lagging copy of the conversation left behind.
    superseded: BTreeSet<(String, String)>,
}

impl WireMask {
    /// The mask for a request whose view of the transcript is `view`.
    pub(crate) fn of<'a>(
        closed: &BTreeSet<String>,
        view: impl IntoIterator<Item = &'a ContextItem>,
    ) -> Self {
        let mut calls = BTreeSet::new();
        let mut superseded = BTreeSet::new();
        let mut latest: HashMap<&str, &str> = HashMap::new();
        for context in view {
            if let ContextItem::Tool {
                provider_call_id: Some(call_id),
                ..
            } = context
            {
                if called_tool(context).is_some_and(|name| closed.contains(name)) {
                    calls.insert(call_id.clone());
                }
            }
            for tool in added_tools(context) {
                // A card can reach here twice, from the transcript and from what
                // the run has generated.
                if let Some(older) = latest.insert(tool, context.id()) {
                    if older != context.id() {
                        superseded.insert((older.to_owned(), tool.clone()));
                    }
                }
            }
        }
        Self {
            closed: closed.clone(),
            calls,
            superseded,
        }
    }

    /// Whether `context` is a call of a tool that is off.
    pub(crate) fn hides_call(&self, context: &ContextItem) -> bool {
        called_tool(context).is_some_and(|name| self.closed.contains(name))
    }

    /// Whether `call_id` is a call left out, whose late result has nothing left
    /// to answer.
    pub(crate) fn hides_result_of(&self, call_id: &str) -> bool {
        self.calls.contains(call_id)
    }

    /// The tools a registration on card `card_id` still hands over.
    pub(crate) fn registered(&self, card_id: &str, tools: &[String]) -> Vec<String> {
        tools
            .iter()
            .filter(|tool| !self.superseded.contains(&(card_id.to_owned(), (*tool).clone())))
            .cloned()
            .collect()
    }
}

/// The transcript record of `tools` joining here.
///
/// Local-only: the text is for a person reading an export, never for the
/// model, and the renderer draws no row for it. What the model gets is the
/// protocol's own tool addition, projected from `tools_added`.
pub(crate) fn marker(id: String, tools: Vec<String>, created_at: String) -> ContextItem {
    ContextItem::System {
        id,
        content: marker_text(&tools),
        local_only: true,
        hook_execution: None,
        tools_added: tools,
        native_compaction: None,
        created_at,
    }
}

fn marker_text(tools: &[String]) -> String {
    format!("Tools added: {}", tools.join(", "))
}

/// Takes `tools` off every registration in `contexts[start..]`, for a tool about
/// to be registered anew: the only edit the host makes to the transcript for a
/// tool that comes and goes, because a registration is the host's own record
/// and the model is never shown two of one tool.
///
/// A marker left with no tool goes altogether. A native compaction's card stays
/// for what else it holds. `own` names the cards this run has written itself:
/// the run still holds those by position, so they stay and [`WireMask`] covers
/// them. Returns the cards changed and the ids of those removed, for the store.
pub(crate) fn take_registrations(
    contexts: &mut Vec<ContextItem>,
    start: usize,
    tools: &[String],
    own: &HashSet<String>,
) -> (Vec<ContextItem>, Vec<String>) {
    let mut changed = Vec::new();
    let mut removed = Vec::new();
    let mut index = 0;
    contexts.retain_mut(|context| {
        let at = index;
        index += 1;
        if at < start || own.contains(context.id()) {
            return true;
        }
        match &mut *context {
            ContextItem::System {
                native_compaction: Some(compaction),
                ..
            } => {
                let before = compaction.appended_tools.len();
                compaction.appended_tools.retain(|tool| !tools.contains(tool));
                if compaction.appended_tools.len() != before {
                    changed.push(context.clone());
                }
                true
            }
            ContextItem::System {
                id,
                content,
                tools_added,
                ..
            } if !tools_added.is_empty() => {
                let before = tools_added.len();
                tools_added.retain(|tool| !tools.contains(tool));
                if tools_added.len() == before {
                    return true;
                }
                if tools_added.is_empty() {
                    removed.push(id.clone());
                    return false;
                }
                *content = marker_text(tools_added);
                changed.push(context.clone());
                true
            }
            _ => true,
        }
    });
    (changed, removed)
}

/// The marker as a `ModelMessage`: a system message with no text whose
/// provider options name the tools. The sidecar replaces it with the
/// protocol's tool addition, or drops it where the protocol has none.
pub(crate) fn marker_message(tools: &[String]) -> Value {
    json!({
        "role": "system",
        "content": "",
        "providerOptions": { MARKER_OPTIONS_KEY: { MARKER_TOOLS_KEY: tools } },
    })
}

/// The tool set each conversation's last request offered, so the first
/// request of the next run can tell a tool the user just enabled from one the
/// declared list already had.
///
/// Process-local on purpose. Losing it (a restart) costs one request whose
/// declared list takes a newly enabled tool in — the old behaviour, correct
/// and uncached — and nothing else; persisting it would put a second record of
/// the tool set beside the transcript for the two to disagree about.
///
/// Clones share one map, as every other registry on `AppState` does.
#[derive(Clone, Default)]
pub(crate) struct OfferedToolRegistry {
    offered: Arc<Mutex<HashMap<String, BTreeSet<String>>>>,
}

impl OfferedToolRegistry {
    pub(crate) fn last(&self, conversation_id: &str) -> Option<BTreeSet<String>> {
        self.offered
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(conversation_id)
            .cloned()
    }

    pub(crate) fn record(&self, conversation_id: &str, offered: BTreeSet<String>) {
        self.offered
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(conversation_id.to_owned(), offered);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    /// The fixture both sides of the mirror check (`modelCapabilities.test.ts`):
    /// family, base URL, model, then what Mewrk knows about appending a tool
    /// ([`known`]) and a system prompt (`system_append::known`) there, about
    /// the model taking asynchronous tool calls (`async_tools::known`), and
    /// about it compacting natively (`native_compaction::known`). `None` is
    /// the user's to say.
    #[allow(clippy::type_complexity)]
    const APPEND_FIXTURE: &[(
        ProviderFamily,
        &str,
        &str,
        Option<bool>,
        Option<bool>,
        Option<bool>,
        Option<bool>,
    )] = {
        use crate::model::ProviderFamily::*;
        &[
            (Anthropic, "https://api.anthropic.com/v1", "claude-opus-5-5", Some(true), Some(true), Some(false), Some(false)),
            (Anthropic, "https://api.anthropic.com/v1", "claude-opus-5-20260201", Some(true), Some(true), Some(false), Some(false)),
            (Anthropic, "https://api.anthropic.com/v1", "claude-opus-4-8", Some(true), Some(true), Some(false), Some(false)),
            (Anthropic, "https://api.anthropic.com/v1", "claude-opus-4-5", Some(false), Some(false), Some(false), Some(false)),
            (Anthropic, "https://api.anthropic.com/v1", "claude-sonnet-5-5", Some(true), Some(true), Some(false), Some(false)),
            (Anthropic, "https://api.anthropic.com/v1", "claude-sonnet-5", Some(false), Some(false), Some(false), Some(false)),
            (Anthropic, "https://api.anthropic.com/v1", "claude-sonnet-5-20260115", Some(false), Some(false), Some(false), Some(false)),
            (Anthropic, "https://api.anthropic.com/v1", "claude-sonnet-4.5", Some(false), Some(false), Some(false), Some(false)),
            (Anthropic, "https://api.anthropic.com/v1", "claude-fable-5-1", Some(true), Some(true), Some(false), Some(false)),
            (Anthropic, "https://api.anthropic.com/v1", "claude-mythos-5", Some(true), Some(true), Some(false), Some(false)),
            (Anthropic, "https://api.anthropic.com/v1", "claude-haiku-4-5", Some(false), Some(false), Some(false), Some(false)),
            (Anthropic, "https://api.anthropic.com/v1", "claude-3-5-sonnet-20241022", Some(false), Some(false), Some(false), Some(false)),
            (Anthropic, "https://API.Anthropic.com", "claude-opus-5-5", Some(true), Some(true), Some(false), Some(false)),
            (Anthropic, "", "claude-opus-5-5", Some(true), Some(true), Some(false), Some(false)),
            (Anthropic, "https://api.anthropic.com/v1", "claude-haiku-5", None, None, Some(false), Some(false)),
            (Anthropic, "https://api.anthropic.com/v1", "claude-sonnet-5-6", None, None, Some(false), Some(false)),
            (Anthropic, "https://relay.example.com/v1", "claude-opus-5-5", None, None, Some(false), Some(false)),
            (Anthropic, "https://relay.example.com/v1", "claude-haiku-4-5", None, None, Some(false), Some(false)),
            (ClaudeAgent, "", "claude-opus-5-5[1m]", Some(true), Some(false), Some(false), Some(false)),
            (ClaudeAgent, "", "claude-sonnet-5-5", Some(true), Some(false), Some(false), Some(false)),
            (ClaudeAgent, "", "claude-haiku-5-5", Some(true), Some(false), Some(false), Some(false)),
            (ClaudeAgent, "", "claude-sonnet-5", Some(false), Some(false), Some(false), Some(false)),
            (ClaudeAgent, "", "claude-fable-5", Some(true), Some(false), Some(false), Some(false)),
            (OpenaiResponses, "https://api.openai.com/v1", "gpt-5.5", Some(true), Some(true), Some(false), Some(true)),
            (OpenaiResponses, "https://relay.example.com/v1", "gpt-5.5", None, Some(true), None, None),
            (OpenaiResponses, "https://api.openai.com/v1", "gpt-6-astra", Some(true), Some(true), Some(true), Some(true)),
            (OpenaiResponses, "https://api.openai.com/v1", "gpt-6.1-sol", Some(true), Some(true), Some(true), Some(true)),
            (OpenaiResponses, "https://api.openai.com/v1", "gpt-6-luna", Some(true), Some(true), None, Some(true)),
            (OpenaiResponses, "", "gpt-6-astra", Some(true), Some(true), Some(true), Some(true)),
            (OpenaiResponses, "https://relay.example.com/v1", "gpt-6-astra", None, Some(true), None, None),
            (OpenaiCodex, "", "gpt-6-astra", Some(true), Some(true), Some(true), Some(true)),
            (OpenaiCodex, "", "gpt-5.6-codex", Some(true), Some(true), Some(false), Some(true)),
            (OpenaiCodex, "http://127.0.0.1:9000/codex", "gpt-5.6-codex", Some(true), Some(true), Some(false), Some(true)),
            (OpenaiCodex, "", "gpt-reserve", Some(true), Some(true), None, Some(true)),
            (OpenaiCodex, "", "gpt-6.1-sol", Some(true), Some(true), Some(true), Some(true)),
            (OpenaiCodex, "", "gpt-6-luna", Some(true), Some(true), Some(true), Some(true)),
            (Azure, "https://mine.openai.azure.com/openai", "gpt-5.4", Some(true), Some(true), None, None),
            (Azure, "https://gateway.example.com/openai", "gpt-5.4", None, Some(true), None, None),
            (OpenaiChat, "https://api.openai.com/v1", "gpt-5.5", Some(false), Some(true), Some(false), Some(false)),
            (OpenaiChat, "https://api.deepseek.com", "deepseek-v4", Some(false), None, Some(false), Some(false)),
            (OpenaiCompatible, "https://api.deepseek.com", "deepseek-v4", Some(false), None, Some(false), Some(false)),
            (Google, "", "gemini-3-pro", Some(false), Some(false), Some(false), Some(false)),
            (Xai, "https://api.x.ai/v1", "grok-5", Some(false), None, Some(false), Some(false)),
            (Bedrock, "", "anthropic.claude-opus-5-5", Some(false), Some(false), Some(false), Some(false)),
            (Vertex, "", "gemini-3-flash", Some(false), Some(false), Some(false), Some(false)),
        ]
    };

    #[test]
    fn mewrk_knows_the_vendors_endpoints_and_leaves_the_rest_to_the_user() {
        for (family, base_url, model, tool, system, asynchronous, compacting) in APPEND_FIXTURE {
            assert_eq!(known(*family, base_url, model), *tool, "tool: {family:?} {base_url} {model}");
            assert_eq!(
                crate::native_compaction::known(*family, base_url, model),
                *compacting,
                "compaction: {family:?} {base_url} {model}"
            );
            assert_eq!(
                crate::system_append::known(*family, base_url, model),
                *system,
                "system: {family:?} {base_url} {model}"
            );
            assert_eq!(
                crate::async_tools::known(*family, base_url, model),
                *asynchronous,
                "async: {family:?} {base_url} {model}"
            );
        }
    }

    #[test]
    fn the_declared_capability_decides_where_the_protocol_can_append() {
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
                [ModelCapability::ToolAppend].into()
            } else {
                Default::default()
            },
            reasoning_content: Default::default(),
            prompt_cache: true,
            cache_ttl_minutes: None,
        };
        // A relay appends exactly when the model says it does, and so does
        // Anthropic's own endpoint: the declaration is the model's, whoever
        // filled it in.
        let relay = provider(ProviderFamily::Anthropic, "https://relay.example.com/v1");
        assert!(appends_tools(&relay, &model("claude-opus-5-5", true)));
        assert!(!appends_tools(&relay, &model("claude-opus-5-5", false)));
        let anthropic = provider(ProviderFamily::Anthropic, "https://api.anthropic.com/v1");
        assert!(!appends_tools(&anthropic, &model("claude-opus-5-5", false)));
        // A protocol without the interface has nothing to append through.
        let chat = provider(ProviderFamily::OpenaiCompatible, "https://relay.example.com/v1");
        assert!(!appends_tools(&chat, &model("deepseek-v4", true)));
    }

    /// Every family Mewrk could ever answer "yes" for reads the capability,
    /// and every family it always answers "no" for leaves it idle.
    #[test]
    fn mewrk_declares_the_capability_only_where_it_takes_effect() {
        for (family, base_url, model, tool, _, _, _) in APPEND_FIXTURE {
            if *tool == Some(true) || tool.is_none() {
                assert!(family.tool_append_takes_effect(), "{family:?} {base_url} {model}");
            }
        }
        for (family, base_url, model, _, system, _, _) in APPEND_FIXTURE {
            if *system == Some(true) || system.is_none() {
                assert!(family.system_append_takes_effect(), "{family:?} {base_url} {model}");
            }
        }
        for (family, base_url, model, _, _, asynchronous, _) in APPEND_FIXTURE {
            if *asynchronous == Some(true) || asynchronous.is_none() {
                assert!(family.async_tools_take_effect(), "{family:?} {base_url} {model}");
            }
        }
        for (family, base_url, model, _, _, _, compacting) in APPEND_FIXTURE {
            if *compacting == Some(true) || compacting.is_none() {
                assert!(family.native_compaction_takes_effect(), "{family:?} {base_url} {model}");
            }
        }
    }

    #[test]
    fn nothing_is_new_without_a_previous_request() {
        assert!(newly_offered(&names(&["read", "write"]), None, &HashSet::new()).is_empty());
    }

    #[test]
    fn a_tool_is_new_once_and_in_offered_order() {
        let previous = BTreeSet::from(["read".to_owned()]);
        let offered = names(&["read", "handoff", "create_handoff_note"]);
        assert_eq!(
            newly_offered(&offered, Some(&previous), &HashSet::new()),
            names(&["handoff", "create_handoff_note"])
        );
        // Offered by the request before: it is no newcomer.
        let previous = BTreeSet::from(["read".to_owned(), "handoff".to_owned()]);
        assert_eq!(
            newly_offered(&offered, Some(&previous), &HashSet::new()),
            names(&["create_handoff_note"])
        );
    }

    #[test]
    fn a_tool_coming_back_to_its_own_calls_is_not_appended() {
        let previous = BTreeSet::from(["read".to_owned()]);
        let offered = names(&["read", "bash", "grep"]);
        let history = HashSet::from(["bash".to_owned()]);
        assert_eq!(
            newly_offered(&offered, Some(&previous), &history),
            names(&["grep"])
        );
    }

    fn tool_card(id: &str, tool: &str, turn: Option<&str>, call: Option<&str>) -> ContextItem {
        ContextItem::Tool {
            id: id.into(),
            tool_name: tool.into(),
            round: None,
            model_turn_id: turn.map(str::to_owned),
            provider_call_id: call.map(str::to_owned),
            requested_input: None,
            input: Default::default(),
            result: crate::model::ToolResult {
                success: true,
                output: "ok".into(),
                images: Vec::new(),
                diff: None,
                executed_at: String::new(),
                duration_ms: 0,
            },
            subagent: None,
            notice: None,
            attestation: String::new(),
            created_at: String::new(),
        }
    }

    #[test]
    fn only_the_models_own_calls_count_as_a_tools_history() {
        let contexts = [
            tool_card("a", "bash", Some("turn"), None),
            tool_card("b", "grep", None, None),
            marker("c".into(), names(&["read"]), "t".into()),
        ];
        assert_eq!(called(&contexts), HashSet::from(["bash".to_owned()]));
    }

    #[test]
    fn the_mask_covers_a_closed_tools_calls_and_nothing_else() {
        let closed = BTreeSet::from(["bash".to_owned()]);
        let call = tool_card("a", "bash", Some("turn"), Some("call_1"));
        let other = tool_card("b", "read", Some("turn"), Some("call_2"));
        let written = tool_card("c", "bash", None, Some("call_3"));
        let mask = WireMask::of(&closed, [&call, &other, &written]);
        assert!(mask.hides_call(&call));
        assert!(!mask.hides_call(&other));
        assert!(!mask.hides_call(&written), "somebody wrote it; nothing was turned off");
        assert!(mask.hides_result_of("call_1"));
        assert!(!mask.hides_result_of("call_2"));
        assert!(!mask.hides_result_of("call_3"));
    }

    #[test]
    fn taking_a_registration_rewrites_or_removes_its_card_and_nothing_else() {
        let mut contexts = vec![
            marker("before_the_view".into(), names(&["a"]), "t".into()),
            marker("shared".into(), names(&["a", "b"]), "t".into()),
            marker("alone".into(), names(&["a"]), "t".into()),
            marker("other".into(), names(&["b"]), "t".into()),
            marker("this_run".into(), names(&["a"]), "t".into()),
        ];
        let own = HashSet::from(["this_run".to_owned()]);
        let (changed, removed) = take_registrations(&mut contexts, 1, &names(&["a"]), &own);
        assert_eq!(removed, names(&["alone"]));
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].id(), "shared");
        assert_eq!(added_tools(&changed[0]), names(&["b"]).as_slice());
        assert!(matches!(&changed[0], ContextItem::System { content, .. } if content == "Tools added: b"));
        let left: Vec<&str> = contexts.iter().map(ContextItem::id).collect();
        assert_eq!(left, ["before_the_view", "shared", "other", "this_run"]);
        assert_eq!(added_tools(&contexts[0]), names(&["a"]).as_slice());
        assert_eq!(added_tools(&contexts[3]), names(&["a"]).as_slice());
    }

    #[test]
    fn only_the_latest_registration_of_a_tool_counts() {
        let first = marker("m1".into(), names(&["a", "b"]), "t".into());
        let second = marker("m2".into(), names(&["a"]), "t".into());
        let mask = WireMask::of(&BTreeSet::new(), [&first, &second, &second]);
        assert_eq!(mask.registered("m1", &names(&["a", "b"])), names(&["b"]));
        assert_eq!(mask.registered("m2", &names(&["a"])), names(&["a"]));
    }

    #[test]
    fn a_withdrawn_tool_is_not_new() {
        let previous = BTreeSet::from(["read".to_owned(), "plan".to_owned()]);
        assert!(newly_offered(&names(&["read"]), Some(&previous), &HashSet::new()).is_empty());
    }

    #[test]
    fn the_marker_is_local_and_projects_as_an_empty_system_message() {
        let marker = marker("ctx_tools-added_1".into(), names(&["handoff"]), "t".into());
        assert!(matches!(marker, ContextItem::System { local_only: true, .. }));
        assert_eq!(added_tools(&marker), names(&["handoff"]).as_slice());
        assert_eq!(
            marker_message(&names(&["handoff"])),
            json!({
                "role": "system",
                "content": "",
                "providerOptions": { "mewrk": { "toolAddition": ["handoff"] } },
            })
        );
    }

    #[test]
    fn the_registry_keeps_the_last_set_per_conversation() {
        let registry = OfferedToolRegistry::default();
        assert!(registry.last("c").is_none());
        registry.record("c", BTreeSet::from(["read".to_owned()]));
        registry.record("c", BTreeSet::from(["write".to_owned()]));
        assert_eq!(registry.last("c"), Some(BTreeSet::from(["write".to_owned()])));
    }
}
