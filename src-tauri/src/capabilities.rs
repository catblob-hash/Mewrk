use std::{
    collections::{HashMap, HashSet},
    fs,
    hash::{DefaultHasher, Hash, Hasher},
    path::{Path, PathBuf},
};

use serde_json::Value;

use crate::{
    mcp::RuntimeMcpServer,
    mcp_config, memory_archive_file,
    model::{
        AddedSkill, AppDocument, AttachedWorkspace, CapabilityCatalog, Conversation, HookDefinition,
        HookEvent,
        McpServerConfig, ResolvedLanguage, ResolvedSkill, ResourceDescriptor, ResourceSource,
        ToolDescriptionEntry, Workspace,
    },
    prompt_profile::{self, PromptKey, PromptProfile},
    skills,
};

const METADATA_READ_LIMIT: u64 = 64 * 1024;
pub(crate) const SKILL_READ_LIMIT: u64 = 256 * 1024;
const RUNTIME_CONTEXT_LIMIT: usize = 1024 * 1024;
/// Maximum metadata length for catalog display. Names, descriptions, authors,
/// versions, and tags share this limit.
const METADATA_VALUE_LIMIT: usize = 240;
/// Maximum trigger text supplied to the model. Trigger conditions must remain intact,
/// so this limit is larger than the catalog display limit.
const SKILL_TRIGGER_LIMIT: usize = 1024;
const CONFIG_DIRECTORY: &str = ".mewrk";
const LEGACY_CONFIG_DIRECTORY: &str = ".naiword";

/// Skill body file name. Readers accept only this exact name.
pub const SKILL_MANIFEST: &str = "SKILL.md";

/// Tool name for on-demand skill loading.
///
/// `catalog::tool_catalog` provides a localized model-facing descriptor and timeline
/// card, but this tool is derived from `skill_tool_enabled`, not user-selected. The
/// renderer mirror is `SKILL_TOOL_NAME` in `src/lib/taskTools.ts`.
pub const SKILL_TOOL: &str = "skill";

/// Derives `skill` into the enabled tools for this turn.
///
/// Remove it before adding it so stale persisted names cannot re-enable a disabled
/// setting. Expose it only when skills resolved; an empty enum would invite a call
/// guaranteed to fail.
pub fn apply_skill_tool(enabled_tools: &mut Vec<String>, resolved_skills: usize) {
    enabled_tools.retain(|name| name != SKILL_TOOL);
    if resolved_skills > 0 {
        enabled_tools.push(SKILL_TOOL.to_owned());
    }
}

/// Tool name for on-demand MCP tool loading.
///
/// Like [`SKILL_TOOL`] it has a catalog descriptor for its timeline card and
/// its localized prose, but it is never a user selection: it follows
/// `mcp_tool_discovery_enabled` and the presence of at least one withheld tool.
/// The renderer mirror is `TOOL_SEARCH_TOOL_NAME` in `src/lib/taskTools.ts`.
pub const TOOL_SEARCH_TOOL: &str = "tool_search";

/// Derives `tool_search` into the enabled tools for this turn.
///
/// Strip before adding, like [`apply_skill_tool`], so a name left in a
/// conversation's persisted list cannot re-enable a switch the user turned off.
/// Withheld tools are the whole condition: with none of them, the tool has
/// nothing to hand out and every call it invited would fail.
pub fn apply_tool_search_tool(enabled_tools: &mut Vec<String>, deferred_tools: usize) {
    enabled_tools.retain(|name| name != TOOL_SEARCH_TOOL);
    if deferred_tools > 0 {
        enabled_tools.push(TOOL_SEARCH_TOOL.to_owned());
    }
}

/// The kinds of capability a `.mewrk` directory can hold, for the commands
/// that reveal or delete one.
///
/// `ToolDescriptions` is the global `~/.mewrk/tool-descriptions` folder, so it
/// has no workspace level. The app only reveals it: it never writes or deletes
/// a tool-description file, and no deleting command accepts this kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CapabilityKind {
    Skills,
    Mcp,
    Hooks,
    Lsp,
    /// The renderer sends `"toolDescriptions"`, which `lowercase` would not spell.
    #[serde(rename = "toolDescriptions")]
    ToolDescriptions,
    /// Subagent roles, one `agents/<file>.json` each (`agent_roles`).
    Agents,
}

/// One place configuration is read from: the user's home or one workspace.
///
/// Everything under a level sits in its `.mewrk` directory (or the legacy
/// `.naiword` one when that is all there is): `skills/<dir>/SKILL.md`,
/// `mcp.json`, `hooks.json` and `agents/*.json`. Only the global level also
/// holds `tool-descriptions/*.json`: a workspace's own folder of that name is
/// never read.
#[derive(Clone, Debug)]
pub struct ConfigLevel {
    pub source: ResourceSource,
    pub base: PathBuf,
    /// Set for a workspace level: the workspace, by its machine and its
    /// registered directory. The descriptors it yields carry its
    /// [`workspace_key`].
    pub workspace: Option<AttachedWorkspace>,
    /// Set for a workspace on another machine. Its `base` names a directory
    /// over there — `/Users/me/Documents/x` on an SSH machine is not the
    /// local folder of that spelling — so it is never read from this host's
    /// filesystem: its skills, `mcp.json` and `hooks.json` are read on that
    /// machine ([`crate::remote_capabilities`]), and what they declare runs
    /// there.
    pub remote: Option<crate::remote_capabilities::RemoteLevel>,
}

/// What identifies a workspace's level wherever it is listed: its machine and
/// its registered directory, less trailing separators
/// ([`crate::run_environment::workspace_env_key`] of the trimmed path). A
/// level belongs to the directory, not to a project or a conversation, so two
/// projects or conversations that list the same directory share its entries.
/// The renderer mirror is `capabilityWorkspaceKey` in `src/lib/workspaces.ts`.
pub fn workspace_key(location: &AttachedWorkspace) -> String {
    let trimmed = location.path.trim_end_matches(['/', '\\']);
    let path = if trimmed.is_empty() { location.path.as_str() } else { trimmed };
    crate::run_environment::workspace_env_key(location.machine.as_ref(), path)
}

impl ConfigLevel {
    /// The global level, `~`; `None` when the platform has no home directory.
    pub fn user() -> Option<Self> {
        dirs::home_dir().map(|home| Self {
            source: ResourceSource::User,
            base: home,
            workspace: None,
            remote: None,
        })
    }

    /// The level of the workspace at `location`; `None` for a location with
    /// no directory and for one whose machine is no longer in the catalog,
    /// which has nowhere to be read.
    pub fn workspace(document: &AppDocument, location: &AttachedWorkspace) -> Option<Self> {
        if location.path.trim().is_empty() {
            return None;
        }
        let remote = match location.machine.as_ref() {
            None => None,
            Some(target) => Some(crate::remote_capabilities::RemoteLevel {
                // With the workspace's variables, which reach everything run
                // in it on that machine.
                runner: crate::run_environment::resolve_shell_runner(
                    &document.assets.execution_environments,
                    Some(target),
                    Some(&location.path),
                )
                .ok()?,
                machine: crate::run_environment::env_key(Some(target)),
                root: location.path.clone(),
            }),
        };
        Some(Self {
            source: ResourceSource::Workspace,
            base: PathBuf::from(&location.path),
            workspace: Some(location.clone()),
            remote,
        })
    }

    /// The level's [`workspace_key`]; `None` for the global level.
    pub fn workspace_key(&self) -> Option<String> {
        self.workspace.as_ref().map(workspace_key)
    }

    /// Whether the level is on this computer, and so read from its files.
    pub fn is_local(&self) -> bool {
        self.remote.is_none()
    }

    /// The directory or file holding one kind of capability at this level.
    pub fn path_for(&self, kind: CapabilityKind) -> PathBuf {
        preferred_config_path(&self.base, Path::new(kind.relative_path()))
    }

}

impl CapabilityKind {
    /// Where one kind lives relative to a level's config directory. Writers use
    /// it against the `.mewrk` directory rather than
    /// [`ConfigLevel::path_for`], which resolves the legacy fallback.
    pub(crate) fn relative_path(self) -> &'static str {
        match self {
            CapabilityKind::Skills => "skills",
            CapabilityKind::Mcp => "mcp.json",
            CapabilityKind::Hooks => "hooks.json",
            CapabilityKind::Lsp => "lsp.json",
            CapabilityKind::ToolDescriptions => "tool-descriptions",
            CapabilityKind::Agents => "agents",
        }
    }
}

/// Where one kind of capability lives under `base`, for callers that hold a
/// directory rather than a [`ConfigLevel`] — the language-server resolver reads
/// a workspace path straight off a tool request.
pub fn config_path_for(base: &Path, kind: CapabilityKind) -> PathBuf {
    preferred_config_path(base, Path::new(kind.relative_path()))
}

/// The spellings of one kind's file relative to a base directory, preferred
/// first, for a caller that has to test them on a machine whose filesystem it
/// cannot join paths on — the same fallback [`config_path_for`] applies here.
pub(crate) fn relative_config_paths(kind: CapabilityKind) -> [String; 2] {
    [
        format!("{CONFIG_DIRECTORY}/{}", kind.relative_path()),
        format!("{LEGACY_CONFIG_DIRECTORY}/{}", kind.relative_path()),
    ]
}

/// A project's own workspaces as registered, in order: workspace 1, then its
/// further ones. The temporary project has none: its workspace 1 is one
/// conversation's scratch folder, which holds no `.mewrk` of its own.
fn project_locations(project: &Workspace) -> Vec<AttachedWorkspace> {
    if project.kind != crate::model::WorkspaceKind::Directory || project.path.trim().is_empty() {
        return Vec::new();
    }
    std::iter::once(AttachedWorkspace {
        machine: project.machine.clone(),
        path: project.path.clone(),
    })
    .chain(project.member_workspaces().iter().cloned())
    .collect()
}

/// The global level followed by the levels of `locations`, each once.
fn levels_of(document: &AppDocument, locations: impl IntoIterator<Item = AttachedWorkspace>) -> Vec<ConfigLevel> {
    let mut seen = HashSet::new();
    ConfigLevel::user()
        .into_iter()
        .chain(
            locations
                .into_iter()
                .filter(|location| seen.insert(workspace_key(location)))
                .filter_map(|location| ConfigLevel::workspace(document, &location)),
        )
        .collect()
}

/// The global level plus every workspace of the document — each project's
/// workspaces and each conversation's attached ones — on this machine or
/// another: what the catalog shows, so a preset can select from any of them.
pub fn all_levels(document: &AppDocument) -> Vec<ConfigLevel> {
    levels_of(
        document,
        document.workspaces.iter().flat_map(|project| {
            project_locations(project).into_iter().chain(
                project
                    .conversations
                    .iter()
                    .flat_map(Conversation::effective_attached_workspaces),
            )
        }),
    )
}

/// [`all_levels`] on this computer only: what is read from local files.
fn local_levels(document: &AppDocument) -> Vec<ConfigLevel> {
    all_levels(document)
        .into_iter()
        .filter(ConfigLevel::is_local)
        .collect()
}

/// The workspaces of `conversation` as its runs number them, by registered
/// location: its project's workspace 1 and further workspaces, then its own
/// attached ones. `None` stands for a workspace with no directory to read —
/// the temporary project's scratch folder — which still takes its number.
pub(crate) fn conversation_locations(
    document: &AppDocument,
    conversation: &Conversation,
) -> Vec<Option<AttachedWorkspace>> {
    let Some(project) = document.workspaces.iter().find(|project| {
        project
            .conversations
            .iter()
            .any(|candidate| candidate.id == conversation.id)
    }) else {
        return Vec::new();
    };
    let own = project_locations(project);
    let primary = own.first().cloned();
    std::iter::once(primary)
        .chain(
            project
                .conversation_workspaces_after_primary(conversation)
                .into_iter()
                .map(Some),
        )
        .collect()
}

/// The global level plus the level of every workspace of `conversation` —
/// what a run of that conversation may use: the union of its workspaces'
/// skills, MCP servers and hooks. A conversation nobody owns (which the
/// trusted request path rejects before getting here) sees the global level
/// only.
pub fn levels_for_conversation(
    document: &AppDocument,
    conversation: &Conversation,
) -> Vec<ConfigLevel> {
    levels_of(
        document,
        conversation_locations(document, conversation).into_iter().flatten(),
    )
}

/// Discovers the current capability catalog: skills, MCP servers, hooks and
/// subagent roles from `~/.mewrk` and every workspace's `.mewrk`, plus the
/// global tool-description files, the built-in prompt profile and the built-in
/// roles. The app itself goes through [`discover_document`], which also hands
/// back what the role registry is refreshed from.
#[cfg(test)]
pub fn discover(document: &AppDocument, _app_data: &Path) -> CapabilityCatalog {
    discover_document(document).catalog
}

/// [`discover`] with the executable definitions behind the rows: the role
/// files it read are what the host's role registry is refreshed from.
pub(crate) fn discover_document(document: &AppDocument) -> DiscoveredCapabilities {
    let mut discovered = discover_levels(
        &all_levels(document),
        document.global_settings.resolved_app_language,
    );
    discovered.catalog.tool_description_files = discover_tool_description_files();
    // The built-in roles are not a level either; like the built-in skill they
    // are listed first, whatever levels the scan read.
    let mut agents = crate::agent_roles::builtin_descriptors(document);
    agents.append(&mut discovered.catalog.agents);
    discovered.catalog.agents = agents;
    discovered
}

/// A cheap summary of everything [`discover`] would read: every configuration
/// file's and skill folder's name, size and modification time at every level,
/// and the tool-description files' at the global level alone.
///
/// The settings pane asks for it every few seconds while it is open and
/// rescans only when it changes, so a saved `mcp.json` or a new skill folder
/// shows up by itself without the pane re-reading every manifest on a timer.
///
/// A level on another machine has no modification times to stat from here, so
/// it contributes a digest of what its probe last brought back, probed again
/// at most every [`REMOTE_FINGERPRINT_TTL`].
pub fn fingerprint(document: &AppDocument) -> String {
    let mut hasher = DefaultHasher::new();
    let stamp = |hasher: &mut DefaultHasher, path: &Path| {
        path.hash(hasher);
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                metadata.is_dir().hash(hasher);
                metadata.len().hash(hasher);
                metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|time| time.as_nanos())
                    .hash(hasher);
            }
            Err(_) => "absent".hash(hasher),
        }
    };
    for level in all_levels(document) {
        if let Some(remote) = &level.remote {
            remote_fingerprint(remote).hash(&mut hasher);
            continue;
        }
        for directory in [CONFIG_DIRECTORY, LEGACY_CONFIG_DIRECTORY] {
            let config = level.base.join(directory);
            for kind in [CapabilityKind::Mcp, CapabilityKind::Hooks, CapabilityKind::Lsp] {
                stamp(&mut hasher, &config.join(kind.relative_path()));
            }
            // Tool-description files are read at the global level only, so a
            // workspace's own folder of that name changes nothing to rescan.
            // Role files live at every level.
            let folders: &[(&str, Option<&str>)] = if level.source == ResourceSource::User {
                &[("skills", Some(SKILL_MANIFEST)), ("tool-descriptions", None), ("agents", None)]
            } else {
                &[("skills", Some(SKILL_MANIFEST)), ("agents", None)]
            };
            for &(folder, manifest) in folders {
                let root = config.join(folder);
                stamp(&mut hasher, &root);
                let mut entries = fs::read_dir(&root)
                    .map(|entries| entries.flatten().map(|entry| entry.path()).collect::<Vec<_>>())
                    .unwrap_or_default();
                entries.sort();
                for entry in entries {
                    stamp(&mut hasher, &entry);
                    if let Some(manifest) = manifest {
                        stamp(&mut hasher, &entry.join(manifest));
                    }
                }
            }
        }
    }
    format!("{:016x}", hasher.finish())
}

/// How long a level on another machine keeps the digest its last probe gave.
/// Each probe is a round trip to the machine, and the pane polls every two
/// seconds.
const REMOTE_FINGERPRINT_TTL: std::time::Duration = std::time::Duration::from_secs(10);

/// A digest of what a level on another machine holds: its probe's answer,
/// reused for [`REMOTE_FINGERPRINT_TTL`]. A machine that does not answer
/// digests as its error, which changes nothing until it answers again.
fn remote_fingerprint(level: &crate::remote_capabilities::RemoteLevel) -> u64 {
    type Cache = std::sync::Mutex<HashMap<String, (std::time::Instant, u64)>>;
    static CACHE: std::sync::OnceLock<Cache> = std::sync::OnceLock::new();
    let key = format!("{}\u{0}{}\u{0}{}", level.machine, level.root, level.runner.fingerprint());
    let cache = CACHE.get_or_init(Default::default);
    if let Some((at, digest)) = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&key)
        .copied()
    {
        if at.elapsed() < REMOTE_FINGERPRINT_TTL {
            return digest;
        }
    }
    let present = crate::remote_link::machine_is_there(
        &level.runner,
        crate::remote_capabilities::SCAN_PRESENCE_CHECK,
    );
    let mut hasher = DefaultHasher::new();
    if present {
        match crate::remote_capabilities::read(level, crate::remote_capabilities::SCAN_TIMEOUT) {
            Ok(files) => format!("{files:?}").hash(&mut hasher),
            Err(error) => error.hash(&mut hasher),
        }
    } else {
        "away".hash(&mut hasher);
    }
    let digest = hasher.finish();
    cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(key, (std::time::Instant::now(), digest));
    digest
}

/// What a scan of some levels found: the catalog rows, and the executable
/// definitions behind the hook and MCP rows that are available.
pub(crate) struct DiscoveredCapabilities {
    pub catalog: CapabilityCatalog,
    pub hooks: HashMap<String, HookDefinition>,
    pub mcp_servers: HashMap<String, McpServerConfig>,
    /// Skills read off another machine, by id: the manifest's text (or why it
    /// cannot be used) and the folder it is in there. A local skill is read
    /// from its own location when a run needs it.
    pub remote_skills: HashMap<String, RemoteSkill>,
    /// Levels on another machine that could not be read, by [`workspace_key`],
    /// with the reason: a selection that only they could have resolved fails
    /// with it rather than as dangling.
    pub level_errors: Vec<(String, String)>,
    /// The available role files, as the runtime resolves them, and the levels
    /// whose `agents/` was read — what the host's role registry is refreshed
    /// from (`AgentRoleRegistry::replace_levels`).
    pub agent_roles: Vec<crate::agent_roles::RegisteredAgentRole>,
    pub role_levels: Vec<crate::agent_roles::RoleLevel>,
}

/// What scanning the `agents/` directories of some levels found.
#[derive(Default)]
pub(crate) struct AgentRoleScan {
    /// Every file's row, usable or not, sorted like the other kinds.
    pub rows: Vec<crate::agent_roles::AgentRoleDescriptor>,
    /// The usable ones, as the runtime resolves them.
    pub roles: Vec<crate::agent_roles::RegisteredAgentRole>,
    /// The levels that were read.
    pub levels: Vec<crate::agent_roles::RoleLevel>,
    /// The ids listed so far. Two files can mint one id — `Reviewer.json` and
    /// `reviewer.json` side by side on a machine whose file names keep case,
    /// since an id folds case — and an id must name one role, so the first
    /// is listed and the rest are passed over.
    ids: HashSet<String>,
}

/// Adds one level's role files to `scan`: read from this computer's files for
/// a local level, from what its probe brought back (`files`) for a level on
/// another machine. A level on another machine that was not read adds
/// nothing, not even itself to the levels read.
fn scan_level_roles(
    level: &ConfigLevel,
    files: Option<&crate::remote_capabilities::LevelFiles>,
    seen: &mut HashSet<String>,
    scan: &mut AgentRoleScan,
) {
    let level_key = level.workspace_key();
    let rows = match (&level.remote, files) {
        (None, _) => crate::agent_roles::discover_in_root(
            &level.path_for(CapabilityKind::Agents),
            level.source,
            level_key.as_deref(),
        )
        .into_iter()
        .filter(|row| seen.insert(seen_key(None, &row.descriptor.location)))
        .collect::<Vec<_>>(),
        (Some(remote), Some(files)) => files
            .agents
            .iter()
            .filter_map(|file| {
                crate::agent_roles::descriptor_from_remote(file, level.source, level_key.as_deref())
            })
            .filter(|row| seen.insert(seen_key(Some(&remote.machine), &row.descriptor.location)))
            .map(|mut row| {
                row.descriptor.id = on_machine_id(&row.descriptor.id, &remote.machine);
                row
            })
            .collect(),
        (Some(_), None) => return,
    };
    let rows = rows
        .into_iter()
        .filter(|row| {
            let first = scan.ids.insert(row.descriptor.id.clone());
            if !first {
                eprintln!(
                    "Role file {} has the id of a role file already listed ({}); it is passed over. Rename one of them.",
                    row.descriptor.location, row.descriptor.id
                );
            }
            first
        })
        .collect::<Vec<_>>();
    let role_level = crate::agent_roles::RoleLevel::of_workspace_key(level_key.as_deref());
    for row in &rows {
        if let Some(role) = &row.role {
            scan.roles.push(crate::agent_roles::RegisteredAgentRole {
                id: row.descriptor.id.clone(),
                level: role_level.clone(),
                definition: crate::agent_roles::registered_definition(role, &role_level, &row.descriptor.id),
            });
        }
    }
    scan.rows.extend(rows);
    scan.levels.push(role_level);
}

/// The role files of `levels`, freshly read — those on other machines there,
/// not waiting on a machine that does not answer at once.
pub(crate) fn scan_agent_roles(levels: &[ConfigLevel]) -> AgentRoleScan {
    let remote = read_remote_levels(levels, crate::remote_capabilities::SCAN_TIMEOUT, true);
    let mut seen = HashSet::new();
    let mut scan = AgentRoleScan::default();
    for level in levels {
        let files = level
            .workspace_key()
            .and_then(|key| remote.get(&key))
            .and_then(|read| read.as_ref().ok());
        scan_level_roles(level, files, &mut seen, &mut scan);
    }
    scan.rows
        .sort_by(|left, right| resource_sort(&left.descriptor, &right.descriptor));
    scan
}

/// The level among `levels` a workspace key names; `None` is the global one.
pub(crate) fn level_of<'a>(
    levels: &'a [ConfigLevel],
    workspace_key: Option<&str>,
) -> Result<&'a ConfigLevel, String> {
    match workspace_key {
        None => levels.iter().find(|level| level.workspace.is_none()).ok_or_else(|| {
            crate::ui_text::pick("无法确定用户主目录", "Could not find the home folder").to_owned()
        }),
        Some(key) => levels
            .iter()
            .find(|level| level.workspace_key().as_deref() == Some(key))
            .ok_or_else(|| {
                crate::ui_text::pick(
                    "这个工作区没有可写入的目录",
                    "This workspace has no folder to write to",
                )
                .to_owned()
            }),
    }
}

/// A skill found on another machine.
#[derive(Clone, Debug)]
pub(crate) struct RemoteSkill {
    pub text: Result<String, String>,
    pub directory: String,
}

/// What reading the levels on other machines found, by [`workspace_key`].
pub(crate) type RemoteReads =
    HashMap<String, Result<crate::remote_capabilities::LevelFiles, String>>;

/// Reads every level on another machine among `levels`, at once, each within
/// `timeout`. With `check_presence`, a machine that does not answer a quick
/// check is not waited on at all — the settings pane's scan, which must not
/// hang on a machine that is switched off.
pub(crate) fn read_remote_levels(
    levels: &[ConfigLevel],
    timeout: std::time::Duration,
    check_presence: bool,
) -> RemoteReads {
    // Each machine is read on a thread of its own, which logs in for whoever
    // this thread logs in for: a scan nobody is waiting on asks nobody.
    let attendance = crate::ssh_askpass::attendance();
    let workers = levels
        .iter()
        .filter_map(|level| {
            let remote = level.remote.clone()?;
            let workspace_key = level.workspace_key()?;
            Some(std::thread::spawn(move || {
                let _asking = crate::ssh_askpass::carry(attendance);
                let present = !check_presence
                    || crate::remote_link::machine_is_there(
                        &remote.runner,
                        crate::remote_capabilities::SCAN_PRESENCE_CHECK,
                    );
                let read = if present {
                    crate::remote_capabilities::read(&remote, timeout)
                } else {
                    Err(crate::ui_text::ui_text!(
                        "工作区 {} 所在的机器没有响应，暂时读不到它的 .mewrk",
                        "The machine workspace {} is on did not answer, so its .mewrk cannot be read now",
                        remote.root
                    ))
                };
                (workspace_key, read)
            }))
        })
        .collect::<Vec<_>>();
    workers
        .into_iter()
        .filter_map(|worker| worker.join().ok())
        .collect()
}

/// Scans `levels` in order, reading those on other machines there. The same
/// location reached through two levels (a workspace that is the home
/// directory) is listed once, under the first.
pub(crate) fn discover_levels(
    levels: &[ConfigLevel],
    language: ResolvedLanguage,
) -> DiscoveredCapabilities {
    let remote = read_remote_levels(levels, crate::remote_capabilities::SCAN_TIMEOUT, true);
    discover_levels_with(levels, language, &remote)
}

