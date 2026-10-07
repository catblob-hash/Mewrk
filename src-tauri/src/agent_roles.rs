//! Subagent roles, as files.
//!
//! A role is one JSON file directly inside an `agents/` directory, in the same
//! two levels skills are read from: `~/.mewrk/agents/<file>.json` for every
//! workspace and `<workspace>/.mewrk/agents/<file>.json` for one (the legacy
//! `.naiword` spelling is read when that is all there is). The file is the
//! role: a conversation offers the ones it selects by the id discovery mints
//! from the file's location (`ConversationSettings::agent_ids`), and the host
//! resolves a spawn against those files, never against anything the renderer
//! sends.
//!
//! The four built-in roles are the exception: they are compiled in, computed
//! against the document's providers on demand, read-only, and addressed by
//! constant ids so a conversation keeps them across versions.
//!
//! Everything tool-like a role holds is its own — its tool list, its skills
//! and MCP servers, and its whole web-search configuration. Only its model
//! (`inherit`) and its reasoning effort (`null`) may follow the caller. Hooks
//! are the one kind that adds rather than replaces: a role's child runs the
//! guards its caller runs (`api::hook_runs_in_subagent`) and the role's own
//! hooks on top, so no role can step outside a conversation's guards.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    sync::{OnceLock, RwLock},
};

use serde::{Deserialize, Serialize};

use crate::model::{
    AgentDefinition, AgentDefinitionMemory, AgentDefinitionSource, AgentModelSelection,
    AppDocument, Conversation, ConversationWebSearchSettings, ProviderFamily, ReasoningEffort,
    ResourceDescriptor, ResourceSource, ToolCategory,
};

/// The built-in roles' ids. Constants rather than hashes of a location: a
/// built-in has no file, and a conversation that selected one keeps it across
/// versions. `src/seed.ts` names the same four for the built-in preset.
pub const BUILTIN_OPUS_ID: &str = "agent_builtin_opus";
pub const BUILTIN_SONNET_ID: &str = "agent_builtin_sonnet";
pub const BUILTIN_SOL_ID: &str = "agent_builtin_sol";
pub const BUILTIN_LUNA_ID: &str = "agent_builtin_luna";

/// The four built-in ids, in the order the catalog lists them.
pub const BUILTIN_ROLE_IDS: [&str; 4] = [
    BUILTIN_OPUS_ID,
    BUILTIN_SONNET_ID,
    BUILTIN_SOL_ID,
    BUILTIN_LUNA_ID,
];

/// The largest role file read, as for a skill's manifest.
pub(crate) const ROLE_FILE_READ_LIMIT: usize = 256 * 1024;

/// The longest file name a new role gets, extension aside: this many
/// characters, and never more than [`MAX_FILE_SLUG_BYTES`] bytes of UTF-8 —
/// a file system counts the bytes, and 64 CJK characters are 192 of them.
const MAX_FILE_SLUG_CHARS: usize = 64;
const MAX_FILE_SLUG_BYTES: usize = 100;

/// How many names a new role file tries before giving up, when each one it
/// picks turns out to be taken by the time it is created.
const MAX_NEW_FILE_ATTEMPTS: usize = 64;

/// The file in the application data directory that records which role file
/// each legacy role was exported to (see [`migrate_legacy_agent_definitions`]).
const EXPORT_RECORD_FILE: &str = "agent-role-export.json";

/// The built-in roles: id, name, the provider family it runs on, its model,
/// and what it is for.
///
/// Each role is bound to one model, and the binding is kept while that model
/// does not exist yet — Codex lists nothing until the user signs in — so the
/// role starts working the moment its model shows up. The descriptions reach
/// the model as the role listing on `agent_spawn`/`workflow`, so they say what
/// each model is good and bad at and when to pick it.
const BUILTIN_ROLES: &[(&str, &str, ProviderFamily, &str, &str)] = &[
    (
        BUILTIN_OPUS_ID,
        "Opus",
        ProviderFamily::ClaudeAgent,
        "claude-opus-5-5",
        "Claude Opus 5.5 (Anthropic). Excellent judgement and taste. Takes on ambiguous, open-ended and hard problems, including ones where it is not yet clear what the problem is, and its conclusions can be relied on. Pick it for the hardest work and for anything that needs sound judgement across a wide area: design, diagnosis, review and deciding what to do.",
    ),
    (
        BUILTIN_SONNET_ID,
        "Sonnet",
        ProviderFamily::ClaudeAgent,
        "claude-sonnet-5-5",
        "Claude Sonnet 5.5 (Anthropic). Has the strengths of Opus in a lighter form: good judgement and able to work through ambiguous problems, though less reliably on the hardest ones, and at a lower cost. Pick it for work that needs judgement but not Opus's full depth.",
    ),
    (
        BUILTIN_SOL_ID,
        "Sol",
        ProviderFamily::OpenaiCodex,
        "gpt-6.1-sol",
        "GPT-6.1 Sol (OpenAI). Rigorous, precise reasoning that very rarely makes a mistake, but weak judgement: it does poorly on vague tasks and on ones that call for weighing many things across a wide area, and is at its best on a focused, local problem. Pick it for clearly specified, verifiable work where correctness matters most; give it the exact goal and how to check the result, and keep open-ended decisions away from it.",
    ),
    (
        BUILTIN_LUNA_ID,
        "Luna",
        ProviderFamily::OpenaiCodex,
        "gpt-6-luna",
        "GPT-6 Luna (OpenAI). Very cheap, with limited reasoning: don't ask it to make judgement calls or to verify anything that is hard to check. Pick it for simple, mechanical, easily checked chores in bulk, such as repetitive edits, searches and lookups, and collecting or summarizing output, and for many parallel workers.",
    ),
];

/// The model ids and descriptions earlier builds seeded the built-in roles
/// with, by built-in id — the current ones are in [`BUILTIN_ROLES`]. A legacy
/// copy a conversation kept from one of those builds is still an untouched
/// built-in, not a role of the user's (see [`builtin_copy_of`]).
const EARLIER_BUILTIN_ROLES: &[(&str, &str, &str)] = &[
    (
        BUILTIN_OPUS_ID,
        "claude-opus-5-5",
        "Claude Opus 5.5 (Anthropic). Strongest at long, sprawling engineering work: codebase-wide migrations and audits, hard debugging, and changes that need careful judgement and checking its own work. Pick it for the largest and hardest tasks.",
    ),
    (
        BUILTIN_SOL_ID,
        "gpt-6-sol",
        "GPT-6 Sol (OpenAI). Built for complex coding and agentic workflows that need strong reasoning: multi-step implementation, refactoring and debugging across several files. Pick it for demanding coding tasks.",
    ),
    (
        BUILTIN_LUNA_ID,
        "gpt-6-luna",
        "GPT-6 Luna (OpenAI). Fast and low-cost, for focused, high-volume work: well-scoped edits, searches and lookups, running tests and summarizing their output. Pick it for repeatable tasks at scale and for many parallel workers.",
    ),
];

/// One role file's content.
///
/// Every key may be missing: a missing `name` reads as the file's stem, a
/// missing model as `inherit`, a missing `tools` as every tool a role can hold
/// ([`all_role_tool_names`]), and a missing `webSearch` as the defaults.
/// Unknown keys are read past. The host writes these with
/// `serde_json::to_string_pretty`, `tools` always as a concrete list.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentRoleFile {
    /// The name the model selects the role by (`agent_spawn.agent_type`).
    #[serde(default)]
    pub name: String,
    /// What the role is for, in the user's words; the model reads it in the
    /// role listing.
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub model_selection: AgentModelSelection,
    /// `None` runs at the caller's reasoning effort.
    #[serde(default)]
    pub effort: Option<ReasoningEffort>,
    /// The role's tools. `None` (no key) is every tool a role can hold; a
    /// list, empty included, is exactly that list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    #[serde(default)]
    pub disallowed_tools: Vec<String>,
    /// The role's own skills, MCP servers and hooks, by catalog id. Its
    /// skills and servers are the child's whole set; its hooks run on top of
    /// the calling conversation's guards, which every child runs.
    #[serde(default)]
    pub skill_ids: Vec<String>,
    #[serde(default)]
    pub mcp_ids: Vec<String>,
    #[serde(default)]
    pub hook_ids: Vec<String>,
    /// The role's whole web-search configuration, in a conversation's shape.
    /// Whether the child reaches the web at all is still the caller's switch.
    #[serde(default)]
    pub web_search: ConversationWebSearchSettings,
    /// A template from the host's template store, seeded as the opening of an
    /// `agent_spawn` child. May dangle.
    #[serde(default)]
    pub template_id: Option<String>,
}

/// A role as the catalog lists it: the row every capability has, and the
/// role's content when the file could be used.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentRoleDescriptor {
    /// `description` is the role's own, or why the file cannot be used.
    #[serde(flatten)]
    pub descriptor: ResourceDescriptor,
    /// `None` when the file cannot be used. `tools` is always a list here.
    pub role: Option<AgentRoleFile>,
}

/// Where `save_agent_role` writes: over the file of the role `id` names (as a
/// fresh scan finds it), or a new file in the level `workspace_key` names —
/// `None` is the global `~/.mewrk`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveAgentRoleTarget {
    pub id: Option<String>,
    pub workspace_key: Option<String>,
}

/// Which level a role file was read at.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum RoleLevel {
    Global,
    /// A workspace, by [`crate::capabilities::workspace_key`].
    Workspace(String),
}

impl RoleLevel {
    pub(crate) fn of_workspace_key(workspace_key: Option<&str>) -> Self {
        match workspace_key {
            Some(key) => Self::Workspace(key.to_owned()),
            None => Self::Global,
        }
    }
}

/// An available role file, as the runtime resolves it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RegisteredAgentRole {
    pub id: String,
    pub level: RoleLevel,
    pub definition: AgentDefinition,
}

/// The available role files the host last read, by id.
///
/// Refreshed from every scan that read them — the catalog's, every top-level
/// run's, and the two role commands — so a spawn resolves against the files
/// as the latest scan found them. Built-in roles are not kept here: they are
/// computed from the document whenever they are asked for.
///
/// Every change is made under the named-agent write fence
/// (`AppState::definition_authority_lock`, [`publish_scan`] and the fenced
/// role commands), so no child validates its role against one registry and
/// acts against another. A save or a delete also advances `generation`: a
/// scan carries the generation it started at, and one that started before
/// the latest save or delete is not published at all (see
/// [`Self::replace_levels_since`]).
#[derive(Debug, Default)]
pub(crate) struct AgentRoleRegistry {
    roles: HashMap<String, RegisteredAgentRole>,
    generation: u64,
}

impl AgentRoleRegistry {
    /// Replaces what the levels in `scanned` held with `roles`. A level the
    /// scan could not read — a machine that did not answer — keeps what it
    /// held, so a role there does not vanish from under a running child
    /// because of a network blip.
    pub(crate) fn replace_levels(&mut self, scanned: &[RoleLevel], roles: Vec<RegisteredAgentRole>) {
        self.roles.retain(|_, role| !scanned.contains(&role.level));
        for role in roles {
            self.roles.insert(role.id.clone(), role);
        }
    }

    /// [`Self::replace_levels`] for a scan that started when the registry was
    /// at `started`, and only then: a save or a delete since means the scan
    /// may have read the files before it, and publishing it would put a
    /// deleted role back or take a saved one away. Such a scan is dropped
    /// whole — the simplest rule that is never wrong, since the save or
    /// delete already published what it changed and the next scan publishes
    /// the rest. Returns whether the scan was published.
    pub(crate) fn replace_levels_since(
        &mut self,
        started: u64,
        scanned: &[RoleLevel],
        roles: Vec<RegisteredAgentRole>,
    ) -> bool {
        if started != self.generation {
            return false;
        }
        self.replace_levels(scanned, roles);
        true
    }

    /// The generation a scan starting now carries.
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// A role a save just wrote.
    pub(crate) fn insert(&mut self, role: RegisteredAgentRole) {
        self.generation += 1;
        self.roles.insert(role.id.clone(), role);
    }

    /// A role a delete just removed.
    pub(crate) fn remove(&mut self, id: &str) {
        self.generation += 1;
        self.roles.remove(id);
    }

    pub(crate) fn get(&self, id: &str) -> Option<&RegisteredAgentRole> {
        self.roles.get(id)
    }

    #[cfg(test)]
    pub(crate) fn clear(&mut self) {
        self.roles.clear();
    }
}

/// The generation a scan about to start carries, for [`publish_scan`]. Read
/// before the scan reads any file.
pub(crate) fn scan_generation(registry: &RwLock<AgentRoleRegistry>) -> u64 {
    registry
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .generation()
}

