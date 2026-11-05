//! Rules the pages share and the client has no home for: `lib/catalogOrder.ts`, `lib/cacheBreakWarning.ts`, `DocsLink.tsx`'s addresses and the role row's model label.
//! The role, tool, preset and workspace rules are the client's.

use std::collections::{HashMap, HashSet};

pub use mewrk_client::agent_roles::*;
pub use mewrk_client::model_capabilities::supports_vision;
pub use mewrk_client::presets::*;
pub use mewrk_client::doc::model_choice as model_choice_of;
pub use mewrk_client::task_tools::*;
pub use mewrk_client::workspaces::{capability_workspace_key, capability_workspaces, parse_capability_workspace_key, workspace_location_title, workspace_machine_label, CapabilityWorkspace};

use mewrk_client::model::{self, AgentModelSelection, ApiProvider, ConversationPreset, ConversationSettings, ReasoningEffort};
use mewrk_client::{t, tf, App};

/// The roles out of a catalog. The host's own row type lives in a private module, and the client's is the same JSON.
pub fn agent_resources(catalog: &model::CapabilityCatalog) -> Vec<AgentRoleResource> {
    catalog
        .agents
        .iter()
        .map(|d| AgentRoleResource {
            descriptor: d.descriptor.clone(),
            role: d.role.as_ref().map(|r| AgentRole {
                name: r.name.clone(),
                description: r.description.clone(),
                model_selection: r.model_selection.clone(),
                effort: r.effort,
                tools: r.tools.clone().unwrap_or_default(),
                disallowed_tools: r.disallowed_tools.clone(),
                skill_ids: r.skill_ids.clone(),
                mcp_ids: r.mcp_ids.clone(),
                hook_ids: r.hook_ids.clone(),
                web_search: r.web_search.clone(),
                template_id: r.template_id.clone(),
                tool_description_file_id: r.tool_description_file_id.clone(),
            }),
        })
        .collect()
}

/// The five reasoning levels, lowest first.
pub const REASONING_EFFORTS: [ReasoningEffort; 5] = [ReasoningEffort::Low, ReasoningEffort::Medium, ReasoningEffort::High, ReasoningEffort::Extra, ReasoningEffort::Max];

pub fn effort_label(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::Extra => "extra",
        ReasoningEffort::Max => "max",
    }
}

/// The row's model, said the way the host will resolve it. A bound pair names its provider by display name, never by the random
/// per-installation id, which reads as a hash; a provider deleted outright leaves the model id standing alone.
pub fn agent_role_model_label(selection: &AgentModelSelection, providers: &[ApiProvider]) -> String {
    match selection {
        AgentModelSelection::Inherit => t("跟随对话模型", "Follows the conversation's model").to_owned(),
        AgentModelSelection::Unavailable => t("没有可用的模型，模型看不到这个角色", "No model to run on; hidden from the model").to_owned(),
        AgentModelSelection::Explicit { provider_id, model_id } => {
            let label = providers.iter().find(|p| p.id == *provider_id).map_or_else(|| model_id.clone(), |p| format!("{} · {model_id}", p.name));
            if agent_model_selection_is_available(selection, providers) {
                label
            } else {
                tf("{model} · 模型暂时取不到，模型看不到这个角色", "{model} · model unavailable for now, hidden from the model", &[("model", &label)])
            }
        }
    }
}

/// A preset body widened into the shape the pane edits. What a preset does not own is borrowed so the pane has something coherent
/// to draw and dropped again on save. `tool_lock` is deliberately not borrowed: a preset has run nothing and cached nothing.
pub fn preset_body_as_settings(preset: &ConversationPreset, conversation: &ConversationSettings) -> ConversationSettings {
    let p = &preset.settings;
    ConversationSettings {
        enabled_tools: p.enabled_tools.clone(),
        hook_ids: p.hook_ids.clone(),
        skill_ids: p.skill_ids.clone(),
        mcp_ids: p.mcp_ids.clone(),
        tool_description_file_id: p.tool_description_file_id.clone(),
        agent_ids: p.agent_ids.clone(),
        agent_definitions: Vec::new(),
        allow_roleless_subagents: p.allow_roleless_subagents,
        web_search: p.web_search.clone(),
        web_search_enabled: p.web_search_enabled,
        subagent_messaging_enabled: p.subagent_messaging_enabled,
        reasoning_effort: conversation.reasoning_effort,
        security_level: p.security_level,
        plan_mode_enabled: false,
        global_memory_enabled: p.global_memory_enabled,
        project_memory_enabled: p.project_memory_enabled,
        skill_tool_enabled: p.skill_tool_enabled,
        mcp_tool_discovery_enabled: p.mcp_tool_discovery_enabled,
        host_message_container: p.host_message_container,
        file_write_guards_enabled: p.file_write_guards_enabled,
        environment_prompt_enabled: p.environment_prompt_enabled,
        compaction_method: None,
        legacy_sandbox: Default::default(),
        tool_lock: None,
    }
}

// ---------------------------------------------------------------- catalog order (lib/catalogOrder.ts)