/// [`discover_levels`] with the levels on other machines already read — by a
/// caller that read them without holding a lock it would otherwise hold for
/// as long as a machine takes to answer.
pub(crate) fn discover_levels_with(
    levels: &[ConfigLevel],
    language: ResolvedLanguage,
    remote_reads: &RemoteReads,
) -> DiscoveredCapabilities {
    let mut seen: HashSet<String> = HashSet::new();
    let mut skills_out = Vec::new();
    let mut mcps = Vec::new();
    let mut lsps = Vec::new();
    let mut hooks = Vec::new();
    let mut hook_definitions = HashMap::new();
    let mut mcp_servers = HashMap::new();
    let mut remote_skills = HashMap::new();
    let mut level_errors = Vec::new();
    let mut roles = AgentRoleScan::default();
    for level in levels {
        let level_key = level.workspace_key();
        let Some(remote) = &level.remote else {
            scan_level_roles(level, None, &mut seen, &mut roles);
            for descriptor in skills::discover_in_root(
                &level.path_for(CapabilityKind::Skills),
                level.source,
                level_key.as_deref(),
            ) {
                if seen.insert(seen_key(None, &descriptor.location)) {
                    skills_out.push(descriptor);
                }
            }
            for entry in mcp_config::read_file(
                &level.path_for(CapabilityKind::Mcp),
                level.source,
                level_key.as_deref(),
            ) {
                if seen.insert(seen_key(None, &entry.descriptor.location)) {
                    if let Some(config) = entry.config {
                        mcp_servers.insert(entry.descriptor.id.clone(), config);
                    }
                    mcps.push(entry.descriptor);
                }
            }
            for entry in read_hooks_file(
                &level.path_for(CapabilityKind::Hooks),
                level.source,
                level_key.as_deref(),
            ) {
                if seen.insert(seen_key(None, &entry.descriptor.location)) {
                    hook_definitions.insert(entry.descriptor.id.clone(), entry.definition);
                    hooks.push(entry.descriptor);
                }
            }
            for entry in crate::lsp_config::read_file(
                &level.path_for(CapabilityKind::Lsp),
                level.source,
                level_key.as_deref(),
            ) {
                if seen.insert(seen_key(None, &entry.descriptor.location)) {
                    lsps.push(entry.descriptor);
                }
            }
            continue;
        };
        // A level on another machine: what its probe brought back, parsed the
        // way a local level's files are. Its language servers are read there
        // when a call needs one (`remote_lsp`), so they are not listed here.
        let Some(workspace_key) = level_key.as_deref() else {
            continue;
        };
        let files = match remote_reads.get(workspace_key) {
            Some(Ok(files)) => files,
            Some(Err(error)) => {
                level_errors.push((workspace_key.to_owned(), error.clone()));
                continue;
            }
            None => continue,
        };
        scan_level_roles(level, Some(files), &mut seen, &mut roles);
        let machine = remote.machine.as_str();
        for file in &files.skills {
            let Some(mut descriptor) =
                skills::descriptor_from_remote(file, level.source, Some(workspace_key))
            else {
                continue;
            };
            if !seen.insert(seen_key(Some(machine), &descriptor.location)) {
                continue;
            }
            descriptor.id = on_machine_id(&descriptor.id, machine);
            remote_skills.insert(
                descriptor.id.clone(),
                RemoteSkill {
                    text: skills::remote_manifest_text(file),
                    directory: remote_parent(&file.manifest),
                },
            );
            skills_out.push(descriptor);
        }
        // `${VAR}` in that machine's `mcp.json` means that machine's
        // variable, and the workspace's own variables come on top, as they do
        // for everything run there.
        let env = |name: &str| {
            remote
                .runner
                .env()
                .get(name)
                .or_else(|| files.env.get(name))
                .cloned()
        };
        if let Some(file) = &files.mcp {
            for mut entry in mcp_config::parse_contents(
                &file.bytes,
                Path::new(&file.path),
                level.source,
                Some(workspace_key),
                &env,
                Some(remote),
            ) {
                if !seen.insert(seen_key(Some(machine), &entry.descriptor.location)) {
                    continue;
                }
                entry.descriptor.id = on_machine_id(&entry.descriptor.id, machine);
                if let Some(mut config) = entry.config {
                    config.id = entry.descriptor.id.clone();
                    mcp_servers.insert(entry.descriptor.id.clone(), config);
                }
                mcps.push(entry.descriptor);
            }
        }
        if let Some(file) = &files.hooks {
            for mut entry in parse_hooks(
                &file.bytes,
                Path::new(&file.path),
                level.source,
                Some(workspace_key),
            ) {
                if !seen.insert(seen_key(Some(machine), &entry.descriptor.location)) {
                    continue;
                }
                entry.descriptor.id = on_machine_id(&entry.descriptor.id, machine);
                entry.definition.id = entry.descriptor.id.clone();
                // It runs where it was declared, in the workspace folder; a run
                // on a worktree of it moves it there (`relocate_remote_capabilities`).
                entry.definition.on_machine = Some(crate::remote_capabilities::HookPlace {
                    runner: remote.runner.clone(),
                    cwd: remote.root.clone(),
                    windows: files.windows,
                });
                hook_definitions.insert(entry.descriptor.id.clone(), entry.definition);
                hooks.push(entry.descriptor);
            }
        }
    }
    // The built-in presets are not a level: they have no file and no workspace,
    // and they are listed last because any `lsp.json` entry of the same name
    // replaces them.
    for entry in crate::lsp_config::builtin_entries(language) {
        if seen.insert(seen_key(None, &entry.descriptor.location))
            && !lsps
                .iter()
                .any(|existing| existing.name == entry.descriptor.name)
        {
            lsps.push(entry.descriptor);
        }
    }
    skills_out.sort_by(resource_sort);
    mcps.sort_by(resource_sort);
    hooks.sort_by(resource_sort);
    roles
        .rows
        .sort_by(|left, right| resource_sort(&left.descriptor, &right.descriptor));
    // The built-in skill is not a level either. Every scan lists it, whatever
    // levels it read, and lists it first, like the built-in prompt profile.
    let mut skills = skills::builtin_skills(language);
    skills.extend(skills_out);
    DiscoveredCapabilities {
        catalog: CapabilityCatalog {
            hooks,
            skills,
            mcps,
            lsps,
            tool_description_files: Vec::new(),
            agents: roles.rows,
            unreadable_levels: level_errors
                .iter()
                .map(|(workspace_key, message)| crate::model::UnreadableLevel {
                    workspace_key: workspace_key.clone(),
                    message: message.clone(),
                })
                .collect(),
        },
        hooks: hook_definitions,
        mcp_servers,
        remote_skills,
        level_errors,
        agent_roles: roles.roles,
        role_levels: roles.levels,
    }
}

/// What makes two rows the same entry: where it was read, on which machine.
fn seen_key(machine: Option<&str>, location: &str) -> String {
    format!("{}\u{0}{location}", machine.unwrap_or("local"))
}

/// An id minted from a location on another machine, made that machine's: the
/// same path on two machines is two entries, and neither may be mistaken for a
/// local file of that spelling.
pub(crate) fn on_machine_id(id: &str, machine: &str) -> String {
    let (head, _) = id.rsplit_once('_').unwrap_or((id, ""));
    format!(
        "{head}_{:08x}",
        location_id_hash(&format!("{machine}\n{}", normalized_location_for_id(id)))
    )
}

/// The folder a path on another machine is in, whichever separator it uses.
fn remote_parent(path: &str) -> String {
    match path.rfind(['/', '\\']) {
        Some(0) => "/".to_owned(),
        Some(index) => path[..index].to_owned(),
        None => String::new(),
    }
}

/// Scans tool-description files separately because [`resolve_prompt_profile`]
/// needs only these files and must not require a skills root.
///
/// Only the global folder is read: `~/.mewrk/tool-descriptions`, or the legacy
/// `.naiword` one when `.mewrk` lacks it. A workspace's own
/// `.mewrk/tool-descriptions` is never scanned, so a project cannot change what
/// the model is told a tool is. A file selected from a workspace by an older
/// version now dangles, which [`resolve_prompt_profile`] resolves to the
/// built-in profile.
///
/// The built-in profile comes first and is always present: it is the default
/// every conversation renders with until it selects something else, and it has
/// no location a user could delete.
fn discover_tool_description_files() -> Vec<ResourceDescriptor> {
    tool_description_files_under(ConfigLevel::user().as_ref().map(|level| level.base.as_path()))
}

/// [`discover_tool_description_files`] for the global level at `home`.
fn tool_description_files_under(home: Option<&Path>) -> Vec<ResourceDescriptor> {
    let mut files: Vec<ResourceDescriptor> = Vec::new();
    if let Some(home) = home {
        scan_tool_description_root(
            &preferred_config_path(home, Path::new(CapabilityKind::ToolDescriptions.relative_path())),
            &mut files,
        );
    }
    files.sort_by(resource_sort);
    let mut catalog = vec![builtin_prompt_profile_descriptor()];
    catalog.extend(files);
    catalog
}

/// The built-in profile as a catalog entry. `location` is a `builtin:`
/// pseudo-location so the renderer can tell it from the files a user added.
fn builtin_prompt_profile_descriptor() -> ResourceDescriptor {
    let profile = PromptProfile::builtin_english();
    ResourceDescriptor {
        id: profile.id,
        name: profile.name,
        description: "Built-in prompts and tool descriptions; ships with this version of Mewrk"
            .to_owned(),
        location: "builtin:en-US".to_owned(),
        source: ResourceSource::Builtin,
        available: true,
        workspace_key: None,
    }
}

/// Runtime context for this turn: the system-prompt addendum, the skills
/// supplied to the `skill` tool, the skills that arrive as their own system
/// message, and the MCP servers to dial.
///
/// `skill_tool_enabled` decides the FORM the selected skills take — bodies when
/// it is off, a name-and-trigger listing when it is on — and the conversation's
/// tool lock decides the ROUTE. Skills the opening prompt was built with stay in
/// `addendum`; anything selected after that goes in `added_skills`, because the
/// prompt the earlier rounds were answered against cannot be rewritten under
/// them. MCP and hooks always enter `addendum`.
#[derive(Debug)]
pub struct RuntimeContext {
    pub addendum: String,
    /// Non-empty only when the skill tool is enabled — every selected skill,
    /// wherever its listing went, so the tool can serve any of them. Otherwise
    /// skill bodies are in `addendum` or `added_skills`; using both outputs
    /// would duplicate them.
    pub skills: Vec<ResolvedSkill>,
    /// Skills selected after the opening prompt, already rendered for delivery
    /// as system messages.
    pub added_skills: Vec<AddedSkill>,
    /// The selected, available servers, in selection order. Host-owned: the
    /// renderer's `mcp_ids` only ever pick from what discovery read off disk.
    pub mcp_servers: Vec<RuntimeMcpServer>,
    /// The selected hooks, in selection order, with their executable commands.
    /// Never from the renderer or the persisted document: only from the files.
    pub hooks: Vec<HookDefinition>,
    /// The "Selected MCP servers" section as `addendum` carries it, so a server
    /// that cannot be used this turn can be taken out of the prompt again.
    pub mcp_section: McpPromptSection,
    /// The own hooks of each of the conversation's roles that chooses them,
    /// by role name, for the turn's hook confirmation to list: a child
    /// spawned this turn runs the run's own guards, which the confirmation
    /// lists anyway, and of its role's own hooks only those it allowed
    /// (`api::apply_role_capabilities`). A role whose hooks cannot be
    /// resolved is left out; its spawn fails with the reason.
    pub role_hooks: std::collections::BTreeMap<String, Vec<HookDefinition>>,
    /// The role files the scan of this conversation's levels read, and those
    /// levels, for the host's role registry: a run's children resolve their
    /// roles against the files as this run found them.
    pub agent_roles: Vec<crate::agent_roles::RegisteredAgentRole>,
    pub role_levels: Vec<crate::agent_roles::RoleLevel>,
}

/// What a run hands its children for resolving a role's own skills, MCP
/// servers and hooks when one is spawned (`api::apply_role_capabilities`).
///
/// A role is resolved at its spawn, from the files and the role as they are
/// then: its settings take no part in the caller's prompt cache, so a change
/// applies to the next child. The caller's half is fixed for the turn, as the
/// caller's own run is.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RoleBasis {
    /// The run's environment block, which a role's prompt opens with as the
    /// run's own does.
    pub environment: String,
    /// The conversation's selections the run was resolved from, which a role
    /// takes for any kind it leaves to its caller: the ones the caller runs
    /// with.
    pub skill_ids: Vec<String>,
    pub mcp_ids: Vec<String>,
    pub hook_ids: Vec<String>,
    /// How the conversation delivers skills. A role's are delivered the same
    /// way.
    pub skill_tool: bool,
    /// [`RuntimeContext::role_hooks`]: what the turn's hook confirmation
    /// listed for the roles.
    pub confirmed_role_hooks: std::collections::BTreeMap<String, Vec<HookDefinition>>,
}

/// One subagent role's skills, MCP servers and hooks: its own for each kind
/// it chooses, its caller's for the rest, so the prompt describes the child's
/// whole set.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RoleCapabilities {
    /// The capability sections of the child's system prompt.
    pub addendum: String,
    /// As [`RuntimeContext::skills`]: filled only when skills load on demand.
    pub skills: Vec<ResolvedSkill>,
    pub mcp_servers: Vec<RuntimeMcpServer>,
    pub mcp_section: McpPromptSection,
    pub hooks: Vec<HookDefinition>,
}

/// Whether a role chooses any of its skills, MCP servers or hooks itself.
pub fn role_chooses_capabilities(definition: &crate::model::AgentDefinition) -> bool {
    definition.skill_ids.is_some() || definition.mcp_ids.is_some() || definition.hook_ids.is_some()
}

/// What one run's capabilities are resolved from: the selected ids, how
/// skills are delivered, and which skills its opening prompt was built with
/// (`None`: every selected one).
struct CapabilitySelection<'a> {
    skill_ids: &'a [String],
    mcp_ids: &'a [String],
    hook_ids: &'a [String],
    via_tool: bool,
    prompt_skill_ids: Option<&'a [String]>,
}

impl<'a> CapabilitySelection<'a> {
    /// A role's: its own list for each kind it chooses, the caller's for the
    /// rest, delivered the way the caller delivers skills. A child opens a
    /// prompt of its own, so every skill belongs to it.
    fn of_role(basis: &'a RoleBasis, definition: &'a crate::model::AgentDefinition) -> Self {
        Self {
            skill_ids: definition.skill_ids.as_deref().unwrap_or(&basis.skill_ids),
            mcp_ids: definition.mcp_ids.as_deref().unwrap_or(&basis.mcp_ids),
            hook_ids: definition.hook_ids.as_deref().unwrap_or(&basis.hook_ids),
            via_tool: basis.skill_tool,
            prompt_skill_ids: None,
        }
    }

    fn of_conversation(conversation: &'a Conversation) -> Self {
        let settings = &conversation.settings;
        Self {
            skill_ids: &settings.skill_ids,
            mcp_ids: &settings.mcp_ids,
            hook_ids: &settings.hook_ids,
            via_tool: settings.skill_tool_enabled,
            prompt_skill_ids: settings
                .tool_lock
                .as_ref()
                .and_then(|lock| lock.prompt_skill_ids.as_deref()),
        }
    }

}

/// The system prompt's list of selected MCP servers: the section as rendered
/// into the prompt, and the row each server contributed to it.
///
/// The list is written before any server is dialed. One that then fails to
/// connect or is dropped for a limit has no tools this turn, and the prompt
/// must not go on presenting it as selected and usable; [`Self::without`]
/// rewrites the section without it. The tool list already changed with it, so
/// rewriting the prompt costs no cache the drop had not already cost.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct McpPromptSection {
    /// The whole section exactly as it appears in the prompt; empty when no
    /// server was selected.
    pub rendered: String,
    /// `(server id, row)` for each server listed, in order.
    pub rows: Vec<(String, String)>,
}

impl McpPromptSection {
    /// `prompt` with the servers in `dropped` left out of this section, and the
    /// section (with the separator that joined it) gone when none is left.
    pub fn without(
        &self,
        prompt: &str,
        dropped: &HashSet<String>,
        profile: &PromptProfile,
    ) -> String {
        if self.rendered.is_empty() || !self.rows.iter().any(|(id, _)| dropped.contains(id)) {
            return prompt.to_owned();
        }
        let remaining = self
            .rows
            .iter()
            .filter(|(id, _)| !dropped.contains(id))
            .map(|(_, row)| row.as_str())
            .collect::<Vec<_>>();
        if !remaining.is_empty() {
            let section =
                profile.render(PromptKey::SystemMcpSection, &[("servers", &remaining.join("\n"))]);
            return prompt.replacen(&self.rendered, &section, 1);
        }
        const SEPARATOR: &str = "\n\n---\n\n";
        for spelled in [
            format!("{SEPARATOR}{}", self.rendered),
            format!("{}{SEPARATOR}", self.rendered),
            self.rendered.clone(),
        ] {
            if prompt.contains(&spelled) {
                return prompt.replacen(&spelled, "", 1);
            }
        }
        prompt.to_owned()
    }
}

/// Resolve the persisted conversation's selected capability presets into model instructions.
/// Renderer-provided resource text is never trusted: every descriptor is rediscovered from the
/// current filesystem/configuration and skill bodies are read with strict size limits.
///
/// Discovery is scoped to the global level and the conversation's own
/// workspace, so a run can never read another project's skill or launch its
/// MCP server. `profile` supplies the wording of the MCP and hook sections;
/// skill bodies are user-authored and enter verbatim.
#[cfg(test)]
pub fn runtime_context(
    document: &AppDocument,
    conversation: &Conversation,
    profile: &PromptProfile,
) -> Result<RuntimeContext, String> {
    let remote = read_remote_levels(
        &levels_for_conversation(document, conversation),
        crate::remote_capabilities::RUN_TIMEOUT,
        false,
    );
    runtime_context_with(document, conversation, profile, &remote)
}

/// [`runtime_context`] with the conversation's level on another machine, if it
/// has one, already read (`read_remote_levels` over
/// [`levels_for_conversation`]).
pub(crate) fn runtime_context_with(
    document: &AppDocument,
    conversation: &Conversation,
    profile: &PromptProfile,
    remote_reads: &RemoteReads,
) -> Result<RuntimeContext, String> {
    let discovered = discover_levels_with(
        &levels_for_conversation(document, conversation),
        document.global_settings.resolved_app_language,
        remote_reads,
    );
    let places = WorkspacePlaces::of(document, conversation);
    let mut context = runtime_context_from_discovery(conversation, &discovered, profile, &places)?;
    context.role_hooks = role_hooks(conversation, &discovered, profile, &places);
    context.agent_roles = discovered.agent_roles;
    context.role_levels = discovered.role_levels;
    Ok(context)
}

/// The own hooks of every role `conversation` selects, resolved from
/// `discovered`, by role name. A built-in role has none; where two selected
/// roles share a name, the one a spawn resolves to — the higher source — is
/// the one listed.
fn role_hooks(
    conversation: &Conversation,
    discovered: &DiscoveredCapabilities,
    profile: &PromptProfile,
    places: &WorkspacePlaces,
) -> std::collections::BTreeMap<String, Vec<HookDefinition>> {
    let precedence = |source: crate::model::AgentDefinitionSource| match source {
        crate::model::AgentDefinitionSource::Managed => 4,
        crate::model::AgentDefinitionSource::Project => 3,
        crate::model::AgentDefinitionSource::User => 2,
        crate::model::AgentDefinitionSource::Plugin => 1,
    };
    let mut chosen = std::collections::BTreeMap::<String, &crate::model::AgentDefinition>::new();
    for id in &conversation.settings.agent_ids {
        let Some(role) = discovered.agent_roles.iter().find(|role| &role.id == id) else {
            continue;
        };
        let definition = &role.definition;
        match chosen.get(&definition.name) {
            Some(existing) if precedence(existing.source) >= precedence(definition.source) => {}
            _ => {
                chosen.insert(definition.name.clone(), definition);
            }
        }
    }
    let mut roles = std::collections::BTreeMap::new();
    for (name, definition) in chosen {
        // A role with no hooks of its own has nothing for the turn to confirm.
        let Some(hook_ids) = definition.hook_ids.as_deref().filter(|ids| !ids.is_empty()) else {
            continue;
        };
        let selection = CapabilitySelection {
            skill_ids: &[],
            mcp_ids: &[],
            hook_ids,
            via_tool: false,
            prompt_skill_ids: None,
        };
        if let Ok(context) = context_for_selection(&selection, discovered, profile, places) {
            roles.insert(name, context.hooks);
        }
    }
    roles
}

