//! Agent roles (lib/agentRoles.ts): what the renderer works out for itself about the host's subagent role files.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::model::{
    AgentModelSelection, ApiProvider, ConversationWebSearchSettings, FetchProviderSelection, NativeFetchTool, NativeSearchTool, ReasoningEffort,
    ResourceDescriptor, SearchDomainFilterMode, SearchProviderSelection, DEFAULT_SEARCH_CUTOFF_LIMIT, DEFAULT_SEARCH_MAX_RESULTS,
};

pub use crate::model::MAX_AGENT_TYPE_CHARS;

/// The body of one role file (`AgentRole` in types.ts). Mirrors the host's private `agent_roles::AgentRoleFile`.
///
/// Every tool-like answer is the role's own; only the model, the effort and the tool-description file may ride the
/// caller at run time. `tools` is always a concrete list on the wire, and `[]` is a real "none".
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct AgentRole {
    pub name: String,
    pub description: String,
    pub model_selection: AgentModelSelection,
    /// `None` rides the caller's reasoning effort.
    pub effort: Option<ReasoningEffort>,
    pub tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
    pub skill_ids: Vec<String>,
    pub mcp_ids: Vec<String>,
    pub hook_ids: Vec<String>,
    pub web_search: ConversationWebSearchSettings,
    /// May dangle; a deleted template seeds nothing.
    pub template_id: Option<String>,
    /// `None` renders with the caller's. May dangle; the child then falls back to the built-in.
    pub tool_description_file_id: Option<String>,
}

/// One discovered role as the catalog lists it (`AgentRoleResource` in types.ts).
///
/// `descriptor.description` is the role's own, or why the file could not be used, and `role` is then `None`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentRoleResource {
    #[serde(flatten)]
    pub descriptor: ResourceDescriptor,
    pub role: Option<AgentRole>,
}

/// Why a role name is refused. None of them is about shape: see [`validate_agent_type_name`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentTypeNameError {
    Required,
    TooLong,
    Characters,
}

/// Whether a role's bound model resolves against the current providers (`agent_definition_model_is_available`).
///
/// `explicit` records the exact (provider, model) pair: two providers may both carry a `gpt-4o` and they are not the
/// same model, so matching the bare id would silently rebind a role. `inherit` rides the caller; `unavailable` has no
/// pair left to re-check. The host hides a role that doesn't resolve, so the settings list has to say so too.
pub fn agent_model_selection_is_available(selection: &AgentModelSelection, providers: &[ApiProvider]) -> bool {
    let (provider_id, model_id) = match selection {
        AgentModelSelection::Inherit => return true,
        AgentModelSelection::Unavailable => return false,
        AgentModelSelection::Explicit { provider_id, model_id } => (provider_id, model_id),
    };
    let Some(provider) = providers.iter().find(|candidate| candidate.id == *provider_id) else { return false };
    provider.enabled && provider.models.iter().any(|candidate| candidate.id == *model_id)
}

/// The roles a conversation can reach: the global level plus its own workspaces. `None` narrows nothing, which is a
/// preset: it points at no workspace in particular.
fn reachable_agent_roles<'a>(agents: &'a [AgentRoleResource], workspace_keys: Option<&[String]>) -> impl Iterator<Item = &'a AgentRoleResource> {
    let reachable: Option<HashSet<String>> = workspace_keys.map(|keys| keys.iter().cloned().collect());
    agents.iter().filter(move |resource| match (&reachable, resource.descriptor.workspace_key.as_deref()) {
        (Some(reachable), Some(key)) if !key.is_empty() => reachable.contains(key),
        _ => true,
    })
}