/// Publishes the role files a scan read (`scanned`, `roles`) to the registry
/// spawns resolve against, under the named-agent write fence `authority` —
/// unless a save or a delete came after the scan began (`started`, from
/// [`scan_generation`]). Returns whether it was published.
pub(crate) fn publish_scan(
    authority: &RwLock<()>,
    registry: &RwLock<AgentRoleRegistry>,
    started: u64,
    scanned: &[RoleLevel],
    roles: Vec<RegisteredAgentRole>,
) -> bool {
    // A poisoned fence still fences: the guard is all it is for.
    let _authority = authority.write().unwrap_or_else(|poisoned| poisoned.into_inner());
    registry
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .replace_levels_since(started, scanned, roles)
}

/// Every tool a role can hold: the built-in catalog less the orchestration and
/// memory tools and the names the host derives for a child itself
/// (`api::host_derived_child_tool`) — exactly what the role editor's tool
/// picker can show. A role file without `tools` holds all of them.
pub fn all_role_tool_names() -> Vec<String> {
    static NAMES: OnceLock<Vec<String>> = OnceLock::new();
    NAMES
        .get_or_init(|| {
            crate::catalog::tool_catalog()
                .into_iter()
                .filter(|tool| {
                    !matches!(tool.category, ToolCategory::Orchestration | ToolCategory::Memory)
                        && !crate::api::host_derived_child_tool(&tool.name)
                })
                .map(|tool| tool.name)
                .collect()
        })
        .clone()
}

/// Whether `id` is one of the built-in roles.
pub fn is_builtin_id(id: &str) -> bool {
    BUILTIN_ROLE_IDS.contains(&id)
}

fn builtin_read_only() -> String {
    crate::ui_text::pick(
        "内置角色随 Mewrk 发布，不能修改或删除；可以把它另存为全局角色",
        "A built-in role ships with Mewrk and cannot be changed or deleted; save it as a global role instead",
    )
    .to_owned()
}

/// The built-in roles as files, against `document`'s providers: each runs on
/// the first provider of its family, and one whose family no provider has is
/// still listed, with its model unavailable.
pub(crate) fn builtin_role_files(document: &AppDocument) -> Vec<(&'static str, AgentRoleFile)> {
    BUILTIN_ROLES
        .iter()
        .map(|(id, name, family, model_id, description)| {
            let model_selection = document
                .assets
                .api_providers
                .iter()
                .find(|provider| provider.family == *family)
                .map(|provider| AgentModelSelection::Explicit {
                    provider_id: provider.id.clone(),
                    model_id: (*model_id).to_owned(),
                })
                .unwrap_or(AgentModelSelection::Unavailable);
            (
                *id,
                AgentRoleFile {
                    name: (*name).to_owned(),
                    description: (*description).to_owned(),
                    model_selection,
                    effort: Some(ReasoningEffort::Medium),
                    tools: Some(all_role_tool_names()),
                    disallowed_tools: Vec::new(),
                    skill_ids: Vec::new(),
                    mcp_ids: Vec::new(),
                    hook_ids: Vec::new(),
                    web_search: ConversationWebSearchSettings::default(),
                    template_id: None,
                },
            )
        })
        .collect()
}

/// The built-in roles as catalog rows, first in the list.
pub(crate) fn builtin_descriptors(document: &AppDocument) -> Vec<AgentRoleDescriptor> {
    builtin_role_files(document)
        .into_iter()
        .map(|(id, role)| AgentRoleDescriptor {
            descriptor: ResourceDescriptor {
                id: id.to_owned(),
                name: role.name.clone(),
                description: role.description.clone(),
                location: format!("builtin:agents/{}.json", role.name.to_lowercase()),
                source: ResourceSource::Builtin,
                available: true,
                workspace_key: None,
            },
            role: Some(role),
        })
        .collect()
}

/// The built-in role `id` names, as the runtime resolves it.
pub(crate) fn builtin_definition(document: &AppDocument, id: &str) -> Option<AgentDefinition> {
    if !is_builtin_id(id) {
        return None;
    }
    builtin_role_files(document)
        .into_iter()
        .find(|(builtin, _)| *builtin == id)
        .map(|(_, role)| {
            definition_of(&role, AgentDefinitionSource::Plugin, "builtin".to_owned(), id)
        })
}

/// The source a role read at `level` resolves with. Precedence runs Managed >
/// Project > User > Plugin, so a workspace's role shadows a global one of the
/// same name, and either shadows a built-in.
///
/// A workspace role's key is a digest of its workspace's key: stable across
/// runs, bounded in length, and different for every workspace.
fn source_of(level: &RoleLevel) -> (AgentDefinitionSource, String) {
    match level {
        RoleLevel::Global => (AgentDefinitionSource::User, String::new()),
        RoleLevel::Workspace(key) => (
            AgentDefinitionSource::Project,
            format!("workspace:{}", &crate::api::sha256_hex(key.as_bytes())[..16]),
        ),
    }
}

/// A role file read at `level` under `id`, as the runtime resolves it.
pub(crate) fn registered_definition(role: &AgentRoleFile, level: &RoleLevel, id: &str) -> AgentDefinition {
    let (source, source_key) = source_of(level);
    definition_of(role, source, source_key, id)
}

/// The revision a role's children are bound to: a digest of the role's id,
/// and so of its file. It survives every edit of the file, a rename of the
/// role inside it included — a bound child keeps running, re-derived from the
/// file each turn it resumes — and differs for every file, so a child bound
/// to one `reviewer` is refused once another file's `reviewer` takes its
/// place. Positive and within JavaScript's exact integers, as a binding's
/// revision must be.
pub(crate) fn role_revision(id: &str) -> u64 {
    const MAX_EXACT_INTEGER: u64 = (1 << 53) - 1;
    let digest = crate::api::sha256_hex(id.as_bytes());
    let value = u64::from_str_radix(&digest[..16], 16).unwrap_or(1) & MAX_EXACT_INTEGER;
    value.max(1)
}

/// The runtime definition of a role file, `id` the role's. Everything a file
/// can say is the role's own answer, so every override is `Some`: the role
/// never takes its tools, its skills or MCP servers, or any part of its web
/// search from the conversation that calls it. Its hooks run on top of the
/// caller's guards (`api::apply_role_capabilities`).
fn definition_of(
    role: &AgentRoleFile,
    source: AgentDefinitionSource,
    source_key: String,
    id: &str,
) -> AgentDefinition {
    let web = &role.web_search;
    AgentDefinition {
        enabled: true,
        deleted: false,
        name: role.name.clone(),
        description: role.description.clone(),
        source,
        source_key,
        // A file has no revision history; its id stands in for one. A child
        // is revoked when the role is deselected, its file goes, or another
        // file's role of the same name takes its place.
        revision: role_revision(id),
        memory_epoch: 1,
        model_selection: role.model_selection.clone(),
        memory: AgentDefinitionMemory::None,
        effort: role.effort,
        tools: Some(role.tools.clone().unwrap_or_else(all_role_tool_names)),
        disallowed_tools: role.disallowed_tools.clone(),
        skill_ids: Some(role.skill_ids.clone()),
        mcp_ids: Some(role.mcp_ids.clone()),
        hook_ids: Some(role.hook_ids.clone()),
        search_provider: Some(web.provider.clone()),
        fetch_provider: Some(web.fetch_provider.clone()),
        max_results: web.max_results,
        compression_cutoff: web.compression_cutoff,
        fetch_compression_cutoff: web.fetch_compression_cutoff,
        domain_filter: Some(web.domain_filter),
        include_domains: web.include_domains.clone(),
        exclude_domains: web.exclude_domains.clone(),
        max_searches_per_call: Some(web.max_searches_per_call),
        native_search_tool: Some(web.native_search_tool),
        native_fetch_tool: Some(web.native_fetch_tool),
        template_id: role.template_id.clone(),
    }
}

/// The roles `conversation` offers, in the order it selected them: a built-in
/// id resolves against the document, any other against `registry`, where a
/// role counts only if it was read at the global level or at one of this
/// conversation's own workspaces. Anything else — a dangling id, a role of a
/// workspace this conversation does not work in — is skipped.
pub(crate) fn selected_definitions(
    document: &AppDocument,
    conversation: &Conversation,
    registry: &AgentRoleRegistry,
) -> Vec<AgentDefinition> {
    let own = crate::capabilities::conversation_locations(document, conversation)
        .into_iter()
        .flatten()
        .map(|location| crate::capabilities::workspace_key(&location))
        .collect::<HashSet<_>>();
    let mut seen = HashSet::new();
    conversation
        .settings
        .agent_ids
        .iter()
        .filter(|id| seen.insert(id.as_str()))
        .filter_map(|id| {
            if let Some(definition) = builtin_definition(document, id) {
                return Some(definition);
            }
            let role = registry.get(id)?;
            match &role.level {
                RoleLevel::Global => Some(role.definition.clone()),
                RoleLevel::Workspace(key) if own.contains(key) => Some(role.definition.clone()),
                RoleLevel::Workspace(_) => None,
            }
        })
        .collect()
}

/// The id prefix of a role read at `source`'s level. A built-in's whole id is
/// a constant ([`BUILTIN_ROLE_IDS`]); this prefix is theirs only for symmetry.
pub(crate) fn id_prefix(source: ResourceSource) -> &'static str {
    match source {
        ResourceSource::User => "agent_user",
        ResourceSource::Workspace => "agent_workspace",
        ResourceSource::Builtin => "agent_builtin",
    }
}

/// A role file's name less its extension, which its id is minted from: a
/// rename inside the file keeps the id, and with it every selection.
pub(crate) fn file_stem(file_name: &str) -> &str {
    match file_name.rsplit_once('.') {
        Some((stem, extension)) if extension.eq_ignore_ascii_case("json") => stem,
        _ => file_name,
    }
}

/// Whether `file_name` is one a role file may have: a `*.json` with a stem.
pub(crate) fn is_role_file_name(file_name: &str) -> bool {
    file_name
        .rsplit_once('.')
        .is_some_and(|(stem, extension)| extension.eq_ignore_ascii_case("json") && !stem.is_empty())
}

/// Reads one role file's bytes into its content, or why it cannot be used.
///
/// The name falls back to `stem` and is trimmed; `tools` is materialised, so
/// the catalog and the runtime both see a concrete list.
pub(crate) fn role_from_bytes(bytes: &[u8], stem: &str) -> Result<AgentRoleFile, String> {
    if std::str::from_utf8(bytes).is_err() {
        return Err(crate::ui_text::pick("角色文件不是 UTF-8 文本", "The role file is not UTF-8 text").to_owned());
    }
    let value = crate::config_file::parse_json(bytes).map_err(|error| {
        crate::ui_text::ui_text!(
            "角色文件不是有效的 JSON：{error}",
            "The role file is not valid JSON: {error}"
        )
    })?;
    if !value.is_object() {
        return Err(crate::ui_text::pick(
            "角色文件的顶层必须是一个 JSON 对象",
            "The role file must hold one JSON object",
        )
        .to_owned());
    }
    let mut role = serde_json::from_value::<AgentRoleFile>(value).map_err(|error| {
        crate::ui_text::ui_text!(
            "角色文件的内容不符合格式：{error}",
            "The role file does not have the expected shape: {error}"
        )
    })?;
    if role.name.trim().is_empty() {
        role.name = stem.to_owned();
    }
    role.name = role.name.trim().to_owned();
    validate_role(&role)?;
    role.tools.get_or_insert_with(all_role_tool_names);
    Ok(role)
}

/// What makes a role usable. A file that fails is listed unavailable with the
/// reason; a save that would fail is refused.
pub(crate) fn validate_role(role: &AgentRoleFile) -> Result<(), String> {
    let name = role.name.as_str();
    if name.trim().is_empty() {
        return Err(crate::ui_text::pick("角色名不能为空", "A role name may not be empty").to_owned());
    }
    if name.chars().count() > crate::model::MAX_AGENT_TYPE_CHARS {
        let limit = crate::model::MAX_AGENT_TYPE_CHARS;
        return Err(crate::ui_text::ui_text!(
            "角色名不能超过 {limit} 个字符",
            "A role name may have at most {limit} characters"
        ));
    }
    if name.chars().any(char::is_control) {
        return Err(crate::ui_text::pick(
            "角色名不能包含控制字符",
            "A role name may not contain control characters",
        )
        .to_owned());
    }
    if role.description.contains('\0') {
        return Err(crate::ui_text::pick(
            "角色说明不能包含 NUL 字符",
            "A role description may not contain a NUL character",
        )
        .to_owned());
    }
    for (kind, ids) in [
        (crate::ui_text::pick("技能", "skill"), &role.skill_ids),
        (crate::ui_text::pick("MCP 服务器", "MCP server"), &role.mcp_ids),
        (crate::ui_text::pick("钩子", "hook"), &role.hook_ids),
    ] {
        let mut seen = HashSet::new();
        for id in ids {
            if id.trim().is_empty() {
                return Err(crate::ui_text::ui_text!(
                    "角色的{kind} ID 不能为空",
                    "The role's {kind} ids may not be empty"
                ));
            }
            if !seen.insert(id.as_str()) {
                return Err(crate::ui_text::ui_text!(
                    "角色重复选择了{kind}：{id}",
                    "The role selects {kind} {id} twice"
                ));
            }
        }
    }
    if let AgentModelSelection::Explicit {
        provider_id,
        model_id,
    } = &role.model_selection
    {
        if provider_id.is_empty()
            || provider_id.trim() != provider_id
            || model_id.is_empty()
            || model_id.trim() != model_id
        {
            return Err(crate::ui_text::pick(
                "角色的显式模型必须写明 providerId 和 modelId，且不能有首尾空白",
                "A role's explicit model needs a providerId and a modelId without surrounding whitespace",
            )
            .to_owned());
        }
    }
    validate_role_web_search(&role.web_search)
}

