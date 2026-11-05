//! Conversation presets (lib/conversationPresets.ts).

use crate::model::{
    ConversationPreset, ConversationPresetSettings, ConversationSettings, ConversationWebSearchSettings, HostMessageContainer,
    PresetLibrary, SecurityLevel,
};
use crate::settings_flags::{
    environment_prompt_enabled_of, file_write_guards_enabled_of, host_message_container_of, subagent_messaging_enabled_of,
};
use crate::task_tools::{is_host_derived_tool_name, unique_names};
use crate::tool_lock::tool_lock_of;
use std::collections::HashSet;

/// The built-in preset's id. Mirrors `catalog.rs::BUILTIN_PRESET_ID`, which is private to the host.
const BUILTIN_PRESET_ID: &str = "preset_mewrk";

/// Whether `preset_id` names the built-in preset. It ships with the build: the host rewrites it on every start and refuses any
/// save that edits or drops it, so the UI offers no rename, delete or save for it, only apply and saving as one's own.
pub fn is_builtin_conversation_preset(preset_id: &str) -> bool {
    preset_id == BUILTIN_PRESET_ID
}

/// The settings of a new blank preset. Not `Default`: the derive would turn the guards and the environment section off.
pub fn empty_conversation_preset_settings() -> ConversationPresetSettings {
    ConversationPresetSettings {
        enabled_tools: Vec::new(),
        tool_description_file_id: None,
        agent_ids: Vec::new(),
        agent_definitions: Vec::new(),
        allow_roleless_subagents: false,
        hook_ids: Vec::new(),
        skill_ids: Vec::new(),
        mcp_ids: Vec::new(),
        web_search: ConversationWebSearchSettings::default(),
        web_search_enabled: false,
        security_level: SecurityLevel::RequestApproval,
        global_memory_enabled: false,
        project_memory_enabled: false,
        skill_tool_enabled: false,
        mcp_tool_discovery_enabled: false,
        host_message_container: HostMessageContainer::User,
        file_write_guards_enabled: true,
        environment_prompt_enabled: true,
        subagent_messaging_enabled: false,
    }
}

/// Captures the reusable preset subset of current conversation settings. Resource IDs are copied directly and may be dangling.
pub fn capture_conversation_preset_settings(settings: &ConversationSettings) -> ConversationPresetSettings {
    ConversationPresetSettings {
        enabled_tools: settings.enabled_tools.clone(),
        tool_description_file_id: settings.tool_description_file_id.clone(),
        agent_ids: settings.agent_ids.clone(),
        // Host-owned, and never accepted from the client: every save keeps the committed list.
        agent_definitions: Vec::new(),
        allow_roleless_subagents: settings.allow_roleless_subagents,
        hook_ids: settings.hook_ids.clone(),
        skill_ids: settings.skill_ids.clone(),
        mcp_ids: settings.mcp_ids.clone(),
        web_search: settings.web_search.clone(),
        web_search_enabled: settings.web_search_enabled,
        security_level: settings.security_level,
        global_memory_enabled: settings.global_memory_enabled,
        project_memory_enabled: settings.project_memory_enabled,
        skill_tool_enabled: settings.skill_tool_enabled,
        mcp_tool_discovery_enabled: settings.mcp_tool_discovery_enabled,
        host_message_container: host_message_container_of(settings.host_message_container),
        file_write_guards_enabled: file_write_guards_enabled_of(settings.file_write_guards_enabled),
        environment_prompt_enabled: environment_prompt_enabled_of(settings.environment_prompt_enabled),
        subagent_messaging_enabled: subagent_messaging_enabled_of(settings.subagent_messaging_enabled),
    }
}

fn same_id_set(left: &[String], right: &[String]) -> bool {
    left.iter().collect::<HashSet<_>>() == right.iter().collect::<HashSet<_>>()
}