/// Whether the conversation offers the model any role it can actually call: selected, listed at a level it reaches,
/// readable, and with a model that resolves.
///
/// An approximation of `api::available_agent_type_names`: it can't see same-name precedence, so it controls only how
/// the no-role-subagent switch explains itself. That is safe because the host allows role-less children whenever its
/// final set is empty.
pub fn has_usable_agent_role(agents: &[AgentRoleResource], agent_ids: &[String], providers: &[ApiProvider], workspace_keys: Option<&[String]>) -> bool {
    reachable_agent_roles(agents, workspace_keys).any(|resource| {
        agent_ids.contains(&resource.descriptor.id)
            && resource.descriptor.available
            && resource.role.as_ref().is_some_and(|role| agent_model_selection_is_available(&role.model_selection, providers))
    })
}

/// How many of a conversation's selected roles the catalog lists, which every surface that counts roles uses.
///
/// A dangling id is still a row the user can untick, but counting it would announce a role nothing can call.
/// `workspace_keys` narrows as on the roles page, so another workspace's role is dangling here too.
pub fn selected_agent_role_count(agents: &[AgentRoleResource], agent_ids: &[String], workspace_keys: Option<&[String]>) -> usize {
    let known: HashSet<&str> = reachable_agent_roles(agents, workspace_keys).map(|resource| resource.descriptor.id.as_str()).collect();
    agent_ids.iter().map(String::as_str).filter(|id| known.contains(id)).collect::<HashSet<_>>().len()
}

/// What a role may be called, or why not (mirrors the host's `validate_agent_type_name`).
///
/// Free text, deliberately: a role name is prose the user writes and the model reads back, so "代码审查" and
/// "Code Reviewer" are as legitimate as `reviewer`. Refused are only empty, longer than the host stores (counted in
/// characters), and control characters, which can't survive being echoed in a listing or an error. The editor trims
/// before it gets here: a trailing space is a slip, not a decision.
pub fn validate_agent_type_name(value: &str) -> Option<AgentTypeNameError> {
    let characters = value.chars().count();
    if characters == 0 {
        return Some(AgentTypeNameError::Required);
    }
    if characters > MAX_AGENT_TYPE_CHARS {
        return Some(AgentTypeNameError::TooLong);
    }
    // Not `char::is_control`: that also takes U+0080..U+009F, which upstream and the host let through.
    if value.chars().any(|c| c <= '\u{1f}' || c == '\u{7f}') {
        return Some(AgentTypeNameError::Characters);
    }
    None
}

/// Sorted and de-duplicated, matching the host's canonicalization so a reorder in the editor isn't taken for a change.
pub fn canonical_agent_tool_names(names: &[String]) -> Vec<String> {
    let mut names = names.to_vec();
    names.sort();
    names.dedup();
    names
}

/// The web configuration a new role starts with: what the built-in preset ships, and nothing borrowed from the
/// conversation the editor was opened from, because a role is a reusable asset.
pub fn default_agent_role_web_search() -> ConversationWebSearchSettings {
    ConversationWebSearchSettings {
        max_searches_per_call: 0,
        provider: SearchProviderSelection::Native,
        fetch_provider: FetchProviderSelection::Native,
        // The first of each offered list is its default.
        native_search_tool: NativeSearchTool::ALL[0],
        native_fetch_tool: NativeFetchTool::ALL[0],
        max_results: DEFAULT_SEARCH_MAX_RESULTS,
        compression_cutoff: DEFAULT_SEARCH_CUTOFF_LIMIT,
        fetch_compression_cutoff: DEFAULT_SEARCH_CUTOFF_LIMIT,
        domain_filter: SearchDomainFilterMode::Off,
        include_domains: Vec::new(),
        exclude_domains: Vec::new(),
    }
}

/// Whether two role bodies say the same thing, field for field.
///
/// The editor's concurrency check: a save whose file has since been rewritten is refused rather than written over.
/// Order is meaning in every list except `tools`, which the editor and the host both keep canonical.
pub fn same_agent_role(a: &AgentRole, b: &AgentRole) -> bool {
    a == b
}