/// A role's web search meets the limits a conversation's does
/// (`storage::web_search_settings_problem`), in a role's words.
fn validate_role_web_search(settings: &ConversationWebSearchSettings) -> Result<(), String> {
    use crate::storage::WebSearchSettingsProblem as Problem;
    let Some(problem) = crate::storage::web_search_settings_problem(settings) else {
        return Ok(());
    };
    let list = |allow_list: bool| {
        if allow_list {
            crate::ui_text::pick("白名单", "allow list")
        } else {
            crate::ui_text::pick("黑名单", "block list")
        }
    };
    Err(match problem {
        Problem::SearchesPerCall => {
            let limit = crate::storage::MAX_WEB_SEARCHES_PER_CALL;
            crate::ui_text::ui_text!(
                "角色的单次联网搜索次数上限必须在 0–{limit} 之间（0 表示不设限）",
                "The role's web searches per call must be between 0 and {limit} (0 means no limit)"
            )
        }
        Problem::MaxResults => {
            let limit = crate::model::MAX_SEARCH_MAX_RESULTS;
            crate::ui_text::ui_text!(
                "角色的搜索结果数必须在 0–{limit} 之间（0 表示不设限）",
                "The role's number of search results must be between 0 and {limit} (0 means no limit)"
            )
        }
        Problem::CompressionCutoff => {
            let limit = crate::model::MAX_SEARCH_CUTOFF_LIMIT;
            crate::ui_text::ui_text!(
                "角色的搜索结果截断预算必须在 0–{limit} 之间（0 表示不压缩）",
                "The role's search result cutoff must be between 0 and {limit} (0 means no compression)"
            )
        }
        Problem::FetchCompressionCutoff => {
            let limit = crate::model::MAX_SEARCH_CUTOFF_LIMIT;
            crate::ui_text::ui_text!(
                "角色的抓取结果截断预算必须在 0–{limit} 之间（0 表示不压缩）",
                "The role's fetch result cutoff must be between 0 and {limit} (0 means no compression)"
            )
        }
        Problem::TooManyDomainRules { allow_list } => {
            let (list, limit) = (list(allow_list), crate::model::MAX_SEARCH_DOMAIN_RULES);
            crate::ui_text::ui_text!(
                "角色的域名{list}条目过多（最多 {limit} 条）",
                "The role's domain {list} has too many entries (at most {limit})"
            )
        }
        Problem::InvalidDomainRule { allow_list } => {
            let (list, limit) = (list(allow_list), crate::storage::MAX_SEARCH_DOMAIN_RULE_CHARS);
            crate::ui_text::ui_text!(
                "角色的域名{list}里有无效规则：每条最多 {limit} 个字符，且不能包含控制字符",
                "The role's domain {list} has an invalid rule: each may have at most {limit} characters and no control characters"
            )
        }
    })
}

fn too_large() -> String {
    let limit = ROLE_FILE_READ_LIMIT / 1024;
    crate::ui_text::ui_text!(
        "角色文件超过 {limit} KiB 上限",
        "The role file exceeds the {limit} KiB limit"
    )
}

/// One role file's row, from its text or the reason it could not be read.
pub(crate) fn descriptor_from_bytes(
    location: String,
    file_name: &str,
    source: ResourceSource,
    workspace_key: Option<&str>,
    bytes: Result<Vec<u8>, String>,
) -> AgentRoleDescriptor {
    let stem = file_stem(file_name);
    let id = crate::capabilities::stable_id(id_prefix(source), stem, &location);
    let parsed = bytes.and_then(|bytes| role_from_bytes(&bytes, stem));
    let descriptor = |name: String, description: String, available: bool| ResourceDescriptor {
        id: id.clone(),
        name,
        description,
        location: location.clone(),
        source,
        available,
        workspace_key: workspace_key.map(str::to_owned),
    };
    match parsed {
        Ok(role) => AgentRoleDescriptor {
            descriptor: descriptor(role.name.clone(), role.description.clone(), true),
            role: Some(role),
        },
        Err(reason) => AgentRoleDescriptor {
            descriptor: descriptor(stem.to_owned(), reason, false),
            role: None,
        },
    }
}

/// Every role file directly under `root` (an `agents/` directory) on this
/// computer. Only regular files count — a link would delegate what enters a
/// child to its target's owner — and a file that cannot be read is listed,
/// unavailable, with the reason.
pub(crate) fn discover_in_root(
    root: &Path,
    source: ResourceSource,
    workspace_key: Option<&str>,
) -> Vec<AgentRoleDescriptor> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut roles = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        let file_name = entry.file_name().to_string_lossy().into_owned();
        if !metadata.is_file() || !is_role_file_name(&file_name) {
            continue;
        }
        let bytes = if metadata.len() > ROLE_FILE_READ_LIMIT as u64 {
            Err(too_large())
        } else {
            crate::memory_archive_file::read_bounded_nofollow_labeled(
                &path,
                ROLE_FILE_READ_LIMIT,
                crate::ui_text::pick("角色", "role"),
            )
            .map_err(|error| {
                crate::ui_text::ui_text!(
                    "无法读取角色文件：{error}",
                    "Could not read the role file: {error}"
                )
            })
        };
        roles.push(descriptor_from_bytes(
            path.to_string_lossy().into_owned(),
            &file_name,
            source,
            workspace_key,
            bytes,
        ));
    }
    roles
}

/// A role file on another machine, from what its probe brought back. The id
/// is the caller's to make that machine's.
pub(crate) fn descriptor_from_remote(
    file: &crate::remote_capabilities::AgentFileEntry,
    source: ResourceSource,
    workspace_key: Option<&str>,
) -> Option<AgentRoleDescriptor> {
    if !is_role_file_name(&file.file_name) {
        return None;
    }
    Some(descriptor_from_bytes(
        file.path.clone(),
        &file.file_name,
        source,
        workspace_key,
        file.bytes.clone().ok_or_else(too_large),
    ))
}

/// The text a role file is written as.
pub(crate) fn role_file_text(role: &AgentRoleFile) -> Result<String, String> {
    serde_json::to_string_pretty(role)
        .map(|text| format!("{text}\n"))
        .map_err(|error| {
            crate::ui_text::ui_text!("无法写出角色文件：{error}", "Could not write the role file: {error}")
        })
}

/// The file name a new role named `name` is written as, before
/// [`unique_file_name`]: letters, digits, `-` and `_` kept, everything else a
/// `-`, runs of `-` collapsed, at most [`MAX_FILE_SLUG_CHARS`] characters and
/// [`MAX_FILE_SLUG_BYTES`] bytes, cut between characters.
pub(crate) fn file_slug(name: &str) -> String {
    let mut slug = String::new();
    for character in name.chars() {
        let kept = if character.is_alphanumeric() || character == '_' {
            character
        } else {
            '-'
        };
        if kept == '-' && slug.ends_with('-') {
            continue;
        }
        slug.push(kept);
    }
    let mut capped = String::new();
    for character in slug.trim_matches(['-', '.']).chars().take(MAX_FILE_SLUG_CHARS) {
        if capped.len() + character.len_utf8() > MAX_FILE_SLUG_BYTES {
            break;
        }
        capped.push(character);
    }
    let slug = capped.trim_end_matches(['-', '.']).to_owned();
    if slug.is_empty() {
        return "role".to_owned();
    }
    if is_windows_device_name(&slug) {
        return format!("{slug}-role");
    }
    slug
}

/// Whether Windows keeps `stem` for a device, whatever the extension: `CON`,
/// `PRN`, `AUX`, `NUL`, and `COM`/`LPT` followed by one digit or by one of
/// the superscripts `¹²³`, which it reads as digits too.
fn is_windows_device_name(stem: &str) -> bool {
    let lower = stem.to_lowercase();
    if matches!(lower.as_str(), "con" | "prn" | "aux" | "nul") {
        return true;
    }
    ["com", "lpt"].iter().any(|prefix| {
        let mut rest = lower.strip_prefix(prefix).unwrap_or("").chars();
        matches!(
            (rest.next(), rest.next()),
            (Some('0'..='9' | '¹' | '²' | '³'), None)
        )
    })
}

/// `<slug>.json`, or `<slug>-2.json`, `<slug>-3.json`… — the first whose name
/// `existing` does not already hold, compared without case, as the file
/// systems that ignore it would.
pub(crate) fn unique_file_name<'a>(name: &str, existing: impl IntoIterator<Item = &'a str>) -> String {
    let taken = existing
        .into_iter()
        .map(str::to_lowercase)
        .collect::<HashSet<_>>();
    let slug = file_slug(name);
    let mut candidate = format!("{slug}.json");
    let mut number = 2;
    while taken.contains(&candidate.to_lowercase()) {
        candidate = format!("{slug}-{number}.json");
        number += 1;
    }
    candidate
}

/// What a save wrote: the role's id — the one discovery lists the file under
/// — and the role as the registry keeps it.
#[derive(Debug)]
pub(crate) struct SavedRole {
    pub id: String,
    pub registered: RegisteredAgentRole,
}

/// A save worked out before the named-agent fence is taken
/// ([`save_role_fenced`]): the role checked, its text, and where it goes —
/// found by a fresh scan, which may wait on another machine, as may making a
/// remote level's `agents` directory and reading what it holds. Under the
/// fence [`commit_save`] only checks the target again, writes, and says what
/// it wrote.
pub(crate) struct PlannedSave {
    role: AgentRoleFile,
    text: String,
    level: crate::capabilities::ConfigLevel,
    target: PlannedTarget,
    /// Each level on this computer in scan order, with its source, its
    /// workspace key and the `agents` directory it lists: the first of them
    /// whose directory holds a new local file is the level discovery lists
    /// the file under, which is not the level it was saved to when a
    /// workspace is the home folder.
    local_roots: Vec<(ResourceSource, Option<String>, PathBuf)>,
}

enum PlannedTarget {
    /// The file a fresh scan listed under `id`, overwritten in place; it is
    /// listed at the level `owner` names (a workspace key, `None` for the
    /// global level).
    Existing {
        id: String,
        location: String,
        owner: Option<String>,
    },
    /// A new file in `directory`, on this computer.
    NewLocal { directory: PathBuf },
    /// A new file in `directory` on the level's machine, where the role files
    /// `taken` were when it was read.
    NewRemote { directory: String, taken: Vec<String> },
}

fn too_large_to_save() -> String {
    let limit = ROLE_FILE_READ_LIMIT / 1024;
    crate::ui_text::ui_text!(
        "角色文件会超过 {limit} KiB 上限，所以没有保存：请缩短说明或减少列表",
        "The role file would exceed the {limit} KiB limit, so it was not saved: shorten the description or the lists"
    )
}

fn no_free_file_name() -> String {
    crate::ui_text::pick(
        "找不到一个还没被占用的角色文件名，角色没有保存",
        "No free file name was found for the role, so it was not saved",
    )
    .to_owned()
}