#[cfg(test)]
thread_local! {
    /// How many times [`resolve_role`] read the levels' files on this thread,
    /// for the test that a role selecting nothing reads none.
    static ROLE_RESOLUTION_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// `definition`'s skills, MCP servers and hooks, resolved now from the files
/// of `conversation`'s levels: its own for each kind it chooses, `basis`'s for
/// the rest. Reads the levels on other machines, so it waits on them — unless
/// there is nothing to resolve: a selection of no skill, server or hook is
/// none of any, and needs no file read to say so. Every built-in role, and
/// every role file that selects none of its own, is that selection.
pub(crate) fn resolve_role(
    document: &AppDocument,
    conversation: &Conversation,
    basis: &RoleBasis,
    definition: &crate::model::AgentDefinition,
    profile: &PromptProfile,
) -> Result<RoleCapabilities, String> {
    let selection = CapabilitySelection::of_role(basis, definition);
    if selection.skill_ids.is_empty()
        && selection.mcp_ids.is_empty()
        && selection.hook_ids.is_empty()
    {
        return Ok(RoleCapabilities::default());
    }
    #[cfg(test)]
    ROLE_RESOLUTION_READS.with(|reads| reads.set(reads.get() + 1));
    let levels = levels_for_conversation(document, conversation);
    let remote = read_remote_levels(&levels, crate::remote_capabilities::RUN_TIMEOUT, false);
    let discovered =
        discover_levels_with(&levels, document.global_settings.resolved_app_language, &remote);
    let places = WorkspacePlaces::of(document, conversation);
    context_for_selection(&selection, &discovered, profile, &places)
        .map(|context| RoleCapabilities {
            addendum: context.addendum,
            skills: context.skills,
            mcp_servers: context.mcp_servers,
            mcp_section: context.mcp_section,
            hooks: context.hooks,
        })
        .map_err(|error| {
            let name = &definition.name;
            crate::ui_text::ui_text!(
                "子代理角色 {name} 的技能、MCP 或钩子无法使用：{error}",
                "Subagent role {name}'s skills, MCP servers or hooks cannot be used: {error}"
            )
        })
}

/// One of a conversation's workspaces, as its capabilities need to know it.
#[derive(Clone, Debug)]
struct PlacedWorkspace {
    /// Its number in the conversation.
    member: u32,
    /// Its [`workspace_key`]; `None` for the temporary project's scratch
    /// folder, which has no level.
    key: Option<String>,
    machine: Option<crate::model::RunTarget>,
}

/// Where a conversation's workspaces are, for saying where its skills' files
/// are and which machine each MCP server runs on, and for giving each of a
/// workspace's servers and hooks that workspace's number.
#[derive(Clone, Debug, Default)]
pub(crate) struct WorkspacePlaces {
    workspaces: Vec<PlacedWorkspace>,
    /// Machine names, by [`crate::run_environment::env_key`].
    machine_names: HashMap<String, String>,
}

impl WorkspacePlaces {
    pub(crate) fn of(document: &AppDocument, conversation: &Conversation) -> Self {
        let mut machine_names = HashMap::new();
        let workspaces = conversation_locations(document, conversation)
            .into_iter()
            .enumerate()
            .map(|(index, location)| {
                let machine = location.as_ref().and_then(|location| location.machine.clone());
                machine_names
                    .entry(crate::run_environment::env_key(machine.as_ref()))
                    .or_insert_with(|| {
                        crate::workspace_set::machine_label(
                            &document.assets.execution_environments,
                            machine.as_ref(),
                        )
                    });
                PlacedWorkspace {
                    member: index as u32 + 1,
                    key: location.as_ref().map(workspace_key),
                    machine,
                }
            })
            .collect();
        Self {
            workspaces,
            machine_names,
        }
    }

    /// The number of the workspace whose level has `key`: the first, when the
    /// same directory is listed twice.
    fn member_of(&self, key: &str) -> Option<u32> {
        self.workspaces
            .iter()
            .find(|workspace| workspace.key.as_deref() == Some(key))
            .map(|workspace| workspace.member)
    }

    fn machine_of(&self, member: u32) -> Option<&crate::model::RunTarget> {
        self.workspaces
            .iter()
            .find(|workspace| workspace.member == member)
            .and_then(|workspace| workspace.machine.as_ref())
    }

    /// Whether there is nothing to say about places: one workspace, on this
    /// computer, where every path a capability names is a path of it.
    fn is_single_local(&self) -> bool {
        self.workspaces.len() <= 1 && self.workspaces.iter().all(|workspace| workspace.machine.is_none())
    }

    /// The numbers of the workspaces on `machine` (`None` is this computer).
    fn members_on(&self, machine: Option<&crate::model::RunTarget>) -> Vec<u32> {
        let key = crate::run_environment::env_key(machine);
        self.workspaces
            .iter()
            .filter(|workspace| crate::run_environment::env_key(workspace.machine.as_ref()) == key)
            .map(|workspace| workspace.member)
            .collect()
    }

    /// `machine` in the words the Environment section names it by.
    fn machine_phrase(&self, profile: &PromptProfile, machine: Option<&crate::model::RunTarget>) -> String {
        let name = self
            .machine_names
            .get(&crate::run_environment::env_key(machine))
            .cloned()
            .unwrap_or_default();
        match machine {
            None => profile.text(PromptKey::SystemEnvironmentWorkspaceOnHost).to_owned(),
            Some(crate::model::RunTarget::Wsl { distro }) => profile.render(
                PromptKey::SystemEnvironmentWorkspaceOnWsl,
                &[("name", if name.is_empty() { distro } else { &name })],
            ),
            Some(crate::model::RunTarget::Ssh { machine_id }) => profile.render(
                PromptKey::SystemEnvironmentWorkspaceOnSsh,
                &[("name", if name.is_empty() { machine_id } else { &name })],
            ),
        }
    }

    /// The sentence an MCP server's row ends with: where it runs and which
    /// workspaces are there. A server is bound to a machine, not to a
    /// workspace: any workspace on its machine may use it.
    fn mcp_place(&self, profile: &PromptProfile, machine: Option<&crate::model::RunTarget>) -> String {
        let phrase = self.machine_phrase(profile, machine);
        let members = self.members_on(machine);
        if members.is_empty() {
            return profile.render(PromptKey::SystemMcpServerPlaceNone, &[("machine", &phrase)]);
        }
        let numbers = members.iter().map(u32::to_string).collect::<Vec<_>>();
        profile.render(
            PromptKey::SystemMcpServerPlace,
            &[
                ("machine", &phrase),
                ("workspaces", &profile.join_list(numbers.iter().map(String::as_str))),
            ],
        )
    }
}

/// Gives a run's servers the folders they work in, once its workspaces are
/// resolved.
///
/// A workspace's stdio server without a `cwd` starts in that workspace's
/// folder on its machine — the conversation's worktree of it when there is
/// one. A global
/// server runs on this computer: it starts in the first of the
/// conversation's workspaces here, or the home folder when none is here (the
/// temporary project's scratch folder is no one's workspace to start in);
/// one declared `"workspace": true` gets an instance in each workspace here
/// that a call names. Hooks are placed when they run ([`place_hooks`]).
pub(crate) fn place_in_workspace(request: &mut crate::model::RunModelRequest, has_workspace: bool) {
    let home = dirs::home_dir().map(|home| home.to_string_lossy().into_owned());
    let mut local_folders = request
        .workspaces
        .entries()
        .iter()
        .filter(|workspace| workspace.is_local() && (workspace.index != 1 || has_workspace))
        .map(|workspace| (workspace.index, workspace.root.clone()))
        .collect::<Vec<_>>();
    if request.workspaces.is_empty() && has_workspace && !request.workspace_path.trim().is_empty() {
        local_folders.push((1, request.workspace_path.clone()));
    }
    for server in &mut request.mcp_servers {
        match server
            .declared_in
            .and_then(|member| request.workspaces.get(member))
        {
            Some(workspace) if workspace.is_local() => {
                server.default_working_directory(Some(&workspace.root), None)
            }
            Some(workspace) => server.default_working_directory(None, Some(&workspace.root)),
            None => {
                let stdio = matches!(server.transport, crate::mcp::RuntimeMcpTransport::Stdio { .. });
                if server.wants_workspace && stdio && server.declared_in.is_none() {
                    server.workspace_folders = local_folders.clone();
                }
                server.default_working_directory(
                    local_folders
                        .first()
                        .map(|(_, folder)| folder.as_str())
                        .or(home.as_deref()),
                    None,
                );
            }
        }
    }
}

/// `hooks` placed in the folders of the workspaces that declared them, as
/// `workspaces` stands now: a workspace's hook runs in that workspace's
/// folder — on its machine, with its variables — and a global hook in the
/// run's own folder.
///
/// Placed when they run rather than once per run: an isolated workflow step
/// moves workspace 1 into its own worktree after the run's request was built,
/// and workspace 1's hooks follow it there.
pub(crate) fn place_hooks(
    workspaces: &crate::workspace_set::WorkspaceSet,
    mut hooks: Vec<HookDefinition>,
) -> Vec<HookDefinition> {
    for hook in &mut hooks {
        let Some(workspace) = hook.member.and_then(|member| workspaces.get(member)) else {
            continue;
        };
        if let Some(place) = hook.on_machine.as_mut() {
            place.cwd = workspace.root.clone();
        } else if workspace.is_local() {
            hook.local_place = Some(crate::model::LocalHookPlace {
                cwd: workspace.root.clone(),
                env: workspace
                    .runner
                    .env()
                    .iter()
                    .map(|(name, value)| (name.clone(), value.clone()))
                    .collect(),
            });
        }
    }
    hooks
}

/// Every selection has to resolve. A selected skill, MCP server or hook that
/// discovery no longer finds — the folder was moved or deleted, the entry left
/// its file, the handler changed — fails the run, naming it, until it is
/// unticked: running without a capability the conversation was set up with
/// (a guard hook, the server a task depends on, the procedure a skill
/// teaches) is worse than a run that does not start, and the row is on screen,
/// marked dangling, to be cleared. An entry discovery finds but cannot use
/// fails the same way, with its reason.
fn runtime_context_from_discovery(
    conversation: &Conversation,
    discovered: &DiscoveredCapabilities,
    profile: &PromptProfile,
    places: &WorkspacePlaces,
) -> Result<RuntimeContext, String> {
    context_for_selection(
        &CapabilitySelection::of_conversation(conversation),
        discovered,
        profile,
        places,
    )
}

/// [`runtime_context_from_discovery`] for any selection: the conversation's,
/// or one of its roles'.
fn context_for_selection(
    selection: &CapabilitySelection<'_>,
    discovered: &DiscoveredCapabilities,
    profile: &PromptProfile,
    places: &WorkspacePlaces,
) -> Result<RuntimeContext, String> {
    let catalog = &discovered.catalog;
    // A selection discovery cannot find may simply be on a level whose machine
    // could not be read; that, not "it no longer exists", is the reason then.
    let missing = |kind: CapabilityKind, id: &str| match discovered.level_errors.first() {
        Some((_, error)) => error.clone(),
        None => dangling_selection_error(kind, id),
    };
    let mut selected_hook_ids = Vec::new();
    let mut selected_skill_ids = Vec::new();
    let mut selected_mcp_ids = Vec::new();

    extend_unique(&mut selected_hook_ids, selection.hook_ids);
    extend_unique(&mut selected_skill_ids, selection.skill_ids);
    extend_unique(&mut selected_mcp_ids, selection.mcp_ids);

    let via_tool = selection.via_tool;
    /* Which skills the system prompt is allowed to carry. A conversation with
       no pin has not opened its prompt yet, so everything selected belongs to
       it — which is also what a conversation predating the pin gets, and what
       it was already doing. */
    let prompt_skill_ids = selection.prompt_skill_ids;
    let in_opening_prompt = |resource_id: &str| match prompt_skill_ids {
        None => true,
        Some(pinned) => pinned.iter().any(|id| id == resource_id),
    };
    let mut sections = Vec::new();
    let mut skills = Vec::new();
    let mut added_skills = Vec::new();
    let mut listing_rows = Vec::new();
    for resource_id in selected_skill_ids {
        let Some(descriptor) = catalog
            .skills
            .iter()
            .find(|resource| resource.id == resource_id)
        else {
            return Err(missing(CapabilityKind::Skills, &resource_id));
        };
        if !descriptor.available {
            return Err(unavailable_selection_error(CapabilityKind::Skills, descriptor));
        }
        let remote = discovered.remote_skills.get(&resource_id);
        let source = match remote {
            Some(remote) => remote.text.clone()?,
            None => read_skill_body(descriptor)?,
        };
        let parsed = skill_document_from_source(&source);
        // Where its scripts and references are. A skill is no workspace's
        // own: every selected skill serves the whole conversation, wherever
        // it was declared. Its folder, though, is in one place — in the
        // workspace that declared it, on that workspace's machine, or on
        // this computer for a global one — and once the conversation has
        // more than that one local folder, the model is told which. The
        // built-in has none.
        let directory = (descriptor.source != ResourceSource::Builtin).then(|| {
            let path = match remote {
                Some(remote) => remote.directory.clone(),
                None => skill_directory(descriptor),
            };
            if places.is_single_local() {
                return path;
            }
            match descriptor.workspace_key.as_deref().and_then(|key| places.member_of(key)) {
                Some(member) => profile.render(
                    PromptKey::SystemSkillWorkspaceDirectory,
                    &[("path", &path), ("workspace", &member.to_string())],
                ),
                None => profile.render(PromptKey::SystemSkillLocalDirectory, &[("path", &path)]),
            }
        });
        let opening = in_opening_prompt(&resource_id);
        if via_tool {
            // The model selects skills by directory name, so every name must
            // identify exactly one skill. Duplicate names make one skill
            // unreachable, wherever its listing was written.
            let name = skills::directory_name_of(descriptor);
            if skills
                .iter()
                .any(|other: &ResolvedSkill| other.name == name)
            {
                return Err(format!(
                    "This conversation selected two skills whose directories are both named \"{name}\". On-demand loading selects skills by directory name, so one would be unreachable; rename one directory or select only one."
                ));
            }
            if opening {
                if !parsed.trigger.trim().is_empty() {
                    listing_rows.push(profile.render(
                        PromptKey::SkillListingRow,
                        &[("name", &name), ("trigger", &parsed.trigger)],
                    ));
                }
            } else {
                added_skills.push(AddedSkill {
                    resource_id: resource_id.clone(),
                    name: name.clone(),
                    content: profile.render(
                        PromptKey::SystemSkillAddedTrigger,
                        &[("name", &name), ("trigger", &parsed.trigger)],
                    ),
                });
            }
            skills.push(ResolvedSkill {
                name,
                trigger: parsed.trigger,
                body: parsed.body,
                directory: directory.unwrap_or_else(|| skill_directory(descriptor)),
            });
        } else if !parsed.body.is_empty() {
            // The body says `scripts/check.sh`, so the model is told where that
            // is, as the `skill` tool tells it on demand.
            let body = match &directory {
                Some(directory) => format!(
                    "{}\n\n{}",
                    parsed.body,
                    profile.render(PromptKey::SystemSkillFolder, &[("directory", directory)])
                ),
                None => parsed.body,
            };
            if opening {
                // Include only the body. The heading and frontmatter are registration
                // metadata, while `---` below provides structure between skill bodies.
                sections.push(body);
            } else {
                added_skills.push(AddedSkill {
                    resource_id: resource_id.clone(),
                    name: descriptor.name.clone(),
                    content: profile.render(
                        PromptKey::SystemSkillAddedBody,
                        &[("name", &descriptor.name), ("body", &body)],
                    ),
                });
            }
        }
    }
    /* On-demand loading puts the catalog in the prompt rather than in the
       tool's schema: a schema is re-declared on every request, so a listing
       there would change the tool set the moment another skill was selected. */
    if !listing_rows.is_empty() {
        sections.push(format!(
            "{}\n{}",
            profile.text(PromptKey::SkillListingHeading),
            listing_rows.join("\n")
        ));
    }

    let mut mcp_servers = Vec::new();
    let mut server_rows = Vec::new();
    for resource_id in selected_mcp_ids {
        let Some(descriptor) = catalog
            .mcps
            .iter()
            .find(|resource| resource.id == resource_id)
        else {
            return Err(missing(CapabilityKind::Mcp, &resource_id));
        };
        let Some(config) = discovered
            .mcp_servers
            .get(&descriptor.id)
            .filter(|_| descriptor.available)
        else {
            return Err(unavailable_selection_error(CapabilityKind::Mcp, descriptor));
        };
        let mut server = RuntimeMcpServer::from_config(config);
        // A workspace's server is that workspace's: it starts in its folder
        // on its machine. A global one runs on this computer.
        server.declared_in = descriptor
            .workspace_key
            .as_deref()
            .and_then(|key| places.member_of(key));
        let machine = server.declared_in.and_then(|member| places.machine_of(member)).cloned();
        mcp_servers.push(server);
        // A server without a description is listed in the catalog with a
        // label in the app language; the model-facing row says it in the
        // profile's words.
        let description = if config.description.is_empty() {
            profile.text(PromptKey::SystemMcpServerDefaultDescription).to_owned()
        } else {
            config.description.clone()
        };
        // Servers are bound to machines, not workspaces: once the
        // conversation spans more than one folder here, each row says which
        // machine its server runs on and which workspaces are there.
        let description = if places.is_single_local() {
            description
        } else {
            format!("{description} {}", places.mcp_place(profile, machine.as_ref()))
        };
        server_rows.push((
            descriptor.id.clone(),
            profile.render(
                PromptKey::SystemCapabilityRow,
                &[("name", &descriptor.name), ("description", &description)],
            ),
        ));
    }
    let mut mcp_section = McpPromptSection::default();
    if !server_rows.is_empty() {
        let rows = server_rows
            .iter()
            .map(|(_, row)| row.as_str())
            .collect::<Vec<_>>();
        mcp_section.rendered =
            profile.render(PromptKey::SystemMcpSection, &[("servers", &rows.join("\n"))]);
        sections.push(mcp_section.rendered.clone());
        mcp_section.rows = server_rows;
    }

    let mut hooks = Vec::new();
    if !selected_hook_ids.is_empty() {
        let selected = selected_hook_ids
            .iter()
            .map(|resource_id| {
                let descriptor = catalog
                    .hooks
                    .iter()
                    .find(|resource| resource.id == *resource_id)
                    .ok_or_else(|| missing(CapabilityKind::Hooks, resource_id))?;
                if descriptor.available {
                    Ok(descriptor)
                } else {
                    Err(unavailable_selection_error(CapabilityKind::Hooks, descriptor))
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        // The catalog description is UI text; the model-facing row is rendered
        // from the hook's event and matcher in the profile's words.
        let rows = selected
            .iter()
            .map(|resource| {
                let definition = discovered.hooks.get(&resource.id);
                // A workspace's hook watches that workspace: its number
                // decides which tool calls it sees.
                hooks.extend(definition.cloned().map(|mut definition| {
                    definition.member = definition
                        .workspace_key
                        .as_deref()
                        .and_then(|key| places.member_of(key));
                    definition
                }));
                let description = definition
                    .map(|definition| hook_description(profile, definition))
                    .unwrap_or_else(|| resource.description.clone());
                profile.render(
                    PromptKey::SystemCapabilityRow,
                    &[("name", &resource.name), ("description", &description)],
                )
            })
            .collect::<Vec<_>>();
        let hook_names = profile.join_list(selected.iter().map(|resource| resource.name.as_str()));
        sections.push(profile.render(
            PromptKey::SystemHooksSection,
            &[("hook_names", &hook_names), ("hooks", &rows.join("\n"))],
        ));
    }

    let addendum = sections.join("\n\n---\n\n");
    // Every output is independently measured. Tool-mode bodies and the messages
    // a later skill arrives in do not enter the system prompt, but they still
    // reside in the request and share its size limit.
    let skill_bytes = skills.iter().map(|skill| skill.body.len()).sum::<usize>();
    let added_bytes = added_skills
        .iter()
        .map(|skill| skill.content.len())
        .sum::<usize>();
    if addendum.len() > RUNTIME_CONTEXT_LIMIT
        || skill_bytes > RUNTIME_CONTEXT_LIMIT
        || added_bytes > RUNTIME_CONTEXT_LIMIT
    {
        return Err(
            "The runtime context for enabled skills and hooks exceeds the 1 MiB limit.".into(),
        );
    }
    Ok(RuntimeContext {
        addendum,
        skills,
        added_skills,
        mcp_servers,
        hooks,
        mcp_section,
        role_hooks: Default::default(),
        agent_roles: Vec::new(),
        role_levels: Vec::new(),
    })
}

/// Why a run cannot start while a selection names nothing discovery found.
/// A dangling selection has no name left to show, only its id — which is also
/// what the settings page lists it under.
fn dangling_selection_error(kind: CapabilityKind, id: &str) -> String {
    match kind {
        CapabilityKind::Skills => crate::ui_text::ui_text!(
            "已勾选的技能 {id} 已不存在：它的文件夹被移动、改名或删除了。在“对话设置 → 技能”里取消勾选它之后才能运行。",
            "The selected skill {id} no longer exists: its folder was moved, renamed or deleted. Untick it in Conversation settings → Skills to run again."
        ),
        CapabilityKind::Mcp => crate::ui_text::ui_text!(
            "已勾选的 MCP 服务器 {id} 已不存在：它的条目已从 mcp.json 里移除或改名。在“对话设置 → MCP”里取消勾选它之后才能运行。",
            "The selected MCP server {id} no longer exists: its entry was removed from mcp.json or renamed. Untick it in Conversation settings → MCP to run again."
        ),
        CapabilityKind::Hooks | CapabilityKind::Lsp | CapabilityKind::ToolDescriptions => crate::ui_text::ui_text!(
            "已勾选的钩子 {id} 已不存在：hooks.json 里的这个处理程序被移动、修改或删除了。在“对话设置 → 钩子”里取消勾选它之后才能运行。",
            "The selected hook {id} no longer exists: its handler in hooks.json was moved, changed or removed. Untick it in Conversation settings → Hooks to run again."
        ),
        // A dangling role is skipped rather than failing the run; this wording
        // exists for completeness.
        CapabilityKind::Agents => crate::ui_text::ui_text!(
            "已勾选的角色 {id} 已不存在：它的文件被移动、改名或删除了。",
            "The selected role {id} no longer exists: its file was moved, renamed or deleted."
        ),
    }
}

/// Why a run cannot start while a selection names an entry that cannot be used.
fn unavailable_selection_error(kind: CapabilityKind, descriptor: &ResourceDescriptor) -> String {
    let name = &descriptor.name;
    let reason = &descriptor.description;
    match kind {
        CapabilityKind::Skills => crate::ui_text::ui_text!(
            "已勾选的技能“{name}”无法使用：{reason}",
            "The selected skill \"{name}\" cannot be used: {reason}"
        ),
        CapabilityKind::Mcp => crate::ui_text::ui_text!(
            "已勾选的 MCP 服务器“{name}”无法使用：{reason}",
            "The selected MCP server \"{name}\" cannot be used: {reason}"
        ),
        CapabilityKind::Hooks | CapabilityKind::Lsp | CapabilityKind::ToolDescriptions => crate::ui_text::ui_text!(
            "已勾选的钩子“{name}”无法使用：{reason}",
            "The selected hook \"{name}\" cannot be used: {reason}"
        ),
        CapabilityKind::Agents => crate::ui_text::ui_text!(
            "已勾选的角色“{name}”无法使用：{reason}",
            "The selected role \"{name}\" cannot be used: {reason}"
        ),
    }
}

/// Skill directory: the parent of its body file.
///
/// Skills can include `scripts/` and `references/` addressed relative to `SKILL.md`;
/// providing the directory lets the model resolve those references. A built-in
/// skill has none, and its pseudo-location is not a path to hand the model.
fn skill_directory(descriptor: &ResourceDescriptor) -> String {
    if descriptor.source == ResourceSource::Builtin {
        return "none (this skill is built into Mewrk and has no files on disk)".to_owned();
    }
    Path::new(&descriptor.location)
        .parent()
        .map(|parent| parent.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn resource_sort(left: &ResourceDescriptor, right: &ResourceDescriptor) -> std::cmp::Ordering {
    left.name
        .to_lowercase()
        .cmp(&right.name.to_lowercase())
        .then_with(|| left.location.cmp(&right.location))
}

#[derive(Clone)]
struct HookEntry {
    descriptor: ResourceDescriptor,
    definition: HookDefinition,
    /// The id this handler had while hook ids were hashes over its position
    /// in the file, for translating selections saved back then.
    legacy_id: String,
}

/// What makes two handlers the same hook: the event they run at, the matcher
/// they run for, and the commands they run. Name, timeout and status message
/// are how a hook is shown and bounded, not which command runs, so editing them
/// keeps every selection of it.
fn hook_content_key(
    event: &str,
    matcher: Option<&str>,
    command: &str,
    command_windows: Option<&str>,
) -> String {
    format!(
        "{event}\u{0}{}\u{0}{command}\u{0}{}",
        matcher.unwrap_or_default(),
        command_windows.unwrap_or_default()
    )
}

/// A hook's id: its level, its event for legibility, and a hash over the file
/// it is written in and its [`hook_content_key`] (with the count of identical
/// handlers before it). The file's path is folded as [`stable_id`] folds it;
/// the content is hashed as written, because `echo A` is not `echo a`.
fn hook_id(scope: &str, event: &str, file: &str, content: &str, occurrence: usize) -> String {
    let identity = format!("{}\n{content}\n{occurrence}", normalized_location_for_id(file));
    format!(
        "hook_{scope}_{}_{:08x}",
        id_slug(event),
        location_id_hash(&identity)
    )
}

/// Where a hook sits inside its `hooks.json` — the file, and the position
/// [`read_hooks_file`] read the handler from.
///
/// The position is not the hook's identity (that is [`hook_id`]), but it is
/// where the handler was when the scan that listed it ran, which is what a
/// delete has to cut: the delete command rescans and finds the address of the
/// handler the id names now.
pub(crate) struct HookAddress {
    pub path: PathBuf,
    pub event: String,
    pub group_index: usize,
    pub handler_index: usize,
}

/// Reads a hook descriptor's `location` back into the address it was built from.
///
/// The format is the one [`read_hooks_file`] writes: the config path, then the
/// JSON pointer `#/hooks/<event>/<group>/hooks/<handler>`. Anything else is not
/// an address this application produced, so it resolves to nothing rather than
/// to a guess.
pub(crate) fn parse_hook_location(location: &str) -> Option<HookAddress> {
    let (path, pointer) = location.rsplit_once("#/hooks/")?;
    let mut parts = pointer.split('/');
    let event = parts.next()?;
    let group_index = parts.next()?.parse().ok()?;
    if parts.next()? != "hooks" {
        return None;
    }
    let handler_index = parts.next()?.parse().ok()?;
    if parts.next().is_some() || event.is_empty() {
        return None;
    }
    Some(HookAddress {
        path: PathBuf::from(path),
        event: event.to_owned(),
        group_index,
        handler_index,
    })
}

/// Drops one handler from a `hooks.json`, leaving every other entry and every
/// other field of the file as it was.
///
/// The remaining handlers in the same group move up but keep their ids, which
/// do not depend on position, so every other selection still runs the command
/// it ticked. Conversations that had selected the removed one show it as a
/// dangling selection, exactly as they would had the user deleted the line by
/// hand.
///
/// An emptied group is left in place rather than pruned: the file is the user's,
/// and a group with no handlers reads as nothing while keeping the matcher they
/// wrote.
pub(crate) fn remove_hook_from_file(address: &HookAddress) -> Result<(), String> {
    let display = address.path.display();
    let text = fs::read_to_string(&address.path)
        .map_err(|error| crate::ui_text::ui_text!("无法读取 {display}：{error}", "Could not read {display}: {error}"))?;
    let edited = remove_hook_from_text(&text, address)?;
    write_config_file(&address.path, &edited)
}

/// [`remove_hook_from_file`]'s edit on text already in hand — a `hooks.json`
/// read off another machine, to be written back there.
fn remove_hook_from_text(text: &str, address: &HookAddress) -> Result<String, String> {
    let display = address.path.display();
    let (bom, text) = crate::config_file::split_bom(text);
    // Parse first, so a malformed file is reported as such rather than cut into,
    // and so the address is checked against what the file actually holds.
    let document: Value = serde_json::from_str(text)
        .map_err(|error| crate::ui_text::ui_text!("{display} 不是合法的 JSON：{error}", "{display} is not valid JSON: {error}"))?;
    document
        .get("hooks")
        .and_then(Value::as_object)
        .and_then(|hooks| hooks.get(&address.event))
        .and_then(Value::as_array)
        .and_then(|groups| groups.get(address.group_index))
        .and_then(Value::as_object)
        .and_then(|group| group.get("hooks"))
        .and_then(Value::as_array)
        .filter(|handlers| address.handler_index < handlers.len())
        .ok_or_else(|| crate::ui_text::ui_text!("{display} 里已经没有这个钩子了", "{display} no longer has this hook"))?;
    // The edit is textual: this is the user's file, and re-serializing it would
    // reorder every object key and reflow every line around the one deletion.
    let edited = crate::json_edit::remove_array_element(
        text,
        &[
            crate::json_edit::Step::Key("hooks"),
            crate::json_edit::Step::Key(&address.event),
            crate::json_edit::Step::Index(address.group_index),
            crate::json_edit::Step::Key("hooks"),
        ],
        address.handler_index,
    )
    .map_err(|error| crate::ui_text::ui_text!("无法从 {display} 里删除这个钩子：{error}", "Could not remove this hook from {display}: {error}"))?;
    Ok(format!("{bom}{edited}"))
}

/// Replaces a config file through a sibling temporary, so an interrupted write
/// leaves the original rather than a half file.
pub(crate) fn write_config_file(path: &Path, contents: &str) -> Result<(), String> {
    let display = path.display();
    let parent = path
        .parent()
        .ok_or_else(|| crate::ui_text::ui_text!("{display} 没有可写入的目录", "{display} has no directory to write to"))?;
    let temporary = parent.join(format!(".config.{}.tmp", uuid::Uuid::new_v4().simple()));
    fs::write(&temporary, contents)
        .map_err(|error| crate::ui_text::ui_text!("无法写入 {display}：{error}", "Could not write {display}: {error}"))?;
    match fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            Err(crate::ui_text::ui_text!("无法替换 {display}：{error}", "Could not replace {display}: {error}"))
        }
    }
}

/// Deletes one hook from the `hooks.json` it was discovered in — on this
/// computer, or on the machine of the workspace that declared it.
///
/// The id names a handler by what it runs, so a fresh scan finds that handler
/// wherever it now sits in its file, and its address there is what is cut.
pub(crate) fn delete_hook(document: &AppDocument, hook_id: &str) -> Result<(), String> {
    let levels = all_levels(document);
    let descriptor = discover_levels(&levels, document.global_settings.resolved_app_language)
        .catalog
        .hooks
        .into_iter()
        .find(|descriptor| descriptor.id == hook_id)
        .ok_or_else(|| crate::ui_text::ui_text!("钩子 {hook_id} 不存在", "There is no hook {hook_id}"))?;
    if descriptor.source == ResourceSource::Builtin {
        return Err(crate::ui_text::pick("内置条目不能删除", "A built-in entry cannot be deleted").into());
    }
    let address = parse_hook_location(&descriptor.location).ok_or_else(|| {
        crate::ui_text::ui_text!("钩子 {hook_id} 的位置无法解析", "The location of hook {hook_id} cannot be read")
    })?;
    match remote_level_of(&levels, &descriptor) {
        None => remove_hook_from_file(&address),
        Some(remote) => {
            let path = address.path.to_string_lossy().into_owned();
            let text = remote_config_text(remote, &path, |files| files.hooks.as_ref())?;
            crate::remote_capabilities::replace_file(remote, &path, &remove_hook_from_text(&text, &address)?)
        }
    }
}

/// The level on another machine a discovered row came from, if it did.
fn remote_level_of<'a>(
    levels: &'a [ConfigLevel],
    descriptor: &ResourceDescriptor,
) -> Option<&'a crate::remote_capabilities::RemoteLevel> {
    let key = descriptor.workspace_key.as_deref()?;
    levels
        .iter()
        .find(|level| level.workspace_key().as_deref() == Some(key))?
        .remote
        .as_ref()
}

/// The current text of a configuration file of a level on another machine,
/// read again rather than trusted from the scan the row came from.
fn remote_config_text(
    remote: &crate::remote_capabilities::RemoteLevel,
    path: &str,
    file: impl Fn(&crate::remote_capabilities::LevelFiles) -> Option<&crate::remote_capabilities::ConfigFile>,
) -> Result<String, String> {
    let files = crate::remote_capabilities::read(remote, crate::remote_capabilities::RUN_TIMEOUT)?;
    let current = file(&files)
        .filter(|current| current.path == path)
        .ok_or_else(|| crate::ui_text::ui_text!("{path} 已经不在了", "{path} is no longer there"))?;
    String::from_utf8(current.bytes.clone())
        .map_err(|_| crate::ui_text::ui_text!("{path} 不是 UTF-8 文本", "{path} is not UTF-8 text"))
}

/// The file in the application data directory that records, for every
/// selection made while hook ids were positions, the handler it was translated
/// to — or that it named none.
const LEGACY_HOOK_IDS_FILE: &str = "legacy-hook-ids.json";

/// Whether `id` has the shape hook ids had while they hashed a handler's
/// position (`hook_<level>_<event>_<group>_<handler>_<hash>`). Current ids
/// carry no position, so the two never look alike.
fn is_legacy_hook_id(id: &str) -> bool {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    PATTERN
        .get_or_init(|| {
            regex::Regex::new(r"^hook_(user|workspace)_[a-z]+_[0-9]+_[0-9]+_[0-9a-f]{8}$")
                .expect("valid pattern")
        })
        .is_match(id)
}

/// Translates hook selections saved while hook ids were hashes over a
/// handler's position into the ids the handlers have now.
///
/// A position id named whatever handler sat there when it was read, so the
/// only honest translation is the one made against the files as they stand
/// when this build first sees the id — and only once: translating again after
/// the file changed is exactly the silent retargeting the new ids exist to
/// end. So each translation (or the finding that the position holds nothing)
/// is recorded in [`LEGACY_HOOK_IDS_FILE`] the first time, and every later
/// load reuses the record. An id translated to nothing stays as it is and
/// shows as dangling.
///
/// Runs on every load, because a conversation keeps its stored settings until
/// it is next written; with nothing legacy selected it reads nothing.
pub(crate) fn migrate_legacy_hook_ids(document: &mut AppDocument, app_data: &Path) {
    let mut legacy = std::collections::BTreeSet::new();
    for_each_hook_selection(document, &mut |ids: &mut Vec<String>| {
        legacy.extend(ids.iter().filter(|id| is_legacy_hook_id(id)).cloned());
    });
    if legacy.is_empty() {
        return;
    }
    let record_path = app_data.join(LEGACY_HOOK_IDS_FILE);
    let mut record: std::collections::BTreeMap<String, Option<String>> = fs::read(&record_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let unseen = legacy
        .iter()
        .filter(|id| !record.contains_key(*id))
        .cloned()
        .collect::<Vec<_>>();
    if !unseen.is_empty() {
        let current = local_levels(document)
            .iter()
            .flat_map(|level| {
                read_hooks_file(
                    &level.path_for(CapabilityKind::Hooks),
                    level.source,
                    level.workspace_key().as_deref(),
                )
            })
            .map(|entry| (entry.legacy_id, entry.descriptor.id))
            .collect::<HashMap<_, _>>();
        for id in unseen {
            let translated = current.get(&id).cloned();
            record.insert(id, translated);
        }
        match serde_json::to_string_pretty(&record) {
            Ok(text) => {
                if let Err(error) = write_config_file(&record_path, &text) {
                    eprintln!("Could not record the translated hook selections: {error}");
                }
            }
            Err(error) => eprintln!("Could not record the translated hook selections: {error}"),
        }
    }
    for_each_hook_selection(document, &mut |ids: &mut Vec<String>| {
        let mut translated: Vec<String> = Vec::with_capacity(ids.len());
        for id in ids.iter() {
            let id = record.get(id).cloned().flatten().unwrap_or_else(|| id.clone());
            if !translated.contains(&id) {
                translated.push(id);
            }
        }
        *ids = translated;
    });
}

/// Every list of selected hook ids the document holds: each conversation's,
/// each preset's, and each workspace's remembered last settings and draft.
fn for_each_hook_selection(document: &mut AppDocument, visit: &mut dyn FnMut(&mut Vec<String>)) {
    for preset in &mut document.presets.conversation_presets {
        visit(&mut preset.settings.hook_ids);
    }
    for workspace in &mut document.workspaces {
        if let Some(settings) = workspace.last_conversation_settings.as_mut() {
            visit(&mut settings.hook_ids);
        }
        if let Some(draft) = workspace.draft_conversation.as_mut() {
            visit(&mut draft.settings.hook_ids);
        }
        for conversation in &mut workspace.conversations {
            visit(&mut conversation.settings.hook_ids);
        }
    }
}

/// Deletes a discovered skill's folder, whichever level it was read from.
///
/// The skill is looked up in a fresh scan rather than trusted from the
/// renderer, and `skills::delete_directory` only ever removes a direct child of
/// one of the `skills/` roots that scan read.
pub(crate) fn delete_skill(document: &AppDocument, skill_id: &str) -> Result<(), String> {
    let levels = all_levels(document);
    let descriptor = discover_levels(&levels, document.global_settings.resolved_app_language)
        .catalog
        .skills
        .into_iter()
        .find(|descriptor| descriptor.id == skill_id)
        .ok_or_else(|| crate::ui_text::ui_text!("技能 {skill_id} 不存在", "There is no skill {skill_id}"))?;
    if descriptor.source != ResourceSource::Builtin {
        if let Some(remote) = remote_level_of(&levels, &descriptor) {
            // On another machine, which has no Trash: the folder goes for good,
            // and only a plain folder directly inside the skills directory.
            let directory = remote_parent(&descriptor.location);
            let root = remote_parent(&directory);
            let name = &directory[root.len()..].trim_start_matches(['/', '\\']);
            return crate::remote_capabilities::remove_skill(remote, &root, name);
        }
    }
    let roots = levels
        .iter()
        .filter(|level| level.is_local())
        .map(|level| level.path_for(CapabilityKind::Skills))
        .collect::<Vec<_>>();
    skills::delete_directory(&roots, &descriptor)
}

/// Removes a discovered MCP server from the `mcp.json` it was read from.
pub(crate) fn delete_mcp_server(document: &AppDocument, server_id: &str) -> Result<(), String> {
    let descriptor = discover_levels(
        &all_levels(document),
        document.global_settings.resolved_app_language,
    )
    .catalog
    .mcps
    .into_iter()
    .find(|descriptor| descriptor.id == server_id)
    .ok_or_else(|| crate::ui_text::ui_text!("MCP 服务器 {server_id} 不存在", "There is no MCP server {server_id}"))?;
    let (path, name) = mcp_config::parse_location(&descriptor.location)
        .ok_or_else(|| crate::ui_text::ui_text!("MCP 服务器 {server_id} 的位置无法解析", "The location of MCP server {server_id} cannot be read"))?;
    let levels = all_levels(document);
    match remote_level_of(&levels, &descriptor) {
        None => mcp_config::remove_server_from_file(&path, &name),
        Some(remote) => {
            let path = path.to_string_lossy().into_owned();
            let text = remote_config_text(remote, &path, |files| files.mcp.as_ref())?;
            let edited = mcp_config::remove_server_from_text(&text, Path::new(&path), &name)?;
            crate::remote_capabilities::replace_file(remote, &path, &edited)
        }
    }
}

/// The launch configuration of one discovered, available MCP server, for a
/// probe. Read off disk on every call: the renderer names a server, never
/// describes one.
#[cfg(test)]
pub(crate) fn mcp_server_config(
    document: &AppDocument,
    server_id: &str,
) -> Result<McpServerConfig, String> {
    let discovered = discover_levels(
        &all_levels(document),
        document.global_settings.resolved_app_language,
    );
    mcp_config_from(&discovered, server_id)
}

/// The launch configuration a scan found for `server_id`, or why there is none.
fn mcp_config_from(
    discovered: &DiscoveredCapabilities,
    server_id: &str,
) -> Result<McpServerConfig, String> {
    if let Some(config) = discovered.mcp_servers.get(server_id) {
        return Ok(config.clone());
    }
    match discovered
        .catalog
        .mcps
        .iter()
        .find(|descriptor| descriptor.id == server_id)
    {
        Some(descriptor) => Err(crate::ui_text::ui_text!(
            "MCP 服务器 {} 无法使用：{}",
            "MCP server {} cannot be used: {}",
            descriptor.name,
            descriptor.description
        )),
        None => Err(crate::ui_text::ui_text!(
            "MCP 服务器 {server_id} 不存在",
            "There is no MCP server {server_id}"
        )),
    }
}

/// The server a connection test dials: one discovered, available server,
/// started where a run would start it when its entry names no `cwd` — the
/// folder of the workspace that declared it (on its machine, for one on WSL or
/// SSH), or the home folder for a global one.
pub(crate) fn mcp_probe_server(
    document: &AppDocument,
    server_id: &str,
) -> Result<RuntimeMcpServer, String> {
    let discovered = discover_levels(
        &all_levels(document),
        document.global_settings.resolved_app_language,
    );
    let config = mcp_config_from(&discovered, server_id)?;
    // A server a workspace on this computer declared starts in that
    // workspace's folder; one on another machine starts in its folder there
    // (`config.machine`), and a global one in the home folder.
    let workspace_dir = discovered
        .catalog
        .mcps
        .iter()
        .find(|descriptor| descriptor.id == server_id)
        .and_then(|descriptor| descriptor.workspace_key.as_deref())
        .and_then(|key| {
            all_levels(document)
                .into_iter()
                .find(|level| level.is_local() && level.workspace_key().as_deref() == Some(key))
        })
        .map(|level| level.base.to_string_lossy().into_owned());
    let home = dirs::home_dir().map(|home| home.to_string_lossy().into_owned());
    let mut server = RuntimeMcpServer::from_config(&config);
    server.default_working_directory(workspace_dir.or(home).as_deref(), None);
    Ok(server)
}

/// Where to show one kind of capability's configuration.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RevealTarget {
    /// A file or folder on this computer, for the system file manager.
    Local(PathBuf),
    /// A folder on another machine, for the Files pane: no file manager here
    /// can open it.
    Remote {
        machine: crate::model::RunTarget,
        path: String,
    },
}

/// The place one kind of capability lives at one level, created on demand so
/// there is somewhere to open: the `skills/` or `agents/` directory, or the `.mewrk`
/// directory holding `mcp.json` / `hooks.json` (the file itself when it
/// already exists on this computer). A workspace on another machine has its
/// folder created and shown there.
///
/// Tool descriptions are global only: that kind opens `~/.mewrk/tool-descriptions`
/// (the legacy `.naiword` one while that is the only one there) and is refused
/// for a workspace, which has no such folder.
pub(crate) fn capability_location_to_reveal(
    document: &AppDocument,
    kind: CapabilityKind,
    workspace_key: Option<&str>,
) -> Result<RevealTarget, String> {
    if kind == CapabilityKind::ToolDescriptions && workspace_key.is_some() {
        return Err(crate::ui_text::pick(
            "工具描述文件只在全局目录读取，工作区没有可打开的这个目录",
            "Tool-description files are read from the global folder only; a workspace has no such folder to open",
        )
        .to_owned());
    }
    let level = match workspace_key {
        Some(key) => all_levels(document)
            .into_iter()
            .find(|level| level.workspace_key().as_deref() == Some(key))
            .ok_or_else(|| {
                crate::ui_text::pick(
                    "这个工作区没有可打开的目录",
                    "This workspace has no folder to open",
                )
                .to_owned()
            })?,
        None => ConfigLevel::user().ok_or_else(|| {
            crate::ui_text::pick("无法确定用户主目录", "Could not find the home folder").to_owned()
        })?,
    };
    let relative = match kind {
        CapabilityKind::Skills => format!("{CONFIG_DIRECTORY}/skills"),
        CapabilityKind::ToolDescriptions => format!("{CONFIG_DIRECTORY}/tool-descriptions"),
        CapabilityKind::Agents => format!("{CONFIG_DIRECTORY}/agents"),
        CapabilityKind::Mcp | CapabilityKind::Hooks | CapabilityKind::Lsp => CONFIG_DIRECTORY.to_owned(),
    };
    if let Some(remote) = &level.remote {
        let machine = level
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.machine.clone())
            .ok_or_else(|| {
                crate::ui_text::pick(
                    "这个工作区没有可打开的目录",
                    "This workspace has no folder to open",
                )
                .to_owned()
            })?;
        let path = crate::remote_capabilities::ensure_directory(remote, &relative)?;
        return Ok(RevealTarget::Remote { machine, path });
    }
    reveal_local_location(&level, kind, &relative)
}

/// [`capability_location_to_reveal`] for a level on this computer: the file or
/// folder the level already has for `kind`, else `relative` under its base,
/// created.
fn reveal_local_location(
    level: &ConfigLevel,
    kind: CapabilityKind,
    relative: &str,
) -> Result<RevealTarget, String> {
    let existing = level.path_for(kind);
    if existing.exists() {
        return Ok(RevealTarget::Local(existing));
    }
    let directory = level.base.join(relative);
    fs::create_dir_all(&directory).map_err(|error| {
        crate::ui_text::ui_text!(
            "无法创建 {}：{error}",
            "Could not create {}: {error}",
            directory.display()
        )
    })?;
    Ok(RevealTarget::Local(directory))
}

fn read_hooks_file(
    path: &Path,
    source: ResourceSource,
    workspace_key: Option<&str>,
) -> Vec<HookEntry> {
    let Ok(bytes) = fs::read(path) else {
        return Vec::new();
    };
    parse_hooks(&bytes, path, source, workspace_key)
}

/// Every handler of one `hooks.json` whose bytes the caller already holds — a
/// file read off another machine, where `path` is its spelling there.
fn parse_hooks(
    bytes: &[u8],
    path: &Path,
    source: ResourceSource,
    workspace_key: Option<&str>,
) -> Vec<HookEntry> {
    let Ok(value) = crate::config_file::parse_json(bytes) else {
        return Vec::new();
    };
    let Some(root) = value.as_object() else {
        return Vec::new();
    };
    let Some(hooks) = root.get("hooks").and_then(Value::as_object) else {
        return Vec::new();
    };
    let config_location = path.to_string_lossy();
    let scope = if source == ResourceSource::Workspace {
        "workspace"
    } else {
        "user"
    };
    let mut entries = Vec::new();
    let mut occurrences: HashMap<String, usize> = HashMap::new();
    for (event_name, groups) in hooks {
        let Some(event) = parse_hook_event(event_name) else {
            continue;
        };
        let Some(groups) = groups.as_array() else {
            continue;
        };
        for (group_index, group) in groups.iter().filter_map(Value::as_object).enumerate() {
            let matcher = group
                .get("matcher")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty() && *value != "*")
                .map(str::to_owned);
            if matcher
                .as_deref()
                .is_some_and(|value| regex::Regex::new(value).is_err())
            {
                continue;
            }
            let Some(handlers) = group.get("hooks").and_then(Value::as_array) else {
                continue;
            };
            for (handler_index, handler) in handlers.iter().filter_map(Value::as_object).enumerate()
            {
                if handler.get("type").and_then(Value::as_str) != Some("command") {
                    continue;
                }
                let Some(command) = handler
                    .get("command")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty() && value.len() <= 64 * 1024)
                    .map(str::to_owned)
                else {
                    continue;
                };
                if handler.get("asyncRewake").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                if handler.get("async").and_then(Value::as_bool) == Some(true)
                    && event != HookEvent::InstructionsLoaded
                {
                    continue;
                }
                let timeout_seconds = handler.get("timeout").and_then(Value::as_u64).unwrap_or(30);
                if !(1..=600).contains(&timeout_seconds) {
                    continue;
                }
                let command_windows = handler
                    .get("commandWindows")
                    .or_else(|| handler.get("command_windows"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty() && value.len() <= 64 * 1024)
                    .map(str::to_owned);
                let status_message = handler
                    .get("statusMessage")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(|value| truncate_chars(value, 240));
                let name = handler
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(|value| truncate_chars(value, 240))
                    .or_else(|| status_message.clone())
                    .unwrap_or_else(|| format!("{} #{}", event_name, handler_index + 1));
                let pointer = format!("#/hooks/{event_name}/{group_index}/hooks/{handler_index}");
                let location = format!("{config_location}{pointer}");
                // The id says which command runs and when, never where it is
                // written: inserting, removing or reordering handlers leaves a
                // selection on the command that was ticked, and a handler whose
                // command, event or matcher changed is a different handler, so
                // a selection of the old one dangles instead of running the new.
                // Identical handlers in one file are told apart by how many
                // came before them, so neither can stand in for another command.
                let content = hook_content_key(
                    event_name,
                    matcher.as_deref(),
                    &command,
                    command_windows.as_deref(),
                );
                let occurrence = {
                    let count = occurrences.entry(content.clone()).or_insert(0usize);
                    *count += 1;
                    *count - 1
                };
                let id = hook_id(scope, event_name, &config_location, &content, occurrence);
                let legacy_id = stable_id(
                    &format!("hook_{scope}"),
                    &format!("{event_name}:{group_index}:{handler_index}"),
                    &location,
                );
                let definition = HookDefinition {
                    id: id.clone(),
                    name: name.clone(),
                    event,
                    matcher: matcher.clone(),
                    command,
                    command_windows,
                    status_message,
                    enabled: true,
                    timeout_ms: timeout_seconds * 1_000,
                    on_machine: None,
                    workspace_key: workspace_key.map(str::to_owned),
                    member: None,
                    local_place: None,
                };
                entries.push(HookEntry {
                    descriptor: ResourceDescriptor {
                        id,
                        name,
                        description: hook_catalog_description(&definition),
                        location,
                        source,
                        available: true,
                        workspace_key: workspace_key.map(str::to_owned),
                    },
                    definition,
                    legacy_id,
                });
            }
        }
    }
    entries
}