/// Whether two preset-owned bodies mean the same thing, deciding when a conversation stops belonging to the preset it was
/// applied from. Tool and resource ID lists compare as sets: the settings panel rewrites `enabled_tools` in view order, which
/// isn't an edit the user made.
pub fn same_conversation_preset_settings(a: &ConversationPresetSettings, b: &ConversationPresetSettings) -> bool {
    a.tool_description_file_id == b.tool_description_file_id
        && a.allow_roleless_subagents == b.allow_roleless_subagents
        && a.security_level == b.security_level
        && a.global_memory_enabled == b.global_memory_enabled
        && a.project_memory_enabled == b.project_memory_enabled
        && a.skill_tool_enabled == b.skill_tool_enabled
        && a.mcp_tool_discovery_enabled == b.mcp_tool_discovery_enabled
        && a.host_message_container == b.host_message_container
        && a.file_write_guards_enabled == b.file_write_guards_enabled
        && a.environment_prompt_enabled == b.environment_prompt_enabled
        && a.subagent_messaging_enabled == b.subagent_messaging_enabled
        && a.web_search_enabled == b.web_search_enabled
        && same_id_set(&a.enabled_tools, &b.enabled_tools)
        && same_id_set(&a.hook_ids, &b.hook_ids)
        && same_id_set(&a.skill_ids, &b.skill_ids)
        && same_id_set(&a.mcp_ids, &b.mcp_ids)
        && same_id_set(&a.agent_ids, &b.agent_ids)
        && a.web_search == b.web_search
}

/// The preset a new conversation starts from. The host keeps the built-in preset in every document, so `None`, nothing to
/// apply, only answers a document that never went through it.
pub fn default_conversation_preset(library: &PresetLibrary) -> Option<&ConversationPreset> {
    let presets = &library.conversation_presets;
    presets.iter().find(|preset| preset.id == library.default_conversation_preset_id).or(presets.first())
}

/// Resolves an applicable preset by ID. Missing IDs return `None` because deleted presets may still be referenced.
pub fn conversation_preset_by_id<'a>(library: &'a PresetLibrary, preset_id: &str) -> Option<&'a ConversationPreset> {
    if preset_id.is_empty() {
        return None;
    }
    library.conversation_presets.iter().find(|preset| preset.id == preset_id)
}

/// De-duplicated tool names, without vanished and host-derived ones.
fn persistable_tools(names: &[String], known_tool_names: Option<&HashSet<String>>) -> Vec<String> {
    unique_names(names.iter().cloned())
        .into_iter()
        .filter(|name| known_tool_names.is_none_or(|known| known.contains(name)) && !is_host_derived_tool_name(name))
        .collect()
}

/// Clones a workspace's saved settings snapshot into a new conversation. The snapshot is complete rather than an overlay, so
/// every field is copied. Vanished and host-derived tool names are removed.
pub fn clone_conversation_settings(snapshot: &ConversationSettings, known_tool_names: Option<&HashSet<String>>) -> ConversationSettings {
    ConversationSettings {
        enabled_tools: persistable_tools(&snapshot.enabled_tools, known_tool_names),
        hook_ids: unique_names(snapshot.hook_ids.iter().cloned()),
        skill_ids: unique_names(snapshot.skill_ids.iter().cloned()),
        mcp_ids: unique_names(snapshot.mcp_ids.iter().cloned()),
        tool_description_file_id: snapshot.tool_description_file_id.clone(),
        agent_ids: unique_names(snapshot.agent_ids.iter().cloned()),
        // Host-owned input of a one-time migration; a new conversation starts without it, as upstream's clone does.
        agent_definitions: Vec::new(),
        allow_roleless_subagents: snapshot.allow_roleless_subagents,
        web_search: snapshot.web_search.clone(),
        web_search_enabled: snapshot.web_search_enabled,
        reasoning_effort: snapshot.reasoning_effort,
        security_level: snapshot.security_level,
        plan_mode_enabled: snapshot.plan_mode_enabled,
        global_memory_enabled: snapshot.global_memory_enabled,
        project_memory_enabled: snapshot.project_memory_enabled,
        skill_tool_enabled: snapshot.skill_tool_enabled,
        mcp_tool_discovery_enabled: snapshot.mcp_tool_discovery_enabled,
        host_message_container: host_message_container_of(snapshot.host_message_container),
        file_write_guards_enabled: file_write_guards_enabled_of(snapshot.file_write_guards_enabled),
        environment_prompt_enabled: environment_prompt_enabled_of(snapshot.environment_prompt_enabled),
        subagent_messaging_enabled: subagent_messaging_enabled_of(snapshot.subagent_messaging_enabled),
        // The conversation's own choice, which a branch keeps. Not a preset's: a preset carries none, so a new conversation
        // chooses by its model.
        compaction_method: snapshot.compaction_method,
        // Read and never written (it has moved to each workspace), so a new conversation has none.
        legacy_sandbox: Default::default(),
        // Deliberately none: the new conversation has run nothing yet, so it has exposed nothing and every setting is still
        // free to move.
        tool_lock: None,
    }
}