/// Works out the save of `role` where `target` says, against `levels`.
///
/// An id names a file a fresh scan of the levels finds, never one the
/// renderer describes: the file is overwritten in place and keeps its name,
/// so renaming a role keeps its id. Without an id the role gets a new file in
/// `.mewrk/agents` of the level the workspace key names. A built-in role is
/// never written, and a role that would be unusable — invalid, or too large
/// to read back — is refused rather than written.
pub(crate) fn plan_save(
    levels: &[crate::capabilities::ConfigLevel],
    target: &SaveAgentRoleTarget,
    mut role: AgentRoleFile,
) -> Result<PlannedSave, String> {
    role.name = role.name.trim().to_owned();
    validate_role(&role)?;
    role.tools.get_or_insert_with(all_role_tool_names);
    let text = role_file_text(&role)?;
    if text.len() > ROLE_FILE_READ_LIMIT {
        return Err(too_large_to_save());
    }
    let (level, planned) = match target.id.as_deref() {
        Some(id) if is_builtin_id(id) => return Err(builtin_read_only()),
        Some(id) => {
            let scan = crate::capabilities::scan_agent_roles(levels);
            let row = scan
                .rows
                .iter()
                .find(|row| row.descriptor.id == id)
                .ok_or_else(|| {
                    crate::ui_text::ui_text!(
                        "角色 {id} 不存在：它的文件可能已被移动或删除",
                        "There is no role {id}: its file may have been moved or deleted"
                    )
                })?;
            let owner = row.descriptor.workspace_key.clone();
            let level = crate::capabilities::level_of(levels, owner.as_deref())?.clone();
            let planned = PlannedTarget::Existing {
                id: row.descriptor.id.clone(),
                location: row.descriptor.location.clone(),
                owner,
            };
            (level, planned)
        }
        None => {
            let level = crate::capabilities::level_of(levels, target.workspace_key.as_deref())?.clone();
            let planned = match &level.remote {
                Some(remote) => {
                    let directory = crate::remote_capabilities::ensure_directory(remote, ".mewrk/agents")?;
                    let taken = crate::remote_capabilities::read(remote, crate::remote_capabilities::RUN_TIMEOUT)?
                        .agents
                        .into_iter()
                        .map(|entry| entry.file_name)
                        .collect();
                    PlannedTarget::NewRemote { directory, taken }
                }
                None => PlannedTarget::NewLocal {
                    directory: level.base.join(".mewrk").join("agents"),
                },
            };
            (level, planned)
        }
    };
    let local_roots = levels
        .iter()
        .filter(|level| level.is_local())
        .map(|level| {
            (level.source, level.workspace_key(), level.base.join(".mewrk").join("agents"))
        })
        .collect();
    Ok(PlannedSave {
        role,
        text,
        level,
        target: planned,
        local_roots,
    })
}

/// Writes what [`plan_save`] worked out, under the named-agent fence: an
/// existing file only while it is still a plain file, a new one only under a
/// name nothing has taken — reserved by creating it, so it never replaces
/// anything, not even what appeared since the plan was made.
pub(crate) fn commit_save(plan: PlannedSave) -> Result<SavedRole, String> {
    let PlannedSave {
        role,
        text,
        level,
        target,
        local_roots,
    } = plan;
    let (id, owner) = match target {
        PlannedTarget::Existing {
            id,
            location,
            owner,
        } => {
            write_existing(&level, &location, &text)?;
            (id, owner)
        }
        PlannedTarget::NewLocal { directory } => {
            let path = create_local_file(&directory, &role.name, &text)?;
            let location = path.to_string_lossy().into_owned();
            let file_name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            // Discovery lists a location under the first level that reads
            // it: the global one, when this workspace is the home folder.
            let (source, owner) = local_roots
                .iter()
                .find(|(_, _, root)| root.join(&file_name).to_string_lossy() == location)
                .map(|(source, key, _)| (*source, key.clone()))
                .unwrap_or((level.source, level.workspace_key()));
            let id = crate::capabilities::stable_id(id_prefix(source), file_stem(&file_name), &location);
            (id, owner)
        }
        PlannedTarget::NewRemote { directory, taken } => {
            let Some(remote) = &level.remote else {
                return Err(no_free_file_name());
            };
            let location = create_remote_file(remote, &directory, &role.name, &text, taken)?;
            let file_name = location.rsplit(['/', '\\']).next().unwrap_or_default();
            let id = crate::capabilities::stable_id(id_prefix(level.source), file_stem(file_name), &location);
            (crate::capabilities::on_machine_id(&id, &remote.machine), level.workspace_key())
        }
    };
    let role_level = RoleLevel::of_workspace_key(owner.as_deref());
    Ok(SavedRole {
        registered: RegisteredAgentRole {
            definition: registered_definition(&role, &role_level, &id),
            id: id.clone(),
            level: role_level,
        },
        id,
    })
}

/// [`plan_save`] and [`commit_save`] in one go, with no fence: for tests.
#[cfg(test)]
pub(crate) fn save_role_in(
    levels: &[crate::capabilities::ConfigLevel],
    target: &SaveAgentRoleTarget,
    role: AgentRoleFile,
) -> Result<SavedRole, String> {
    commit_save(plan_save(levels, target, role)?)
}

fn broken_fence(saved: bool) -> String {
    if saved {
        crate::ui_text::pick(
            "命名 Agent 定义授权锁已损坏；角色尚未保存",
            "The named-agent definition lock is broken; the role was not saved",
        )
    } else {
        crate::ui_text::pick(
            "命名 Agent 定义授权锁已损坏；角色尚未删除",
            "The named-agent definition lock is broken; the role was not deleted",
        )
    }
    .to_owned()
}

/// Saves `role` (see [`plan_save`]) and returns its id. The scan and every
/// wait on another machine happen before the named-agent write fence
/// `authority` is taken; under it the target is checked again and written,
/// and `registry` takes the role at once, so no child validates against a
/// half-published role. The caller says whether it may log in to a machine.
pub(crate) fn save_role_fenced(
    levels: &[crate::capabilities::ConfigLevel],
    target: &SaveAgentRoleTarget,
    role: AgentRoleFile,
    authority: &RwLock<()>,
    registry: &RwLock<AgentRoleRegistry>,
) -> Result<String, String> {
    let plan = plan_save(levels, target, role)?;
    let _authority = authority.write().map_err(|_| broken_fence(true))?;
    let saved = commit_save(plan)?;
    registry
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(saved.registered);
    Ok(saved.id)
}

/// Overwrites the role file at `location`, which a fresh scan listed — on
/// this computer only while it is still a plain file there.
fn write_existing(
    level: &crate::capabilities::ConfigLevel,
    location: &str,
    text: &str,
) -> Result<(), String> {
    match &level.remote {
        Some(remote) => crate::remote_capabilities::replace_file(remote, location, text),
        None => {
            let path = Path::new(location);
            let metadata = fs::symlink_metadata(path).map_err(|error| {
                crate::ui_text::ui_text!("无法查看角色文件：{error}", "Could not inspect the role file: {error}")
            })?;
            if !metadata.is_file() {
                return Err(crate::ui_text::pick(
                    "角色文件不是普通文件",
                    "The role file is not a plain file",
                )
                .to_owned());
            }
            crate::capabilities::write_config_file(path, text)
        }
    }
}

/// Creates a new role file for a role named `name` in `directory` on this
/// computer, making the directory, and returns its path. The name is
/// reserved by creating the file — `create_new`, so an existing entry of any
/// kind is never opened, followed or replaced — and the next name is tried
/// when one turns out to be taken.
fn create_local_file(directory: &Path, name: &str, text: &str) -> Result<PathBuf, String> {
    fs::create_dir_all(directory).map_err(|error| {
        crate::ui_text::ui_text!(
            "无法创建 {}：{error}",
            "Could not create {}: {error}",
            directory.display()
        )
    })?;
    let mut tried = Vec::new();
    for _ in 0..MAX_NEW_FILE_ATTEMPTS {
        let path = new_file_in(directory, name, &tried)?;
        let mut file = match fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                tried.push(path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default());
                continue;
            }
            Err(error) => {
                return Err(crate::ui_text::ui_text!(
                    "无法创建 {}：{error}",
                    "Could not create {}: {error}",
                    path.display()
                ))
            }
        };
        if let Err(error) = file.write_all(text.as_bytes()).and_then(|()| file.sync_all()) {
            drop(file);
            // The file is this save's own, so a half-written one goes again.
            let _ = fs::remove_file(&path);
            return Err(crate::ui_text::ui_text!(
                "无法写入 {}：{error}",
                "Could not write {}: {error}",
                path.display()
            ));
        }
        return Ok(path);
    }
    Err(no_free_file_name())
}

/// [`create_local_file`] on another machine: `directory` is the `agents`
/// directory there, `taken` the role files it held when read. A name the
/// machine finds taken in any form — a file, a folder, a link — is passed
/// over for the next (`remote_capabilities::create_file`).
fn create_remote_file(
    remote: &crate::remote_capabilities::RemoteLevel,
    directory: &str,
    name: &str,
    text: &str,
    mut taken: Vec<String>,
) -> Result<String, String> {
    let separator = if directory.contains('\\') && !directory.contains('/') { '\\' } else { '/' };
    for _ in 0..MAX_NEW_FILE_ATTEMPTS {
        let file_name = unique_file_name(name, taken.iter().map(String::as_str));
        let path = format!("{}{separator}{file_name}", directory.trim_end_matches(['/', '\\']));
        if crate::remote_capabilities::create_file(remote, &path, text)? {
            return Ok(path);
        }
        taken.push(file_name);
    }
    Err(no_free_file_name())
}

/// A path in `directory` no entry has yet, nor any of `tried`, for a role
/// named `name`.
fn new_file_in(directory: &Path, name: &str, tried: &[String]) -> Result<PathBuf, String> {
    let existing = fs::read_dir(directory)
        .map_err(|error| {
            crate::ui_text::ui_text!(
                "无法读取 {}：{error}",
                "Could not read {}: {error}",
                directory.display()
            )
        })?
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    Ok(directory.join(unique_file_name(
        name,
        existing.iter().chain(tried).map(String::as_str),
    )))
}

/// A delete worked out before the named-agent fence is taken
/// ([`delete_role_fenced`]): the file a fresh scan lists under the id.
pub(crate) struct PlannedDelete {
    location: String,
    level: crate::capabilities::ConfigLevel,
}

/// Finds the role file `id` names in a fresh scan of `levels`. A built-in
/// role is never deleted.
pub(crate) fn plan_delete(
    levels: &[crate::capabilities::ConfigLevel],
    id: &str,
) -> Result<PlannedDelete, String> {
    if is_builtin_id(id) {
        return Err(builtin_read_only());
    }
    let scan = crate::capabilities::scan_agent_roles(levels);
    let row = scan
        .rows
        .iter()
        .find(|row| row.descriptor.id == id)
        .ok_or_else(|| crate::ui_text::ui_text!("角色 {id} 不存在", "There is no role {id}"))?;
    let level = crate::capabilities::level_of(levels, row.descriptor.workspace_key.as_deref())?.clone();
    Ok(PlannedDelete {
        location: row.descriptor.location.clone(),
        level,
    })
}

/// Deletes what [`plan_delete`] found, under the named-agent fence. On this
/// computer only a regular, unlinked file directly inside the level's
/// `agents/` directory is removed, as it is when the fence is taken; on
/// another machine the file goes the way a skill folder does.
pub(crate) fn commit_delete(plan: &PlannedDelete) -> Result<(), String> {
    let PlannedDelete { location, level } = plan;
    if let Some(remote) = &level.remote {
        return crate::remote_capabilities::remove_file(remote, location);
    }
    let path = Path::new(location);
    let root = level.path_for(crate::capabilities::CapabilityKind::Agents);
    let inside_root = path.parent().is_some_and(|parent| {
        crate::capabilities::normalized_location_for_id(&parent.to_string_lossy())
            == crate::capabilities::normalized_location_for_id(&root.to_string_lossy())
    });
    if !inside_root {
        return Err(crate::ui_text::pick(
            "这个角色不在 Mewrk 读取的 agents 文件夹里",
            "The role is not inside an agents directory Mewrk reads",
        )
        .to_owned());
    }
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        crate::ui_text::ui_text!("无法查看角色文件：{error}", "Could not inspect the role file: {error}")
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(crate::ui_text::pick(
            "角色文件不是普通文件",
            "The role file is not a plain file",
        )
        .to_owned());
    }
    fs::remove_file(path).map_err(|error| {
        crate::ui_text::ui_text!("无法删除角色文件：{error}", "Could not delete the role file: {error}")
    })
}

/// [`plan_delete`] and [`commit_delete`] in one go, with no fence: for tests.
#[cfg(test)]
pub(crate) fn delete_role_in(
    levels: &[crate::capabilities::ConfigLevel],
    id: &str,
) -> Result<(), String> {
    commit_delete(&plan_delete(levels, id)?)
}

/// Deletes the role file `id` names (see [`plan_delete`]): the scan before
/// the named-agent write fence `authority` is taken, the deletion and the
/// registry's update under it, so a child bound to the role is refused at its
/// next fence.
pub(crate) fn delete_role_fenced(
    levels: &[crate::capabilities::ConfigLevel],
    id: &str,
    authority: &RwLock<()>,
    registry: &RwLock<AgentRoleRegistry>,
) -> Result<(), String> {
    let plan = plan_delete(levels, id)?;
    let _authority = authority.write().map_err(|_| broken_fence(false))?;
    commit_delete(&plan)?;
    registry
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(id);
    Ok(())
}