fn parse_hook_event(value: &str) -> Option<HookEvent> {
    match value {
        "SessionStart" => Some(HookEvent::SessionStart),
        "InstructionsLoaded" => Some(HookEvent::InstructionsLoaded),
        "UserPromptSubmit" => Some(HookEvent::UserPromptSubmit),
        "PreToolUse" => Some(HookEvent::PreToolUse),
        "PermissionRequest" => Some(HookEvent::PermissionRequest),
        "PostToolUse" => Some(HookEvent::PostToolUse),
        "Stop" => Some(HookEvent::Stop),
        _ => None,
    }
}

/// The profile key naming a hook event.
pub fn hook_event_key(event: HookEvent) -> PromptKey {
    match event {
        HookEvent::SessionStart => PromptKey::SystemHookEventSessionStart,
        HookEvent::InstructionsLoaded => PromptKey::SystemHookEventInstructionsLoaded,
        HookEvent::UserPromptSubmit => PromptKey::SystemHookEventUserPromptSubmit,
        HookEvent::PreToolUse => PromptKey::SystemHookEventPreToolUse,
        HookEvent::PermissionRequest => PromptKey::SystemHookEventPermissionRequest,
        HookEvent::PostToolUse => PromptKey::SystemHookEventPostToolUse,
        HookEvent::Stop => PromptKey::SystemHookEventStop,
    }
}

/// A hook's row in the Hooks list: its event, plus its matcher when it has one,
/// in the app language. What the model reads is [`hook_description`].
fn hook_catalog_description(definition: &HookDefinition) -> String {
    let event = match definition.event {
        HookEvent::SessionStart => crate::ui_text::pick("会话开始", "Session start"),
        HookEvent::InstructionsLoaded => crate::ui_text::pick("指令文件加载", "Instructions loaded"),
        HookEvent::UserPromptSubmit => crate::ui_text::pick("用户发送消息", "User prompt submitted"),
        HookEvent::PreToolUse => crate::ui_text::pick("工具执行前", "Before a tool runs"),
        HookEvent::PermissionRequest => {
            crate::ui_text::pick("工具请求权限时", "Tool permission request")
        }
        HookEvent::PostToolUse => crate::ui_text::pick("工具执行后", "After a tool ran"),
        HookEvent::Stop => crate::ui_text::pick("回合结束前", "Before the turn stops"),
    };
    match definition.matcher.as_deref() {
        Some(matcher) => format!("{event} · matcher {matcher}"),
        None => event.to_owned(),
    }
}

/// The model-facing description of a hook: its event, plus its matcher when it
/// has one, in the profile's words.
fn hook_description(profile: &PromptProfile, definition: &HookDefinition) -> String {
    let mut description = profile.text(hook_event_key(definition.event)).to_owned();
    if let Some(matcher) = definition.matcher.as_deref() {
        description
            .push_str(&profile.render(PromptKey::SystemHookMatcherDetail, &[("matcher", matcher)]));
    }
    description
}

fn extend_unique(target: &mut Vec<String>, values: &[String]) {
    for value in values {
        if !target.iter().any(|existing| existing == value) {
            target.push(value.clone());
        }
    }
}

/// Reads a skill body. Symlinks and reparse points are rejected: following one would
/// delegate the catalog snapshot's trust boundary to the link owner. A built-in
/// skill's body is compiled in.
fn read_skill_body(descriptor: &ResourceDescriptor) -> Result<String, String> {
    if let Some(source) = skills::builtin_manifest(descriptor) {
        return Ok(source);
    }
    let path = Path::new(&descriptor.location);
    let bytes = memory_archive_file::read_bounded_nofollow_labeled(
        path,
        SKILL_READ_LIMIT as usize,
        "skill",
    )
    .map_err(|error| format!("Could not read skill {}: {error}", descriptor.name))?;
    String::from_utf8(bytes).map_err(|_| {
        format!(
            "Skill {}'s SKILL.md is not valid UTF-8 text",
            descriptor.name
        )
    })
}

/// Metadata for `SKILL.md`. Frontmatter takes precedence; missing fields are inferred
/// from the first heading and paragraph in the body.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub author: String,
    pub version: String,
    pub tags: Vec<String>,
}

/// Parses a `SKILL.md` body.
///
/// Installation and catalog display share this parser so skill names cannot diverge.
/// The model-facing parser uses the same scan with [`SKILL_TRIGGER_LIMIT`].
///
/// This is not a full YAML parser: it reads top-level `key: value` pairs, a value
/// may continue on indented lines or be a `|` / `>` block scalar as Claude Code's
/// YAML reader takes it, tags may be `[a, b]`, `a, b` or a `- a` list, and
/// unknown keys are ignored. A leading byte-order mark is not part of the file.
pub fn skill_metadata_from_source(source: &str, fallback_name: &str) -> SkillMetadata {
    let source = crate::config_file::text_without_bom(source);
    let lines = source.lines().collect::<Vec<_>>();
    let parsed = scan_skill_frontmatter(&lines);
    let capped = |value: String| truncate_chars(&value, METADATA_VALUE_LIMIT);

    SkillMetadata {
        name: parsed
            .name
            .or_else(|| inferred_skill_name(&lines, parsed.body_start))
            .map(capped)
            .unwrap_or_else(|| fallback_name.to_owned()),
        description: parsed
            .description
            .or_else(|| inferred_skill_description(&lines, parsed.body_start))
            .map(capped)
            .unwrap_or_default(),
        author: parsed.author.map(capped).unwrap_or_default(),
        version: parsed.version.map(capped).unwrap_or_default(),
        tags: parsed.tags,
    }
}

/// The two model-facing parts of a `SKILL.md`.
///
/// They are a second view of the text used by [`SkillMetadata`]. Catalog metadata and
/// model context have different length limits and frontmatter requirements.
pub struct SkillDocument {
    /// Trigger text: frontmatter `description`, optionally followed by
    /// `{description} - {when_to_use}`. If both fields are absent, infer the first
    /// body paragraph using the catalog rule.
    pub trigger: String,
    /// The body with frontmatter removed.
    ///
    /// Frontmatter indexes installation and catalog display, not model instructions;
    /// `name:`, `version:`, and `tags:` do not belong in model context.
    pub body: String,
}

/// Parses `SKILL.md` using the model-facing view. See [`SkillDocument`].
pub fn skill_document_from_source(source: &str) -> SkillDocument {
    let source = crate::config_file::text_without_bom(source);
    let lines = source.lines().collect::<Vec<_>>();
    let parsed = scan_skill_frontmatter(&lines);
    let description = parsed
        .description
        .or_else(|| inferred_skill_description(&lines, parsed.body_start))
        .unwrap_or_default();
    // `when_to_use` supplements `description`; it replaces it only when the
    // description is absent.
    let trigger = match parsed.when_to_use {
        Some(when) if !description.is_empty() => format!("{description} - {when}"),
        Some(when) => when,
        None => description,
    };
    SkillDocument {
        trigger: truncate_chars(trigger.trim(), SKILL_TRIGGER_LIMIT),
        body: lines[parsed.body_start..].join("\n").trim().to_owned(),
    }
}

/// Untruncated frontmatter values and the body start line.
///
/// Do not truncate during scanning: metadata and trigger consumers have different
/// limits, and scan-time truncation would impose the stricter limit on both.
#[derive(Default)]
struct SkillFrontmatter {
    name: Option<String>,
    description: Option<String>,
    when_to_use: Option<String>,
    author: Option<String>,
    version: Option<String>,
    /// Tags are consumed only by catalog display, so truncate them here.
    tags: Vec<String>,
    body_start: usize,
}

fn scan_skill_frontmatter(lines: &[&str]) -> SkillFrontmatter {
    let mut parsed = SkillFrontmatter::default();
    if !lines.first().is_some_and(|line| line.trim() == "---") {
        return parsed;
    }
    let Some(frontmatter_end) = lines
        .iter()
        .enumerate()
        .skip(1)
        .find_map(|(index, line)| (line.trim() == "---").then_some(index))
    else {
        return parsed;
    };
    let block = &lines[1..frontmatter_end];
    let mut index = 0;
    while index < block.len() {
        let line = block[index];
        index += 1;
        // A key starts at the beginning of its line; the indented and blank
        // lines under it are the rest of its value.
        let start = index;
        while index < block.len() && is_continuation_line(block[index]) {
            index += 1;
        }
        let continuation = &block[start..index];
        if let Some(value) = line.strip_prefix("name:") {
            parsed.name = scalar_value(value, continuation);
        } else if let Some(value) = line.strip_prefix("description:") {
            parsed.description = scalar_value(value, continuation);
        } else if let Some(value) = line.strip_prefix("when_to_use:") {
            parsed.when_to_use = scalar_value(value, continuation);
        } else if let Some(value) = line.strip_prefix("author:") {
            parsed.author = scalar_value(value, continuation);
        } else if let Some(value) = line.strip_prefix("version:") {
            parsed.version = scalar_value(value, continuation);
        } else if let Some(value) = line.strip_prefix("tags:") {
            parsed.tags = parse_metadata_tags(value, continuation);
        }
    }
    parsed.body_start = frontmatter_end + 1;
    parsed
}

/// A line that belongs to the value of the key above it: indented, or blank.
fn is_continuation_line(line: &str) -> bool {
    line.trim().is_empty() || line.starts_with([' ', '\t'])
}

/// How far a line is indented, in characters of leading space or tab.
fn indentation(line: &str) -> usize {
    line.len() - line.trim_start_matches([' ', '\t']).len()
}

/// The value of one key: what follows the colon, plus the lines under it.
///
/// `|` keeps the lines as written and `>` folds them into one paragraph per
/// blank-line-separated group, as YAML block scalars do; any other value may
/// continue on indented lines, which join it with a space. Trailing line breaks
/// are dropped whatever the chomping indicator says, because every reader of
/// these values trims them.
fn scalar_value(inline: &str, continuation: &[&str]) -> Option<String> {
    let header = inline.trim();
    if let Some(style) = block_scalar_style(header) {
        let explicit = header[1..]
            .chars()
            .take_while(|character| !character.is_whitespace())
            .find_map(|character| character.to_digit(10))
            .map(|digit| digit as usize);
        let indent = explicit.unwrap_or_else(|| {
            continuation
                .iter()
                .find(|line| !line.trim().is_empty())
                .map(|line| indentation(line))
                .unwrap_or(0)
        });
        let lines = continuation
            .iter()
            .map(|line| {
                if line.trim().is_empty() {
                    ""
                } else {
                    &line[indentation(line).min(indent)..]
                }
            })
            .collect::<Vec<_>>();
        let value = match style {
            BlockScalar::Literal => lines.join("\n"),
            BlockScalar::Folded => fold_lines(&lines),
        };
        let value = value.trim_end_matches('\n');
        return (!value.trim().is_empty()).then(|| value.to_owned());
    }
    let mut value = header.to_owned();
    let mut blank_lines = 0;
    for line in continuation {
        let line = line.trim();
        if line.is_empty() {
            blank_lines += 1;
            continue;
        }
        if !value.is_empty() {
            if blank_lines > 0 {
                value.push_str(&"\n".repeat(blank_lines));
            } else {
                value.push(' ');
            }
        }
        value.push_str(line);
        blank_lines = 0;
    }
    raw_metadata_value(&value)
}