/// A role body copied deep enough that editing the copy never touches the catalog.
pub fn clone_agent_role(role: &AgentRole) -> AgentRole {
    role.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ResourceSource;
    use serde_json::json;

    fn strs(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    fn provider(id: &str, model_ids: &[&str], enabled: bool) -> ApiProvider {
        let models: Vec<_> = model_ids
            .iter()
            .map(|id| json!({"id": id, "name": "", "group": "", "capabilities": [], "reasoningContent": "plaintext", "promptCache": true}))
            .collect();
        serde_json::from_value(json!({
            "id": id, "name": id, "enabled": enabled, "family": "openai_chat", "baseUrl": "https://example.invalid/v1",
            "familySettings": {}, "notes": "", "models": models, "activeModelId": model_ids.first(),
        }))
        .unwrap()
    }

    fn role() -> AgentRole {
        AgentRole {
            name: "reviewer".to_owned(),
            description: "Reads a diff and says what is wrong with it.".to_owned(),
            model_selection: AgentModelSelection::Inherit,
            effort: None,
            tools: strs(&["read_file"]),
            disallowed_tools: Vec::new(),
            skill_ids: Vec::new(),
            mcp_ids: Vec::new(),
            hook_ids: Vec::new(),
            web_search: default_agent_role_web_search(),
            template_id: None,
            tool_description_file_id: None,
        }
    }

    fn resource(id: &str) -> AgentRoleResource {
        AgentRoleResource {
            descriptor: ResourceDescriptor {
                id: id.to_owned(),
                name: id.to_owned(),
                description: String::new(),
                location: format!("C:/Users/u/.mewrk/agents/{id}.json"),
                source: ResourceSource::User,
                available: true,
                workspace_key: None,
            },
            role: Some(AgentRole { name: id.to_owned(), ..role() }),
        }
    }

    fn with(mut resource: AgentRoleResource, change: impl FnOnce(&mut AgentRoleResource)) -> AgentRoleResource {
        change(&mut resource);
        resource
    }

    fn in_workspace(id: &str, key: &str) -> AgentRoleResource {
        with(resource(id), |r| {
            r.descriptor.source = ResourceSource::Workspace;
            r.descriptor.workspace_key = Some(key.to_owned());
        })
    }

    fn explicit(provider_id: &str, model_id: &str) -> AgentModelSelection {
        AgentModelSelection::Explicit { provider_id: provider_id.to_owned(), model_id: model_id.to_owned() }
    }

    #[test]
    fn takes_a_role_name_as_free_text_and_refuses_only_what_cannot_be_echoed_back() {
        assert_eq!(validate_agent_type_name("reviewer"), None);
        assert_eq!(validate_agent_type_name("code-reviewer_2"), None);
        // The shape rule is gone: case, spaces, punctuation and non-ASCII are all ordinary.
        assert_eq!(validate_agent_type_name("Code Reviewer"), None);
        assert_eq!(validate_agent_type_name("对抗式审查"), None);
        assert_eq!(validate_agent_type_name("2reviewer"), None);
        assert_eq!(validate_agent_type_name("code/reviewer"), None);
        // What is left: nothing to name, more than the host stores, characters that can't survive a listing.
        assert_eq!(validate_agent_type_name(""), Some(AgentTypeNameError::Required));
        assert_eq!(validate_agent_type_name(&format!("a{}", "x".repeat(64))), Some(AgentTypeNameError::TooLong));
        assert_eq!(validate_agent_type_name("review\ner"), Some(AgentTypeNameError::Characters));
    }

    #[test]
    fn counts_a_role_names_length_in_characters_not_utf_16_units() {
        assert_eq!(MAX_AGENT_TYPE_CHARS, 64);
        assert_eq!(validate_agent_type_name(&"x".repeat(MAX_AGENT_TYPE_CHARS)), None);
        assert_eq!(validate_agent_type_name(&"x".repeat(MAX_AGENT_TYPE_CHARS + 1)), Some(AgentTypeNameError::TooLong));
        // An astral character is one character to the user: a name of exactly the limit must pass.
        assert_eq!(validate_agent_type_name(&"😀".repeat(MAX_AGENT_TYPE_CHARS)), None);
        assert_eq!(validate_agent_type_name(&"😀".repeat(MAX_AGENT_TYPE_CHARS + 1)), Some(AgentTypeNameError::TooLong));
        // Control characters are refused wherever they sit, DEL included.
        assert_eq!(validate_agent_type_name("\u{0}reviewer"), Some(AgentTypeNameError::Characters));
        assert_eq!(validate_agent_type_name("review\u{7f}er"), Some(AgentTypeNameError::Characters));
        assert_eq!(validate_agent_type_name("review\ter"), Some(AgentTypeNameError::Characters));
    }

    fn selection_providers() -> Vec<ApiProvider> {
        vec![provider("provider_a", &["gpt-4o", "gpt-5"], true), provider("provider_b", &["claude-x"], true), provider("provider_off", &["gpt-4o"], false)]
    }

    #[test]
    fn always_resolves_an_inherited_model_whatever_providers_exist() {
        assert!(agent_model_selection_is_available(&AgentModelSelection::Inherit, &selection_providers()));
        assert!(agent_model_selection_is_available(&AgentModelSelection::Inherit, &[]));
    }

    #[test]
    fn never_resolves_an_unavailable_binding_there_is_no_pair_left_to_re_check() {
        assert!(!agent_model_selection_is_available(&AgentModelSelection::Unavailable, &selection_providers()));
        assert!(!agent_model_selection_is_available(&AgentModelSelection::Unavailable, &[]));
    }

    #[test]
    fn resolves_an_explicit_pair_only_when_an_enabled_provider_lists_the_model() {
        let providers = selection_providers();
        assert!(agent_model_selection_is_available(&explicit("provider_a", "gpt-4o"), &providers));
        assert!(agent_model_selection_is_available(&explicit("provider_b", "claude-x"), &providers));
        // Provider missing.
        assert!(!agent_model_selection_is_available(&explicit("provider_gone", "gpt-4o"), &providers));
        assert!(!agent_model_selection_is_available(&explicit("provider_a", "gpt-4o"), &[]));
        // Provider disabled: the model is listed, but nothing may call it.
        assert!(!agent_model_selection_is_available(&explicit("provider_off", "gpt-4o"), &providers));
        // Model missing under an enabled provider.
        assert!(!agent_model_selection_is_available(&explicit("provider_a", "gpt-6"), &providers));
    }

    #[test]
    fn matches_the_provider_model_pair_never_the_bare_model_id() {
        let providers = selection_providers();
        // A role bound to provider_b must not silently rebind to the provider that does carry the model.
        assert!(!agent_model_selection_is_available(&explicit("provider_b", "gpt-4o"), &providers));
        assert!(!agent_model_selection_is_available(&explicit("provider_a", "claude-x"), &providers));
    }

    fn usable_providers() -> Vec<ApiProvider> {
        vec![provider("provider_a", &["gpt-4o"], true)]
    }

    fn bound(provider_id: &str, model_id: &str) -> AgentRole {
        AgentRole { model_selection: explicit(provider_id, model_id), ..role() }
    }

    #[test]
    fn counts_only_the_roles_the_conversation_selected() {
        let providers = usable_providers();
        let agents = [resource("agent_a"), resource("agent_b")];
        assert!(has_usable_agent_role(&agents, &strs(&["agent_a"]), &providers, None));
        assert!(has_usable_agent_role(&agents, &strs(&["agent_b"]), &providers, None));
        assert!(!has_usable_agent_role(&agents, &[], &providers, None));
        // A dangling id selects nothing the catalog lists.
        assert!(!has_usable_agent_role(&agents, &strs(&["agent_gone"]), &providers, None));
        assert!(!has_usable_agent_role(&[], &strs(&["agent_a"]), &providers, None));
    }

    #[test]
    fn ignores_a_selected_role_whose_file_could_not_be_used() {
        let providers = usable_providers();
        let mut agents = vec![
            with(resource("agent_broken"), |r| {
                r.descriptor.available = false;
                r.role = None;
                r.descriptor.description = "invalid JSON".to_owned();
            }),
            // `available` and `role` are separate fields on the wire: either alone makes the row uncallable.
            with(resource("agent_no_body"), |r| r.role = None),
            with(resource("agent_flagged"), |r| r.descriptor.available = false),
        ];
        assert!(!has_usable_agent_role(&agents, &strs(&["agent_broken", "agent_no_body", "agent_flagged"]), &providers, None));
        // One usable role among them is enough.
        agents.push(resource("agent_ok"));
        assert!(has_usable_agent_role(&agents, &strs(&["agent_broken", "agent_ok"]), &providers, None));
    }

    #[test]
    fn requires_the_roles_model_to_resolve() {
        let providers = usable_providers();
        let agents = [
            with(resource("agent_explicit"), |r| r.role = Some(bound("provider_a", "gpt-4o"))),
            with(resource("agent_dead_model"), |r| r.role = Some(bound("provider_a", "gpt-gone"))),
            with(resource("agent_dead_provider"), |r| r.role = Some(bound("provider_gone", "gpt-4o"))),
            with(resource("agent_unavailable"), |r| r.role = Some(AgentRole { model_selection: AgentModelSelection::Unavailable, ..role() })),
        ];
        assert!(has_usable_agent_role(&agents, &strs(&["agent_explicit"]), &providers, None));
        assert!(!has_usable_agent_role(&agents, &strs(&["agent_dead_model"]), &providers, None));
        assert!(!has_usable_agent_role(&agents, &strs(&["agent_dead_provider"]), &providers, None));
        assert!(!has_usable_agent_role(&agents, &strs(&["agent_unavailable"]), &providers, None));
        // The same role becomes usable the moment its provider is enabled.
        let mut more = usable_providers();
        more.push(provider("provider_gone", &["gpt-4o"], true));
        assert!(has_usable_agent_role(&agents, &strs(&["agent_dead_provider"]), &more, None));
        assert!(!has_usable_agent_role(&agents, &strs(&["agent_explicit"]), &[provider("provider_a", &["gpt-4o"], false)], None));
    }

    #[test]
    fn narrows_to_the_conversations_own_workspaces_as_the_count_does() {
        let providers = usable_providers();
        let agents = [resource("agent_global"), in_workspace("agent_here", "local|/work/a"), in_workspace("agent_elsewhere", "local|/work/c")];
        let keys = strs(&["local|/work/a"]);
        let none: Vec<String> = Vec::new();
        // The host never offers a role of a workspace the conversation doesn't have.
        assert!(!has_usable_agent_role(&agents, &strs(&["agent_elsewhere"]), &providers, Some(&keys)));
        assert!(!has_usable_agent_role(&agents, &strs(&["agent_elsewhere"]), &providers, Some(&none)));
        // Global roles are always reachable, and so is a workspace's own.
        assert!(has_usable_agent_role(&agents, &strs(&["agent_global"]), &providers, Some(&none)));
        assert!(has_usable_agent_role(&agents, &strs(&["agent_here"]), &providers, Some(&keys)));
        assert!(!has_usable_agent_role(&agents, &strs(&["agent_here"]), &providers, Some(&none)));
        assert!(has_usable_agent_role(&agents, &strs(&["agent_elsewhere", "agent_here"]), &providers, Some(&keys)));
        // A preset points at no workspace in particular, so nothing is narrowed.
        assert!(has_usable_agent_role(&agents, &strs(&["agent_elsewhere"]), &providers, None));
    }

    fn three_agents() -> Vec<AgentRoleResource> {
        vec![resource("agent_a"), resource("agent_b"), resource("agent_c")]
    }

    #[test]
    fn counts_the_selected_ids_the_catalog_lists() {
        let agents = three_agents();
        assert_eq!(selected_agent_role_count(&agents, &[], None), 0);
        assert_eq!(selected_agent_role_count(&agents, &strs(&["agent_a"]), None), 1);
        assert_eq!(selected_agent_role_count(&agents, &strs(&["agent_a", "agent_c"]), None), 2);
        assert_eq!(selected_agent_role_count(&agents, &strs(&["agent_c", "agent_b", "agent_a"]), None), 3);
    }

    #[test]
    fn does_not_count_a_dangling_id_nothing_can_call_that_role() {
        let agents = three_agents();
        assert_eq!(selected_agent_role_count(&agents, &strs(&["agent_a", "agent_gone"]), None), 1);
        assert_eq!(selected_agent_role_count(&agents, &strs(&["agent_gone"]), None), 0);
        assert_eq!(selected_agent_role_count(&[], &strs(&["agent_a"]), None), 0);
    }

    #[test]
    fn counts_a_duplicated_id_once() {
        assert_eq!(selected_agent_role_count(&three_agents(), &strs(&["agent_a", "agent_a", "agent_b"]), None), 2);
    }

    #[test]
    fn counts_a_listed_role_whose_file_is_unusable_it_is_still_a_row_the_user_can_untick() {
        let mut agents = three_agents();
        agents.push(with(resource("agent_broken"), |r| {
            r.descriptor.available = false;
            r.role = None;
        }));
        assert_eq!(selected_agent_role_count(&agents, &strs(&["agent_a", "agent_broken"]), None), 2);
    }

    #[test]
    fn narrows_to_the_conversations_own_workspaces_the_way_the_roles_page_does() {
        let scoped = [resource("agent_global"), in_workspace("agent_here", "local|/work/a"), in_workspace("agent_elsewhere", "local|/work/c")];
        let ids = strs(&["agent_global", "agent_here", "agent_elsewhere"]);
        // A role of a workspace this conversation doesn't have is dangling here.
        assert_eq!(selected_agent_role_count(&scoped, &ids, Some(&strs(&["local|/work/a"]))), 2);
        // A draft with no workspace yet reaches the global level alone.
        assert_eq!(selected_agent_role_count(&scoped, &ids, Some(&[])), 1);
        // A preset points at no workspace in particular, so nothing is narrowed.
        assert_eq!(selected_agent_role_count(&scoped, &ids, None), 3);
    }

    #[test]
    fn canonicalizes_tool_names_to_a_sorted_set_so_a_reorder_is_not_mistaken_for_an_edit() {
        assert_eq!(canonical_agent_tool_names(&strs(&["b_tool", "a_tool", "b_tool", "c_tool"])), strs(&["a_tool", "b_tool", "c_tool"]));
        assert_eq!(canonical_agent_tool_names(&[]), Vec::<String>::new());
        // Sorting the result never reorders the caller's list.
        let input = strs(&["z_tool", "a_tool"]);
        let _ = canonical_agent_tool_names(&input);
        assert_eq!(input, strs(&["z_tool", "a_tool"]));
        assert_eq!(canonical_agent_tool_names(&strs(&["b_tool", "a_tool"])), canonical_agent_tool_names(&strs(&["a_tool", "b_tool", "a_tool"])));
    }

    #[test]
    fn opens_a_new_role_on_native_search_and_fetch_with_the_shared_result_shaping() {
        let value = serde_json::to_value(default_agent_role_web_search()).unwrap();
        assert_eq!(
            value,
            json!({
                "maxSearchesPerCall": 0,
                "provider": {"kind": "native"},
                "fetchProvider": {"kind": "native"},
                "nativeSearchTool": "web_search_20250305",
                "nativeFetchTool": "web_fetch_20250910",
                "maxResults": 5,
                "compressionCutoff": 2000,
                "fetchCompressionCutoff": 2000,
                "domainFilter": "off",
                "includeDomains": [],
                "excludeDomains": [],
            })
        );
    }

    #[test]
    fn returns_a_fresh_object_every_call_so_an_edit_never_leaks_into_the_next_new_role() {
        let mut first = default_agent_role_web_search();
        let second = default_agent_role_web_search();
        first.include_domains.push("example.com".to_owned());
        first.provider = SearchProviderSelection::Unavailable;
        first.max_searches_per_call = 9;
        assert_eq!(default_agent_role_web_search(), second);
    }

    #[test]
    fn treats_identical_bodies_and_a_deep_copy_as_the_same() {
        assert!(same_agent_role(&role(), &role()));
        let original = AgentRole {
            model_selection: explicit("provider_a", "gpt-4o"),
            effort: Some(ReasoningEffort::High),
            skill_ids: strs(&["skill_a", "skill_b"]),
            ..role()
        };
        assert!(same_agent_role(&original, &clone_agent_role(&original)));
    }

    #[test]
    fn ignores_object_key_order_at_every_depth() {
        let original = AgentRole { model_selection: explicit("provider_a", "gpt-4o"), ..role() };
        // A struct has no key order, so the order this can still see is the file's, on the way in.
        let reordered = r#"{
            "toolDescriptionFileId": null, "templateId": null,
            "webSearch": {
                "excludeDomains": [], "includeDomains": [], "domainFilter": "off", "fetchCompressionCutoff": 2000,
                "compressionCutoff": 2000, "maxResults": 5, "nativeFetchTool": "web_fetch_20250910",
                "nativeSearchTool": "web_search_20250305", "fetchProvider": {"kind": "native"},
                "provider": {"kind": "native"}, "maxSearchesPerCall": 0
            },
            "hookIds": [], "mcpIds": [], "skillIds": [], "disallowedTools": [], "tools": ["read_file"], "effort": null,
            "modelSelection": {"modelId": "gpt-4o", "providerId": "provider_a", "kind": "explicit"},
            "description": "Reads a diff and says what is wrong with it.", "name": "reviewer"
        }"#;
        let reordered: AgentRole = serde_json::from_str(reordered).unwrap();
        assert!(same_agent_role(&original, &reordered));
    }

    #[test]
    fn keeps_list_order_significant_order_is_meaning_in_every_list() {
        assert!(!same_agent_role(
            &AgentRole { skill_ids: strs(&["skill_a", "skill_b"]), ..role() },
            &AgentRole { skill_ids: strs(&["skill_b", "skill_a"]), ..role() },
        ));
        let domains = |list: &[&str]| AgentRole {
            web_search: ConversationWebSearchSettings { include_domains: strs(list), ..default_agent_role_web_search() },
            ..role()
        };
        assert!(!same_agent_role(&domains(&["a.com", "b.com"]), &domains(&["b.com", "a.com"])));
    }

    #[test]
    fn detects_a_change_to_each_field() {
        let cases: Vec<(&str, Box<dyn Fn(&mut AgentRole)>)> = vec![
            ("name", Box::new(|r| r.name = "auditor".to_owned())),
            ("description", Box::new(|r| r.description = "something else".to_owned())),
            ("model selection kind", Box::new(|r| r.model_selection = AgentModelSelection::Unavailable)),
            ("model selection pair", Box::new(|r| r.model_selection = explicit("provider_a", "gpt-5"))),
            ("effort", Box::new(|r| r.effort = Some(ReasoningEffort::Low))),
            ("tool allowlist", Box::new(|r| r.tools = strs(&["read_file", "write_file"]))),
            ("tool deny list", Box::new(|r| r.disallowed_tools = strs(&["run_command"]))),
            ("skills", Box::new(|r| r.skill_ids = strs(&["skill_a"]))),
            ("MCP servers", Box::new(|r| r.mcp_ids = strs(&["mcp_a"]))),
            ("hooks", Box::new(|r| r.hook_ids = strs(&["hook_a"]))),
            ("search backend", Box::new(|r| r.web_search.provider = SearchProviderSelection::Unavailable)),
            ("search call limit", Box::new(|r| r.web_search.max_searches_per_call = 3)),
            ("domain filter", Box::new(|r| r.web_search.domain_filter = SearchDomainFilterMode::Include)),
            ("domain list", Box::new(|r| r.web_search.exclude_domains = strs(&["ads.example"]))),
            ("template", Box::new(|r| r.template_id = Some("template_a".to_owned()))),
            ("tool-description file", Box::new(|r| r.tool_description_file_id = Some("tooldesc_builtin_concise_en_us".to_owned()))),
        ];
        let base = AgentRole { model_selection: explicit("provider_a", "gpt-4o"), ..role() };
        for (label, change) in cases {
            let mut changed = base.clone();
            change(&mut changed);
            assert!(!same_agent_role(&base, &changed), "{label}");
            assert!(!same_agent_role(&changed, &base), "{label}");
        }
    }

    #[test]
    fn tells_an_absent_effort_from_a_set_one_and_an_empty_list_from_a_populated_one() {
        assert!(!same_agent_role(&AgentRole { effort: None, ..role() }, &AgentRole { effort: Some(ReasoningEffort::Medium), ..role() }));
        assert!(!same_agent_role(&AgentRole { tools: Vec::new(), ..role() }, &AgentRole { tools: strs(&["read_file"]), ..role() }));
    }

    #[test]
    fn tells_one_tool_description_file_from_another_and_following_the_caller_from_naming_the_guided_built_in() {
        let named = |id: Option<&str>| AgentRole { tool_description_file_id: id.map(str::to_owned), ..role() };
        assert!(same_agent_role(&named(Some("tooldesc_builtin_concise_en_us")), &named(Some("tooldesc_builtin_concise_en_us"))));
        assert!(!same_agent_role(&named(Some("tooldesc_builtin_concise_en_us")), &named(Some("tooldesc_user_main_0f0f0f0f"))));
        // For a role these differ: the guided id pins the default under a caller on another file.
        assert!(!same_agent_role(&named(None), &named(Some("tooldesc_builtin_en_us"))));
    }

    #[test]
    fn copies_every_nested_value_so_editing_the_copy_never_touches_the_catalog() {
        let mut original = AgentRole {
            model_selection: explicit("provider_a", "gpt-4o"),
            tools: strs(&["read_file"]),
            disallowed_tools: strs(&["run_command"]),
            skill_ids: strs(&["skill_a"]),
            mcp_ids: strs(&["mcp_a"]),
            hook_ids: strs(&["hook_a"]),
            effort: Some(ReasoningEffort::High),
            template_id: Some("template_a".to_owned()),
            tool_description_file_id: Some("tooldesc_builtin_concise_en_us".to_owned()),
            ..role()
        };
        original.web_search.include_domains = strs(&["a.example"]);
        let snapshot = original.clone();

        let mut copy = clone_agent_role(&original);

        assert_eq!(copy, original);
        assert_eq!(copy.tool_description_file_id.as_deref(), Some("tooldesc_builtin_concise_en_us"));
        copy.tools.push("write_file".to_owned());
        copy.disallowed_tools.clear();
        copy.skill_ids.push("skill_b".to_owned());
        copy.mcp_ids.push("mcp_b".to_owned());
        copy.hook_ids.push("hook_b".to_owned());
        copy.model_selection = AgentModelSelection::Inherit;
        copy.web_search.include_domains.push("b.example".to_owned());
        copy.web_search.provider = SearchProviderSelection::Unavailable;
        copy.name = "renamed".to_owned();
        assert_eq!(original, snapshot);
    }

    // Not upstream's: the shape the host sends, which TypeScript took on trust.
    #[test]
    fn reads_a_catalog_row_as_the_host_sends_it() {
        let row: AgentRoleResource = serde_json::from_value(json!({
            "id": "agent_a", "name": "a", "description": "why", "location": "/x.json", "source": "workspace",
            "available": false, "workspaceKey": "local|/work/a", "role": null,
        }))
        .unwrap();
        assert_eq!(row.descriptor.workspace_key.as_deref(), Some("local|/work/a"));
        assert_eq!(row.descriptor.source, ResourceSource::Workspace);
        assert_eq!(row.role, None);
    }
}