/// Where the global level's role files are: `~/.mewrk/agents`. A test build
/// keeps them under its application data directory, so no test writes into
/// the home directory of whoever runs it.
pub(crate) fn user_agents_dir(app_data: &Path) -> Option<PathBuf> {
    #[cfg(test)]
    {
        Some(app_data.join("home").join(".mewrk").join("agents"))
    }
    #[cfg(not(test))]
    {
        let _ = app_data;
        crate::capabilities::ConfigLevel::user().map(|level| level.base.join(".mewrk").join("agents"))
    }
}

/// The content of a legacy role that decides whether two are the same role.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LegacyRoleContent<'a> {
    name: &'a str,
    description: &'a str,
    model_selection: &'a AgentModelSelection,
    effort: &'a Option<ReasoningEffort>,
    tools: &'a Option<Vec<String>>,
    disallowed_tools: &'a [String],
    skill_ids: &'a Option<Vec<String>>,
    mcp_ids: &'a Option<Vec<String>>,
    hook_ids: &'a Option<Vec<String>>,
    search_provider: &'a Option<crate::model::SearchProviderSelection>,
    fetch_provider: &'a Option<crate::model::FetchProviderSelection>,
    max_results: u32,
    compression_cutoff: u32,
    fetch_compression_cutoff: u32,
    domain_filter: &'a Option<crate::model::SearchDomainFilterMode>,
    include_domains: &'a [String],
    exclude_domains: &'a [String],
    template_id: &'a Option<String>,
}

/// The key a legacy role is recorded under: a digest of its content, so the
/// same role kept by several conversations is exported once.
fn legacy_role_key(definition: &AgentDefinition) -> String {
    let content = LegacyRoleContent {
        name: &definition.name,
        description: &definition.description,
        model_selection: &definition.model_selection,
        effort: &definition.effort,
        tools: &definition.tools,
        disallowed_tools: &definition.disallowed_tools,
        skill_ids: &definition.skill_ids,
        mcp_ids: &definition.mcp_ids,
        hook_ids: &definition.hook_ids,
        search_provider: &definition.search_provider,
        fetch_provider: &definition.fetch_provider,
        max_results: definition.max_results,
        compression_cutoff: definition.compression_cutoff,
        fetch_compression_cutoff: definition.fetch_compression_cutoff,
        domain_filter: &definition.domain_filter,
        include_domains: &definition.include_domains,
        exclude_domains: &definition.exclude_domains,
        template_id: &definition.template_id,
    };
    let canonical = serde_json::to_vec(&content).unwrap_or_default();
    crate::api::sha256_hex(&canonical)
}

/// The built-in role a legacy role is an untouched copy of, if it is one:
/// the preset this build replaces seeded these into every conversation, with
/// the model and description of the build that seeded it — this build's
/// ([`BUILTIN_ROLES`]) or an earlier one's ([`EARLIER_BUILTIN_ROLES`]).
fn builtin_copy_of(
    definition: &AgentDefinition,
    providers: &[crate::model::ApiProvider],
) -> Option<&'static str> {
    let AgentModelSelection::Explicit {
        provider_id,
        model_id,
    } = &definition.model_selection
    else {
        return None;
    };
    let family = providers
        .iter()
        .find(|provider| &provider.id == provider_id)?
        .family;
    let seeded_as = |id: &str, model: &str, description: &str| {
        BUILTIN_ROLES
            .iter()
            .map(|(builtin, _, _, model, description)| (*builtin, *model, *description))
            .chain(EARLIER_BUILTIN_ROLES.iter().copied())
            .any(|seeded| seeded == (id, model, description))
    };
    BUILTIN_ROLES
        .iter()
        .find(|(id, name, builtin_family, _, _)| {
            definition.name == *name
                && family == *builtin_family
                && seeded_as(id, model_id, &definition.description)
                && definition.effort == Some(ReasoningEffort::Medium)
                && definition.tools.is_none()
                && definition.disallowed_tools.is_empty()
                && definition.skill_ids.is_none()
                && definition.mcp_ids.is_none()
                && definition.hook_ids.is_none()
                && definition.search_provider.is_none()
                && definition.fetch_provider.is_none()
                && definition.domain_filter.is_none()
                && definition.include_domains.is_empty()
                && definition.exclude_domains.is_empty()
                && definition.template_id.is_none()
        })
        .map(|(id, ..)| *id)
}

/// A legacy role as a role file: what it named, and the defaults for what it
/// left to its caller — every tool for `tools: None`, no skills, servers or
/// hooks for an absent list, the default web search for an absent backend.
fn legacy_role_file(definition: &AgentDefinition) -> AgentRoleFile {
    let mut web_search = ConversationWebSearchSettings::default();
    if let Some(provider) = &definition.search_provider {
        web_search.provider = provider.clone();
    }
    if let Some(provider) = &definition.fetch_provider {
        web_search.fetch_provider = provider.clone();
    }
    if let Some(mode) = definition.domain_filter {
        web_search.domain_filter = mode;
        web_search.include_domains = definition.include_domains.clone();
        web_search.exclude_domains = definition.exclude_domains.clone();
    }
    // Result shaping past a ceiling is pulled back onto it, as it always was
    // for a role, rather than exported as a file discovery would refuse.
    web_search.max_results = definition.max_results.min(crate::model::MAX_SEARCH_MAX_RESULTS);
    web_search.compression_cutoff = definition.compression_cutoff.min(crate::model::MAX_SEARCH_CUTOFF_LIMIT);
    web_search.fetch_compression_cutoff = definition
        .fetch_compression_cutoff
        .min(crate::model::MAX_SEARCH_CUTOFF_LIMIT);
    AgentRoleFile {
        name: definition.name.clone(),
        description: definition.description.clone(),
        model_selection: definition.model_selection.clone(),
        effort: definition.effort,
        tools: definition.tools.clone(),
        disallowed_tools: definition.disallowed_tools.clone(),
        skill_ids: definition.skill_ids.clone().unwrap_or_default(),
        mcp_ids: definition.mcp_ids.clone().unwrap_or_default(),
        hook_ids: definition.hook_ids.clone().unwrap_or_default(),
        web_search,
        template_id: definition.template_id.clone(),
    }
}

/// Every place the document keeps a role selection: each preset, each
/// workspace's remembered last settings and new-task draft, and every
/// conversation — the legacy list and the ids, side by side.
fn for_each_role_carrier(
    document: &mut AppDocument,
    visit: &mut dyn FnMut(&mut Vec<AgentDefinition>, &mut Vec<String>),
) {
    for preset in &mut document.presets.conversation_presets {
        visit(&mut preset.settings.agent_definitions, &mut preset.settings.agent_ids);
    }
    for workspace in &mut document.workspaces {
        if let Some(settings) = workspace.last_conversation_settings.as_mut() {
            visit(&mut settings.agent_definitions, &mut settings.agent_ids);
        }
        if let Some(draft) = workspace.draft_conversation.as_mut() {
            visit(&mut draft.settings.agent_definitions, &mut draft.settings.agent_ids);
        }
    }
    for workspace in &mut document.workspaces {
        for conversation in &mut workspace.conversations {
            visit(
                &mut conversation.settings.agent_definitions,
                &mut conversation.settings.agent_ids,
            );
        }
    }
}