#[derive(Clone, Copy)]
enum BlockScalar {
    Literal,
    Folded,
}

/// `|` or `>`, followed by at most a chomping and an indentation indicator and
/// then nothing but a comment.
fn block_scalar_style(header: &str) -> Option<BlockScalar> {
    let style = match header.chars().next()? {
        '|' => BlockScalar::Literal,
        '>' => BlockScalar::Folded,
        _ => return None,
    };
    let rest = &header[1..];
    let indicators = rest
        .chars()
        .take_while(|character| matches!(character, '+' | '-' | '1'..='9'))
        .count();
    let after = rest[indicators..].trim_start();
    (indicators <= 2 && (after.is_empty() || after.starts_with('#'))).then_some(style)
}

/// YAML's folding: the lines of one paragraph join with a space, a blank line
/// becomes a line break, and lines indented past the block keep their breaks.
fn fold_lines(lines: &[&str]) -> String {
    let mut folded = String::new();
    // Whether the previous text line was a plain one; only two plain lines fold.
    let mut previous_plain: Option<bool> = None;
    let mut blank_lines = 0;
    for line in lines {
        if line.is_empty() {
            blank_lines += 1;
            continue;
        }
        let plain = !line.starts_with([' ', '\t']);
        match previous_plain {
            None => folded.push_str(&"\n".repeat(blank_lines)),
            Some(true) if plain => {
                if blank_lines == 0 {
                    folded.push(' ');
                } else {
                    folded.push_str(&"\n".repeat(blank_lines));
                }
            }
            Some(_) => folded.push_str(&"\n".repeat(blank_lines + 1)),
        }
        folded.push_str(line);
        previous_plain = Some(plain);
        blank_lines = 0;
    }
    folded
}

fn inferred_skill_name(lines: &[&str], body_start: usize) -> Option<String> {
    lines[body_start..]
        .iter()
        .find_map(|line| line.trim().strip_prefix("# "))
        .and_then(raw_metadata_value)
}

fn inferred_skill_description(lines: &[&str], body_start: usize) -> Option<String> {
    lines[body_start..]
        .iter()
        .map(|line| line.trim())
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
}

fn parse_metadata_tags(value: &str, continuation: &[&str]) -> Vec<String> {
    let trimmed = value.trim();
    // A block sequence: `tags:` alone, then one `- tag` per line.
    if trimmed.is_empty() {
        return continuation
            .iter()
            .filter_map(|line| line.trim().strip_prefix('-'))
            .filter_map(nonempty_metadata_value)
            .take(16)
            .collect();
    }
    let trimmed = trimmed.trim_start_matches('[').trim_end_matches(']');
    trimmed
        .split(',')
        .filter_map(nonempty_metadata_value)
        .take(16)
        .collect()
}

/// Trim whitespace and paired quotes without truncating.
fn raw_metadata_value(value: &str) -> Option<String> {
    let value = value.trim().trim_matches(['\'', '"']);
    (!value.is_empty()).then(|| value.to_owned())
}

fn nonempty_metadata_value(value: &str) -> Option<String> {
    raw_metadata_value(value).map(|value| truncate_chars(&value, METADATA_VALUE_LIMIT))
}

fn truncate_chars(value: &str, limit: usize) -> String {
    let mut chars = value.chars();
    let result = chars.by_ref().take(limit).collect::<String>();
    if chars.next().is_some() {
        format!("{result}…")
    } else {
        result
    }
}

fn preferred_config_path(base: &Path, relative: &Path) -> PathBuf {
    let primary = base.join(CONFIG_DIRECTORY).join(relative);
    if primary.exists() {
        primary
    } else {
        base.join(LEGACY_CONFIG_DIRECTORY).join(relative)
    }
}

/// Parsed tool-description file (prompt profile): display name, per-tool
/// entries and prompt overrides.
///
/// Unknown fields do not appear here because the application never writes these files;
/// their on-disk source remains authoritative.
pub struct ToolDescriptionDocument {
    pub name: String,
    pub entries: Vec<ToolDescriptionEntry>,
    /// `prompts` overrides keyed by injection point; unknown ids are dropped.
    pub prompts: HashMap<PromptKey, String>,
}

const TOOL_DESCRIPTION_FILE_LABEL: &str = "工具描述";
const MAX_TOOL_DESCRIPTION_NAME_CHARS: usize = 120;

/// The scope a tool-description file's id is minted under. The files are global
/// only, so there is one scope; an id minted under a workspace scope by an older
/// version matches nothing and dangles.
const TOOL_DESCRIPTION_ID_SCOPE: &str = "tooldesc_user";

/// Parses a tool-description JSON file into entries. Accept either a top-level array
/// or `{"tools": [...]}`; an entry's text is its `description`, or — when that is
/// missing or blank — the `schemaNotes` an older file called it. Drop entries
/// whose text is blank, and retain only the first occurrence of each name. Any
/// other key of an entry — an older file's `usageGuidance` — is ignored, so such
/// a file keeps loading.
///
/// A name is reserved only once an entry actually carries an override. The
/// authoring scaffold ships a blank row per tool, so reserving names before the
/// blank check would let those rows shadow every populated row a user appends
/// after them — the file would parse, be selectable, and change nothing.
///
/// The editable built-in profiles use the same `tools` shape, so they parse
/// through this function rather than a second one that could drift from it.
pub(crate) fn parse_tool_description_entries(value: &Value) -> Vec<ToolDescriptionEntry> {
    let items = value
        .get("tools")
        .and_then(Value::as_array)
        .or_else(|| value.as_array());
    let Some(items) = items else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    let mut entries = Vec::new();
    for item in items {
        let Some(record) = item.as_object() else {
            continue;
        };
        let tool_name = record
            .get("toolName")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned();
        if tool_name.is_empty() {
            continue;
        }
        // `schemaNotes` is the field's old name: a file written for it keeps
        // working, and an entry carrying both takes `description`.
        let description = ["description", "schemaNotes"]
            .into_iter()
            .filter_map(|key| record.get(key).and_then(Value::as_str))
            .find(|text| !text.trim().is_empty())
            .unwrap_or_default()
            .to_owned();
        if description.trim().is_empty() {
            continue;
        }
        if !seen.insert(tool_name.clone()) {
            continue;
        }
        entries.push(ToolDescriptionEntry {
            tool_name,
            description,
        });
    }
    entries
}

/// Reads a tool-description file. Symlinks and reparse points are rejected so an
/// entry that can be selected is also eligible for the matching no-follow save path.
fn read_tool_description_document(
    path: &Path,
    maximum_bytes: usize,
) -> Result<ToolDescriptionDocument, String> {
    let bytes = memory_archive_file::read_bounded_nofollow_labeled(
        path,
        maximum_bytes,
        TOOL_DESCRIPTION_FILE_LABEL,
    )?;
    let value: Value = crate::config_file::parse_json(&bytes)
        .map_err(|_| format!("{TOOL_DESCRIPTION_FILE_LABEL}文件不是有效的 JSON"))?;
    let fallback_name = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "tool-descriptions".to_owned());
    // The display name comes from content, while `stable_id` hashes the location.
    // Renaming content preserves selections; moving a file intentionally changes its ID.
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| truncate_chars(name, MAX_TOOL_DESCRIPTION_NAME_CHARS))
        .unwrap_or(fallback_name);
    let entries = parse_tool_description_entries(&value);
    let prompts = prompt_profile::parse_prompt_overrides(&value);
    Ok(ToolDescriptionDocument {
        name,
        entries,
        prompts,
    })
}

/// Whether a `tools[]` entry can reach anything at run time.
///
/// A built-in tool is addressed by its exact catalog name; an MCP tool by the
/// `mcp__server__tool` name the model sees. Anything else — a retired name, a
/// display label, the wrong case — parses fine and then matches nothing, so the
/// catalog entry says so rather than counting it as an override that works.
fn tool_description_entry_is_addressable(tool_name: &str) -> bool {
    let name = tool_name.trim();
    PromptKey::for_tool_description(name).is_some() || name.starts_with("mcp__")
}

fn tool_description_descriptor(path: &Path, location: String) -> ResourceDescriptor {
    let document = read_tool_description_document(path, METADATA_READ_LIMIT as usize).ok();
    let fallback_name = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "tool-descriptions".to_owned());
    let (tool_count, unmatched, prompt_count) = document
        .as_ref()
        .map(|document| {
            let unmatched = document
                .entries
                .iter()
                .filter(|entry| !tool_description_entry_is_addressable(&entry.tool_name))
                .count();
            (document.entries.len(), unmatched, document.prompts.len())
        })
        .unwrap_or((0, 0, 0));
    ResourceDescriptor {
        // Hash the file stem, not the display name, so editing the name preserves its ID.
        id: stable_id(TOOL_DESCRIPTION_ID_SCOPE, &fallback_name, &location),
        name: document
            .as_ref()
            .map(|document| document.name.clone())
            .unwrap_or(fallback_name),
        description: if tool_count == 0 && prompt_count == 0 {
            "未解析出可用条目".to_owned()
        } else if unmatched > 0 {
            format!(
                "{tool_count} 个工具描述（{unmatched} 个工具名无法匹配，不会生效） · {prompt_count} 条提示词覆盖"
            )
        } else {
            format!("{tool_count} 个工具描述 · {prompt_count} 条提示词覆盖")
        },
        location,
        source: ResourceSource::User,
        available: tool_count > 0 || prompt_count > 0,
        workspace_key: None,
    }
}

/// Scans all `*.json` files in the global `tool-descriptions/` directory. Each
/// file is one complete tool-description table, parallel to a skill directory or
/// MCP entry.
fn scan_tool_description_root(root: &Path, output: &mut Vec<ResourceDescriptor>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // Use `symlink_metadata`, not `is_file`: the latter follows links and would
        // list an entry that the write path must later reject.
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_file()
            || !path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
        {
            continue;
        }
        let location = path.to_string_lossy().into_owned();
        output.push(tool_description_descriptor(&path, location));
    }
}

/// Resolves the conversation's selected tool-description file into the prompt
/// profile the run renders with.
///
/// No selection, the built-in id, a dangling id, or an unreadable file all
/// resolve to the built-in profile — the one that is always present, compiled
/// into this build. A selected file declares no language of its own, so it
/// carries the application language; the keys it does not override keep the
/// built-in English wording.
pub fn resolve_prompt_profile(
    document: &AppDocument,
    conversation: &Conversation,
    _app_data: &Path,
) -> PromptProfile {
    prompt_profile_at(
        ConfigLevel::user().as_ref().map(|level| level.base.as_path()),
        document,
        conversation,
    )
}

/// [`resolve_prompt_profile`] for the global level at `home`. The folder is
/// scanned only when a file is selected: a run with the built-in profile reads
/// nothing.
fn prompt_profile_at(
    home: Option<&Path>,
    document: &AppDocument,
    conversation: &Conversation,
) -> PromptProfile {
    let app_language = document.global_settings.resolved_app_language;
    let Some(selected) = conversation
        .settings
        .tool_description_file_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty() && *id != prompt_profile::BUILTIN_EN_US_ID)
    else {
        return PromptProfile::builtin_english();
    };
    tool_description_files_under(home)
        .iter()
        .find(|descriptor| descriptor.id == selected)
        .and_then(|descriptor| {
            read_tool_description_document(
                Path::new(&descriptor.location),
                METADATA_READ_LIMIT as usize,
            )
            .ok()
            .map(|file| {
                PromptProfile::from_file(
                    descriptor.id.clone(),
                    file.name,
                    app_language,
                    file.prompts,
                    file.entries,
                )
            })
        })
        .unwrap_or_else(PromptProfile::builtin_english)
}

/*
 * Tool-description files are read-only. The application does not create roots,
 * resolve IDs to write paths, write, or delete files; it discovers, selects, and
 * rereads them from the global `~/.mewrk/tool-descriptions/` for trusted
 * requests. It only reveals that folder, so a user can add files to it.
 */

/// Windows paths are case-insensitive and treat `\` and `/` interchangeably. Fold
/// both degrees of freedom before hashing so one resource retains its ID; preserve
/// the original location string for display.
pub(crate) fn normalized_location_for_id(location: &str) -> String {
    location.replace('\\', "/").to_lowercase()
}

/// Hashes a location exactly as supplied. Callers choose raw locations when matching
/// legacy IDs and normalized locations when minting current IDs.
pub(crate) fn location_id_hash(location: &str) -> u32 {
    let mut hasher = DefaultHasher::new();
    location.hash(&mut hasher);
    hasher.finish() as u32
}

/// The id of a discovered resource: a prefix naming its kind and level, a slug
/// of its name for legibility, and a hash of where it was read from. Skills,
/// MCP servers, hooks and tool-description files all mint theirs here, so an
/// entry keeps its id across rescans as long as it stays where it is.
pub(crate) fn stable_id(prefix: &str, name: &str, location: &str) -> String {
    format!(
        "{prefix}_{}_{:08x}",
        id_slug(name),
        location_id_hash(&normalized_location_for_id(location))
    )
}