/// Applies a preset as a complete overlay for preset-owned fields. Conversation-specific reasoning effort and app-data path
/// remain unchanged; host-derived memory tools are excluded because layer switches derive them. The one thing a preset can't
/// move is a pin: a preset naming another backend where a native one has already run applies everything else and leaves that
/// one field where the transcript put it. A host-run backend isn't pinned and moves with the preset. Everything else moves
/// with it: the lock only warns before a warm cache is thrown away, and never holds the surface where it was.
pub fn apply_conversation_preset_settings(
    current: &ConversationSettings,
    preset: &ConversationPresetSettings,
    known_tool_names: Option<&HashSet<String>>,
) -> ConversationSettings {
    let lock = tool_lock_of(current);
    let mut web_search = preset.web_search.clone();
    if let Some(provider) = lock.search_provider {
        web_search.provider = provider;
    }
    if let Some(fetch_provider) = lock.fetch_provider {
        web_search.fetch_provider = fetch_provider;
    }
    ConversationSettings {
        enabled_tools: persistable_tools(&preset.enabled_tools, known_tool_names),
        tool_description_file_id: preset.tool_description_file_id.clone(),
        agent_ids: unique_names(preset.agent_ids.iter().cloned()),
        allow_roleless_subagents: preset.allow_roleless_subagents,
        hook_ids: unique_names(preset.hook_ids.iter().cloned()),
        skill_ids: unique_names(preset.skill_ids.iter().cloned()),
        mcp_ids: unique_names(preset.mcp_ids.iter().cloned()),
        web_search,
        web_search_enabled: preset.web_search_enabled,
        security_level: preset.security_level,
        global_memory_enabled: preset.global_memory_enabled,
        project_memory_enabled: preset.project_memory_enabled,
        skill_tool_enabled: preset.skill_tool_enabled,
        mcp_tool_discovery_enabled: preset.mcp_tool_discovery_enabled,
        host_message_container: host_message_container_of(preset.host_message_container),
        file_write_guards_enabled: file_write_guards_enabled_of(preset.file_write_guards_enabled),
        environment_prompt_enabled: environment_prompt_enabled_of(preset.environment_prompt_enabled),
        subagent_messaging_enabled: subagent_messaging_enabled_of(preset.subagent_messaging_enabled),
        ..current.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ConversationToolLock, FetchProviderSelection, ReasoningEffort, SearchProviderKind, SearchProviderSelection};
    use crate::tool_defaults::test_catalog::tool_catalog;
    use serde_json::{json, Value};

    /// Reusable preset fields after flattening, sorted.
    const PRESET_FIELDS: [&str; 18] = [
        "agentIds",
        "allowRolelessSubagents",
        "enabledTools",
        "environmentPromptEnabled",
        "fileWriteGuardsEnabled",
        "globalMemoryEnabled",
        "hookIds",
        "hostMessageContainer",
        "mcpIds",
        "mcpToolDiscoveryEnabled",
        "projectMemoryEnabled",
        "securityLevel",
        "skillIds",
        "skillToolEnabled",
        "subagentMessagingEnabled",
        "toolDescriptionFileId",
        "webSearch",
        "webSearchEnabled",
    ];

    fn ids(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    /// Upstream's fixture conversation. Built from JSON so what it says nothing about reads as serde's defaults do.
    fn fixture_settings() -> ConversationSettings {
        serde_json::from_value(json!({
            "enabledTools": ["read", "write", "bash"],
            "skillIds": ["skill_code_review"],
            "mcpIds": ["mcp_workspace"],
            "webSearchEnabled": true
        }))
        .unwrap()
    }

    fn known_tools() -> HashSet<String> {
        tool_catalog().into_iter().map(|tool| tool.name).collect()
    }

    fn preset(id: &str, name: &str) -> ConversationPreset {
        ConversationPreset {
            id: id.into(),
            name: name.into(),
            description: String::new(),
            template_id: String::new(),
            settings: empty_conversation_preset_settings(),
        }
    }

    fn library(presets: Vec<ConversationPreset>, default_id: &str) -> PresetLibrary {
        PresetLibrary { conversation_presets: presets, default_conversation_preset_id: default_id.into() }
    }

    /// A preset as an older build wrote it: JSON without `key`, read back through serde's defaults.
    fn without_key(preset: &ConversationPresetSettings, key: &str) -> ConversationPresetSettings {
        let mut value = serde_json::to_value(preset).unwrap();
        value.as_object_mut().unwrap().remove(key).unwrap();
        serde_json::from_value(value).unwrap()
    }

    fn sorted_keys(value: &ConversationPresetSettings) -> Vec<String> {
        let Value::Object(fields) = serde_json::to_value(value).unwrap() else { panic!("not an object") };
        let mut keys: Vec<String> = fields.keys().cloned().collect();
        keys.sort();
        keys
    }

    #[test]
    fn uses_the_configured_default_preset_id() {
        let library = library(vec![preset("first", "First"), preset("conversation_second", "Second")], "conversation_second");
        assert_eq!(default_conversation_preset(&library).unwrap().id, "conversation_second");
    }

    #[test]
    fn falls_back_to_the_first_preset_and_to_none_when_there_is_none() {
        let fallback = library(vec![preset("first", "First"), preset("second", "Second")], "preset-gone");
        assert_eq!(default_conversation_preset(&fallback).unwrap().id, "first");
        // The host keeps the built-in preset in every document, so an empty list only answers a document that never went
        // through it: nothing to apply.
        assert!(default_conversation_preset(&library(Vec::new(), "preset-gone")).is_none());
    }

    #[test]
    fn recognizes_the_built_in_preset_by_its_id_alone() {
        assert!(is_builtin_conversation_preset(BUILTIN_PRESET_ID));
        assert!(!is_builtin_conversation_preset("preset_codex"));
        assert!(!is_builtin_conversation_preset(""));
    }

    #[test]
    fn keeps_the_empty_preset_shape_flat() {
        let empty = empty_conversation_preset_settings();
        assert_eq!(
            serde_json::to_value(&empty).unwrap(),
            json!({
                "enabledTools": [],
                "toolDescriptionFileId": null,
                "agentIds": [],
                "allowRolelessSubagents": false,
                "hookIds": [],
                "skillIds": [],
                "mcpIds": [],
                "webSearch": serde_json::to_value(ConversationWebSearchSettings::default()).unwrap(),
                "webSearchEnabled": false,
                "securityLevel": "request_approval",
                "globalMemoryEnabled": false,
                "projectMemoryEnabled": false,
                "skillToolEnabled": false,
                "mcpToolDiscoveryEnabled": false,
                "hostMessageContainer": "user",
                "fileWriteGuardsEnabled": true,
                "environmentPromptEnabled": true,
                "subagentMessagingEnabled": false
            })
        );
        assert_eq!(sorted_keys(&empty), PRESET_FIELDS);
    }

    #[test]
    fn captures_the_reusable_subset_from_one_settings_argument() {
        let mut settings = fixture_settings();
        settings.hook_ids = ids(&["hook_format"]);
        settings.tool_description_file_id = Some("tooldesc_user_main_0f0f0f0f".into());
        settings.agent_ids = ids(&["agent_a", "agent_b"]);
        // Conversation-only fields don't enter presets.
        settings.plan_mode_enabled = true;

        let captured = capture_conversation_preset_settings(&settings);

        assert_eq!(
            captured,
            ConversationPresetSettings {
                enabled_tools: settings.enabled_tools.clone(),
                tool_description_file_id: Some("tooldesc_user_main_0f0f0f0f".into()),
                agent_ids: ids(&["agent_a", "agent_b"]),
                hook_ids: ids(&["hook_format"]),
                skill_ids: ids(&["skill_code_review"]),
                mcp_ids: ids(&["mcp_workspace"]),
                web_search: settings.web_search.clone(),
                web_search_enabled: true,
                // The fixture says nothing about the guards or the environment section, which reads as on.
                ..empty_conversation_preset_settings()
            }
        );
        assert_eq!(sorted_keys(&captured), PRESET_FIELDS);
    }

    #[test]
    fn applies_a_preset_straight_onto_conversation_settings_and_keeps_conversation_only_fields() {
        let mut current = fixture_settings();
        current.web_search.max_searches_per_call = 7;
        current.agent_ids = ids(&["agent_old"]);
        current.reasoning_effort = ReasoningEffort::High;
        current.plan_mode_enabled = true;
        let mut web_search = ConversationWebSearchSettings::default();
        web_search.max_searches_per_call = 42;
        let preset = ConversationPresetSettings {
            enabled_tools: ids(&["read", "read", "extinct-tool"]),
            tool_description_file_id: Some("tooldesc_user_preset_11112222".into()),
            agent_ids: ids(&["agent_a", "agent_a", "agent_b"]),
            allow_roleless_subagents: true,
            hook_ids: ids(&["hook_a", "hook_a"]),
            skill_ids: ids(&["skill_installer"]),
            mcp_ids: Vec::new(),
            web_search,
            web_search_enabled: true,
            security_level: SecurityLevel::FullAccess,
            global_memory_enabled: true,
            project_memory_enabled: false,
            skill_tool_enabled: true,
            mcp_tool_discovery_enabled: true,
            host_message_container: HostMessageContainer::Box,
            file_write_guards_enabled: false,
            environment_prompt_enabled: false,
            subagent_messaging_enabled: true,
            agent_definitions: Vec::new(),
        };

        let applied = apply_conversation_preset_settings(&current, &preset, Some(&known_tools()));

        assert_eq!(applied.enabled_tools, ids(&["read"]));
        assert_eq!(applied.hook_ids, ids(&["hook_a"]));
        assert_eq!(applied.tool_description_file_id.as_deref(), Some("tooldesc_user_preset_11112222"));
        // The preset's roles replace the conversation's rather than joining them, de-duplicated like every other id list.
        assert_eq!(applied.agent_ids, ids(&["agent_a", "agent_b"]));
        assert_eq!(current.agent_ids, ids(&["agent_old"]));
        // Applying a preset replaces web-search behavior.
        assert_eq!(applied.web_search, preset.web_search);
        // Security policy is a preset component.
        assert_eq!(applied.security_level, SecurityLevel::FullAccess);
        assert_eq!(applied.reasoning_effort, current.reasoning_effort);
        assert!(applied.plan_mode_enabled);
        // Memory-tier switches are preset components and replace their respective values.
        assert!(!current.global_memory_enabled);
        assert!(applied.global_memory_enabled);
        assert!(!applied.project_memory_enabled);
        // So is the container host messages come in.
        assert_eq!(current.host_message_container, HostMessageContainer::User);
        assert_eq!(applied.host_message_container, HostMessageContainer::Box);
        // And whether the file write guards run.
        assert!(current.file_write_guards_enabled);
        assert!(!applied.file_write_guards_enabled);
        // A preset that says nothing about them (one written before the switch) turns them on.
        let silent = without_key(&preset, "fileWriteGuardsEnabled");
        assert!(apply_conversation_preset_settings(&applied, &silent, None).file_write_guards_enabled);
        // And whether the prompt opens with the environment section.
        assert!(current.environment_prompt_enabled);
        assert!(!applied.environment_prompt_enabled);
        let unspoken = without_key(&preset, "environmentPromptEnabled");
        assert!(apply_conversation_preset_settings(&applied, &unspoken, None).environment_prompt_enabled);
        // And whether subagents can be talked to: off in the conversation, on in the preset.
        assert!(!current.subagent_messaging_enabled);
        assert!(applied.subagent_messaging_enabled);
        let quiet = without_key(&preset, "subagentMessagingEnabled");
        assert!(!apply_conversation_preset_settings(&applied, &quiet, None).subagent_messaging_enabled);
    }

    #[test]
    fn applies_a_preset_over_a_conversation_that_has_run_moving_everything_but_the_pins() {
        let mut current = fixture_settings();
        current.skill_ids = ids(&["skill_locked"]);
        current.skill_tool_enabled = false;
        current.web_search_enabled = true;
        current.tool_lock = Some(ConversationToolLock {
            web_search: true,
            skill_ids: ids(&["skill_locked"]),
            prompt_skill_ids: Some(ids(&["skill_locked"])),
            search_backend: Some(SearchProviderSelection::Native),
            fetch_backend: Some(FetchProviderSelection::Native),
            web_fetch: true,
            search_provider: Some(SearchProviderSelection::Native),
            fetch_provider: Some(FetchProviderSelection::Native),
            ..Default::default()
        });
        let mut web_search = ConversationWebSearchSettings::default();
        web_search.provider = SearchProviderSelection::Explicit { provider_kind: SearchProviderKind::Tavily };
        web_search.fetch_provider = FetchProviderSelection::Explicit { provider_kind: SearchProviderKind::Jina };
        web_search.max_searches_per_call = 9;
        let preset = ConversationPresetSettings {
            skill_ids: ids(&["skill_other"]),
            skill_tool_enabled: true,
            web_search,
            ..capture_conversation_preset_settings(&current)
        };

        let applied = apply_conversation_preset_settings(&current, &preset, None);

        // The surface is the preset's: the lock only warns before a warm cache is thrown away, on every model.
        assert_eq!(applied.skill_ids, ids(&["skill_other"]));
        assert!(applied.skill_tool_enabled);
        // A backend the transcript already used can't be swapped part-way through.
        assert_eq!(applied.web_search.provider, SearchProviderSelection::Native);
        assert_eq!(applied.web_search.fetch_provider, FetchProviderSelection::Native);
        // The rest of the preset's web-search body still applies.
        assert_eq!(applied.web_search.max_searches_per_call, preset.web_search.max_searches_per_call);
    }

    #[test]
    fn keeps_dangling_capability_ids_on_both_sides_of_a_capture_apply_round_trip() {
        // Capability IDs may outlive installation, so they must not be filtered by the catalog.
        let mut current = fixture_settings();
        current.hook_ids = ids(&["hook_not_installed"]);
        current.skill_ids = ids(&["skill_not_installed"]);
        current.mcp_ids = ids(&["mcp_not_installed"]);
        current.agent_ids = ids(&["agent_not_installed"]);

        let captured = capture_conversation_preset_settings(&current);
        let applied = apply_conversation_preset_settings(&current, &captured, None);

        assert_eq!(captured.hook_ids, ids(&["hook_not_installed"]));
        assert_eq!(captured.skill_ids, ids(&["skill_not_installed"]));
        assert_eq!(captured.mcp_ids, ids(&["mcp_not_installed"]));
        assert_eq!(captured.agent_ids, ids(&["agent_not_installed"]));
        assert_eq!(applied.hook_ids, ids(&["hook_not_installed"]));
        assert_eq!(applied.skill_ids, ids(&["skill_not_installed"]));
        assert_eq!(applied.mcp_ids, ids(&["mcp_not_installed"]));
        assert_eq!(applied.agent_ids, ids(&["agent_not_installed"]));
    }

    #[test]
    fn deep_copies_a_remembered_snapshot_so_the_new_conversation_cannot_edit_the_workspaces_copy() {
        let snapshot = ConversationSettings {
            reasoning_effort: ReasoningEffort::High,
            security_level: SecurityLevel::FullAccess,
            enabled_tools: ids(&["read", "read", "no-such-tool", "web_search"]),
            agent_ids: ids(&["agent_a", "agent_a", "agent_b"]),
            global_memory_enabled: true,
            tool_lock: Some(ConversationToolLock::default()),
            ..fixture_settings()
        };

        let cloned = clone_conversation_settings(&snapshot, Some(&known_tools()));

        // Snapshot-only fields distinguish remembered settings from a preset application.
        assert_eq!(cloned.reasoning_effort, ReasoningEffort::High);
        assert_eq!(cloned.security_level, SecurityLevel::FullAccess);
        // Remove duplicate, retired and host-derived tool names.
        assert_eq!(cloned.enabled_tools, ids(&["read"]));
        assert!(cloned.global_memory_enabled);
        // Selected roles are de-duplicated, like the other id lists.
        assert_eq!(cloned.agent_ids, ids(&["agent_a", "agent_b"]));
        // The new conversation has run nothing yet, so it has exposed nothing.
        assert_eq!(cloned.tool_lock, None);
    }

    #[test]
    fn carries_the_file_write_guards_into_a_clone_as_an_explicit_boolean() {
        // Absent reads as on, and the clone says so outright.
        let snapshot = fixture_settings();
        assert!(clone_conversation_settings(&snapshot, None).file_write_guards_enabled);
        let off = ConversationSettings { file_write_guards_enabled: false, ..snapshot.clone() };
        assert!(!clone_conversation_settings(&off, None).file_write_guards_enabled);
        let on = ConversationSettings { file_write_guards_enabled: true, ..snapshot };
        assert!(clone_conversation_settings(&on, None).file_write_guards_enabled);
    }

    #[test]
    fn carries_the_environment_prompt_into_a_clone_as_an_explicit_boolean() {
        let snapshot = fixture_settings();
        assert!(clone_conversation_settings(&snapshot, None).environment_prompt_enabled);
        let off = ConversationSettings { environment_prompt_enabled: false, ..snapshot.clone() };
        assert!(!clone_conversation_settings(&off, None).environment_prompt_enabled);
        let on = ConversationSettings { environment_prompt_enabled: true, ..snapshot };
        assert!(clone_conversation_settings(&on, None).environment_prompt_enabled);
    }

    #[test]
    fn carries_subagent_communication_into_a_clone_as_an_explicit_boolean() {
        // Absent reads as off, and the clone says so outright.
        let snapshot = fixture_settings();
        assert!(!clone_conversation_settings(&snapshot, None).subagent_messaging_enabled);
        let on = ConversationSettings { subagent_messaging_enabled: true, ..snapshot.clone() };
        assert!(clone_conversation_settings(&on, None).subagent_messaging_enabled);
        let off = ConversationSettings { subagent_messaging_enabled: false, ..snapshot };
        assert!(!clone_conversation_settings(&off, None).subagent_messaging_enabled);
    }

    #[test]
    fn resolves_a_preset_id_leniently() {
        let library = library(vec![preset("preset-a", "审阅")], "preset-a");
        assert_eq!(conversation_preset_by_id(&library, "preset-a").unwrap().name, "审阅");
        // Missing and empty IDs resolve to no preset rather than throwing or falling back.
        assert!(conversation_preset_by_id(&library, "preset-gone").is_none());
        assert!(conversation_preset_by_id(&library, "").is_none());
    }

    #[test]
    fn treats_reordered_tool_and_resource_lists_as_the_same_preset_body() {
        let base = ConversationPresetSettings {
            enabled_tools: ids(&["read_file", "write_file", "bash"]),
            hook_ids: ids(&["hook-a", "hook-b"]),
            skill_ids: ids(&["skill-a"]),
            mcp_ids: ids(&["mcp-a"]),
            agent_ids: ids(&["agent-a", "agent-b"]),
            ..empty_conversation_preset_settings()
        };
        let same = |other: &ConversationPresetSettings| same_conversation_preset_settings(&base, other);
        // The settings panel rewrites `enabled_tools` in view order; that isn't an edit.
        assert!(same(&ConversationPresetSettings {
            enabled_tools: ids(&["bash", "write_file", "read_file"]),
            hook_ids: ids(&["hook-b", "hook-a"]),
            agent_ids: ids(&["agent-b", "agent-a"]),
            ..base.clone()
        }));
        assert!(!same(&ConversationPresetSettings { enabled_tools: ids(&["read_file", "write_file"]), ..base.clone() }));
        assert!(!same(&ConversationPresetSettings { security_level: SecurityLevel::FullAccess, ..base.clone() }));
        // The file write guards are part of the body; an absent one is the same as on.
        let unsaid = without_key(&base, "fileWriteGuardsEnabled");
        let guards_off = ConversationPresetSettings { file_write_guards_enabled: false, ..base.clone() };
        assert!(same(&unsaid));
        assert!(!same(&guards_off));
        assert!(!same_conversation_preset_settings(&unsaid, &guards_off));
        // The environment section is too; an absent one is the same as on.
        let unspoken = without_key(&base, "environmentPromptEnabled");
        let environment_off = ConversationPresetSettings { environment_prompt_enabled: false, ..base.clone() };
        assert!(same(&unspoken));
        assert!(!same(&environment_off));
        assert!(!same_conversation_preset_settings(&unspoken, &environment_off));
        // Subagent communication is too; an absent one is the same as off.
        let quiet = without_key(&base, "subagentMessagingEnabled");
        let messaging_on = ConversationPresetSettings { subagent_messaging_enabled: true, ..base.clone() };
        assert!(same(&quiet));
        assert!(!same(&messaging_on));
        assert!(!same_conversation_preset_settings(&quiet, &messaging_on));
        let mut web_search = base.web_search.clone();
        web_search.max_searches_per_call += 1;
        assert!(!same(&ConversationPresetSettings { web_search, ..base.clone() }));
    }

    #[test]
    fn compares_the_selected_roles_as_a_set_reordering_is_no_edit_adding_or_removing_one_is() {
        let with_roles =
            |agent_ids: &[&str]| ConversationPresetSettings { agent_ids: ids(agent_ids), ..empty_conversation_preset_settings() };
        let original = with_roles(&["agent_a", "agent_b"]);
        let same = |other: &ConversationPresetSettings| same_conversation_preset_settings(&original, other);
        // Applying a preset copies its ids, so a capture of the same selection is the same body.
        let settings: ConversationSettings =
            serde_json::from_value(json!({ "enabledTools": [], "agentIds": ["agent_a", "agent_b"] })).unwrap();
        assert!(same(&capture_conversation_preset_settings(&settings)));
        // A selection has no order: ticking the same roles in another order is no edit.
        assert!(same(&with_roles(&["agent_b", "agent_a"])));
        assert!(same(&with_roles(&["agent_a", "agent_a", "agent_b"])));
        assert!(!same(&with_roles(&["agent_a"])));
        assert!(!same(&with_roles(&["agent_a", "agent_b", "agent_c"])));
        // Same size, different member: a count check alone would miss it.
        assert!(!same(&with_roles(&["agent_a", "agent_c"])));
        assert!(!same(&with_roles(&[])));
        assert!(same_conversation_preset_settings(&with_roles(&[]), &with_roles(&[])));
    }
}