/// Moves the roles presets and conversations kept before roles were files
/// into role files, and selects them by id where they were switched on.
///
/// Each distinct user role becomes one file in `user_agents_dir`
/// (`~/.mewrk/agents`), named after it and never over another file — or
/// reuses the file there that already holds exactly that role, which another
/// data folder sharing the directory exported; an untouched copy of a
/// built-in role, as this build or an earlier one seeded it, becomes that
/// built-in's id instead. Which
/// file each role went to is recorded in [`EXPORT_RECORD_FILE`] under a
/// digest of its content, so this is done once: every later load — which
/// still finds the legacy list in a conversation not written since — reuses
/// the record, and a role whose file the user has since deleted is not
/// written again (its id simply dangles).
///
/// A role whose file could not be written stays in its list and is tried
/// again on the next load. Deletion tombstones carry no role and are dropped.
pub(crate) fn migrate_legacy_agent_definitions(
    document: &mut AppDocument,
    app_data: &Path,
    user_agents_dir: Option<&Path>,
) {
    let mut pending = Vec::new();
    for_each_role_carrier(document, &mut |definitions, _| {
        pending.extend(definitions.iter().cloned());
    });
    if pending.is_empty() {
        return;
    }
    let providers = document.assets.api_providers.clone();
    let record_path = app_data.join(EXPORT_RECORD_FILE);
    let mut record: BTreeMap<String, String> = fs::read(&record_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let mut recorded = false;
    // Each distinct role this load meets, by key: its id, or `None` when its
    // file could not be written.
    let mut mapped: HashMap<String, Option<String>> = HashMap::new();
    for definition in &pending {
        if definition.deleted || definition.source != AgentDefinitionSource::User {
            continue;
        }
        if builtin_copy_of(definition, &providers).is_some() {
            continue;
        }
        let key = legacy_role_key(definition);
        if mapped.contains_key(&key) {
            continue;
        }
        if let Some(id) = record.get(&key) {
            mapped.insert(key, Some(id.clone()));
            continue;
        }
        let exported = user_agents_dir
            .ok_or_else(|| "no home directory to write ~/.mewrk/agents in".to_owned())
            .and_then(|directory| export_legacy_role(directory, definition));
        match exported {
            Ok(id) => {
                record.insert(key.clone(), id.clone());
                recorded = true;
                mapped.insert(key, Some(id));
            }
            Err(error) => {
                eprintln!(
                    "Could not export subagent role {} to a role file; it is kept and tried again next time: {error}",
                    definition.name
                );
                mapped.insert(key, None);
            }
        }
    }
    if recorded {
        match serde_json::to_string_pretty(&record) {
            Ok(text) => {
                if let Err(error) = crate::capabilities::write_config_file(&record_path, &text) {
                    eprintln!("Could not record the exported subagent roles: {error}");
                }
            }
            Err(error) => eprintln!("Could not record the exported subagent roles: {error}"),
        }
    }
    for_each_role_carrier(document, &mut |definitions, ids| {
        let mut kept = Vec::new();
        for definition in std::mem::take(definitions) {
            if definition.deleted {
                continue;
            }
            if definition.source != AgentDefinitionSource::User {
                kept.push(definition);
                continue;
            }
            let id = match builtin_copy_of(&definition, &providers) {
                Some(id) => Some(id.to_owned()),
                None => mapped.get(&legacy_role_key(&definition)).cloned().flatten(),
            };
            match id {
                Some(id) => {
                    if definition.enabled && !ids.contains(&id) {
                        ids.push(id);
                    }
                }
                None => kept.push(definition),
            }
        }
        *definitions = kept;
    });
}

/// Writes one legacy role as a new file in `directory` and returns the id
/// discovery will list it under — or, when a file there already holds
/// exactly this role, that file's id and nothing is written.
///
/// `~/.mewrk/agents` is shared by every data folder on the computer (the
/// app's and a development build's alike) while each keeps its own export
/// record, so the second folder to migrate the same role finds the first's
/// file here rather than in its record.
fn export_legacy_role(directory: &Path, definition: &AgentDefinition) -> Result<String, String> {
    let role = legacy_role_file(definition);
    let id_of = |path: &Path| {
        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        crate::capabilities::stable_id(
            id_prefix(ResourceSource::User),
            file_stem(&file_name),
            &path.to_string_lossy(),
        )
    };
    if let Some(path) = file_holding(directory, &role) {
        return Ok(id_of(&path));
    }
    let text = role_file_text(&role)?;
    Ok(id_of(&create_local_file(directory, &definition.name, &text)?))
}

/// The role file directly in `directory` whose content is `role`, compared as
/// written — a file that omits `tools` is not one that lists every tool — with
/// the name read the way discovery reads it. The first by file name wins.
fn file_holding(directory: &Path, role: &AgentRoleFile) -> Option<PathBuf> {
    let mut paths = fs::read_dir(directory)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| is_role_file_name(&name.to_string_lossy()))
                && fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
        })
        .collect::<Vec<_>>();
    paths.sort();
    paths.into_iter().find(|path| {
        let stem = path
            .file_name()
            .map(|name| file_stem(&name.to_string_lossy()).to_owned())
            .unwrap_or_default();
        crate::memory_archive_file::read_bounded_nofollow_labeled(
            path,
            ROLE_FILE_READ_LIMIT,
            crate::ui_text::pick("角色", "role"),
        )
        .ok()
        .and_then(|bytes| crate::config_file::parse_json(&bytes).ok())
        .and_then(|value| serde_json::from_value::<AgentRoleFile>(value).ok())
        .is_some_and(|mut found| {
            if found.name.trim().is_empty() {
                found.name = stem;
            }
            found.name = found.name.trim().to_owned();
            let mut expected = role.clone();
            expected.name = expected.name.trim().to_owned();
            found == expected
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role(name: &str) -> AgentRoleFile {
        AgentRoleFile {
            name: name.into(),
            description: String::new(),
            model_selection: AgentModelSelection::Inherit,
            effort: None,
            tools: Some(vec!["read".into()]),
            disallowed_tools: Vec::new(),
            skill_ids: Vec::new(),
            mcp_ids: Vec::new(),
            hook_ids: Vec::new(),
            web_search: ConversationWebSearchSettings::default(),
            template_id: None,
        }
    }

    /// A level whose base is a temporary directory, standing in for `~` or a
    /// workspace so nothing is written outside it.
    fn level(base: &Path, workspace: Option<&str>) -> crate::capabilities::ConfigLevel {
        crate::capabilities::ConfigLevel {
            source: if workspace.is_some() { ResourceSource::Workspace } else { ResourceSource::User },
            base: base.to_path_buf(),
            workspace: workspace.map(|path| crate::model::AttachedWorkspace {
                machine: None,
                path: path.to_owned(),
            }),
            remote: None,
        }
    }

    #[test]
    fn a_file_parses_with_defaults_and_materialises_every_tool() {
        let role = role_from_bytes(b"\xEF\xBB\xBF{\"description\":\"Reviews\"}", "reviewer").unwrap();
        assert_eq!(role.name, "reviewer", "a missing name reads as the stem");
        assert_eq!(role.description, "Reviews");
        assert_eq!(role.model_selection, AgentModelSelection::Inherit);
        assert_eq!(role.effort, None);
        assert_eq!(role.tools, Some(all_role_tool_names()));
        assert_eq!(role.web_search, ConversationWebSearchSettings::default());

        let named = role_from_bytes(br#"{"name":"  Code Reviewer  ","tools":[]}"#, "x").unwrap();
        assert_eq!(named.name, "Code Reviewer");
        assert_eq!(named.tools, Some(Vec::new()), "an empty list is a real none");
        let blank = role_from_bytes(br#"{"name":"   "}"#, "fallback").unwrap();
        assert_eq!(blank.name, "fallback");
    }

    #[test]
    fn every_tool_a_role_can_hold_is_what_the_picker_shows() {
        let names = all_role_tool_names();
        assert!(names.iter().any(|name| name == "read"));
        for excluded in ["agent_spawn", "workflow", "skill", "web_search", "web_fetch"] {
            assert!(!names.iter().any(|name| name == excluded), "{excluded}");
        }
        assert!(!names.iter().any(|name| crate::mewrk_memory::is_memory_tool(name)));
    }

    #[test]
    fn an_invalid_file_is_unavailable_with_the_reason() {
        for (bytes, needle) in [
            (&b"not json"[..], "JSON"),
            (&b"[1,2]"[..], "JSON"),
            (&br#"{"name":5}"#[..], ""),
            (&br#"{"description":"a\u0000b"}"#[..], "NUL"),
            (&br#"{"skillIds":["a","a"]}"#[..], "a"),
            (&br#"{"hookIds":[" "]}"#[..], ""),
            (&br#"{"modelSelection":{"kind":"explicit","providerId":" p","modelId":"m"}}"#[..], "providerId"),
            (&b"\xff\xfe"[..], "UTF-8"),
        ] {
            let error = role_from_bytes(bytes, "x").unwrap_err();
            assert!(error.contains(needle), "{error}");
        }
        let long = format!(r#"{{"name":"{}"}}"#, "x".repeat(65));
        assert!(role_from_bytes(long.as_bytes(), "x").is_err());
        assert!(role_from_bytes(br#"{"name":"a\nb"}"#, "x").is_err());
        // The unavailable model a broken binding reads as is accepted, and a
        // description has no length limit of its own.
        assert!(role_from_bytes(br#"{"modelSelection":{"kind":"unavailable"}}"#, "x").is_ok());
        let long = format!(r#"{{"description":"{}"}}"#, r"很长的一段说明。\n".repeat(5_000));
        assert!(role_from_bytes(long.as_bytes(), "x").is_ok());

        let row = descriptor_from_bytes("/x/broken.json".into(), "broken.json", ResourceSource::User, None, Ok(b"{".to_vec()));
        assert!(!row.descriptor.available);
        assert_eq!(row.descriptor.name, "broken");
        assert!(row.role.is_none());
        assert!(row.descriptor.description.contains("JSON"), "{}", row.descriptor.description);
    }

    #[test]
    fn discovery_reads_direct_regular_json_files_and_ids_survive_a_rename() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("agents");
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::write(root.join("reviewer.json"), r#"{"name":"Reviewer"}"#).unwrap();
        fs::write(root.join("notes.txt"), "not a role").unwrap();
        fs::write(root.join("nested").join("inner.json"), "{}").unwrap();
        fs::write(root.join("huge.json"), vec![b' '; ROLE_FILE_READ_LIMIT + 1]).unwrap();

        let mut rows = discover_in_root(&root, ResourceSource::Workspace, Some("ws"));
        rows.sort_by(|left, right| left.descriptor.name.cmp(&right.descriptor.name));
        assert_eq!(rows.len(), 2, "{rows:?}");
        let reviewer = rows.iter().find(|row| row.descriptor.name == "Reviewer").unwrap();
        assert!(reviewer.descriptor.available);
        assert!(reviewer.descriptor.id.starts_with("agent_workspace_reviewer_"));
        assert_eq!(reviewer.descriptor.workspace_key.as_deref(), Some("ws"));
        let huge = rows.iter().find(|row| row.descriptor.name == "huge").unwrap();
        assert!(!huge.descriptor.available);
        assert!(huge.descriptor.description.contains("256"), "{}", huge.descriptor.description);

        // Renaming the role inside its file keeps its id.
        fs::write(root.join("reviewer.json"), r#"{"name":"Critic"}"#).unwrap();
        let renamed = discover_in_root(&root, ResourceSource::Workspace, Some("ws"));
        let critic = renamed.iter().find(|row| row.descriptor.name == "Critic").unwrap();
        assert_eq!(critic.descriptor.id, reviewer.descriptor.id);
    }

    #[test]
    fn built_in_roles_run_on_the_first_provider_of_their_family() {
        let mut document = crate::catalog::product_default_document();
        let rows = builtin_descriptors(&document);
        assert_eq!(
            rows.iter().map(|row| row.descriptor.id.as_str()).collect::<Vec<_>>(),
            BUILTIN_ROLE_IDS
        );
        assert!(rows.iter().all(|row| row.descriptor.source == ResourceSource::Builtin));
        assert_eq!(rows[0].descriptor.location, "builtin:agents/opus.json");

        document.assets.api_providers.retain(|provider| provider.family != ProviderFamily::OpenaiCodex);
        let opus = builtin_definition(&document, BUILTIN_OPUS_ID).unwrap();
        let claude = document
            .assets
            .api_providers
            .iter()
            .find(|provider| provider.family == ProviderFamily::ClaudeAgent)
            .map(|provider| provider.id.clone());
        match (&opus.model_selection, claude) {
            (AgentModelSelection::Explicit { provider_id, model_id }, Some(claude)) => {
                assert_eq!(provider_id, &claude);
                assert_eq!(model_id, "claude-opus-5-5");
            }
            (selection, claude) => panic!("{selection:?} for {claude:?}"),
        }
        assert_eq!(opus.source, AgentDefinitionSource::Plugin);
        assert_eq!(opus.source_key, "builtin");
        assert_eq!(opus.effort, Some(ReasoningEffort::Medium));
        assert_eq!(opus.tools, Some(all_role_tool_names()));
        assert_eq!(opus.skill_ids, Some(Vec::new()));
        assert_eq!(opus.search_provider, Some(crate::model::SearchProviderSelection::Native));
        // No provider of its family: still listed, with its model unavailable.
        let sol = builtin_definition(&document, BUILTIN_SOL_ID).unwrap();
        assert_eq!(sol.model_selection, AgentModelSelection::Unavailable);
        assert!(builtin_definition(&document, "agent_user_x_00000000").is_none());
    }

    #[test]
    fn a_workspace_role_projects_to_a_project_source_its_caller_cannot_widen() {
        let mut file = role("reviewer");
        file.web_search.max_searches_per_call = 3;
        file.web_search.native_search_tool = crate::model::NativeSearchTool::WebSearch20260209;
        file.web_search.fetch_compression_cutoff = 750;
        let global = registered_definition(&file, &RoleLevel::Global, "agent_user_reviewer_00000001");
        assert_eq!(global.source, AgentDefinitionSource::User);
        assert!(global.source_key.is_empty());
        let workspace = registered_definition(
            &file,
            &RoleLevel::Workspace("local:/repo".into()),
            "agent_workspace_reviewer_00000002",
        );
        assert_eq!(workspace.source, AgentDefinitionSource::Project);
        assert_eq!(workspace.source_key.len(), "workspace:".len() + 16);
        assert_ne!(
            workspace.source_key,
            registered_definition(&file, &RoleLevel::Workspace("local:/other".into()), "x").source_key
        );
        assert_eq!(workspace.max_searches_per_call, Some(3));
        assert_eq!(workspace.native_search_tool, Some(crate::model::NativeSearchTool::WebSearch20260209));
        assert_eq!(workspace.native_fetch_tool, Some(crate::model::NativeFetchTool::default()));
        assert_eq!(workspace.domain_filter, Some(crate::model::SearchDomainFilterMode::Off));
        // Each leg's token cap is the role's own, and they stay apart.
        assert_eq!(workspace.fetch_compression_cutoff, 750);
        assert_eq!(workspace.compression_cutoff, crate::model::DEFAULT_SEARCH_CUTOFF_LIMIT);
        assert_eq!(workspace.memory_epoch, 1);
        assert_eq!(workspace.revision, role_revision("agent_workspace_reviewer_00000002"));
    }

    /// A role's revision is its id's: the same for every edit of the file, a
    /// rename of the role inside it included, and different for every other
    /// file — a built-in's constant id among them — always a positive integer
    /// JavaScript holds exactly.
    #[test]
    fn a_roles_revision_follows_its_file_not_its_content() {
        let id = "agent_user_reviewer_1a2b3c4d";
        let mut edited = role("reviewer");
        let first = registered_definition(&edited, &RoleLevel::Global, id);
        edited.name = "critic".into();
        edited.description = "Reviews harder".into();
        edited.tools = Some(vec!["grep".into()]);
        assert_eq!(registered_definition(&edited, &RoleLevel::Global, id).revision, first.revision);
        assert_ne!(
            registered_definition(&role("reviewer"), &RoleLevel::Global, "agent_user_reviewer_5e6f7a8b").revision,
            first.revision
        );
        let opus = builtin_definition(&crate::catalog::product_default_document(), BUILTIN_OPUS_ID).unwrap();
        assert_eq!(opus.revision, role_revision(BUILTIN_OPUS_ID));
        for id in ["", id, BUILTIN_OPUS_ID, BUILTIN_SONNET_ID, BUILTIN_SOL_ID, BUILTIN_LUNA_ID] {
            let revision = role_revision(id);
            assert!(revision >= 1 && revision <= (1 << 53) - 1, "{id}: {revision}");
            assert_eq!(revision, role_revision(id), "stable");
        }
    }

    #[test]
    fn the_registry_keeps_the_roles_of_a_level_it_could_not_read() {
        let entry = |id: &str, level: RoleLevel| RegisteredAgentRole {
            id: id.into(),
            definition: registered_definition(&role(id), &level, id),
            level,
        };
        let mut registry = AgentRoleRegistry::default();
        registry.replace_levels(
            &[RoleLevel::Global, RoleLevel::Workspace("remote".into())],
            vec![entry("g", RoleLevel::Global), entry("r", RoleLevel::Workspace("remote".into()))],
        );
        // The remote level did not answer: only the global one was read.
        registry.replace_levels(&[RoleLevel::Global], vec![entry("g2", RoleLevel::Global)]);
        assert!(registry.get("g").is_none(), "a level that was read is replaced");
        assert!(registry.get("g2").is_some());
        assert!(registry.get("r").is_some(), "a level that was not read keeps its roles");
    }

    #[test]
    fn a_conversation_sees_global_roles_and_those_of_its_own_workspaces_only() {
        let mut document = crate::catalog::default_document();
        let conversation = document.workspaces[0].conversations[0].clone();
        let own = crate::capabilities::conversation_locations(&document, &conversation)
            .into_iter()
            .flatten()
            .map(|location| crate::capabilities::workspace_key(&location))
            .next()
            .expect("the test conversation has a workspace");
        let mut registry = AgentRoleRegistry::default();
        for (id, level) in [
            ("global", RoleLevel::Global),
            ("mine", RoleLevel::Workspace(own.clone())),
            ("elsewhere", RoleLevel::Workspace("local:/another/project".into())),
        ] {
            registry.insert(RegisteredAgentRole {
                id: id.into(),
                definition: registered_definition(&role(id), &level, id),
                level,
            });
        }
        let settings = &mut document.workspaces[0].conversations[0].settings;
        settings.agent_ids = ["elsewhere", "mine", "dangling", BUILTIN_LUNA_ID, "global", "mine"]
            .map(str::to_owned)
            .to_vec();
        let conversation = document.workspaces[0].conversations[0].clone();
        let names = selected_definitions(&document, &conversation, &registry)
            .into_iter()
            .map(|definition| definition.name)
            .collect::<Vec<_>>();
        assert_eq!(names, ["mine", "Luna", "global"]);
    }

    #[test]
    fn slugs_keep_letters_and_digits_and_never_collide() {
        assert_eq!(file_slug("Code Reviewer"), "Code-Reviewer");
        assert_eq!(file_slug("审查/修复: v2?"), "审查-修复-v2");
        assert_eq!(file_slug("..--"), "role");
        assert_eq!(file_slug("con"), "con-role");
        assert_eq!(file_slug(&"x".repeat(100)).chars().count(), 64);
        assert_eq!(unique_file_name("Reviewer", ["reviewer.json", "REVIEWER-2.json"]), "Reviewer-3.json");
        assert_eq!(unique_file_name("a", std::iter::empty()), "a.json");
    }

    /// Every name Windows keeps for a device is kept from a file: the
    /// numbered ones from 0 and with a superscript digit too.
    #[test]
    fn slugs_never_name_a_windows_device() {
        for device in ["CON", "prn", "Aux", "nul", "COM0", "com9", "LPT0", "lpt5", "COM¹", "com²", "LPT³"] {
            assert_eq!(file_slug(device), format!("{device}-role"), "{device}");
        }
        for ordinary in ["com", "lpt", "com10", "lpt⁴", "console", "comx", "nul1"] {
            assert_eq!(file_slug(ordinary), ordinary, "{ordinary}");
        }
    }

    /// A slug is capped by its UTF-8 bytes as well as its characters, and is
    /// cut between characters.
    #[test]
    fn slugs_are_capped_in_bytes_on_a_character_boundary() {
        let wide = file_slug(&"审".repeat(64));
        assert!(wide.len() <= MAX_FILE_SLUG_BYTES, "{}", wide.len());
        assert_eq!(wide, "审".repeat(MAX_FILE_SLUG_BYTES / 3));
        // Four bytes each, so the cut falls inside one unless it is made
        // between characters.
        let mixed = file_slug(&format!("a{}", "𠀀".repeat(40)));
        assert!(mixed.len() <= MAX_FILE_SLUG_BYTES);
        assert_eq!(mixed, format!("a{}", "𠀀".repeat((MAX_FILE_SLUG_BYTES - 1) / 4)));
        // Characters still cap a narrow name first.
        assert_eq!(file_slug(&"x".repeat(100)).len(), MAX_FILE_SLUG_CHARS);
    }

    #[test]
    fn saving_creates_unique_files_overwrites_in_place_and_refuses_built_ins() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let workspace_path = workspace.path().to_string_lossy().into_owned();
        let levels = [level(home.path(), None), level(workspace.path(), Some(&workspace_path))];
        let workspace_key = levels[1].workspace_key();

        let mut reviewer = role("  reviewer ");
        reviewer.tools = None;
        let first = save_role_in(&levels, &SaveAgentRoleTarget::default(), reviewer.clone()).unwrap();
        let second = save_role_in(&levels, &SaveAgentRoleTarget::default(), reviewer.clone()).unwrap();
        assert_ne!(first.id, second.id);
        let agents = home.path().join(".mewrk").join("agents");
        assert!(agents.join("reviewer.json").is_file());
        assert!(agents.join("reviewer-2.json").is_file());
        let written: AgentRoleFile =
            serde_json::from_slice(&fs::read(agents.join("reviewer.json")).unwrap()).unwrap();
        assert_eq!(written.name, "reviewer", "the name is trimmed");
        assert_eq!(written.tools, Some(all_role_tool_names()), "tools are written as a list");
        let registered = first.registered;
        assert_eq!(registered.level, RoleLevel::Global);
        // The id a save returns, and the role it registers, are what a scan
        // lists the file as.
        let scanned = crate::capabilities::scan_agent_roles(&levels);
        let listed = scanned.roles.iter().find(|role| role.id == first.id).expect("listed");
        assert_eq!(listed, &registered);

        // In a workspace.
        let in_workspace = save_role_in(
            &levels,
            &SaveAgentRoleTarget { id: None, workspace_key: workspace_key.clone() },
            role("reviewer"),
        )
        .unwrap();
        assert!(in_workspace.id.starts_with("agent_workspace_reviewer_"));
        assert_eq!(
            in_workspace.registered.level,
            RoleLevel::Workspace(workspace_key.unwrap())
        );

        // In place: a rename keeps the file and the id.
        let renamed = save_role_in(
            &levels,
            &SaveAgentRoleTarget { id: Some(first.id.clone()), workspace_key: None },
            role("critic"),
        )
        .unwrap();
        assert_eq!(renamed.id, first.id);
        let written: AgentRoleFile =
            serde_json::from_slice(&fs::read(agents.join("reviewer.json")).unwrap()).unwrap();
        assert_eq!(written.name, "critic");

        for refused in [
            SaveAgentRoleTarget { id: Some(BUILTIN_OPUS_ID.into()), workspace_key: None },
            SaveAgentRoleTarget { id: Some("agent_user_ghost_00000000".into()), workspace_key: None },
        ] {
            assert!(save_role_in(&levels, &refused, role("x")).is_err());
        }
        assert!(save_role_in(&levels, &SaveAgentRoleTarget::default(), role("")).is_err());
        assert!(!agents.join("x.json").exists(), "a refused save writes nothing");

        // Delete.
        assert!(delete_role_in(&levels, BUILTIN_OPUS_ID).is_err());
        assert!(delete_role_in(&levels, "agent_user_ghost_00000000").is_err());
        delete_role_in(&levels, &second.id).unwrap();
        assert!(!agents.join("reviewer-2.json").exists());
    }

    fn legacy(name: &str, enabled: bool) -> AgentDefinition {
        let mut definition = registered_definition(&role(name), &RoleLevel::Global, name);
        definition.tools = None;
        definition.skill_ids = None;
        definition.mcp_ids = None;
        definition.hook_ids = None;
        definition.search_provider = None;
        definition.fetch_provider = None;
        definition.domain_filter = None;
        definition.max_searches_per_call = None;
        definition.native_search_tool = None;
        definition.native_fetch_tool = None;
        definition.enabled = enabled;
        definition
    }

    #[test]
    fn legacy_roles_are_exported_once_and_selected_where_they_were_on() {
        let app_data = tempfile::tempdir().unwrap();
        let agents = app_data.path().join("home-agents");
        let mut document = crate::catalog::default_document();
        let provider = crate::catalog::product_default_document()
            .assets
            .api_providers
            .into_iter()
            .find(|provider| provider.family == ProviderFamily::ClaudeAgent)
            .unwrap();
        let claude = provider.id.clone();
        document.assets.api_providers.push(provider);
        let builtin_copy = AgentDefinition {
            name: "Opus".into(),
            description: BUILTIN_ROLES[0].4.into(),
            model_selection: AgentModelSelection::Explicit {
                provider_id: claude,
                model_id: "claude-opus-5-5".into(),
            },
            effort: Some(ReasoningEffort::Medium),
            ..legacy("Opus", true)
        };
        let reviewer = legacy("reviewer", true);
        let other_reviewer = AgentDefinition {
            description: "a different one".into(),
            ..legacy("reviewer", true)
        };
        let quiet = legacy("quiet", false);
        let mut tombstone = legacy("gone", false);
        tombstone.deleted = true;
        document.presets.conversation_presets[0].settings.agent_definitions =
            vec![builtin_copy.clone(), reviewer.clone(), quiet.clone()];
        document.workspaces[0].conversations[0].settings.agent_definitions =
            vec![reviewer.clone(), other_reviewer.clone(), tombstone];

        migrate_legacy_agent_definitions(&mut document, app_data.path(), Some(&agents));

        let mut files = fs::read_dir(&agents)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        files.sort();
        assert_eq!(files, ["quiet.json", "reviewer-2.json", "reviewer.json"]);
        let exported: AgentRoleFile =
            serde_json::from_slice(&fs::read(agents.join("reviewer-2.json")).unwrap()).unwrap();
        assert_eq!(exported.name, "reviewer", "the name is kept as it was");
        assert_eq!(exported.tools, None, "no list means every tool");
        let text = fs::read_to_string(agents.join("reviewer.json")).unwrap();
        assert!(!text.contains("\"tools\""), "{text}");

        let preset = &document.presets.conversation_presets[0].settings;
        assert!(preset.agent_definitions.is_empty());
        assert_eq!(preset.agent_ids.len(), 2, "the disabled role is exported but not selected");
        assert_eq!(preset.agent_ids[0], BUILTIN_OPUS_ID);
        let conversation = &document.workspaces[0].conversations[0].settings;
        assert!(conversation.agent_definitions.is_empty(), "tombstones go too");
        assert_eq!(conversation.agent_ids.len(), 2);
        assert_eq!(conversation.agent_ids[0], preset.agent_ids[1], "the same role is one file");
        assert_ne!(conversation.agent_ids[0], conversation.agent_ids[1]);

        // A later load still finds the legacy lists; the record answers them
        // and nothing is written again, even for a file deleted since.
        fs::remove_file(agents.join("quiet.json")).unwrap();
        let mut again = crate::catalog::default_document();
        again.workspaces[0].conversations[0].settings.agent_definitions = vec![quiet, reviewer];
        migrate_legacy_agent_definitions(&mut again, app_data.path(), Some(&agents));
        assert!(!agents.join("quiet.json").exists());
        assert_eq!(fs::read_dir(&agents).unwrap().count(), 2);
        assert_eq!(again.workspaces[0].conversations[0].settings.agent_ids, [preset.agent_ids[1].clone()]);

        // Nowhere to write: the roles stay for the next load.
        let mut stuck = crate::catalog::default_document();
        stuck.workspaces[0].conversations[0].settings.agent_definitions = vec![legacy("new-one", true)];
        migrate_legacy_agent_definitions(&mut stuck, app_data.path(), None);
        assert_eq!(stuck.workspaces[0].conversations[0].settings.agent_definitions.len(), 1);
        assert!(stuck.workspaces[0].conversations[0].settings.agent_ids.is_empty());
    }

    /// A legacy role's fetch cap is its own answer and goes into its file's
    /// web search beside the search cap — clamped onto the ceiling like the
    /// search numbers rather than exported as a file discovery would refuse —
    /// and two roles differing only in it are two files. A project's own
    /// new-task draft is a carrier like the remembered settings.
    #[test]
    fn legacy_roles_carry_their_fetch_cap_and_leave_each_project_draft() {
        let app_data = tempfile::tempdir().unwrap();
        let agents = app_data.path().join("home-agents");
        let mut document = crate::catalog::default_document();
        let mut capped = legacy("scout", true);
        capped.compression_cutoff = 0;
        capped.fetch_compression_cutoff = crate::model::MAX_SEARCH_CUTOFF_LIMIT * 2;
        let plain = legacy("scout", true);
        let settings = document.workspaces[0].conversations[0].settings.clone();
        document.workspaces[0].draft_conversation = Some(crate::model::DraftConversationSnapshot {
            settings: crate::model::ConversationSettings {
                agent_definitions: vec![capped.clone(), plain],
                agent_ids: Vec::new(),
                ..settings
            },
            preset_id: String::new(),
        });

        migrate_legacy_agent_definitions(&mut document, app_data.path(), Some(&agents));

        let draft = &document.workspaces[0].draft_conversation.as_ref().unwrap().settings;
        assert!(draft.agent_definitions.is_empty());
        assert_eq!(draft.agent_ids.len(), 2, "a different fetch cap is a different role");
        let exported: AgentRoleFile =
            serde_json::from_slice(&fs::read(agents.join("scout.json")).unwrap()).unwrap();
        assert_eq!(exported.web_search.compression_cutoff, 0);
        assert_eq!(exported.web_search.fetch_compression_cutoff, crate::model::MAX_SEARCH_CUTOFF_LIMIT);
        assert!(validate_role(&exported).is_ok());
        let other: AgentRoleFile =
            serde_json::from_slice(&fs::read(agents.join("scout-2.json")).unwrap()).unwrap();
        assert_eq!(other.web_search.fetch_compression_cutoff, crate::model::DEFAULT_SEARCH_CUTOFF_LIMIT);
    }

    /// A role whose file would be too large to read back is refused before
    /// anything is written, rather than written and then listed unavailable.
    #[test]
    fn a_role_too_large_to_read_back_is_refused_rather_than_written() {
        let home = tempfile::tempdir().unwrap();
        let levels = [level(home.path(), None)];
        let mut huge = role("huge");
        huge.description = "x".repeat(ROLE_FILE_READ_LIMIT);
        let error = save_role_in(&levels, &SaveAgentRoleTarget::default(), huge.clone()).unwrap_err();
        assert!(error.contains("256"), "{error}");
        assert!(!home.path().join(".mewrk").join("agents").join("huge.json").exists());

        // Nor over an existing file.
        let saved = save_role_in(&levels, &SaveAgentRoleTarget::default(), role("huge")).unwrap();
        let path = home.path().join(".mewrk").join("agents").join("huge.json");
        let before = fs::read(&path).unwrap();
        let target = SaveAgentRoleTarget { id: Some(saved.id), workspace_key: None };
        assert!(save_role_in(&levels, &target, huge).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    /// A role's web search meets the limits a conversation's does: a file
    /// that breaks one is listed unavailable with the reason, and a save that
    /// would is refused.
    #[test]
    fn a_role_whose_web_search_breaks_a_limit_is_unavailable_and_refused() {
        let mut broken = Vec::new();
        let mut too_many_searches = role("r");
        too_many_searches.web_search.max_searches_per_call = crate::storage::MAX_WEB_SEARCHES_PER_CALL + 1;
        broken.push(too_many_searches);
        let mut too_many_results = role("r");
        too_many_results.web_search.max_results = crate::model::MAX_SEARCH_MAX_RESULTS + 1;
        broken.push(too_many_results);
        let mut too_long_cutoff = role("r");
        too_long_cutoff.web_search.compression_cutoff = crate::model::MAX_SEARCH_CUTOFF_LIMIT + 1;
        broken.push(too_long_cutoff);
        let mut too_long_fetch_cutoff = role("r");
        too_long_fetch_cutoff.web_search.fetch_compression_cutoff = crate::model::MAX_SEARCH_CUTOFF_LIMIT + 1;
        broken.push(too_long_fetch_cutoff);
        let mut too_many_rules = role("r");
        too_many_rules.web_search.include_domains =
            vec!["example.com".into(); crate::model::MAX_SEARCH_DOMAIN_RULES + 1];
        broken.push(too_many_rules);
        let mut long_rule = role("r");
        long_rule.web_search.exclude_domains = vec!["x".repeat(513)];
        broken.push(long_rule);
        let mut control = role("r");
        control.web_search.exclude_domains = vec!["evil.com\r\nHost: x".into()];
        broken.push(control);

        let home = tempfile::tempdir().unwrap();
        let levels = [level(home.path(), None)];
        for role in broken {
            let reason = validate_role(&role).unwrap_err();
            let bytes = serde_json::to_vec(&role).unwrap();
            let row = descriptor_from_bytes("/x/r.json".into(), "r.json", ResourceSource::User, None, Ok(bytes));
            assert!(!row.descriptor.available, "{reason}");
            assert_eq!(row.descriptor.description, reason);
            assert!(save_role_in(&levels, &SaveAgentRoleTarget::default(), role).is_err());
        }
        assert!(!home.path().join(".mewrk").join("agents").exists(), "nothing was written");
        // At the limits themselves, a role is fine.
        let mut at_limits = role("r");
        at_limits.web_search.max_searches_per_call = crate::storage::MAX_WEB_SEARCHES_PER_CALL;
        at_limits.web_search.include_domains = vec!["x".repeat(512)];
        assert!(validate_role(&at_limits).is_ok());
    }

    /// A workspace that is the home folder reads the same `agents` directory
    /// as the global level, and discovery lists such a file once, under the
    /// global level. A role saved to that workspace gets that id — the one
    /// the conversation then selects — and is registered at that level.
    #[test]
    fn a_role_saved_to_a_workspace_that_is_the_home_folder_gets_the_id_discovery_lists() {
        let home = tempfile::tempdir().unwrap();
        let home_path = home.path().to_string_lossy().into_owned();
        let levels = [level(home.path(), None), level(home.path(), Some(&home_path))];
        let target = SaveAgentRoleTarget {
            id: None,
            workspace_key: levels[1].workspace_key(),
        };
        let saved = save_role_in(&levels, &target, role("reviewer")).unwrap();
        let scan = crate::capabilities::scan_agent_roles(&levels);
        assert_eq!(scan.rows.len(), 1, "{:?}", scan.rows);
        assert_eq!(saved.id, scan.rows[0].descriptor.id);
        assert!(saved.id.starts_with("agent_user_reviewer_"), "{}", saved.id);
        assert_eq!(saved.registered.level, RoleLevel::Global);
        assert_eq!(scan.roles, [saved.registered]);
    }

    /// A new role file is reserved by creating it, so whatever already holds
    /// the name it would get — a folder, a link — is passed over for the next
    /// name and left as it was. Exports of legacy roles go the same way.
    #[test]
    fn a_new_role_file_never_replaces_or_enters_what_holds_its_name() {
        let home = tempfile::tempdir().unwrap();
        let agents = home.path().join(".mewrk").join("agents");
        fs::create_dir_all(agents.join("reviewer.json")).unwrap();
        fs::write(agents.join("reviewer.json").join("keep"), "x").unwrap();
        let levels = [level(home.path(), None)];
        save_role_in(&levels, &SaveAgentRoleTarget::default(), role("reviewer")).unwrap();
        assert!(agents.join("reviewer-2.json").is_file());
        assert_eq!(fs::read_dir(agents.join("reviewer.json")).unwrap().count(), 1);
        assert!(agents.join("reviewer.json").join("keep").is_file());

        // Each new file takes the next free name, an export's included.
        let written = create_local_file(&agents, "reviewer", "{}\n").unwrap();
        assert_eq!(written.file_name().unwrap(), "reviewer-3.json");
        let id = export_legacy_role(&agents, &AgentDefinition {
            description: "exported".into(),
            ..legacy("reviewer", true)
        })
        .unwrap();
        assert!(agents.join("reviewer-4.json").is_file());
        assert!(id.starts_with("agent_user_reviewer_4_"), "{id}");
        assert!(agents.join("reviewer.json").is_dir());
    }

    /// The scan a save stands on is made before the fence; under it the
    /// target is checked again, so a file that stopped being a plain file in
    /// between is refused rather than replaced.
    #[test]
    fn a_save_checks_its_target_again_when_it_writes() {
        let home = tempfile::tempdir().unwrap();
        let levels = [level(home.path(), None)];
        let saved = save_role_in(&levels, &SaveAgentRoleTarget::default(), role("reviewer")).unwrap();
        let target = SaveAgentRoleTarget { id: Some(saved.id.clone()), workspace_key: None };
        let save = plan_save(&levels, &target, role("critic")).unwrap();
        let delete = plan_delete(&levels, &saved.id).unwrap();
        let path = home.path().join(".mewrk").join("agents").join("reviewer.json");
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(commit_save(save).is_err());
        assert!(path.is_dir());
        assert!(commit_delete(&delete).is_err(), "a folder where the file was is not removed");
        assert!(path.is_dir());
    }

    /// The fenced commands write the registry as they write the file, and
    /// advance its generation, so a scan that started before them is not
    /// published over what they did — while one that starts after is.
    #[test]
    fn a_scan_that_started_before_a_save_or_delete_is_not_published() {
        let home = tempfile::tempdir().unwrap();
        let levels = [level(home.path(), None)];
        let authority = RwLock::new(());
        let registry = RwLock::new(AgentRoleRegistry::default());

        // A scan reads the directory before the role exists...
        let started = scan_generation(&registry);
        let early = crate::capabilities::scan_agent_roles(&levels);
        // ...then the role is saved...
        let id = save_role_fenced(&levels, &SaveAgentRoleTarget::default(), role("reviewer"), &authority, &registry)
            .unwrap();
        assert!(registry.read().unwrap().get(&id).is_some());
        // ...and the early scan, finishing late, does not take it away.
        assert!(!publish_scan(&authority, &registry, started, &early.levels, early.roles));
        assert!(registry.read().unwrap().get(&id).is_some());

        // A scan that starts after the save is published.
        let started = scan_generation(&registry);
        let late = crate::capabilities::scan_agent_roles(&levels);
        assert!(publish_scan(&authority, &registry, started, &late.levels, late.roles));
        assert!(registry.read().unwrap().get(&id).is_some());

        // The same for a delete: a scan from before it cannot put the role back.
        let started = scan_generation(&registry);
        let early = crate::capabilities::scan_agent_roles(&levels);
        delete_role_fenced(&levels, &id, &authority, &registry).unwrap();
        assert!(registry.read().unwrap().get(&id).is_none());
        assert!(!publish_scan(&authority, &registry, started, &early.levels, early.roles));
        assert!(registry.read().unwrap().get(&id).is_none());
        assert!(authority.try_write().is_ok(), "the fence is let go");
        assert!(delete_role_fenced(&levels, BUILTIN_OPUS_ID, &authority, &registry).is_err());
    }

    /// `~/.mewrk/agents` is shared by every data folder on the computer, and
    /// each keeps its own record of what it exported: the second to migrate a
    /// role finds the first's file and selects it, rather than writing a copy.
    #[test]
    fn legacy_roles_reuse_the_file_another_data_folder_exported() {
        let agents = tempfile::tempdir().unwrap();
        let agents = agents.path().join("agents");
        let reviewer = legacy("reviewer", true);
        let migrate = |app_data: &Path| {
            let mut document = crate::catalog::default_document();
            document.workspaces[0].conversations[0].settings.agent_definitions = vec![reviewer.clone()];
            migrate_legacy_agent_definitions(&mut document, app_data, Some(&agents));
            document.workspaces[0].conversations[0].settings.agent_ids.clone()
        };
        let production = tempfile::tempdir().unwrap();
        let development = tempfile::tempdir().unwrap();
        let first = migrate(production.path());
        let second = migrate(development.path());
        assert_eq!(first, second);
        assert_eq!(fs::read_dir(&agents).unwrap().count(), 1);
        assert!(agents.join("reviewer.json").is_file());

        // A file of that name holding another role is no match.
        fs::write(agents.join("reviewer.json"), r#"{"name":"reviewer","description":"edited"}"#).unwrap();
        let third = migrate(tempfile::tempdir().unwrap().path());
        assert_ne!(third, first);
        assert!(agents.join("reviewer-2.json").is_file());
    }

    /// A copy of a built-in role an earlier build seeded — with that build's
    /// model or description — is still the built-in, not a role of the
    /// user's to export; a copy the user changed is theirs.
    #[test]
    fn legacy_copies_of_earlier_built_ins_map_to_the_built_in() {
        let app_data = tempfile::tempdir().unwrap();
        let agents = app_data.path().join("agents");
        let mut document = crate::catalog::default_document();
        let codex = crate::catalog::product_default_document()
            .assets
            .api_providers
            .into_iter()
            .find(|provider| provider.family == ProviderFamily::OpenaiCodex)
            .unwrap();
        let claude = crate::catalog::product_default_document()
            .assets
            .api_providers
            .into_iter()
            .find(|provider| provider.family == ProviderFamily::ClaudeAgent)
            .unwrap();
        let copy = |name: &str, provider: &crate::model::ApiProvider, model: &str, description: &str| AgentDefinition {
            name: name.into(),
            description: description.into(),
            model_selection: AgentModelSelection::Explicit {
                provider_id: provider.id.clone(),
                model_id: model.into(),
            },
            effort: Some(ReasoningEffort::Medium),
            ..legacy(name, true)
        };
        let mut seeded = EARLIER_BUILTIN_ROLES
            .iter()
            .map(|&(id, model, description)| {
                let (_, name, family, ..) = BUILTIN_ROLES.iter().find(|(builtin, ..)| *builtin == id).unwrap();
                let provider = if *family == ProviderFamily::OpenaiCodex { &codex } else { &claude };
                copy(name, provider, model, description)
            })
            .collect::<Vec<_>>();
        // The earlier model with today's description was never seeded: it is
        // the user's.
        let edited = copy("Sol", &codex, "gpt-6-sol", BUILTIN_ROLES[2].4);
        seeded.push(edited);
        document.assets.api_providers.extend([codex.clone(), claude.clone()]);
        document.workspaces[0].conversations[0].settings.agent_definitions = seeded;
        migrate_legacy_agent_definitions(&mut document, app_data.path(), Some(&agents));
        let ids = &document.workspaces[0].conversations[0].settings.agent_ids;
        assert_eq!(ids[..3], [BUILTIN_OPUS_ID, BUILTIN_SOL_ID, BUILTIN_LUNA_ID]);
        assert_eq!(ids.len(), 4, "{ids:?}");
        assert!(ids[3].starts_with("agent_user_sol_"), "{ids:?}");
        assert_eq!(fs::read_dir(&agents).unwrap().count(), 1, "only the edited copy is exported");
    }
}