/// The legible middle of an id: `name` lowercased, with everything but ASCII
/// letters and digits turned into `_`.
fn id_slug(name: &str) -> String {
    let slug = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .to_owned();
    if slug.is_empty() {
        "resource".to_owned()
    } else {
        slug
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A project's workspace 1, as a level is looked up by it.
    fn primary(project: &Workspace) -> AttachedWorkspace {
        AttachedWorkspace {
            machine: project.machine.clone(),
            path: project.path.clone(),
        }
    }

    /// A scan result assembled by hand: the catalog rows plus the hook
    /// definitions behind them. MCP launch configurations are filled in by the
    /// tests that dial.
    fn discovered(
        catalog: CapabilityCatalog,
        hooks: HashMap<String, HookDefinition>,
    ) -> DiscoveredCapabilities {
        DiscoveredCapabilities {
            catalog,
            hooks,
            mcp_servers: HashMap::new(),
            remote_skills: HashMap::new(),
            level_errors: Vec::new(),
            agent_roles: Vec::new(),
            role_levels: Vec::new(),
        }
    }

    /// Windows can spell one path with different casing and separators; IDs must
    /// collapse both variations so references remain stable.
    #[test]
    fn stable_id_collapses_windows_path_casing_and_separators() {
        let base = stable_id("skill_user", "Demo", "C:/Users/dev/skills/demo/SKILL.md");
        for variant in [
            "c:/users/dev/skills/demo/skill.md",
            "C:\\Users\\dev\\skills\\demo\\SKILL.md",
            "c:\\USERS\\Dev\\Skills\\Demo\\Skill.MD",
        ] {
            assert_eq!(stable_id("skill_user", "Demo", variant), base);
        }
        // Distinct locations must still produce distinct IDs.
        assert_ne!(
            stable_id("skill_user", "Demo", "C:/Users/dev/skills/other/SKILL.md"),
            base
        );
        // Legacy-ID recognition requires a mixed-case raw hash to differ from the
        // normalized hash.
        assert_ne!(
            location_id_hash("C:\\Users\\dev\\skills\\demo\\SKILL.md"),
            location_id_hash(&normalized_location_for_id(
                "C:\\Users\\dev\\skills\\demo\\SKILL.md"
            ))
        );
    }

    #[test]
    fn metadata_prefers_frontmatter() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("SKILL.md");
        fs::write(
            &path,
            "---\nname: Example Skill\ndescription: Does useful things\n---\n# Ignored\nBody",
        )
        .unwrap();
        let source = fs::read_to_string(&path).unwrap();
        let metadata = skill_metadata_from_source(&source, "fallback");
        assert_eq!(metadata.name, "Example Skill");
        assert_eq!(metadata.description, "Does useful things");
    }

    /// A byte-order mark is not text: the frontmatter behind it is still one.
    #[test]
    fn a_byte_order_mark_does_not_turn_the_frontmatter_into_body() {
        let plain = "---\nname: Example\ndescription: Does things\nwhen_to_use: Asked to\n---\n\nBODY\n";
        let marked = format!("\u{feff}{plain}");
        assert_eq!(
            skill_metadata_from_source(&marked, "fallback"),
            skill_metadata_from_source(plain, "fallback")
        );
        let document = skill_document_from_source(&marked);
        assert_eq!(document.trigger, "Does things - Asked to");
        assert_eq!(document.body, "BODY");
    }

    /// `|` keeps the lines, `>` folds them, and a plain value may continue on
    /// indented lines — all read whole, as Claude Code's YAML reader reads them.
    #[test]
    fn multi_line_descriptions_are_read_whole() {
        let source = "---\nname: Deploy\ndescription: |\n  Deploys the app.\n  Use it after tests pass.\nwhen_to_use: >-\n  The user asks to\n  ship it.\n\n  Or to release.\ntags:\n  - ops\n  - release\nversion: 1.0\n---\n\nBODY\n";
        let metadata = skill_metadata_from_source(source, "fallback");
        assert_eq!(metadata.name, "Deploy");
        assert_eq!(
            metadata.description,
            "Deploys the app.\nUse it after tests pass."
        );
        assert_eq!(metadata.tags, ["ops", "release"]);
        assert_eq!(metadata.version, "1.0");
        let document = skill_document_from_source(source);
        assert_eq!(
            document.trigger,
            "Deploys the app.\nUse it after tests pass. - The user asks to ship it.\nOr to release."
        );
        assert_eq!(document.body, "BODY");

        let folded = "---\ndescription: >\n  One\n  paragraph.\n\n    kept as is\n  Two.\nwhen_to_use: first part\n  continued here\n---\nBODY";
        let document = skill_document_from_source(folded);
        assert_eq!(
            document.trigger,
            "One paragraph.\n\n  kept as is\nTwo. - first part continued here"
        );

        // A block that says nothing leaves the description to be inferred.
        let empty = "---\ndescription: |\nname: Empty\n---\nFirst paragraph.";
        let metadata = skill_metadata_from_source(empty, "fallback");
        assert_eq!(metadata.name, "Empty");
        assert_eq!(metadata.description, "First paragraph.");
    }

    /// Prompt profiles resolve from the selected resource and retain a file's tool entries.
    #[test]
    fn selected_prompt_profile_file_preserves_entries_and_overrides() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("home");
        let root = home.join(".mewrk").join("tool-descriptions");
        fs::create_dir_all(&root).unwrap();
        // An older file's `usageGuidance` is an unknown key: it neither makes an
        // entry nor survives on one that has a `description`. `grep` is written
        // under the field's old name, `schemaNotes`, which still loads.
        fs::write(
            root.join("strict.json"),
            r#"{
              "name": "strict",
              "prompts": {"system.mcp_section": "Custom MCP section: {servers}"},
              "tools": [
                {"toolName": "ls", "description": "Paths must be absolute."},
                {"toolName": "read", "description": "", "usageGuidance": "Search before reading."},
                {"toolName": "grep", "schemaNotes": "Regex search.", "usageGuidance": "Ignored."},
                {"toolName": "", "description": "An empty name is dropped."},
                {"toolName": "write", "description": "  ", "usageGuidance": "  "}
              ]
            }"#,
        )
        .unwrap();

        let mut document = crate::catalog::default_document();
        let catalog = tool_description_files_under(Some(&home));
        assert_eq!(catalog.len(), 2);
        // The built-in comes first, under a `builtin:` pseudo-location, and
        // names no path: there is nothing on disk to edit.
        let builtin = &catalog[0];
        assert_eq!(builtin.id, prompt_profile::BUILTIN_EN_US_ID);
        assert_eq!(builtin.name, "Mewrk built-in");
        assert_eq!(builtin.source, ResourceSource::Builtin);
        assert!(builtin.location.starts_with("builtin:"));
        assert!(!builtin.description.contains(&*directory.path().to_string_lossy()));
        assert_eq!(catalog[1].name, "strict");
        let found = &catalog[1];
        assert_eq!(found.description, "2 个工具描述 · 1 条提示词覆盖");
        assert!(found.available);
        assert_eq!(found.source, ResourceSource::User);
        assert_eq!(found.workspace_key, None);
        assert!(found.id.starts_with("tooldesc_user_strict_"));

        document.workspaces[0].conversations[0]
            .settings
            .tool_description_file_id = Some(found.id.clone());
        let profile = prompt_profile_at(
            Some(&home),
            &document,
            &document.workspaces[0].conversations[0],
        );
        assert_eq!(profile.id, found.id);
        // A file declares no language of its own, so it takes the application
        // language; the keys it omits keep the built-in English wording.
        assert_eq!(
            document.global_settings.resolved_app_language,
            ResolvedLanguage::ZhCn
        );
        assert_eq!(profile.language, ResolvedLanguage::ZhCn);
        assert_eq!(
            profile.tools,
            [
                ToolDescriptionEntry {
                    tool_name: "ls".to_owned(),
                    description: "Paths must be absolute.".to_owned(),
                },
                ToolDescriptionEntry {
                    tool_name: "grep".to_owned(),
                    description: "Regex search.".to_owned(),
                },
            ]
        );
        assert_eq!(
            profile.text(PromptKey::SystemMcpSection),
            "Custom MCP section: {servers}"
        );
        assert_eq!(
            profile.text(PromptKey::SystemHooksSection),
            PromptKey::SystemHooksSection.builtin_en()
        );

        // Switching the application language changes the profile's language and
        // nothing else: the overrides and the English fallback stay as they are.
        document.global_settings.resolved_app_language = ResolvedLanguage::EnUs;
        let profile = prompt_profile_at(
            Some(&home),
            &document,
            &document.workspaces[0].conversations[0],
        );
        assert_eq!(profile.language, ResolvedLanguage::EnUs);
        assert_eq!(
            profile.text(PromptKey::SystemMcpSection),
            "Custom MCP section: {servers}"
        );
        assert_eq!(
            profile.text(PromptKey::SystemHooksSection),
            PromptKey::SystemHooksSection.builtin_en()
        );
    }

    #[test]
    fn prompt_profile_defaults_to_the_builtin_and_resolves_dangling_ids_to_it() {
        let directory = tempfile::tempdir().unwrap();
        let app_data = directory.path().join("app-data");
        let mut document = crate::catalog::default_document();
        let resolve = |document: &AppDocument| {
            resolve_prompt_profile(
                document,
                &document.workspaces[0].conversations[0],
                &app_data,
            )
        };
        let profile = resolve(&document);
        assert_eq!(profile, PromptProfile::builtin_english());

        // Selecting the built-in by id is the same as selecting nothing.
        document.workspaces[0].conversations[0]
            .settings
            .tool_description_file_id = Some(prompt_profile::BUILTIN_EN_US_ID.to_owned());
        assert_eq!(resolve(&document), PromptProfile::builtin_english());

        // A dangling id — a removed file, or the retired Chinese built-in's id
        // still saved on an older conversation — resolves to the built-in.
        for dangling in ["missing-profile", "tooldesc_builtin_zh_cn"] {
            document.workspaces[0].conversations[0]
                .settings
                .tool_description_file_id = Some(dangling.to_owned());
            assert_eq!(resolve(&document), PromptProfile::builtin_english());
        }

        // A file left under the application data directory is not read: the
        // built-in renders from the compiled texts.
        let stale = app_data.join("prompt-profiles").join("en-US.json");
        fs::create_dir_all(stale.parent().unwrap()).unwrap();
        fs::write(
            &stale,
            r#"{"name":"edited","prompts":{"system.capability_row":"ROW {name} {description}"},"tools":[]}"#,
        )
        .unwrap();
        document.workspaces[0].conversations[0]
            .settings
            .tool_description_file_id = None;
        let profile = resolve(&document);
        assert_eq!(profile, PromptProfile::builtin_english());
        assert_eq!(
            profile.text(PromptKey::SystemCapabilityRow),
            PromptKey::SystemCapabilityRow.builtin_en()
        );
    }

    /// Tool-description files are global only: a workspace's own
    /// `.mewrk/tool-descriptions` is never offered, and a selection an older
    /// version made from one dangles into the built-in profile.
    #[test]
    fn a_workspace_tool_description_file_is_not_discovered() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("home");
        let global = home.join(".mewrk").join("tool-descriptions");
        fs::create_dir_all(&global).unwrap();
        fs::write(
            global.join("global.json"),
            r#"{"name":"global","tools":[{"toolName":"ls","description":"Global wording."}]}"#,
        )
        .unwrap();
        let project = directory.path().join("project");
        let local = project.join(".mewrk").join("tool-descriptions");
        fs::create_dir_all(&local).unwrap();
        let local_file = local.join("project.json");
        fs::write(
            &local_file,
            r#"{"name":"project","tools":[{"toolName":"ls","description":"Project wording."}]}"#,
        )
        .unwrap();

        let mut document = crate::catalog::default_document();
        for workspace in &mut document.workspaces {
            workspace.path = project.to_string_lossy().into_owned();
        }

        // The scan of the global level finds its own file and nothing else.
        let files = tool_description_files_under(Some(&home));
        assert_eq!(
            files.iter().map(|file| file.name.as_str()).collect::<Vec<_>>(),
            ["Mewrk built-in", "global"]
        );

        // The catalog — whose global level is the real home — offers nothing
        // that lives under the workspace.
        let catalog = discover(&document, &directory.path().join("app-data"));
        assert!(catalog
            .tool_description_files
            .iter()
            .all(|file| !Path::new(&file.location).starts_with(&project)
                && file.source != ResourceSource::Workspace
                && file.workspace_key.is_none()));

        // A selection id an older version minted for the workspace's file
        // matches nothing now, and the run falls back to the built-in profile.
        let stale_id = stable_id(
            "tooldesc_workspace",
            "project",
            &local_file.to_string_lossy(),
        );
        document.workspaces[0].conversations[0]
            .settings
            .tool_description_file_id = Some(stale_id);
        assert_eq!(
            prompt_profile_at(
                Some(&home),
                &document,
                &document.workspaces[0].conversations[0]
            ),
            PromptProfile::builtin_english()
        );
    }

    /// The legacy `.naiword` folder is read only while `.mewrk` lacks one.
    #[test]
    fn the_legacy_tool_description_folder_is_a_fallback_for_the_global_one() {
        let write = |home: &Path, directory: &str, name: &str| {
            let root = home.join(directory).join("tool-descriptions");
            fs::create_dir_all(&root).unwrap();
            fs::write(
                root.join(format!("{name}.json")),
                format!(r#"{{"name":"{name}","tools":[{{"toolName":"ls","description":"x"}}]}}"#),
            )
            .unwrap();
        };
        let names = |home: &Path| {
            tool_description_files_under(Some(home))
                .into_iter()
                .skip(1)
                .map(|file| file.name)
                .collect::<Vec<_>>()
        };
        let directory = tempfile::tempdir().unwrap();

        let legacy_only = directory.path().join("legacy-only");
        write(&legacy_only, ".naiword", "legacy");
        assert_eq!(names(&legacy_only), ["legacy"]);

        let both = directory.path().join("both");
        write(&both, ".naiword", "legacy");
        write(&both, ".mewrk", "current");
        assert_eq!(names(&both), ["current"]);

        // No home, or one with no folder, offers the built-in profile alone.
        assert_eq!(tool_description_files_under(None).len(), 1);
        assert!(names(&directory.path().join("empty")).is_empty());
    }

    /// An entry is its `description`: an older file's `usageGuidance` neither
    /// makes an entry nor changes one.
    #[test]
    fn tool_description_entries_ignore_usage_guidance() {
        let entries = parse_tool_description_entries(&serde_json::json!({
            "tools": [
                {"toolName": "ls", "description": "Paths must be absolute.", "usageGuidance": "Old advice."},
                {"toolName": "read", "usageGuidance": "Search before reading."},
                {"toolName": "write", "description": "   ", "usageGuidance": "Blank notes."},
                {"toolName": "ls", "description": "A duplicate name is dropped."},
                {"toolName": "grep", "description": "Regex search."}
            ]
        }));
        assert_eq!(
            entries,
            [
                ToolDescriptionEntry {
                    tool_name: "ls".to_owned(),
                    description: "Paths must be absolute.".to_owned(),
                },
                ToolDescriptionEntry {
                    tool_name: "grep".to_owned(),
                    description: "Regex search.".to_owned(),
                },
            ]
        );
    }

    /// A file written for the field's old name, `schemaNotes`, keeps working: it
    /// is read where an entry has no `description`, a blank `description` does
    /// not hide it, and an entry carrying both takes `description`.
    #[test]
    fn tool_description_entries_still_read_the_legacy_schema_notes_name() {
        let entries = parse_tool_description_entries(&serde_json::json!({
            "tools": [
                {"toolName": "ls", "schemaNotes": "Legacy wording."},
                {"toolName": "read", "description": "New wording.", "schemaNotes": "Legacy wording."},
                {"toolName": "grep", "description": "  ", "schemaNotes": "Legacy fallback."},
                {"toolName": "write", "description": "", "schemaNotes": "  "},
                {"toolName": "edit", "description": 7, "schemaNotes": "Legacy for a non-string."},
                {"toolName": "ls", "description": "A duplicate name is dropped."}
            ]
        }));
        assert_eq!(
            entries,
            [
                ToolDescriptionEntry {
                    tool_name: "ls".to_owned(),
                    description: "Legacy wording.".to_owned(),
                },
                ToolDescriptionEntry {
                    tool_name: "read".to_owned(),
                    description: "New wording.".to_owned(),
                },
                ToolDescriptionEntry {
                    tool_name: "grep".to_owned(),
                    description: "Legacy fallback.".to_owned(),
                },
                ToolDescriptionEntry {
                    tool_name: "edit".to_owned(),
                    description: "Legacy for a non-string.".to_owned(),
                },
            ]
        );
    }

    /// Entries stored under the old field name still deserialize.
    #[test]
    fn a_stored_tool_description_entry_under_the_old_name_still_loads() {
        let entry: ToolDescriptionEntry =
            serde_json::from_str(r#"{"toolName":"ls","schemaNotes":"Legacy wording."}"#).unwrap();
        assert_eq!(entry.description, "Legacy wording.");
        let entry: ToolDescriptionEntry =
            serde_json::from_str(r#"{"toolName":"ls","description":"New wording."}"#).unwrap();
        assert_eq!(entry.description, "New wording.");
        assert_eq!(
            serde_json::to_value(&entry).unwrap(),
            serde_json::json!({"toolName": "ls", "description": "New wording."})
        );
    }

    /// The tool-descriptions folder can be revealed, for the global level only,
    /// under the wire name the renderer sends.
    #[test]
    fn the_tool_descriptions_folder_reveals_at_the_global_level_only() {
        let parse = |name: &str| serde_json::from_str::<CapabilityKind>(&format!("\"{name}\""));
        assert_eq!(parse("toolDescriptions").unwrap(), CapabilityKind::ToolDescriptions);
        assert!(parse("tooldescriptions").is_err());
        assert!(parse("tool-descriptions").is_err());
        for (name, kind) in [
            ("skills", CapabilityKind::Skills),
            ("mcp", CapabilityKind::Mcp),
            ("hooks", CapabilityKind::Hooks),
            ("lsp", CapabilityKind::Lsp),
        ] {
            assert_eq!(parse(name).unwrap(), kind);
        }

        // A workspace has no such folder; the refusal comes before any level
        // is looked up, so nothing is created.
        let document = crate::catalog::default_document();
        assert!(capability_location_to_reveal(
            &document,
            CapabilityKind::ToolDescriptions,
            Some("local\u{0}/some/workspace"),
        )
        .is_err_and(|message| !message.is_empty()));

        let directory = tempfile::tempdir().unwrap();
        let level = |name: &str| ConfigLevel {
            source: ResourceSource::User,
            base: directory.path().join(name),
            workspace: None,
            remote: None,
        };
        let reveal = |level: &ConfigLevel| {
            reveal_local_location(level, CapabilityKind::ToolDescriptions, ".mewrk/tool-descriptions")
        };

        // Nothing there yet: `.mewrk/tool-descriptions` is created to open.
        let fresh = level("fresh");
        fs::create_dir_all(&fresh.base).unwrap();
        let created = fresh.base.join(".mewrk").join("tool-descriptions");
        assert!(!created.exists());
        assert_eq!(reveal(&fresh).unwrap(), RevealTarget::Local(created.clone()));
        assert!(created.is_dir());
        assert_eq!(reveal(&fresh).unwrap(), RevealTarget::Local(created));

        // The legacy folder is opened while it is the only one, as discovery
        // reads it, and nothing new is created beside it.
        let legacy = level("legacy");
        let legacy_folder = legacy.base.join(".naiword").join("tool-descriptions");
        fs::create_dir_all(&legacy_folder).unwrap();
        assert_eq!(reveal(&legacy).unwrap(), RevealTarget::Local(legacy_folder));
        assert!(!legacy.base.join(".mewrk").exists());
    }

    /// The app ships exactly one skill, Mewrk SDK, and selects it nowhere: the
    /// scan lists it as the only built-in entry (the global level is the real
    /// `~/.mewrk`, so nothing is said about the rest), and the default
    /// conversation selects no skill, server or hook and runs with none.
    #[test]
    fn discovery_ships_only_the_mewrk_sdk_skill_and_selects_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let mut document = crate::catalog::default_document();
        for workspace in &mut document.workspaces {
            workspace.path = directory.path().to_string_lossy().into_owned();
        }
        let conversation = &document.workspaces[0].conversations[0];
        assert!(conversation.settings.skill_ids.is_empty());
        assert!(conversation.settings.mcp_ids.is_empty());
        assert!(conversation.settings.hook_ids.is_empty());

        let catalog = discover(&document, &directory.path().join("app-data"));
        let builtin = catalog
            .skills
            .iter()
            .filter(|skill| skill.source == ResourceSource::Builtin)
            .collect::<Vec<_>>();
        assert_eq!(builtin.len(), 1);
        assert_eq!(builtin[0].id, skills::MEWRK_SDK_ID);
        assert_eq!(builtin[0].name, "Mewrk SDK");
        assert!(builtin[0].available);
        assert!(builtin[0].workspace_key.is_none());
        assert_eq!(skills::directory_name_of(builtin[0]), "mewrk-sdk");

        let profile = PromptProfile::builtin_english();
        let context = runtime_context_from_discovery(
            conversation,
            &discovered(catalog, HashMap::new()),
            &profile, &WorkspacePlaces::default(),
        )
        .unwrap();
        assert!(context.addendum.is_empty());
        assert!(context.skills.is_empty() && context.added_skills.is_empty());
    }

    /// Selected, the built-in skill resolves like a folder would — pasted into
    /// the prompt, or loaded by the `skill` tool under its directory name —
    /// from the compiled text, stamped with this build's version, and with no
    /// directory to hand the model.
    #[test]
    fn the_mewrk_sdk_skill_resolves_from_the_compiled_text_in_both_modes() {
        let mut document = crate::catalog::default_document();
        document.workspaces[0].conversations[0].settings.skill_ids =
            vec![skills::MEWRK_SDK_ID.to_owned()];
        let catalog = CapabilityCatalog {
            hooks: Vec::new(),
            skills: skills::builtin_skills(ResolvedLanguage::ZhCn),
            mcps: Vec::new(),
            lsps: Vec::new(),
            tool_description_files: Vec::new(),
            agents: Vec::new(),
            unreadable_levels: Vec::new(),
        };
        assert!(catalog.skills[0].description.contains("内置"));
        let profile = PromptProfile::builtin_english();
        let version = format!("Mewrk {}", env!("CARGO_PKG_VERSION"));

        document.workspaces[0].conversations[0]
            .settings
            .skill_tool_enabled = false;
        let pasted = runtime_context_from_discovery(
            &document.workspaces[0].conversations[0],
            &discovered(catalog.clone(), HashMap::new()),
            &profile, &WorkspacePlaces::default(),
        )
        .unwrap();
        assert!(
            pasted.addendum.starts_with("# Mewrk SDK"),
            "{}",
            pasted.addendum
        );
        assert!(pasted.addendum.contains(&version));
        assert!(!pasted.addendum.contains("{{MEWRK_VERSION}}"));

        document.workspaces[0].conversations[0]
            .settings
            .skill_tool_enabled = true;
        let on_demand = runtime_context_from_discovery(
            &document.workspaces[0].conversations[0],
            &discovered(catalog, HashMap::new()),
            &profile, &WorkspacePlaces::default(),
        )
        .unwrap();
        assert_eq!(on_demand.skills.len(), 1);
        let skill = &on_demand.skills[0];
        assert_eq!(skill.name, "mewrk-sdk");
        assert!(skill.trigger.starts_with("How to configure Mewrk itself"));
        assert!(skill.body.contains(&version));
        assert!(!skill.directory.contains("builtin:"), "{}", skill.directory);
        assert!(on_demand
            .addendum
            .contains("- mewrk-sdk: How to configure Mewrk itself"));
    }

    #[test]
    fn selected_workspace_skill_mcp_and_hook_are_assembled_in_profile_words() {
        let directory = tempfile::tempdir().unwrap();
        let skill_path = directory.path().join("SKILL.md");
        fs::write(&skill_path, "# Probe skill\n\nMEWRK_SKILL_BODY_E2E").unwrap();
        let mut document = crate::catalog::default_document();
        document.workspaces[0].conversations[0].settings.skill_ids = vec!["skill-probe".into()];
        document.workspaces[0].conversations[0].settings.mcp_ids = vec!["mcp-probe".into()];
        document.workspaces[0].conversations[0].settings.hook_ids = vec!["hook-probe".into()];
        let catalog = CapabilityCatalog {
            lsps: Vec::new(),
            hooks: vec![ResourceDescriptor {
                id: "hook-probe".into(),
                name: "Probe Hook".into(),
                description: "display-only hook description".into(),
                location: "~/.naiword/hooks.json#/hooks/probe".into(),
                source: ResourceSource::User,
                available: true,
                workspace_key: None,
            }],
            skills: vec![ResourceDescriptor {
                id: "skill-probe".into(),
                name: "Probe Skill".into(),
                description: "test skill".into(),
                location: skill_path.to_string_lossy().into_owned(),
                source: ResourceSource::Workspace,
                available: true,
                workspace_key: None,
            }],
            mcps: vec![ResourceDescriptor {
                id: "mcp-probe".into(),
                name: "Probe MCP".into(),
                description: "stdio test server".into(),
                location: "~/.naiword/mcp.json#Probe MCP".into(),
                source: ResourceSource::User,
                available: true,
                workspace_key: None,
            }],
            tool_description_files: Vec::new(),
            agents: Vec::new(),
            unreadable_levels: Vec::new(),
        };
        let hook_definitions = HashMap::from([(
            "hook-probe".to_owned(),
            HookDefinition {
                id: "hook-probe".into(),
                name: "Probe Hook".into(),
                event: HookEvent::PreToolUse,
                matcher: None,
                command: "npm test".into(),
                command_windows: None,
                status_message: None,
                enabled: true,
                timeout_ms: 30_000,
                on_machine: None,
                workspace_key: None,
                member: None,
                local_place: None,
            },
        )]);
        let conversation = &document.workspaces[0].conversations[0];
        let english = PromptProfile::builtin_english();
        let scan = || DiscoveredCapabilities {
            mcp_servers: HashMap::from([(
                "mcp-probe".to_owned(),
                McpServerConfig {
                    id: "mcp-probe".into(),
                    name: "Probe MCP".into(),
                    description: "stdio test server".into(),
                    command: "node".into(),
                    ..Default::default()
                },
            )]),
            ..discovered(catalog.clone(), hook_definitions.clone())
        };

        let context = runtime_context_from_discovery(conversation, &scan(), &english, &WorkspacePlaces::default()).unwrap();
        let addendum = context.addendum;

        let skill_position = addendum.find("MEWRK_SKILL_BODY_E2E").unwrap();
        let mcp_position = addendum.find("## Selected MCP servers").unwrap();
        let hook_position = addendum.find("## Lifecycle hooks").unwrap();
        assert!(skill_position < mcp_position && mcp_position < hook_position);
        assert!(addendum.contains("- Probe MCP: stdio test server"));
        assert!(addendum.contains("- Probe Hook: Before a tool runs"));
        assert!(addendum
            .contains("Their tools can be called only when the host exposed them to this turn"));
        assert!(!addendum.contains("npm test"));
        // The same scan that wrote the section is what gets dialed.
        assert_eq!(context.mcp_servers.len(), 1);
        assert_eq!(context.mcp_servers[0].server_id, "mcp-probe");

        // A tool-description file rewords every one of those texts.
        let file = PromptProfile::from_file(
            "file".into(),
            "File".into(),
            ResolvedLanguage::ZhCn,
            HashMap::from([
                (
                    PromptKey::SystemMcpSection,
                    "## 已选择的 MCP Server\n\n{servers}".to_owned(),
                ),
                (
                    PromptKey::SystemHooksSection,
                    "## 生命周期钩子\n\n{hook_names}\n{hooks}".to_owned(),
                ),
                (
                    PromptKey::SystemCapabilityRow,
                    "- {name}：{description}".to_owned(),
                ),
                (
                    PromptKey::SystemHookEventPreToolUse,
                    "工具执行前".to_owned(),
                ),
            ]),
            Vec::new(),
        );
        let file_addendum = runtime_context_from_discovery(conversation, &scan(), &file, &WorkspacePlaces::default())
            .unwrap()
            .addendum;
        assert!(file_addendum.contains("## 已选择的 MCP Server"));
        assert!(file_addendum.contains("## 生命周期钩子"));
        assert!(file_addendum.contains("- Probe Hook：工具执行前"));
    }

    /// Include only the skill body.
    ///
    /// The body supplies its own heading; frontmatter is consumed by installation and
    /// catalog display, not by model instructions.
    #[test]
    fn a_skill_body_reaches_the_prompt_without_its_heading_or_frontmatter() {
        let directory = tempfile::tempdir().unwrap();
        let skill_path = directory.path().join(SKILL_MANIFEST);
        fs::write(
            &skill_path,
            "---\nname: Probe Skill\ndescription: Use when probing\nversion: 1.2.3\ntags: [a, b]\n---\n\n# Probe skill\n\nMEWRK_SKILL_BODY_E2E\n",
        )
        .unwrap();
        let (document, catalog) = skill_only_fixture(&skill_path);
        let conversation = &document.workspaces[0].conversations[0];

        let hook_definitions = HashMap::new();
        let profile = PromptProfile::builtin_english();
        let context = runtime_context_from_discovery(
            conversation,
            &discovered(catalog.clone(), hook_definitions.clone()),
            &profile, &WorkspacePlaces::default(),
        )
        .unwrap();

        // The body, then where its files are, so `scripts/…` in it can be
        // found the way the `skill` tool's "Base directory" lets it be.
        assert_eq!(
            context.addendum,
            format!(
                "# Probe skill\n\nMEWRK_SKILL_BODY_E2E\n\nThis skill's files are in {}; paths in it are relative to that folder.",
                directory.path().to_string_lossy()
            )
        );
        // With the skill tool disabled, this output must be empty to avoid duplicating bodies.
        assert!(context.skills.is_empty());
    }

    /// With the tool enabled, skill bodies leave the system prompt while the
    /// name-and-trigger listing takes their place there and the directory
    /// reaches the tool output.
    #[test]
    fn the_skill_tool_takes_the_body_out_of_the_prompt_and_carries_trigger_and_directory() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("probe-skill");
        fs::create_dir_all(&directory).unwrap();
        let skill_path = directory.join(SKILL_MANIFEST);
        fs::write(
            &skill_path,
            "---\nname: Probe Skill\ndescription: Use when probing\nwhen_to_use: the user says probe\n---\n\nMEWRK_SKILL_BODY_E2E\n",
        )
        .unwrap();
        let (mut document, catalog) = skill_only_fixture(&skill_path);
        document.workspaces[0].conversations[0]
            .settings
            .skill_tool_enabled = true;
        let conversation = &document.workspaces[0].conversations[0];

        let hook_definitions = HashMap::new();
        let profile = PromptProfile::builtin_english();
        let context = runtime_context_from_discovery(
            conversation,
            &discovered(catalog.clone(), hook_definitions.clone()),
            &profile, &WorkspacePlaces::default(),
        )
        .unwrap();

        // The listing lives in the prompt rather than in the tool's schema, so
        // selecting another skill later never redeclares the tool.
        assert_eq!(
            context.addendum,
            "Available skills:\n- probe-skill: Use when probing - the user says probe"
        );
        // Nothing arrived after the opening prompt, so nothing is delivered as
        // its own message.
        assert!(context.added_skills.is_empty());
        assert!(!context.addendum.contains("MEWRK_SKILL_BODY_E2E"));
        assert_eq!(context.skills.len(), 1);
        let skill = &context.skills[0];
        // The directory is the name the model addresses, as in Claude Code;
        // the frontmatter name is the label the catalog shows.
        assert_eq!(skill.name, "probe-skill");
        // `when_to_use` supplements the description rather than replacing it.
        assert_eq!(skill.trigger, "Use when probing - the user says probe");
        assert_eq!(skill.body, "MEWRK_SKILL_BODY_E2E");
        assert_eq!(skill.directory, directory.to_string_lossy());
    }

    /// A conversation whose prompt is already open delivers a newly selected
    /// skill as its own message instead of rewriting that prompt — in both
    /// delivery modes, and in the form that mode calls for.
    #[test]
    fn a_skill_selected_after_the_opening_prompt_arrives_as_its_own_message() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("probe-skill");
        fs::create_dir_all(&directory).unwrap();
        let skill_path = directory.join(SKILL_MANIFEST);
        fs::write(
            &skill_path,
            "---\nname: Probe Skill\ndescription: Use when probing\n---\n\nMEWRK_SKILL_BODY_E2E\n",
        )
        .unwrap();
        let (mut document, catalog) = skill_only_fixture(&skill_path);
        // The first run opened this prompt with no skill at all, which is what
        // makes the one selected since an addition rather than part of it.
        document.workspaces[0].conversations[0].settings.tool_lock =
            Some(crate::model::ConversationToolLock {
                prompt_skill_ids: Some(Vec::new()),
                ..Default::default()
            });
        let profile = PromptProfile::builtin_english();
        let scan = || discovered(catalog.clone(), HashMap::new());

        let inline = runtime_context_from_discovery(
            &document.workspaces[0].conversations[0],
            &scan(),
            &profile, &WorkspacePlaces::default(),
        )
        .unwrap();
        // The prompt is untouched; the body arrives in the message instead.
        assert_eq!(inline.addendum, "");
        assert_eq!(inline.added_skills.len(), 1);
        assert_eq!(inline.added_skills[0].resource_id, "skill-probe");
        assert!(inline.added_skills[0]
            .content
            .contains("MEWRK_SKILL_BODY_E2E"));

        document.workspaces[0].conversations[0]
            .settings
            .skill_tool_enabled = true;
        let on_demand = runtime_context_from_discovery(
            &document.workspaces[0].conversations[0],
            &scan(),
            &profile, &WorkspacePlaces::default(),
        )
        .unwrap();
        assert_eq!(on_demand.addendum, "");
        assert_eq!(on_demand.added_skills.len(), 1);
        // On demand, only the trigger travels; the body stays behind the tool,
        // which still has to be able to serve it.
        assert!(!on_demand.added_skills[0]
            .content
            .contains("MEWRK_SKILL_BODY_E2E"));
        assert!(on_demand.added_skills[0]
            .content
            .contains("Use when probing"));
        assert_eq!(on_demand.skills.len(), 1);
        assert_eq!(on_demand.skills[0].body, "MEWRK_SKILL_BODY_E2E");
    }

    /// A conversation selecting one skill and a catalog containing only that skill.
    fn skill_only_fixture(skill_path: &Path) -> (AppDocument, CapabilityCatalog) {
        let mut document = crate::catalog::default_document();
        document.workspaces[0].conversations[0].settings.skill_ids = vec!["skill-probe".into()];
        let catalog = CapabilityCatalog {
            lsps: Vec::new(),
            hooks: Vec::new(),
            skills: vec![ResourceDescriptor {
                id: "skill-probe".into(),
                name: "Probe Skill".into(),
                description: "test skill".into(),
                location: skill_path.to_string_lossy().into_owned(),
                source: ResourceSource::Workspace,
                available: true,
                workspace_key: None,
            }],
            mcps: Vec::new(),
            tool_description_files: Vec::new(),
            agents: Vec::new(),
            unreadable_levels: Vec::new(),
        };
        (document, catalog)
    }

    /// Two selected skills with the same directory name — one global, one in the
    /// workspace — must fail in tool mode because one enum value cannot select
    /// two skills. Prompt mode does not select by name and permits duplicates.
    #[test]
    fn two_selected_skills_sharing_a_name_are_refused_only_in_tool_mode() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory
            .path()
            .join("global")
            .join("deploy")
            .join(SKILL_MANIFEST);
        let second = directory
            .path()
            .join("project")
            .join("deploy")
            .join(SKILL_MANIFEST);
        for (path, marker) in [(&first, "BODY-A"), (&second, "BODY-B")] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(
                path,
                format!("---\nname: Deploy ({marker})\n---\n\n{marker}\n"),
            )
            .unwrap();
        }
        let mut document = crate::catalog::default_document();
        document.workspaces[0].conversations[0].settings.skill_ids =
            vec!["skill-a".into(), "skill-b".into()];
        let descriptor = |id: &str, path: &Path| ResourceDescriptor {
            id: id.into(),
            name: "Probe Skill".into(),
            description: "test skill".into(),
            location: path.to_string_lossy().into_owned(),
            source: ResourceSource::User,
            available: true,
            workspace_key: None,
        };
        let catalog = CapabilityCatalog {
            hooks: Vec::new(),
            skills: vec![
                descriptor("skill-a", &first),
                descriptor("skill-b", &second),
            ],
            mcps: Vec::new(),
            lsps: Vec::new(),
            tool_description_files: Vec::new(),
            agents: Vec::new(),
            unreadable_levels: Vec::new(),
        };

        // Prompt mode permits duplicate names because both bodies reach the model.
        let hook_definitions = HashMap::new();
        let profile = PromptProfile::builtin_english();
        let concatenated = runtime_context_from_discovery(
            &document.workspaces[0].conversations[0],
            &discovered(catalog.clone(), hook_definitions.clone()),
            &profile, &WorkspacePlaces::default(),
        )
        .unwrap()
        .addendum;
        assert!(concatenated.contains("BODY-A") && concatenated.contains("BODY-B"));

        document.workspaces[0].conversations[0]
            .settings
            .skill_tool_enabled = true;
        let error = runtime_context_from_discovery(
            &document.workspaces[0].conversations[0],
            &discovered(catalog.clone(), hook_definitions.clone()),
            &profile, &WorkspacePlaces::default(),
        )
        .expect_err("duplicate skill directory names cannot be selected in tool mode");

        assert!(error.contains("\"deploy\""), "{error}");
    }

    /// Every selection has to resolve. A selected skill, MCP server or hook the
    /// scan no longer finds fails the run, naming it, until it is unticked; an
    /// entry the scan finds but cannot use fails it too, with its reason.
    #[test]
    fn dangling_and_unusable_selections_fail_the_run_naming_them() {
        let directory = tempfile::tempdir().unwrap();
        let skill_path = directory.path().join("ok").join(SKILL_MANIFEST);
        fs::create_dir_all(skill_path.parent().unwrap()).unwrap();
        fs::write(&skill_path, "# Ok\n\nBODY-OK\n").unwrap();
        let mut document = crate::catalog::default_document();
        let settings = &mut document.workspaces[0].conversations[0].settings;
        settings.skill_ids = vec!["skill_ok".into()];
        settings.mcp_ids = vec!["mcp_ok".into()];
        let descriptor =
            |id: &str, name: &str, location: &str, available: bool| ResourceDescriptor {
                id: id.into(),
                name: name.into(),
                description: if available {
                    "fine".into()
                } else {
                    "Missing environment variables: TOKEN".into()
                },
                location: location.into(),
                source: ResourceSource::User,
                available,
                workspace_key: None,
            };
        let catalog = CapabilityCatalog {
            lsps: Vec::new(),
            hooks: Vec::new(),
            skills: vec![descriptor(
                "skill_ok",
                "Ok",
                &skill_path.to_string_lossy(),
                true,
            )],
            mcps: vec![
                descriptor("mcp_ok", "Ok server", "x#/mcpServers/ok", true),
                descriptor("mcp_broken", "Broken", "x#/mcpServers/broken", false),
            ],
            tool_description_files: Vec::new(),
            agents: Vec::new(),
            unreadable_levels: Vec::new(),
        };
        let scan = DiscoveredCapabilities {
            mcp_servers: HashMap::from([(
                "mcp_ok".to_owned(),
                McpServerConfig {
                    id: "mcp_ok".into(),
                    name: "Ok server".into(),
                    description: "fine".into(),
                    command: "node".into(),
                    ..Default::default()
                },
            )]),
            ..discovered(catalog, HashMap::new())
        };
        let profile = PromptProfile::builtin_english();
        let run = |document: &AppDocument| {
            runtime_context_from_discovery(&document.workspaces[0].conversations[0], &scan, &profile, &WorkspacePlaces::default())
        };

        let context = run(&document).unwrap();
        assert!(context.addendum.contains("BODY-OK"));
        assert!(context.addendum.contains("- Ok server: fine"));
        assert_eq!(context.mcp_servers.len(), 1);

        // Each kind's dangling selection is named, in the app language, with
        // where to untick it.
        for (kind, id, page) in [
            ("skill", "skill_user_gone_0123abcd", "Skills"),
            ("mcp", "mcp_user_gone_0123abcd", "MCP"),
            ("hook", "hook_user_stop_0123abcd", "Hooks"),
        ] {
            let mut dangling = document.clone();
            let settings = &mut dangling.workspaces[0].conversations[0].settings;
            match kind {
                "skill" => settings.skill_ids.insert(0, id.into()),
                "mcp" => settings.mcp_ids.insert(0, id.into()),
                _ => settings.hook_ids.push(id.into()),
            }
            let error = crate::ui_text::with_language(ResolvedLanguage::EnUs, || run(&dangling))
                .unwrap_err();
            assert!(error.contains(id) && error.contains(page), "{error}");
            let error = run(&dangling).unwrap_err();
            assert!(error.contains(id) && error.contains("取消勾选"), "{error}");
        }

        let settings = &mut document.workspaces[0].conversations[0].settings;
        settings.mcp_ids.push("mcp_broken".into());
        let error = run(&document).unwrap_err();
        assert!(
            error.contains("Broken") && error.contains("TOKEN"),
            "{error}"
        );
    }

    /// One workspace's `.mewrk` yields all three kinds, each tagged with the
    /// workspace, and a run of a conversation in another workspace sees none
    /// of them.
    #[test]
    fn a_workspace_level_yields_skills_servers_and_hooks_scoped_to_that_workspace() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join(".mewrk");
        fs::create_dir_all(config.join("skills").join("review")).unwrap();
        fs::write(
            config.join("skills").join("review").join(SKILL_MANIFEST),
            "---\nname: Review\ndescription: Review code\n---\n\nREVIEW-BODY\n",
        )
        .unwrap();
        fs::write(
            config.join("mcp.json"),
            r#"{"mcpServers":{"docs":{"command":"node","args":["docs.js"]},"legacy":{"type":"sse","url":"http://127.0.0.1:1/"}}}"#,
        )
        .unwrap();
        fs::write(
            config.join("hooks.json"),
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","name":"Tests","command":"npm test"}]}]}}"#,
        )
        .unwrap();
        let mut document = crate::catalog::default_document();
        document.workspaces[0].path = directory.path().to_string_lossy().into_owned();
        document.workspaces[0].id = "ws_project".into();
        let other = tempfile::tempdir().unwrap();
        let mut elsewhere = document.workspaces[0].clone();
        elsewhere.id = "ws_other".into();
        elsewhere.path = other.path().to_string_lossy().into_owned();
        elsewhere.conversations[0].id = "conv_other".into();
        document.workspaces.push(elsewhere);

        let level = ConfigLevel::workspace(&document, &primary(&document.workspaces[0])).unwrap();
        let key = workspace_key(&primary(&document.workspaces[0]));
        let scan = discover_levels(std::slice::from_ref(&level), ResolvedLanguage::EnUs);
        // The built-in skill heads every scan; the workspace's own follows it.
        assert_eq!(scan.catalog.skills.len(), 2);
        assert_eq!(scan.catalog.skills[0].id, skills::MEWRK_SDK_ID);
        assert_eq!(scan.catalog.skills[1].name, "Review");
        assert!(scan.catalog.skills[1]
            .id
            .starts_with("skill_workspace_review_"));
        assert_eq!(scan.catalog.mcps.len(), 2);
        let docs = scan
            .catalog
            .mcps
            .iter()
            .find(|row| row.name == "docs")
            .unwrap();
        assert!(docs.available && scan.mcp_servers.contains_key(&docs.id));
        let legacy = scan
            .catalog
            .mcps
            .iter()
            .find(|row| row.name == "legacy")
            .unwrap();
        assert!(!legacy.available && !scan.mcp_servers.contains_key(&legacy.id));
        assert_eq!(scan.catalog.hooks.len(), 1);
        for row in scan
            .catalog
            .skills
            .iter()
            .skip(1)
            .chain(&scan.catalog.mcps)
            .chain(&scan.catalog.hooks)
        {
            assert_eq!(row.workspace_key, Some(key.clone()));
            assert_eq!(row.source, ResourceSource::Workspace);
        }

        // The conversation in the other project only gets the global level and
        // its own workspace's.
        let other_conversation = &document.workspaces.last().unwrap().conversations[0];
        assert_eq!(other_conversation.id, "conv_other");
        let levels = levels_for_conversation(&document, other_conversation);
        assert!(levels
            .iter()
            .all(|level| level.workspace_key().as_ref() != Some(&key)));
        let levels = levels_for_conversation(&document, &document.workspaces[0].conversations[0]);
        assert!(levels
            .iter()
            .any(|level| level.workspace_key().as_ref() == Some(&key)));
    }

    /// A conversation uses the union of its workspaces' levels — the
    /// project's workspace 1 and further workspaces, then its own attached
    /// ones. A skill serves the whole conversation and its folder is named
    /// by the workspace it is in; each server is that workspace's and says
    /// which machine it runs on; each hook knows its workspace's number.
    #[test]
    fn a_conversation_uses_the_union_of_its_workspaces_levels() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let attached = tempfile::tempdir().unwrap();
        let path = |dir: &tempfile::TempDir| dir.path().to_string_lossy().into_owned();
        for (root, name) in [(&first, "alpha"), (&second, "beta"), (&attached, "gamma")] {
            let config = root.path().join(".mewrk");
            fs::create_dir_all(config.join("skills").join(name)).unwrap();
            fs::write(
                config.join("skills").join(name).join(SKILL_MANIFEST),
                format!("---\nname: {name}\ndescription: {name} things\n---\n\n{name}-BODY\n"),
            )
            .unwrap();
            fs::write(
                config.join("mcp.json"),
                format!(r#"{{"mcpServers":{{"{name}-server":{{"command":"node"}}}}}}"#),
            )
            .unwrap();
            fs::write(
                config.join("hooks.json"),
                format!(r#"{{"hooks":{{"PreToolUse":[{{"hooks":[{{"type":"command","name":"{name}-hook","command":"echo {name}"}}]}}]}}}}"#),
            )
            .unwrap();
        }
        let mut document = crate::catalog::default_document();
        document.workspaces[0].path = path(&first);
        document.workspaces[0].additional_workspaces =
            vec![AttachedWorkspace { machine: None, path: path(&second) }];
        document.workspaces[0].conversations[0].attached_workspaces =
            vec![AttachedWorkspace { machine: None, path: path(&attached) }];

        let conversation = document.workspaces[0].conversations[0].clone();
        let keys = levels_for_conversation(&document, &conversation)
            .iter()
            .filter_map(ConfigLevel::workspace_key)
            .collect::<Vec<_>>();
        let key = |dir: &tempfile::TempDir| workspace_key(&AttachedWorkspace { machine: None, path: path(dir) });
        assert_eq!(keys, [key(&first), key(&second), key(&attached)]);
        // The catalog lists all three too, each row tagged with its workspace.
        let catalog = discover(&document, first.path());
        let id_of = |rows: &[ResourceDescriptor], name: &str, dir: &tempfile::TempDir| {
            let row = rows.iter().find(|row| row.name == name).unwrap_or_else(|| panic!("{name}"));
            assert_eq!(row.workspace_key, Some(key(dir)));
            row.id.clone()
        };
        let mut settings = conversation.settings.clone();
        settings.skill_tool_enabled = false;
        settings.skill_ids = vec![
            id_of(&catalog.skills, "alpha", &first),
            id_of(&catalog.skills, "beta", &second),
            id_of(&catalog.skills, "gamma", &attached),
        ];
        settings.mcp_ids = vec![
            id_of(&catalog.mcps, "alpha-server", &first),
            id_of(&catalog.mcps, "beta-server", &second),
            id_of(&catalog.mcps, "gamma-server", &attached),
        ];
        settings.hook_ids = vec![
            id_of(&catalog.hooks, "alpha-hook", &first),
            id_of(&catalog.hooks, "beta-hook", &second),
            id_of(&catalog.hooks, "gamma-hook", &attached),
        ];
        document.workspaces[0].conversations[0].settings = settings;
        let conversation = &document.workspaces[0].conversations[0];

        let context = runtime_context(&document, conversation, &PromptProfile::builtin_english()).unwrap();
        for (name, dir, member) in [("alpha", &first, 1), ("beta", &second, 2), ("gamma", &attached, 3)] {
            assert!(context.addendum.contains(&format!("{name}-BODY")), "{}", context.addendum);
            let folder = dir.path().join(".mewrk").join("skills").join(name);
            assert!(
                context.addendum.contains(&format!("{} in workspace {member}", folder.display())),
                "{}",
                context.addendum
            );
        }
        let declared = context
            .mcp_servers
            .iter()
            .map(|server| (server.name.as_str(), server.declared_in))
            .collect::<Vec<_>>();
        assert_eq!(
            declared,
            [("alpha-server", Some(1)), ("beta-server", Some(2)), ("gamma-server", Some(3))]
        );
        assert!(
            context.addendum.contains("Runs on this machine: use it only for workspaces on that machine (1, 2, 3)."),
            "{}",
            context.addendum
        );
        let members = context.hooks.iter().map(|hook| hook.member).collect::<Vec<_>>();
        assert_eq!(members, [Some(1), Some(2), Some(3)]);
    }

    /// A role that chooses its own skills or hooks gets them in place of the
    /// conversation's, and its caller's for every kind it leaves alone; its
    /// prompt describes the whole set. Only a role's own hooks are listed for
    /// the turn's confirmation, and a selection that cannot resolve is the
    /// role's own failure, named after it.
    #[test]
    fn a_role_resolves_its_own_selections_and_its_callers_for_the_rest() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join(".mewrk");
        let mut hooks = Vec::new();
        for name in ["alpha", "beta"] {
            fs::create_dir_all(config.join("skills").join(name)).unwrap();
            fs::write(
                config.join("skills").join(name).join(SKILL_MANIFEST),
                format!("---\nname: {name}\ndescription: {name} things\n---\n\n{name}-BODY\n"),
            )
            .unwrap();
            hooks.push(format!(
                r#"{{"hooks":[{{"type":"command","name":"{name}-hook","command":"echo {name}"}}]}}"#
            ));
        }
        fs::write(
            config.join("mcp.json"),
            r#"{"mcpServers":{"alpha-server":{"command":"node"},"beta-server":{"command":"node"}}}"#,
        )
        .unwrap();
        fs::write(
            config.join("hooks.json"),
            format!(r#"{{"hooks":{{"PreToolUse":[{}]}}}}"#, hooks.join(",")),
        )
        .unwrap();
        let mut document = crate::catalog::default_document();
        document.workspaces[0].path = root.path().to_string_lossy().into_owned();
        let catalog = discover(&document, root.path());
        let id_of = |rows: &[ResourceDescriptor], name: &str| {
            rows.iter().find(|row| row.name == name).unwrap_or_else(|| panic!("{name}")).id.clone()
        };
        // The roles are files in the workspace's `.mewrk/agents`, each
        // choosing its own skills, servers and hooks — or none of them.
        let agents = config.join("agents");
        fs::create_dir_all(&agents).unwrap();
        let write_role = |file: &str, body: serde_json::Value| {
            fs::write(agents.join(file), body.to_string()).unwrap();
        };
        write_role(
            "reviewer.json",
            serde_json::json!({
                "name": "reviewer",
                "skillIds": [id_of(&catalog.skills, "beta")],
                "hookIds": [id_of(&catalog.hooks, "beta-hook")],
            }),
        );
        write_role("follower.json", serde_json::json!({ "name": "follower" }));
        write_role(
            "quiet.json",
            serde_json::json!({ "name": "quiet", "hookIds": [id_of(&catalog.hooks, "beta-hook")] }),
        );
        write_role(
            "broken.json",
            serde_json::json!({ "name": "broken", "skillIds": ["skill_workspace_gone_00000000"] }),
        );
        let catalog = discover(&document, root.path());
        let role_id = |name: &str| {
            catalog
                .agents
                .iter()
                .find(|row| row.descriptor.name == name)
                .unwrap_or_else(|| panic!("{name}"))
                .descriptor
                .id
                .clone()
        };
        let settings = &mut document.workspaces[0].conversations[0].settings;
        settings.skill_tool_enabled = false;
        settings.skill_ids = vec![id_of(&catalog.skills, "alpha")];
        settings.mcp_ids = vec![id_of(&catalog.mcps, "alpha-server")];
        settings.hook_ids = vec![id_of(&catalog.hooks, "alpha-hook")];
        // `quiet` is on disk but not selected, so its hooks are not listed.
        settings.agent_ids = vec![role_id("reviewer"), role_id("follower"), role_id("broken")];
        let conversation = document.workspaces[0].conversations[0].clone();
        let profile = PromptProfile::builtin_english();

        let context = runtime_context(&document, &conversation, &profile).unwrap();
        assert_eq!(context.role_hooks.keys().collect::<Vec<_>>(), ["reviewer"]);
        let definition = |name: &str| {
            context
                .agent_roles
                .iter()
                .find(|role| role.definition.name == name)
                .unwrap_or_else(|| panic!("{name}"))
                .definition
                .clone()
        };
        assert_eq!(
            context.role_hooks["reviewer"].iter().map(|hook| hook.name.as_str()).collect::<Vec<_>>(),
            ["beta-hook"]
        );

        let basis = RoleBasis {
            environment: String::new(),
            skill_ids: conversation.settings.skill_ids.clone(),
            mcp_ids: conversation.settings.mcp_ids.clone(),
            hook_ids: conversation.settings.hook_ids.clone(),
            skill_tool: false,
            confirmed_role_hooks: context.role_hooks.clone(),
        };
        let reviewer = definition("reviewer");
        let resolved = resolve_role(&document, &conversation, &basis, &reviewer, &profile).unwrap();
        assert!(resolved.addendum.contains("beta-BODY"), "{}", resolved.addendum);
        assert!(!resolved.addendum.contains("alpha-BODY"), "{}", resolved.addendum);
        // A role file always chooses its own servers, so the caller's are not its.
        assert!(!resolved.addendum.contains("alpha-server"), "{}", resolved.addendum);
        assert!(resolved.addendum.contains("beta-hook"), "{}", resolved.addendum);
        assert!(!resolved.addendum.contains("alpha-hook"), "{}", resolved.addendum);
        assert!(resolved.mcp_servers.is_empty());
        assert_eq!(
            resolved.hooks.iter().map(|hook| hook.name.as_str()).collect::<Vec<_>>(),
            ["beta-hook"]
        );

        let broken = definition("broken");
        let error = resolve_role(&document, &conversation, &basis, &broken, &profile).unwrap_err();
        assert!(error.contains("broken") && error.contains("skill_workspace_gone_00000000"), "{error}");
        // A role file names all three kinds, empty lists included — and one
        // that names none of any is resolved without reading a single file,
        // here or on another machine.
        let follower = definition("follower");
        assert!(role_chooses_capabilities(&follower));
        let reads = ROLE_RESOLUTION_READS.with(std::cell::Cell::get);
        let resolved = resolve_role(&document, &conversation, &basis, &follower, &profile).unwrap();
        assert_eq!(resolved, RoleCapabilities::default());
        assert_eq!(ROLE_RESOLUTION_READS.with(std::cell::Cell::get), reads);
        resolve_role(&document, &conversation, &basis, &reviewer, &profile).unwrap();
        assert_eq!(ROLE_RESOLUTION_READS.with(std::cell::Cell::get), reads + 1);
    }

    /// Role files are read at every level and listed whether or not they can
    /// be used; the usable ones are handed on for the registry with the level
    /// they were read at, the catalog lists the built-in roles first, and the
    /// fingerprint the settings pane polls moves when a role file appears.
    #[test]
    fn role_files_are_discovered_at_every_level() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let write = |base: &Path, file: &str, text: &str| {
            let agents = base.join(CONFIG_DIRECTORY).join("agents");
            fs::create_dir_all(&agents).unwrap();
            fs::write(agents.join(file), text).unwrap();
        };
        write(home.path(), "reviewer.json", r#"{"name":"Reviewer","description":"Global"}"#);
        write(workspace.path(), "reviewer.json", r#"{"name":"Reviewer","description":"Project"}"#);
        write(workspace.path(), "broken.json", "{");
        let location = AttachedWorkspace {
            machine: None,
            path: workspace.path().to_string_lossy().into_owned(),
        };
        let levels = [
            ConfigLevel {
                source: ResourceSource::User,
                base: home.path().to_path_buf(),
                workspace: None,
                remote: None,
            },
            ConfigLevel {
                source: ResourceSource::Workspace,
                base: workspace.path().to_path_buf(),
                workspace: Some(location.clone()),
                remote: None,
            },
        ];
        let key = workspace_key(&location);
        let scan = discover_levels(&levels, ResolvedLanguage::EnUs);
        let rows = &scan.catalog.agents;
        assert_eq!(rows.len(), 3, "{rows:?}");
        let global = rows.iter().find(|row| row.descriptor.description == "Global").unwrap();
        assert_eq!(global.descriptor.source, ResourceSource::User);
        assert!(global.descriptor.workspace_key.is_none());
        assert!(global.descriptor.id.starts_with("agent_user_reviewer_"));
        let project = rows.iter().find(|row| row.descriptor.description == "Project").unwrap();
        assert_eq!(project.descriptor.workspace_key.as_ref(), Some(&key));
        assert!(project.descriptor.id.starts_with("agent_workspace_reviewer_"));
        assert!(!rows.iter().find(|row| row.descriptor.name == "broken").unwrap().descriptor.available);
        // Only the usable ones reach the registry, each with its level.
        assert_eq!(scan.agent_roles.len(), 2);
        let level_of_role = |id: &str| {
            scan.agent_roles
                .iter()
                .find(|role| role.id == id)
                .map(|role| role.level.clone())
        };
        assert_eq!(level_of_role(&global.descriptor.id), Some(crate::agent_roles::RoleLevel::Global));
        assert_eq!(
            level_of_role(&project.descriptor.id),
            Some(crate::agent_roles::RoleLevel::Workspace(key.clone()))
        );
        assert_eq!(
            scan.role_levels,
            [crate::agent_roles::RoleLevel::Global, crate::agent_roles::RoleLevel::Workspace(key)]
        );

        let mut document = crate::catalog::default_document();
        document.workspaces[0].path = workspace.path().to_string_lossy().into_owned();
        let catalog = discover(&document, home.path());
        assert_eq!(
            catalog.agents.iter().take(4).map(|row| row.descriptor.id.as_str()).collect::<Vec<_>>(),
            crate::agent_roles::BUILTIN_ROLE_IDS
        );
        let before = fingerprint(&document);
        write(workspace.path(), "new.json", "{}");
        assert_ne!(fingerprint(&document), before);
    }

    /// On a machine whose file names keep case, `Reviewer.json` and
    /// `reviewer.json` are two files with one id. An id must name one role,
    /// so the first is listed and registered and the other is passed over.
    #[test]
    fn two_role_files_with_one_id_list_the_first_only() {
        let location = AttachedWorkspace {
            machine: Some(crate::model::RunTarget::Wsl { distro: "Ubuntu".into() }),
            path: "/srv/app".into(),
        };
        let level = ConfigLevel {
            source: ResourceSource::Workspace,
            base: PathBuf::from("/srv/app"),
            workspace: Some(location.clone()),
            remote: Some(crate::remote_capabilities::RemoteLevel {
                runner: crate::run_environment::ShellRunner::Wsl {
                    distro: "Ubuntu".into(),
                    env: Default::default(),
                    agent_shell: Default::default(),
                },
                machine: "wsl:Ubuntu".into(),
                root: "/srv/app".into(),
            }),
        };
        let entry = |file_name: &str, body: &str| crate::remote_capabilities::AgentFileEntry {
            file_name: file_name.into(),
            path: format!("/srv/app/.mewrk/agents/{file_name}"),
            size: body.len() as u64,
            bytes: Some(body.as_bytes().to_vec()),
        };
        let files = crate::remote_capabilities::LevelFiles {
            agents_root: Some("/srv/app/.mewrk/agents".into()),
            agents: vec![
                entry("Reviewer.json", r#"{"name":"First"}"#),
                entry("reviewer.json", r#"{"name":"Second"}"#),
                entry("other.json", r#"{"name":"Other"}"#),
            ],
            ..Default::default()
        };
        let reads: RemoteReads = [(workspace_key(&location), Ok(files))].into_iter().collect();
        let scan = discover_levels_with(std::slice::from_ref(&level), ResolvedLanguage::EnUs, &reads);
        let names = scan
            .catalog
            .agents
            .iter()
            .map(|row| row.descriptor.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(names.contains(&"First") && names.contains(&"Other"), "{names:?}");
        let ids = scan.agent_roles.iter().map(|role| role.id.as_str()).collect::<HashSet<_>>();
        assert_eq!(ids.len(), 2);
        assert_eq!(
            scan.agent_roles.iter().find(|role| role.definition.name == "First").map(|role| role.id.as_str()),
            scan.catalog.agents.iter().find(|row| row.descriptor.name == "First").map(|row| row.descriptor.id.as_str())
        );
    }

    /// A workspace on an SSH machine records a path on that machine, so its
    /// level is read there, never from this computer's folder of the same
    /// spelling: what the machine's probe brings back is listed under the
    /// workspace, with ids of that machine, its hooks run there, its servers
    /// start there, its skills are read from what was brought back, and a
    /// machine that could not be read says so instead of leaving its
    /// selections looking deleted.
    #[test]
    fn a_remote_workspace_provides_the_capabilities_read_on_its_machine() {
        // A local folder at the remote path's spelling, full of capabilities
        // that must not be listed.
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join(".mewrk");
        fs::create_dir_all(config.join("skills").join("local-only")).unwrap();
        fs::write(
            config.join("skills").join("local-only").join(SKILL_MANIFEST),
            "---\nname: Local Only\n---\n\nBODY\n",
        )
        .unwrap();
        fs::write(config.join("mcp.json"), r#"{"mcpServers":{"local":{"command":"node"}}}"#).unwrap();
        let remote_root = directory.path().to_string_lossy().into_owned();
        let mut document = crate::catalog::default_document();
        document.workspaces[0].id = "ws_remote".into();
        document.workspaces[0].path = remote_root.clone();
        document.workspaces[0].machine = Some(crate::model::RunTarget::Ssh {
            machine_id: "m1".into(),
        });
        document.assets.execution_environments.ssh_machines = vec![crate::model::SshMachineConfig {
            id: "m1".into(),
            name: "devbox".into(),
            host: "devbox".into(),
            ..Default::default()
        }];
        document.assets.execution_environments.env_vars.insert(
            crate::run_environment::workspace_env_key(
                document.workspaces[0].machine.as_ref(),
                &remote_root,
            ),
            [("DOCS_PORT".to_owned(), "4100".to_owned())].into_iter().collect(),
        );

        let levels = all_levels(&document);
        let key = workspace_key(&primary(&document.workspaces[0]));
        assert_eq!(key, format!("ssh:m1|{remote_root}"));
        let level = levels
            .iter()
            .find(|level| level.workspace_key().as_ref() == Some(&key))
            .expect("a remote workspace is a level");
        let remote = level.remote.as_ref().expect("read on its machine");
        assert_eq!(remote.machine, "ssh:m1");
        assert_eq!(remote.runner.env()["DOCS_PORT"], "4100");
        assert!(local_levels(&document).iter().all(|level| level.workspace.is_none()));

        let files = crate::remote_capabilities::LevelFiles {
            windows: false,
            env: [("HOME".to_owned(), "/home/dev".to_owned())].into_iter().collect(),
            mcp: Some(crate::remote_capabilities::ConfigFile {
                path: "/srv/app/.mewrk/mcp.json".into(),
                bytes: br#"{"mcpServers":{"docs":{"command":"node","args":["${HOME}/docs.js"]},"web":{"type":"http","url":"http://localhost:${DOCS_PORT}/mcp"}}}"#.to_vec(),
            }),
            hooks: Some(crate::remote_capabilities::ConfigFile {
                path: "/srv/app/.mewrk/hooks.json".into(),
                bytes: br#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"make check","commandWindows":"nmake check"}]}]}}"#.to_vec(),
            }),
            skills_root: Some("/srv/app/.mewrk/skills".into()),
            skills: vec![crate::remote_capabilities::SkillFile {
                directory: "review".into(),
                manifest: "/srv/app/.mewrk/skills/review/SKILL.md".into(),
                size: 30,
                bytes: Some(b"---\nname: Review\n---\n\nREMOTE-BODY\n".to_vec()),
            }],
            agents_root: Some("/srv/app/.mewrk/agents".into()),
            agents: vec![
                crate::remote_capabilities::AgentFileEntry {
                    file_name: "reviewer.json".into(),
                    path: "/srv/app/.mewrk/agents/reviewer.json".into(),
                    size: 19,
                    bytes: Some(br#"{"name":"Reviewer"}"#.to_vec()),
                },
                crate::remote_capabilities::AgentFileEntry {
                    file_name: "huge.json".into(),
                    path: "/srv/app/.mewrk/agents/huge.json".into(),
                    size: 300_000,
                    bytes: None,
                },
            ],
            ..Default::default()
        };
        let reads: RemoteReads = [(key.clone(), Ok(files))].into_iter().collect();
        let scan = discover_levels_with(&levels, ResolvedLanguage::EnUs, &reads);
        let remote_rows = |rows: &[ResourceDescriptor]| {
            rows.iter()
                .filter(|row| row.workspace_key.as_ref() == Some(&key))
                .cloned()
                .collect::<Vec<_>>()
        };
        let skills = remote_rows(&scan.catalog.skills);
        assert_eq!(skills.len(), 1, "{skills:?}");
        assert_eq!(skills[0].name, "Review");
        assert_eq!(skills[0].location, "/srv/app/.mewrk/skills/review/SKILL.md");
        let skill = &scan.remote_skills[&skills[0].id];
        assert_eq!(skill.directory, "/srv/app/.mewrk/skills/review");
        let mcps = remote_rows(&scan.catalog.mcps);
        assert_eq!(mcps.len(), 2);
        let docs = mcps.iter().find(|row| row.name == "docs").unwrap();
        let docs_config = &scan.mcp_servers[&docs.id];
        assert_eq!(docs_config.id, docs.id);
        assert_eq!(docs_config.args, ["/home/dev/docs.js"], "the machine's HOME");
        assert_eq!(docs_config.machine.as_ref(), Some(remote));
        let web = mcps.iter().find(|row| row.name == "web").unwrap();
        assert_eq!(scan.mcp_servers[&web.id].url, "http://localhost:4100/mcp", "the workspace's variable");
        let hooks = remote_rows(&scan.catalog.hooks);
        assert_eq!(hooks.len(), 1);
        let hook = &scan.hooks[&hooks[0].id];
        let place = hook.on_machine.as_ref().expect("runs on its machine");
        assert_eq!(place.cwd, remote_root);
        assert!(!place.windows);
        // The same path read on this computer would mint another id.
        let local_hook_id = parse_hooks(
            br#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"make check","commandWindows":"nmake check"}]}]}}"#,
            Path::new("/srv/app/.mewrk/hooks.json"),
            ResourceSource::Workspace,
            Some("ws_remote"),
        )[0]
        .descriptor
        .id
        .clone();
        assert_ne!(hooks[0].id, local_hook_id);
        // Its role files, read there too: ids of that machine, a file too
        // large to bring over listed with the reason, and the level counted
        // as read, so the registry replaces what it held.
        let roles = scan
            .catalog
            .agents
            .iter()
            .filter(|row| row.descriptor.workspace_key.as_ref() == Some(&key))
            .collect::<Vec<_>>();
        assert_eq!(roles.len(), 2, "{roles:?}");
        let reviewer = roles.iter().find(|row| row.descriptor.name == "Reviewer").unwrap();
        assert!(reviewer.descriptor.available);
        assert!(reviewer.descriptor.id.starts_with("agent_workspace_reviewer_"));
        assert_ne!(
            reviewer.descriptor.id,
            stable_id("agent_workspace", "reviewer", "/srv/app/.mewrk/agents/reviewer.json")
        );
        assert!(!roles.iter().find(|row| row.descriptor.name == "huge").unwrap().descriptor.available);
        let workspace_level = crate::agent_roles::RoleLevel::Workspace(key.clone());
        assert!(scan
            .agent_roles
            .iter()
            .any(|role| role.id == reviewer.descriptor.id && role.level == workspace_level));
        assert!(scan.role_levels.contains(&workspace_level));

        // A run of the workspace's conversation resolves all of it, the skill
        // from what the probe brought back, with its folder there.
        document.workspaces[0].conversations[0].settings.skill_ids = vec![skills[0].id.clone()];
        document.workspaces[0].conversations[0].settings.mcp_ids = vec![docs.id.clone()];
        document.workspaces[0].conversations[0].settings.hook_ids = vec![hooks[0].id.clone()];
        let profile = PromptProfile::builtin_english();
        let context = runtime_context_with(
            &document,
            &document.workspaces[0].conversations[0],
            &profile,
            &reads,
        )
        .unwrap();
        assert!(context.addendum.contains("REMOTE-BODY"), "{}", context.addendum);
        assert!(
            context.addendum.contains("/srv/app/.mewrk/skills/review in workspace 1"),
            "{}",
            context.addendum
        );
        assert_eq!(context.mcp_servers.len(), 1);
        assert!(matches!(
            &context.mcp_servers[0].transport,
            crate::mcp::RuntimeMcpTransport::Stdio { on_machine: Some(_), .. }
        ));
        assert_eq!(context.hooks.len(), 1);
        assert!(context.hooks[0].on_machine.is_some());

        // A machine that could not be read explains a missing selection, to
        // the run and to the settings page.
        let unreadable: RemoteReads =
            [(key.clone(), Err("devbox did not answer".to_owned()))].into_iter().collect();
        let scan = discover_levels_with(&levels, ResolvedLanguage::EnUs, &unreadable);
        assert!(
            !scan.role_levels.contains(&workspace_level),
            "a level that was not read keeps the roles it had"
        );
        assert_eq!(
            scan.catalog.unreadable_levels,
            [crate::model::UnreadableLevel {
                workspace_key: key.clone(),
                message: "devbox did not answer".into(),
            }]
        );
        let error = runtime_context_with(
            &document,
            &document.workspaces[0].conversations[0],
            &profile,
            &unreadable,
        )
        .unwrap_err();
        assert_eq!(error, "devbox did not answer");
    }

    /// The temporary workspace has no folder of its own and provides nothing.
    #[test]
    fn the_temporary_workspace_provides_no_level() {
        let document = crate::catalog::default_document();
        let temporary = document
            .workspaces
            .iter()
            .find(|workspace| workspace.kind == crate::model::WorkspaceKind::Temporary)
            .expect("the default document has one");
        assert!(project_locations(temporary).is_empty());
        if let Some(conversation) = temporary.conversations.first() {
            assert!(levels_for_conversation(&document, conversation)
                .iter()
                .all(|level| level.workspace.is_none()));
        }
    }

    /// The fingerprint the settings pane polls moves when a configuration file
    /// or a skill folder does, and only then.
    #[test]
    fn the_fingerprint_moves_with_the_files_discovery_reads() {
        let directory = tempfile::tempdir().unwrap();
        let mut document = crate::catalog::default_document();
        document.workspaces[0].path = directory.path().to_string_lossy().into_owned();
        let config = directory.path().join(".mewrk");
        let first = fingerprint(&document);
        assert_eq!(fingerprint(&document), first, "nothing changed");

        fs::create_dir_all(config.join("skills").join("review")).unwrap();
        fs::write(config.join("skills").join("review").join(SKILL_MANIFEST), "# Review\n").unwrap();
        let with_skill = fingerprint(&document);
        assert_ne!(with_skill, first);

        fs::write(config.join("mcp.json"), r#"{"mcpServers":{}}"#).unwrap();
        let with_mcp = fingerprint(&document);
        assert_ne!(with_mcp, with_skill);

        fs::write(config.join("mcp.json"), r#"{"mcpServers":{"a":{"command":"x"}}}"#).unwrap();
        assert_ne!(fingerprint(&document), with_mcp, "a rewrite of a different length");
    }

    /// Tool-description files are read at the global level only, so a file in a
    /// workspace's own `.mewrk/tool-descriptions` moves nothing.
    #[test]
    fn a_workspace_tool_description_file_does_not_move_the_fingerprint() {
        let directory = tempfile::tempdir().unwrap();
        let mut document = crate::catalog::default_document();
        document.workspaces[0].path = directory.path().to_string_lossy().into_owned();
        let before = fingerprint(&document);

        let root = directory.path().join(".mewrk").join("tool-descriptions");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("project.json"),
            r#"{"tools":[{"toolName":"ls","description":"Project wording."}]}"#,
        )
        .unwrap();
        assert_eq!(fingerprint(&document), before);

        // A skill folder beside it does move the fingerprint, so the unchanged
        // value above is not a level that went unread altogether.
        fs::create_dir_all(directory.path().join(".mewrk").join("skills").join("review")).unwrap();
        assert_ne!(fingerprint(&document), before);
    }

    /// Deleting through the catalog reaches the folder or the file entry the
    /// scan read, and nothing else.
    #[test]
    fn discovered_skills_and_servers_can_be_deleted_where_they_were_read() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join(".mewrk");
        fs::create_dir_all(config.join("skills").join("gone")).unwrap();
        fs::write(
            config.join("skills").join("gone").join(SKILL_MANIFEST),
            "# Gone\n",
        )
        .unwrap();
        fs::write(
            config.join("mcp.json"),
            r#"{"mcpServers":{"keep":{"command":"node"},"drop":{"command":"node"}}}"#,
        )
        .unwrap();
        let mut document = crate::catalog::default_document();
        document.workspaces[0].path = directory.path().to_string_lossy().into_owned();
        let level = ConfigLevel::workspace(&document, &primary(&document.workspaces[0])).unwrap();
        let scan = discover_levels(std::slice::from_ref(&level), ResolvedLanguage::EnUs);
        let skill_id = scan
            .catalog
            .skills
            .iter()
            .find(|row| row.name == "Gone")
            .unwrap()
            .id
            .clone();
        let drop_id = scan
            .catalog
            .mcps
            .iter()
            .find(|row| row.name == "drop")
            .unwrap()
            .id
            .clone();

        delete_skill(&document, &skill_id).unwrap();
        assert!(!config.join("skills").join("gone").exists());
        assert!(delete_skill(&document, &skill_id).is_err());
        // The built-in skill is in the same scan, and is refused.
        let error = delete_skill(&document, skills::MEWRK_SDK_ID).unwrap_err();
        assert!(error.contains("内置"), "{error}");

        delete_mcp_server(&document, &drop_id).unwrap();
        let remaining = discover_levels(std::slice::from_ref(&level), ResolvedLanguage::EnUs);
        assert_eq!(remaining.catalog.mcps.len(), 1);
        assert_eq!(remaining.catalog.mcps[0].name, "keep");
        assert!(mcp_server_config(&document, &remaining.catalog.mcps[0].id).is_ok());
        assert!(mcp_server_config(&document, &drop_id).is_err());
    }

    /// Derive the tool from resolved skills, not merely from the enabled setting.
    #[test]
    fn the_skill_tool_is_derived_only_when_a_skill_actually_resolved() {
        let mut enabled = vec!["read".to_owned()];
        apply_skill_tool(&mut enabled, 2);
        assert_eq!(enabled, vec!["read".to_owned(), SKILL_TOOL.to_owned()]);

        // Remove stale persisted names so old data cannot re-enable the setting.
        apply_skill_tool(&mut enabled, 0);
        assert_eq!(enabled, vec!["read".to_owned()]);
    }

    #[test]
    fn hook_file_discovers_only_valid_external_definitions() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("hooks.json");
        fs::write(
            &path,
            r#"{
              "hooks": {
                "Stop": [{
                  "hooks": [{
                    "type": "command",
                    "name": "结束后测试",
                    "command": "npm test",
                    "timeout": 120
                  }]
                }],
                "Unknown": [{"hooks":[{"type":"command","command":"echo no"}]}],
                "PreToolUse": [{"hooks":[{"type":"command","command":"  "}]}]
              }
            }"#,
        )
        .unwrap();

        let entries = read_hooks_file(&path, ResourceSource::User, None);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].descriptor.name, "结束后测试");
        assert_eq!(entries[0].descriptor.description, "回合结束前");
        let english = crate::ui_text::with_language(ResolvedLanguage::EnUs, || {
            read_hooks_file(&path, ResourceSource::User, None)
        });
        assert_eq!(english[0].descriptor.description, "Before the turn stops");
        assert_eq!(entries[0].definition.command, "npm test");
        assert_eq!(entries[0].definition.timeout_ms, 120_000);
        let visible = serde_json::to_string(&entries[0].descriptor).unwrap();
        assert!(!visible.contains("npm test"));
    }

    /// `hooks.json` saved with a byte-order mark yields the same hooks, and a
    /// handler deleted from it leaves the mark where it was.
    #[test]
    fn a_byte_order_mark_does_not_hide_a_hooks_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("hooks.json");
        let json = serde_json::to_string_pretty(&hooks_fixture()).unwrap();
        fs::write(&path, &json).unwrap();
        let plain = read_hooks_file(&path, ResourceSource::User, None);
        fs::write(&path, format!("\u{feff}{json}")).unwrap();
        let marked = read_hooks_file(&path, ResourceSource::User, None);
        assert_eq!(marked.len(), 4);
        assert_eq!(
            marked.iter().map(|entry| &entry.descriptor).collect::<Vec<_>>(),
            plain.iter().map(|entry| &entry.descriptor).collect::<Vec<_>>()
        );

        let address = parse_hook_location(&marked[0].descriptor.location).unwrap();
        remove_hook_from_file(&address).unwrap();
        assert!(fs::read_to_string(&path).unwrap().starts_with('\u{feff}'));
        assert_eq!(read_hooks_file(&path, ResourceSource::User, None).len(), 3);
    }

    #[test]
    fn instructions_loaded_is_discovered_as_forced_async_observability() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("hooks.json");
        fs::write(
            &path,
            r#"{
              "hooks": {
                "InstructionsLoaded": [{
                  "matcher": "^(session_start|include)$",
                  "hooks": [
                    {"type":"command","name":"Observe instructions","command":"observe","async":true},
                    {"type":"command","name":"Must not rewake","command":"rewake","asyncRewake":true}
                  ]
                }]
              }
            }"#,
        )
        .unwrap();

        let entries = read_hooks_file(&path, ResourceSource::User, None);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].definition.event, HookEvent::InstructionsLoaded);
        assert_eq!(
            entries[0].definition.matcher.as_deref(),
            Some("^(session_start|include)$")
        );
        assert_eq!(entries[0].definition.command, "observe");
    }

    #[test]
    fn selected_workspace_hook_resolves_to_external_command() {
        let directory = tempfile::tempdir().unwrap();
        let config_dir = directory.path().join(".naiword");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            config_dir.join("hooks.json"),
            r#"{"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command","name":"Lint","command":"npm run lint"}]}]}}"#,
        )
        .unwrap();
        let mut document = crate::catalog::default_document();
        for workspace in &mut document.workspaces {
            workspace.path = directory.path().to_string_lossy().into_owned();
        }
        let discovered = discover(&document, &directory.path().join("app-data"));
        let hook_id = discovered
            .hooks
            .iter()
            .find(|hook| hook.source == ResourceSource::Workspace)
            .unwrap()
            .id
            .clone();
        document.workspaces[0].conversations[0].settings.hook_ids = vec![hook_id];

        let hooks = runtime_context(
            &document,
            &document.workspaces[0].conversations[0],
            &PromptProfile::builtin_english(),
        )
        .unwrap()
        .hooks;

        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].event, HookEvent::UserPromptSubmit);
        assert_eq!(hooks[0].command, "npm run lint");
        assert!(hooks[0].enabled);
    }

    /// Two events; the first has two groups and one of them two handlers. Plus a
    /// top-level field that has nothing to do with hooks.
    fn hooks_fixture() -> Value {
        serde_json::json!({
            "version": 1,
            "hooks": {
                "Stop": [
                    {
                        "hooks": [
                            {"type": "command", "name": "stop-a", "command": "echo a"},
                            {"type": "command", "name": "stop-b", "command": "echo b"}
                        ]
                    },
                    {
                        "matcher": "Bash",
                        "hooks": [{"type": "command", "name": "bash-a", "command": "echo bash"}]
                    }
                ],
                "UserPromptSubmit": [
                    {"hooks": [{"type": "command", "name": "prompt-a", "command": "echo prompt"}]}
                ]
            }
        })
    }

    /// Every handler name in the file, sorted, so an assertion can state exactly
    /// which handlers a rewrite left behind.
    fn handler_names(document: &Value) -> Vec<String> {
        let mut names = Vec::new();
        if let Some(events) = document.get("hooks").and_then(Value::as_object) {
            for groups in events.values() {
                if let Some(groups) = groups.as_array() {
                    for group in groups {
                        if let Some(handlers) = group.get("hooks").and_then(Value::as_array) {
                            for handler in handlers {
                                if let Some(name) = handler.get("name").and_then(Value::as_str) {
                                    names.push(name.to_owned());
                                }
                            }
                        }
                    }
                }
            }
        }
        names.sort();
        names
    }

    fn write_hooks_fixture(directory: &tempfile::TempDir) -> PathBuf {
        let path = directory.path().join("hooks.json");
        fs::write(
            &path,
            serde_json::to_string_pretty(&hooks_fixture()).unwrap(),
        )
        .unwrap();
        path
    }

    #[test]
    fn hook_locations_parse_back_into_the_file_and_position_they_came_from() {
        let address =
            parse_hook_location(r"C:\Users\dev\.mewrk\hooks.json#/hooks/PostToolUse/2/hooks/1")
                .expect("这是 read_hooks_file 会写出的格式");
        assert_eq!(
            address.path,
            PathBuf::from(r"C:\Users\dev\.mewrk\hooks.json")
        );
        assert_eq!(address.event, "PostToolUse");
        assert_eq!(address.group_index, 2);
        assert_eq!(address.handler_index, 1);

        // The path is whatever the scan spelled, so a POSIX one round-trips too.
        let address = parse_hook_location("/home/dev/.mewrk/hooks.json#/hooks/Stop/0/hooks/0")
            .expect("斜杠路径同样是本应用写出的格式");
        assert_eq!(address.path, PathBuf::from("/home/dev/.mewrk/hooks.json"));
        assert_eq!(address.event, "Stop");
        assert_eq!(address.group_index, 0);
        assert_eq!(address.handler_index, 0);
    }

    #[test]
    fn anything_that_is_not_a_hook_location_resolves_to_nothing() {
        for location in [
            // An ordinary file entry, with no pointer at all.
            r"C:\Users\dev\.mewrk\hooks.json",
            // A pointer of some other shape.
            r"C:\Users\dev\.mewrk\hooks.json#/skills/0/hooks/1",
            // Both indices must be numbers.
            r"C:\Users\dev\.mewrk\hooks.json#/hooks/Stop/zero/hooks/1",
            r"C:\Users\dev\.mewrk\hooks.json#/hooks/Stop/0/hooks/one",
            // The segment between the two indices is always `hooks`.
            r"C:\Users\dev\.mewrk\hooks.json#/hooks/Stop/0/handlers/1",
            // Nothing may follow the handler index, and nothing may be missing.
            r"C:\Users\dev\.mewrk\hooks.json#/hooks/Stop/0/hooks/1/extra",
            r"C:\Users\dev\.mewrk\hooks.json#/hooks/Stop/0/hooks",
            // An event name is required.
            r"C:\Users\dev\.mewrk\hooks.json#/hooks//0/hooks/1",
        ] {
            assert!(
                parse_hook_location(location).is_none(),
                "{location} 不是本应用写出的地址，不该被解析成某个位置"
            );
        }
    }

    #[test]
    fn removing_one_handler_leaves_every_other_entry_and_field_alone() {
        let directory = tempfile::tempdir().unwrap();
        let path = write_hooks_fixture(&directory);
        let original = hooks_fixture();
        assert_eq!(handler_names(&original).len(), 4, "夹具里应有四个处理器");

        remove_hook_from_file(&HookAddress {
            path: path.clone(),
            event: "Stop".into(),
            group_index: 0,
            handler_index: 0,
        })
        .expect("删掉一个存在的处理器应当成功");

        // The expected file is the original with that one handler edited out, so
        // this compares everything: every other handler, every other group, and
        // every field that is not a hook.
        let mut expected = original.clone();
        expected["hooks"]["Stop"][0]["hooks"]
            .as_array_mut()
            .unwrap()
            .remove(0);
        let written: Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).expect("文件仍是合法 JSON");
        assert_eq!(written, expected, "除被删的那条外，文件其余部分必须原样");
        assert_eq!(written["version"], 1);
        assert_eq!(written["hooks"]["Stop"][1]["matcher"], "Bash");
        assert_eq!(handler_names(&written), ["bash-a", "prompt-a", "stop-b"]);
    }

    #[test]
    fn deleting_a_groups_last_handler_keeps_the_group_and_a_valid_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = write_hooks_fixture(&directory);

        remove_hook_from_file(&HookAddress {
            path: path.clone(),
            event: "UserPromptSubmit".into(),
            group_index: 0,
            handler_index: 0,
        })
        .expect("删掉组里最后一个处理器应当成功");

        let text = fs::read_to_string(&path).unwrap();
        let written: Value = serde_json::from_str(&text).expect("删除后文件仍是合法 JSON");
        assert_eq!(
            written["hooks"]["UserPromptSubmit"][0]["hooks"],
            serde_json::json!([]),
            "空组要留在文件里，而不是被剪掉"
        );
        assert_eq!(
            written["hooks"]["UserPromptSubmit"]
                .as_array()
                .unwrap()
                .len(),
            1,
            "组本身仍在"
        );
        assert_eq!(handler_names(&written), ["bash-a", "stop-a", "stop-b"]);
    }

    #[test]
    fn an_out_of_range_position_errors_without_touching_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = write_hooks_fixture(&directory);
        let before = fs::read_to_string(&path).unwrap();

        for address in [
            // One past the handlers of a group that exists.
            HookAddress {
                path: path.clone(),
                event: "Stop".into(),
                group_index: 0,
                handler_index: 2,
            },
            // A group that does not exist.
            HookAddress {
                path: path.clone(),
                event: "Stop".into(),
                group_index: 9,
                handler_index: 0,
            },
            // An event that does not exist.
            HookAddress {
                path: path.clone(),
                event: "SessionStart".into(),
                group_index: 0,
                handler_index: 0,
            },
        ] {
            assert!(
                remove_hook_from_file(&address).is_err(),
                "越界必须报错，而不是写坏文件"
            );
            assert_eq!(
                fs::read_to_string(&path).unwrap(),
                before,
                "报错时文件不能被改动"
            );
        }
    }

    /// A hook's id follows its command, not its place in the file: inserting a
    /// handler ahead of it, deleting one before it or reordering leaves it on
    /// the command that was ticked, while changing what it runs (or when)
    /// makes it a different hook. Identical handlers stay distinct.
    #[test]
    fn a_hook_keeps_its_id_when_handlers_move_and_loses_it_when_its_command_changes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("hooks.json");
        let scan = |json: serde_json::Value| {
            fs::write(&path, serde_json::to_string(&json).unwrap()).unwrap();
            read_hooks_file(&path, ResourceSource::User, None)
                .into_iter()
                .map(|entry| (entry.definition.command.clone(), entry.descriptor.id))
                .collect::<HashMap<_, _>>()
        };
        let handler = |command: &str| serde_json::json!({"type": "command", "command": command});
        let before = scan(serde_json::json!({"hooks": {"PreToolUse": [
            {"matcher": "^bash$", "hooks": [handler("check-a"), handler("check-b")]}
        ]}}));
        assert_eq!(before.len(), 2);
        assert!(before["check-a"].starts_with("hook_user_pretooluse_"));

        // A new handler first, the old two swapped: both keep their ids.
        let after = scan(serde_json::json!({"hooks": {"PreToolUse": [
            {"matcher": "^bash$", "hooks": [handler("new"), handler("check-b"), handler("check-a")]}
        ]}}));
        assert_eq!(after["check-a"], before["check-a"]);
        assert_eq!(after["check-b"], before["check-b"]);
        assert!(!before.values().any(|id| *id == after["new"]));

        // The first deleted: the second does not inherit its id.
        let after = scan(serde_json::json!({"hooks": {"PreToolUse": [
            {"matcher": "^bash$", "hooks": [handler("check-b")]}
        ]}}));
        assert_eq!(after["check-b"], before["check-b"]);

        // Another matcher, event or command is another hook.
        for changed in [
            serde_json::json!({"hooks": {"PreToolUse": [{"matcher": "^zsh$", "hooks": [handler("check-a")]}]}}),
            serde_json::json!({"hooks": {"PostToolUse": [{"matcher": "^bash$", "hooks": [handler("check-a")]}]}}),
        ] {
            assert_ne!(scan(changed)["check-a"], before["check-a"]);
        }

        // Two identical handlers are two hooks.
        fs::write(
            &path,
            serde_json::to_string(&serde_json::json!({"hooks": {"Stop": [
                {"hooks": [handler("same"), handler("same")]}
            ]}}))
            .unwrap(),
        )
        .unwrap();
        let twins = read_hooks_file(&path, ResourceSource::User, None);
        assert_eq!(twins.len(), 2);
        assert_ne!(twins[0].descriptor.id, twins[1].descriptor.id);
        assert!(!is_legacy_hook_id(&twins[0].descriptor.id));
        assert!(is_legacy_hook_id(&twins[0].legacy_id));
    }

    /// Selections saved under position ids are translated to the handler at
    /// that position the first time this build sees them, and that answer is
    /// kept: editing the file afterwards does not retarget them.
    #[test]
    fn legacy_hook_selections_are_translated_once_against_the_file_as_it_was() {
        let home = tempfile::tempdir().unwrap();
        let app_data = tempfile::tempdir().unwrap();
        let config = home.path().join(".mewrk");
        fs::create_dir_all(&config).unwrap();
        let hooks = config.join("hooks.json");
        let write = |commands: &[&str]| {
            let handlers = commands
                .iter()
                .map(|command| serde_json::json!({"type": "command", "command": command}))
                .collect::<Vec<_>>();
            fs::write(
                &hooks,
                serde_json::json!({"hooks": {"Stop": [{"hooks": handlers}]}}).to_string(),
            )
            .unwrap();
        };
        write(&["first", "second"]);
        let mut document = crate::catalog::default_document();
        document.workspaces[0].path = home.path().to_string_lossy().into_owned();
        document.workspaces[0].id = "ws_home".into();
        let entries = read_hooks_file(&hooks, ResourceSource::Workspace, Some("ws_home"));
        let (first, second) = (&entries[0], &entries[1]);
        let gone = "hook_workspace_stop_0_7_0123abcd".to_owned();
        document.workspaces[0].conversations[0].settings.hook_ids =
            vec![second.legacy_id.clone(), gone.clone(), first.legacy_id.clone()];
        document.presets.conversation_presets[0].settings.hook_ids =
            vec![first.legacy_id.clone()];

        migrate_legacy_hook_ids(&mut document, app_data.path());
        assert_eq!(
            document.workspaces[0].conversations[0].settings.hook_ids,
            vec![second.descriptor.id.clone(), gone.clone(), first.descriptor.id.clone()]
        );
        assert_eq!(
            document.presets.conversation_presets[0].settings.hook_ids,
            vec![first.descriptor.id.clone()]
        );

        // The file changes; a conversation that still holds the old ids (it was
        // never written since) is translated as it was the first time.
        write(&["second"]);
        let mut again = crate::catalog::default_document();
        again.workspaces[0].path = home.path().to_string_lossy().into_owned();
        again.workspaces[0].id = "ws_home".into();
        again.workspaces[0].conversations[0].settings.hook_ids =
            vec![first.legacy_id.clone()];
        migrate_legacy_hook_ids(&mut again, app_data.path());
        assert_eq!(
            again.workspaces[0].conversations[0].settings.hook_ids,
            vec![first.descriptor.id.clone()],
            "the first handler's position now holds `second`, but the selection stays on `first`"
        );
    }

    #[test]
    fn a_discovered_hook_can_be_located_and_deleted_end_to_end() {
        let directory = tempfile::tempdir().unwrap();
        let path = write_hooks_fixture(&directory);

        let entries = read_hooks_file(&path, ResourceSource::User, None);
        assert_eq!(entries.len(), 4, "夹具里四个可执行的处理器");
        let target = entries
            .iter()
            .find(|entry| entry.descriptor.name == "stop-a")
            .expect("夹具里应有 stop-a");
        let address = parse_hook_location(&target.descriptor.location)
            .expect("描述符的 location 必须能解析回地址");
        assert_eq!(address.path, path, "解析回来的路径必须是这个文件");
        assert_eq!(
            (
                address.event.as_str(),
                address.group_index,
                address.handler_index
            ),
            ("Stop", 0, 0)
        );

        remove_hook_from_file(&address).expect("删掉刚发现的钩子应当成功");

        let remaining = read_hooks_file(&path, ResourceSource::User, None);
        assert_eq!(remaining.len(), entries.len() - 1);
        let mut names = remaining
            .iter()
            .map(|entry| entry.descriptor.name.as_str())
            .collect::<Vec<_>>();
        names.sort_unstable();
        assert_eq!(names, ["bash-a", "prompt-a", "stop-b"]);
    }
}