const CATALOG_ORDER_KEY: &str = "mewrk.catalog-order.v1";

/// Which order a catalog list is kept under. These names are the stored keys, so they stay as upstream wrote them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OrderScope {
    Skills,
    Mcp,
    Hooks,
    Agents,
    ToolDescriptions,
    Presets,
}

impl OrderScope {
    fn key(self) -> &'static str {
        match self {
            Self::Skills => "skills",
            Self::Mcp => "mcp",
            Self::Hooks => "hooks",
            Self::Agents => "agents",
            Self::ToolDescriptions => "toolDescriptions",
            Self::Presets => "presets",
        }
    }
}

fn stored_orders(app: &mut App) -> HashMap<String, Vec<String>> {
    app.ui_store().get_json(CATALOG_ORDER_KEY).unwrap_or_default()
}

pub fn catalog_order(app: &mut App, scope: OrderScope) -> Vec<String> {
    stored_orders(app).remove(scope.key()).unwrap_or_default()
}

/// A row the catalog gains later is not in the stored order and sits at the end in catalog order; a row it loses drops out.
pub fn sort_by_order<T>(items: &[T], order: &[String], id_of: impl Fn(&T) -> &str) -> Vec<T>
where
    T: Clone,
{
    if order.is_empty() {
        return items.to_vec();
    }
    let rank: HashMap<&str, usize> = order.iter().enumerate().map(|(i, id)| (id.as_str(), i)).collect();
    let mut decorated: Vec<(Option<usize>, usize, &T)> = items.iter().enumerate().map(|(i, item)| (rank.get(id_of(item)).copied(), i, item)).collect();
    decorated.sort_by(|a, b| match (a.0, b.0) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.1.cmp(&b.1),
    });
    decorated.into_iter().map(|(_, _, item)| item.clone()).collect()
}

/// `source` moved next to `target`.
pub fn reorder_ids(ids: &[String], source: &str, target: &str, before: bool) -> Vec<String> {
    if source == target || !ids.iter().any(|i| i == source) || !ids.iter().any(|i| i == target) {
        return ids.to_vec();
    }
    let mut out: Vec<String> = ids.iter().filter(|i| *i != source).cloned().collect();
    let at = out.iter().position(|i| i == target).unwrap_or(out.len());
    out.insert(if before { at } else { at + 1 }, source.to_owned());
    out
}

/// The stored order is rewritten from the whole visible list on every drop, so it re-anchors to what the catalog holds.
pub fn write_catalog_order(app: &mut App, scope: OrderScope, ids: Vec<String>) {
    let mut orders = stored_orders(app);
    orders.insert(scope.key().to_owned(), ids);
    app.ui_store_update(|s| s.set_json(CATALOG_ORDER_KEY, &orders));
}

// ---------------------------------------------------------------- cache-break warning (lib/cacheBreakWarning.ts)

const CACHE_BREAK_KEY: &str = "mewrk.cache-break-warning.v1";

#[derive(Default)]
struct CacheBreakAcks(HashSet<String>);

/// Said once per conversation, for this session of the app only; "don't show again" is a view preference beside the row order.
pub fn should_warn_cache_break(app: &mut App, conversation_id: &str) -> bool {
    !app.local::<CacheBreakAcks>().0.contains(conversation_id) && app.ui_store().get(CACHE_BREAK_KEY) != Some("off")
}

pub fn acknowledge_cache_break(app: &mut App, conversation_id: &str, never: bool) {
    app.local::<CacheBreakAcks>().0.insert(conversation_id.to_owned());
    if never {
        app.ui_store_update(|s| s.set(CACHE_BREAK_KEY, "off"));
    }
}

/// For tests: forget every answer.
pub fn reset_cache_break_warnings(app: &mut App) {
    app.local::<CacheBreakAcks>().0.clear();
    app.ui_store_update(|s| s.remove(CACHE_BREAK_KEY));
}

// ---------------------------------------------------------------- docs (DocsLink.tsx)

const DOCUMENTATION_ORIGIN: &str = "https://mewrk.dev";

fn docs_locale() -> &'static str {
    if mewrk_client::i18n::is_english() { "en" } else { "zh-CN" }
}

/// The address of one page of the site, or of one section of it.
pub fn docs_href(target: &str, section: Option<&str>) -> String {
    let (page, surface_section) = match target {
        "features" => ("working", Some("conversation-settings")),
        "toolDescriptions" => ("prompt-profiles", None),
        "agents" => ("agents", Some("roles")),
        other => (other, None),
    };
    match section.or(surface_section) {
        Some(anchor) => format!("{DOCUMENTATION_ORIGIN}/{}/{page}.html#{anchor}", docs_locale()),
        None => format!("{DOCUMENTATION_ORIGIN}/{}/{page}.html", docs_locale()),
    }
}

/// One tool's own page. The site publishes one per catalog tool.
pub fn tool_docs_href(name: &str) -> String {
    format!("{DOCUMENTATION_ORIGIN}/{}/tools/{name}.html", docs_locale())
}
