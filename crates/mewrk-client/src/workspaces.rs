//! Projects and the machines they are on (`lib/workspaces.ts`), and the commands that add, edit and delete them
//! (`App.tsx`'s `addWorkspace`, `updateProject`, `deleteWorkspace`, `reorderWorkspace`, `setWorkspaceDefaultPreset`).

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::model::{AttachedWorkspace, Conversation, ConversationWorktree, RunTarget, SshMachineConfig, ToolDescriptor, ToolParameter, ToolParameterType, Workspace, WorkspaceKind};
use crate::{t, App};

/// The id of the project every conversation without one falls back to.
pub const TEMPORARY_WORKSPACE_ID: &str = "__temporary__";

/// A machine's identity: `local`, `wsl:<distro>`, or `ssh:<machine id>`. Matches the host `run_environment::env_key`
/// exactly. It is what tells two workspaces apart when their paths are spelled alike: one machine's `/srv/app` is not
/// another's.
pub fn run_env_key(target: Option<&RunTarget>) -> String {
    match target {
        None => "local".to_owned(),
        Some(RunTarget::Wsl { distro }) => format!("wsl:{distro}"),
        Some(RunTarget::Ssh { machine_id }) => format!("ssh:{machine_id}"),
    }
}

/// Key of a workspace's environment-variable table in `ExecutionEnvironmentAssets.env_vars`. Matches the host
/// `run_environment::workspace_env_key` exactly. Variables belong to the workspace, not to the machine it is on.
pub fn workspace_env_key(machine: Option<&RunTarget>, path: &str) -> String {
    format!("{}|{path}", run_env_key(machine))
}

fn trim_separators(path: &str) -> &str {
    path.trim_end_matches(['/', '\\'])
}

/// Key of a workspace's capability level: the machine and the registered directory, less trailing separators. Mirrors
/// the host's `capabilities::workspace_key`. A level belongs to the directory, not to a project or a conversation.
pub fn capability_workspace_key(machine: Option<&RunTarget>, path: &str) -> String {
    let trimmed = trim_separators(path);
    workspace_env_key(machine, if trimmed.is_empty() { path } else { trimmed })
}

/// The machine and directory a [`capability_workspace_key`] names.
pub fn parse_capability_workspace_key(key: &str) -> (Option<RunTarget>, String) {
    let (environment, path) = key.split_once('|').unwrap_or(("local", key));
    if let Some(distro) = environment.strip_prefix("wsl:") {
        return (Some(RunTarget::Wsl { distro: distro.to_owned() }), path.to_owned());
    }
    if let Some(machine_id) = environment.strip_prefix("ssh:") {
        return (Some(RunTarget::Ssh { machine_id: machine_id.to_owned() }), path.to_owned());
    }
    (None, path.to_owned())
}

/// One of a conversation's workspaces whose `.mewrk` it reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapabilityWorkspace {
    /// The number the model addresses it by.
    pub number: usize,
    /// Its [`capability_workspace_key`], which its catalog rows carry.
    pub key: String,
    pub machine: Option<RunTarget>,
    /// The registered directory, absolute.
    pub path: String,
}

/// The workspaces whose skills, MCP servers and hooks a conversation uses, numbered as the model addresses them: the
/// project's workspaces, then the conversation's own attached ones. A workspace is listed as registered, since a
/// worktree reads the configuration of the directory it was checked out from; the temporary project's scratch folder
/// has no `.mewrk` of its own but still takes number 1. Mirrors the host's `capabilities::conversation_locations`.
pub fn capability_workspaces(workspace: Option<&Workspace>, attached: &[AttachedWorkspace]) -> Vec<CapabilityWorkspace> {
    let Some(workspace) = workspace else { return Vec::new() };
    let own: Vec<Option<AttachedWorkspace>> = if is_temporary_workspace(workspace) {
        vec![None]
    } else {
        registered_project_workspaces(Some(workspace)).into_iter().map(Some).collect()
    };
    let mut seen = HashSet::new();
    let mut found = Vec::new();
    for (index, entry) in own.into_iter().chain(attached.iter().cloned().map(Some)).enumerate() {
        let Some(entry) = entry.filter(|entry| !entry.path.trim().is_empty()) else { continue };
        let key = capability_workspace_key(entry.machine.as_ref(), &entry.path);
        if seen.insert(key.clone()) {
            found.push(CapabilityWorkspace { number: index + 1, key, machine: entry.machine, path: entry.path });
        }
    }
    found
}

/// How many projects and conversations have a workspace on one machine.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MachineUsage {
    pub projects: usize,
    pub conversations: usize,
}

/// How many projects and conversations still have a workspace on `machine` (`None` for this one): a project by its
/// first workspace or any of its further ones, a conversation by one of its attached workspaces. The temporary project
/// has no directory of its own and never counts.
pub fn machine_usage(workspaces: &[Workspace], machine: Option<&RunTarget>) -> MachineUsage {
    let key = run_env_key(machine);
    let on_machine = |entry: Option<&RunTarget>| run_env_key(entry) == key;
    let mut usage = MachineUsage::default();
    for project in workspaces {
        if !is_temporary_workspace(project) && (on_machine(project.machine.as_ref()) || project.additional_workspaces.iter().any(|entry| on_machine(entry.machine.as_ref()))) {
            usage.projects += 1;
        }
        usage.conversations += project.conversations.iter().filter(|conversation| conversation.attached_workspaces.iter().any(|entry| on_machine(entry.machine.as_ref()))).count();
    }
    usage
}

/// Whether two machine bindings name the same machine.
pub fn same_machine(left: Option<&RunTarget>, right: Option<&RunTarget>) -> bool {
    run_env_key(left) == run_env_key(right)
}

/// Whether `machine` is an SSH machine that has been deleted from the catalog. Its workspaces stop working until they
/// are given a machine and directory again: registering the same host again makes a new machine id.
pub fn is_deleted_machine(machine: Option<&RunTarget>, ssh_machines: &[SshMachineConfig]) -> bool {
    matches!(machine, Some(RunTarget::Ssh { machine_id }) if !ssh_machines.iter().any(|entry| entry.id == *machine_id))
}

/// How a machine that is not this one is named beside a path: `WSL: Ubuntu`, `SSH: devbox`. `None` for the host machine.
/// The SSH catalog name, not the id: a machine the user renamed is the one they recognize by its name. A machine deleted
/// from the catalog is called that, since its id means nothing to anyone.
pub fn workspace_machine_label(machine: Option<&RunTarget>, ssh_machines: &[SshMachineConfig]) -> Option<String> {
    match machine? {
        RunTarget::Wsl { distro } => Some(format!("WSL: {distro}")),
        RunTarget::Ssh { machine_id } => Some(match ssh_machines.iter().find(|entry| entry.id == *machine_id) {
            Some(entry) => format!("SSH: {}", entry.name),
            None => t("已删除的机器", "Deleted machine").to_owned(),
        }),
    }
}

/// The chip label for a directory: its last segment, since a chip cannot hold an absolute path. The full path stays in
/// the chip's title, which is what the user checks when two directories share a name.
pub fn workspace_directory_label(path: &str) -> String {
    let whole = path.trim();
    let trimmed = trim_separators(whole);
    let last = trimmed.rfind(['/', '\\']).map_or(trimmed, |separator| &trimmed[separator + 1..]);
    // A root has no last segment; it is labelled by itself.
    if !last.is_empty() {
        last.to_owned()
    } else if !trimmed.is_empty() {
        trimmed.to_owned()
    } else {
        whole.to_owned()
    }
}

/// A path with its machine appended when the machine is not this one.
pub fn workspace_location_title(path: &str, machine: Option<&RunTarget>, ssh_machines: &[SshMachineConfig]) -> String {
    match workspace_machine_label(machine, ssh_machines) {
        Some(label) => format!("{path} ({label})"),
        None => path.to_owned(),
    }
}

/// Whether two records name the same directory on the same machine. Mirrors the host's `same_location`.
fn same_workspace_location(left: &AttachedWorkspace, right: &AttachedWorkspace) -> bool {
    same_machine(left.machine.as_ref(), right.machine.as_ref()) && trim_separators(&left.path) == trim_separators(&right.path)
}

/// The project's workspaces as registered, never a worktree standing in for one, in order: its first directory, then
/// the ones added after it.
pub fn registered_project_workspaces(workspace: Option<&Workspace>) -> Vec<AttachedWorkspace> {
    let Some(workspace) = workspace.filter(|workspace| !is_temporary_workspace(workspace)) else { return Vec::new() };
    std::iter::once(AttachedWorkspace { machine: workspace.machine.clone(), path: workspace.path.clone() }).chain(workspace.additional_workspaces.iter().cloned()).collect()
}

/// The worktree standing in for the project workspace at 1-based `member`, registered at `registered`. Mirrors the
/// host's `Conversation::worktree_for`: a record names its workspace by machine and path, never by position, and a
/// record with no workspace is workspace 1's.
pub fn worktree_for<'a>(worktrees: &'a [ConversationWorktree], member: usize, registered: &AttachedWorkspace) -> Option<&'a ConversationWorktree> {
    worktrees.iter().find(|worktree| match &worktree.workspace {
        Some(source) => same_workspace_location(source, registered),
        None => member == 1,
    })
}

/// `worktrees` with the record for the project workspace at `member` replaced by `worktree`, or removed when it is `None`.
pub fn with_conversation_worktree(worktrees: &[ConversationWorktree], member: usize, registered: &AttachedWorkspace, worktree: Option<ConversationWorktree>) -> Vec<ConversationWorktree> {
    let mut others: Vec<ConversationWorktree> = worktrees.iter().filter(|candidate| !match &candidate.workspace {
        Some(source) => same_workspace_location(source, registered),
        None => member == 1,
    }).cloned().collect();
    others.extend(worktree);
    others
}

/// A worktree's short name: the last segment of its directory, which is what Git calls it.
pub fn worktree_name(worktree: &ConversationWorktree) -> String {
    workspace_directory_label(&worktree.path)
}

/// A project's own workspaces in order. With a conversation's worktrees, each entry is the directory that conversation's
/// tools actually use: its isolated worktree of that workspace when it has one. A temporary project has none.
pub fn project_workspaces(workspace: Option<&Workspace>, worktrees: &[ConversationWorktree]) -> Vec<AttachedWorkspace> {
    registered_project_workspaces(workspace).into_iter().enumerate().map(|(index, registered)| match worktree_for(worktrees, index + 1, &registered) {
        Some(worktree) => AttachedWorkspace { machine: registered.machine.clone(), path: worktree.path.clone() },
        None => registered,
    }).collect()
}

/// The conversation's workspaces in the order the model addresses them: the project's first, its primary being 1, and
/// the conversation's own attached ones after them. Mirrors `workspace_set::WorkspaceSet::resolve` on the host, which
/// is the authority. A temporary project still counts as workspace 1: the host gives it a scratch directory.
pub fn conversation_workspaces(workspace: Option<&Workspace>, conversation: Option<&Conversation>) -> Vec<AttachedWorkspace> {
    let worktrees = conversation.map_or(&[][..], |conversation| &conversation.worktrees);
    let mut list = match workspace {
        None => Vec::new(),
        Some(workspace) if is_temporary_workspace(workspace) => vec![AttachedWorkspace { machine: None, path: workspace.path.clone() }],
        Some(_) => project_workspaces(workspace, worktrees),
    };
    if let Some(conversation) = conversation {
        list.extend(conversation.attached_workspaces.iter().cloned());
    }
    list
}

/// A shell the built-in terminal can start. Mirrors the host's `terminal::TerminalShell`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TerminalShell {
    Powershell,
    Bash,
    Zsh,
    Fish,
    Sh,
}

/// Whether the host is Windows, from a platform string.
pub fn host_is_windows(platform: &str) -> bool {
    platform.trim().get(..3).is_some_and(|prefix| prefix.eq_ignore_ascii_case("win"))
}

/// The name a shell goes by in a menu and on a tab.
pub fn terminal_shell_label(shell: TerminalShell) -> &'static str {
    match shell {
        TerminalShell::Powershell => "PowerShell",
        TerminalShell::Bash => "bash",
        TerminalShell::Zsh => "zsh",
        TerminalShell::Fish => "fish",
        TerminalShell::Sh => "sh",
    }
}

/// The tools whose wire schema the host gives a `workspace` argument once the conversation has more than one workspace,
/// besides every shell tool. Mirrors `builtin_schemas::takes_a_workspace`.
const WORKSPACE_SCOPED_TOOLS: [&str; 7] = ["ls", "grep", "find", "read", "write", "edit", "lsp"];

/// The descriptors a timeline's manual tool cards are edited against, with the `workspace` argument the host adds on the
/// wire when the conversation has more than one workspace. The static descriptors cannot carry it: which numbers exist
/// is a property of the conversation, not of the tool, and a card placed by hand has to be able to name workspace 2 the
/// same way the model does.
///
/// The argument is optional and starts empty, so an untouched field records nothing and the host applies its own default
/// of 1. A shell tool lists only the workspaces whose machine has its shell (`shells_at` answers that from the machines'
/// probes), and with none, the host withdraws the tool from the wire and the descriptor is left alone.
pub fn with_workspace_argument(tools: &[ToolDescriptor], workspaces: &[AttachedWorkspace], shells_at: impl Fn(Option<&RunTarget>) -> Vec<String>, label: &str) -> Vec<ToolDescriptor> {
    if workspaces.len() < 2 {
        return tools.to_vec();
    }
    let shells: Vec<Vec<String>> = workspaces.iter().map(|workspace| shells_at(workspace.machine.as_ref())).collect();
    tools.iter().map(|tool| {
        let shell = matches!(tool.name.as_str(), "bash" | "zsh" | "sh" | "pwsh" | "powershell").then_some(tool.name.as_str());
        if (shell.is_none() && !WORKSPACE_SCOPED_TOOLS.contains(&tool.name.as_str())) || tool.parameters.iter().any(|parameter| parameter.name == "workspace") {
            return tool.clone();
        }
        let addresses: Vec<String> = (1..=workspaces.len()).filter(|address| shell.is_none_or(|shell| shells[address - 1].iter().any(|found| found == shell))).map(|address| address.to_string()).collect();
        if addresses.is_empty() {
            return tool.clone();
        }
        let mut next = tool.clone();
        next.parameters.push(ToolParameter { name: "workspace".to_owned(), label: label.to_owned(), parameter_type: ToolParameterType::Number, required: false, placeholder: Some(addresses.join(" | ")), help: None, default_value: None });
        next
    }).collect()
}

/// Identifies the entry point that creates a conversation. The sidebar's New Task, Ctrl+N and the empty-state button use
/// `Global` and always apply the global default preset. A workspace title-row `+` uses `Workspace`, first checking its
/// default preset, then its remembered conversation settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NewConversationSource {
    Global,
    Workspace,
}

/// Whether this is the temporary project.
pub fn is_temporary_workspace(workspace: &Workspace) -> bool {
    workspace.kind == WorkspaceKind::Temporary || workspace.id == TEMPORARY_WORKSPACE_ID
}

/// Whether the project can't be deleted.
pub fn is_reserved_workspace(workspace: &Workspace) -> bool {
    is_temporary_workspace(workspace)
}

/// The temporary project always closes the list: projects are ordered among themselves, and the scratch space they all
/// fall back to stays at the bottom whatever is added or moved.
pub fn with_temporary_workspace_last(workspaces: &mut Vec<Workspace>) {
    let Some(index) = workspaces.iter().position(is_temporary_workspace) else { return };
    if index != workspaces.len() - 1 {
        // A stable partition, as the two filters upstream.
        let (temporary, mut projects): (Vec<Workspace>, Vec<Workspace>) = std::mem::take(workspaces).into_iter().partition(is_temporary_workspace);
        projects.extend(temporary);
        *workspaces = projects;
    }
}

/// `createTemporaryWorkspace`.
pub fn create_temporary_workspace(conversations: Vec<Conversation>) -> Workspace {
    Workspace {
        id: TEMPORARY_WORKSPACE_ID.to_owned(),
        name: "临时工作区".to_owned(),
        kind: WorkspaceKind::Temporary,
        path: String::new(),
        machine: None,
        additional_workspaces: Vec::new(),
        created_at: crate::doc::now_iso(),
        default_conversation_preset_id: String::new(),
        last_conversation_settings: None,
        draft_conversation: None,
        conversations,
    }
}

/// `isAbsoluteWorkspacePath` of `App.tsx`.
fn is_absolute_workspace_path(value: &str) -> bool {
    let path = value.trim();
    path.starts_with('/') || path.starts_with("\\\\") || has_drive_prefix(path)
}

fn has_drive_prefix(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && matches!(bytes[2], b'\\' | b'/')
}

/// `normalizedWorkspacePath` of `App.tsx`. A POSIX path keeps its case; only Windows spellings fold.
pub fn normalized_workspace_path(value: &str) -> String {
    let path = trim_separators(value.trim());
    if has_drive_prefix(path) || path.starts_with("\\\\") {
        path.to_lowercase()
    } else {
        path.to_owned()
    }
}

/// `reorderItems` of `components/usePointerDrag.ts`, in place. `false` when nothing moved.
pub fn reorder_items<T>(items: &mut Vec<T>, source_id: &str, target_id: &str, after: bool, get_id: impl Fn(&T) -> &str) -> bool {
    if source_id == target_id {
        return false;
    }
    let Some(source_index) = items.iter().position(|item| get_id(item) == source_id) else { return false };
    if !items.iter().any(|item| get_id(item) == target_id) {
        return false;
    }
    let moving = items.remove(source_index);
    let target_index = items.iter().position(|item| get_id(item) == target_id).unwrap_or(items.len());
    items.insert(target_index + usize::from(after), moving);
    true
}

/// The state of an SSH machine's link, from `remoteLinkChanged`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteLink {
    /// `connecting`, `connected`, `reconnecting`, `lost` or `unavailable`.
    pub state: String,
    pub detail: Option<String>,
}

/// One subdirectory in a remote listing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteDirectoryEntry {
    pub name: String,
    /// The path to ask for when the user opens it, spelled by the host: only it knows the machine's path rules.
    pub path: String,
}

/// One level of a machine's filesystem, as the remote browser reads it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteDirectoryListing {
    /// The directory the host actually resolved, with `~` expanded and `..` applied.
    pub path: String,
    /// The directory to go up to, or `None` at the top.
    pub parent: Option<String>,
    /// Immediate subdirectories, sorted.
    pub entries: Vec<RemoteDirectoryEntry>,
}

#[derive(Default)]
pub(crate) struct WorkspacesState {
    pub deleting_workspace_ids: HashSet<String>,
    pub remote_links: HashMap<String, RemoteLink>,
    pub project_editor: Option<String>,
    pub workspace_dialog_open: bool,
    pub assign_workspace_after_add: bool,
}

/// Lookups.
impl App {
    /// The workspace with this id.
    pub fn workspace(&self, workspace_id: &str) -> Option<&Workspace> {
        self.doc.workspaces.iter().find(|workspace| workspace.id == workspace_id)
    }

    /// `deletingWorkspaceIds.has`: its deletion is under way, and everything in it is locked.
    pub fn workspace_is_deleting(&mut self, workspace_id: &str) -> bool {
        self.local::<WorkspacesState>().deleting_workspace_ids.contains(workspace_id)
    }

    /// `workspaceMachineLabel` with the document's catalog.
    pub fn machine_label(&self, machine: Option<&RunTarget>) -> Option<String> {
        workspace_machine_label(machine, &self.doc.assets.execution_environments.ssh_machines)
    }

    /// `remoteLinks`: each SSH host's last link state.
    pub fn remote_link(&mut self, host: &str) -> Option<RemoteLink> {
        self.local::<WorkspacesState>().remote_links.get(host).cloned()
    }

    /// `remoteLinkNotice`: what to say about the link to `machine`'s SSH host, when there's something to say.
    pub fn remote_link_notice(&mut self, machine: Option<&RunTarget>) -> Option<String> {
        let Some(RunTarget::Ssh { machine_id }) = machine else { return None };
        let config = self.doc.assets.execution_environments.ssh_machines.iter().find(|candidate| candidate.id == *machine_id)?.clone();
        let link = self.remote_link(&config.host)?;
        let name = if config.name.is_empty() { config.host.clone() } else { config.name.clone() };
        match link.state.as_str() {
            "reconnecting" => Some(crate::tf("与 {name} 的连接中断，正在重新连接；页面和服务器会在连接恢复后继续", "Lost the connection to {name}; reconnecting. The page and its servers carry on once it is back", &[("name", &name)])),
            "lost" => Some(match link.detail.filter(|detail| !detail.is_empty()) {
                Some(detail) => crate::tf("无法连接到 {name}：{detail}", "Cannot reach {name}: {detail}", &[("name", &name), ("detail", &detail)]),
                None => crate::tf("无法连接到 {name}", "Cannot reach {name}", &[("name", &name)]),
            }),
            "unavailable" => Some(crate::tf("{name} 上无法运行 Mewrk 的远程代理，这台机器上的预览不可用", "Mewrk's remote agent cannot run on {name}, so previews there are unavailable", &[("name", &name)])),
            _ => None,
        }
    }

    pub(crate) fn init_workspaces(&mut self) {
        self.on_push_event(|app, event| {
            if event["type"] != "remoteLinkChanged" {
                return;
            }
            let Some(host) = event["host"].as_str() else { return };
            let Some(state) = event["state"].as_str() else { return };
            let link = RemoteLink { state: state.to_owned(), detail: event["detail"].as_str().map(str::to_owned) };
            app.local::<WorkspacesState>().remote_links.insert(host.to_owned(), link);
            crate::prelude::repaint();
        });
    }
}

/// The dialogs `addWorkspace` closes. Views read these; `App.tsx` keeps them next to the logic.
impl App {
    /// `workspaceDialogOpen`.
    pub fn workspace_dialog_open(&mut self) -> bool {
        self.local::<WorkspacesState>().workspace_dialog_open
    }

    /// `setWorkspaceDialogOpen`, and `setAssignWorkspaceAfterAdd` with it: whether the project chosen in the dialog
    /// takes the conversation on screen.
    pub fn set_workspace_dialog_open(&mut self, open: bool, assign_after_add: bool) {
        let state = self.local::<WorkspacesState>();
        state.workspace_dialog_open = open;
        state.assign_workspace_after_add = assign_after_add;
        crate::prelude::repaint();
    }

    /// `projectEditor`: the project whose edit dialog is open.
    pub fn project_editor(&mut self) -> Option<String> {
        self.local::<WorkspacesState>().project_editor.clone()
    }

    /// `setProjectEditor`.
    pub fn set_project_editor(&mut self, project_id: Option<String>) {
        self.local::<WorkspacesState>().project_editor = project_id;
        crate::prelude::repaint();
    }

    fn close_workspace_dialog(&mut self) {
        self.set_workspace_dialog_open(false, false);
    }
}

/// The host's directory pickers.
impl App {
    /// `pickWorkspaceDirectory`: the native folder dialog. `None` when cancelled. What it returns is authorized for this session.
    pub fn pick_workspace_directory(&self, done: impl FnOnce(&mut App, Result<Option<String>, String>) + Send + 'static) {
        self.invoke::<Option<String>>("pick_workspace_directory", json!({}), done);
    }

    /// `listRemoteDirectory`: one level of a WSL or SSH machine. An unreachable machine or an unreadable directory is an
    /// error, not an empty listing that would read as "this directory is empty".
    pub fn list_remote_directory(&self, machine: &RunTarget, path: &str, done: impl FnOnce(&mut App, Result<RemoteDirectoryListing, String>) + Send + 'static) {
        self.invoke::<RemoteDirectoryListing>("list_remote_directory", json!({ "machine": machine, "path": path }), done);
    }

    /// `authorizeRemoteWorkspace`: records a remote directory as granted to this session and answers the path the host
    /// resolved. The browser is what authorizes a remote workspace, as the native dialog authorizes a local one.
    pub fn authorize_remote_workspace(&self, machine: &RunTarget, path: &str, done: impl FnOnce(&mut App, Result<String, String>) + Send + 'static) {
        self.invoke::<String>("authorize_remote_workspace", json!({ "machine": machine, "path": path }), done);
    }

    /// `listWslDistros`: the distributions on this machine, as the host enumerates them.
    pub fn list_wsl_distros(&self, done: impl FnOnce(&mut App, Result<Vec<Value>, String>) + Send + 'static) {
        self.invoke::<Vec<Value>>("list_wsl_distros", json!({}), done);
    }
}

/// Project commands.
impl App {
    /// `addWorkspace`: registers a project (one or more workspaces, the first being its own directory) or reuses the
    /// project already registered with exactly those workspaces. Each workspace's identity is its machine and its path
    /// together. `assign_conversation` is `assignWorkspaceAfterAdd` unless the caller says otherwise.
    pub fn add_workspace(&mut self, name: &str, workspaces: &[AttachedWorkspace], assign_conversation: Option<bool>) {
        let assign_conversation = assign_conversation.unwrap_or_else(|| self.local::<WorkspacesState>().assign_workspace_after_add);
        let Some((first, members)) = workspaces.split_first() else { return };
        let path = first.path.clone();
        let machine = first.machine.clone();
        // A remote directory is POSIX and may begin `~`; only a directory on this machine is held
        // to this machine's idea of an absolute path.
        if machine.is_none() && !is_absolute_workspace_path(&path) {
            return;
        }
        let normalized = normalized_workspace_path(&path);
        let same_members = |workspace: &Workspace| {
            workspace.additional_workspaces.len() == members.len()
                && workspace.additional_workspaces.iter().zip(members).all(|(entry, member)| same_machine(entry.machine.as_ref(), member.machine.as_ref()) && normalized_workspace_path(&entry.path) == normalized_workspace_path(&member.path))
        };
        let source_workspace_id = self.active_workspace_id();
        let moving_conversation_id = self.active_conversation_id();
        let existing = self.doc.workspaces.iter().find(|workspace| workspace.kind == WorkspaceKind::Directory && same_machine(workspace.machine.as_ref(), machine.as_ref()) && normalized_workspace_path(&workspace.path) == normalized && same_members(workspace)).map(|workspace| workspace.id.clone());
        // A new task on screen moves nothing: picking a project for it goes to that project's own
        // draft, the existing project's at once or the new one's once it is registered.
        let assigning_draft = assign_conversation && moving_conversation_id.as_deref().is_some_and(|id| self.draft_of(id).is_some());
        if assigning_draft {
            if let Some(existing) = &existing {
                self.open_draft_conversation(Some(existing));
                self.close_workspace_dialog();
                return;
            }
        }
        if assign_conversation {
            if let (Some(source), Some(moving)) = (&source_workspace_id, &moving_conversation_id) {
                if self.conversation_move_is_blocked(moving, source, existing.as_deref().unwrap_or(source)) {
                    return;
                }
            }
            if let Some(existing) = &existing {
                self.move_active_conversation(existing, |app, _| app.close_workspace_dialog());
                return;
            }
        }
        if existing.is_some() {
            return;
        }
        let segments: Vec<&str> = path.trim().split(['/', '\\']).filter(|segment| !segment.is_empty()).collect();
        let workspace_name = if !name.trim().is_empty() {
            name.trim().to_owned()
        } else {
            segments.last().map_or_else(|| t("新项目", "New project").to_owned(), |last| (*last).to_owned())
        };
        let workspace = Workspace {
            id: crate::doc::create_id("ws"),
            name: workspace_name,
            kind: WorkspaceKind::Directory,
            path: path.trim().to_owned(),
            machine: machine.clone(),
            additional_workspaces: members.iter().map(|member| AttachedWorkspace { machine: member.machine.clone(), path: member.path.trim().to_owned() }).collect(),
            created_at: crate::doc::now_iso(),
            // A new workspace has no history, so leave its default preset empty until first creation records a snapshot.
            default_conversation_preset_id: String::new(),
            last_conversation_settings: None,
            draft_conversation: None,
            conversations: Vec::new(),
        };
        let workspace_id = workspace.id.clone();
        // A new project opens the list; the temporary one keeps closing it.
        self.update_document(|doc| {
            doc.workspaces.insert(0, workspace);
            with_temporary_workspace_last(&mut doc.workspaces);
        });
        // Everything below asks the host about the new project by id (the capability scan, and the
        // draft aimed at it the moment it is: its Git status, its dev servers, its file pane) and the
        // host answers only for a project its document holds. Left to the debounced save, the project
        // reaches the host after all of them, each is refused, and Git only asks again once its
        // backoff runs out. A save that fails is the save status's to report; the draft still opens.
        let moving_is_empty = moving_conversation_id.as_deref().is_some_and(|id| source_workspace_id.as_deref().is_some_and(|source| self.find_conversation(source, id).is_some_and(|conversation| self.conversation_has_no_contexts(conversation))));
        let should_assign = assign_conversation && source_workspace_id.is_some() && moving_is_empty;
        let barrier = workspace_id.clone();
        self.await_workspace_at_host(&barrier, move |app, _| {
            // A new project adds a whole configuration level: its first directory's `.mewrk`
            // may already hold skills, MCP servers and hooks that nothing has scanned
            // yet. A failure here only leaves the catalog as stale as it already was.
            // A directory on another machine is not scanned: the host reads capability
            // files from its own filesystem only.
            if machine.is_none() {
                app.rescan_capabilities(|_, _| {});
            }
            if assigning_draft {
                app.open_draft_conversation(Some(&workspace_id));
                app.close_workspace_dialog();
            } else if should_assign {
                app.move_active_conversation(&workspace_id, |app, _| app.close_workspace_dialog());
            } else {
                // A new workspace has no conversation, so open a draft rather than an empty selection.
                app.open_draft_conversation(Some(&workspace_id));
                app.close_workspace_dialog();
            }
        });
    }

    /// `updateProject`: changes an existing project's name and the workspaces after its first. The first workspace is
    /// the project's identity, so it changes only once its SSH machine has been deleted, when choosing a machine and
    /// directory again is the only way back. A workspace removed or replaced here takes its conversations' worktrees of
    /// it out of use: they name the workspace they came from, never a position. The oldest records name none and would
    /// be read as the new first workspace's, so replacing it drops them.
    pub fn update_project(&mut self, project_id: &str, name: &str, workspaces: &[AttachedWorkspace]) {
        let ssh_machines = self.doc.assets.execution_environments.ssh_machines.clone();
        self.update_document_if(|doc| {
            let Some(workspace) = doc.workspaces.iter_mut().find(|workspace| workspace.id == project_id && workspace.kind == WorkspaceKind::Directory) else { return false };
            let previous_machine = workspace.machine.clone();
            let primary = workspaces.first();
            let replaces_primary = primary.is_some_and(|primary| is_deleted_machine(previous_machine.as_ref(), &ssh_machines) && (run_env_key(primary.machine.as_ref()) != run_env_key(previous_machine.as_ref()) || primary.path.trim() != workspace.path));
            if replaces_primary {
                let primary = primary.expect("checked above");
                workspace.machine = primary.machine.clone();
                workspace.path = primary.path.trim().to_owned();
                for conversation in &mut workspace.conversations {
                    conversation.worktrees.retain(|worktree| worktree.workspace.is_some());
                }
            }
            if !name.trim().is_empty() {
                workspace.name = name.trim().to_owned();
            }
            workspace.additional_workspaces = workspaces.iter().skip(1).map(|member| AttachedWorkspace { machine: member.machine.clone(), path: member.path.trim().to_owned() }).collect();
            true
        });
        self.set_project_editor(None);
    }

    /// `reorderWorkspace`. The temporary project stays at the bottom; it neither moves nor lets a project past it.
    pub fn reorder_workspace(&mut self, workspace_id: &str, target_workspace_id: &str, after: bool) {
        if workspace_id == target_workspace_id {
            return;
        }
        self.update_document_if(|doc| {
            let source = doc.workspaces.iter().find(|workspace| workspace.id == workspace_id);
            let target = doc.workspaces.iter().find(|workspace| workspace.id == target_workspace_id);
            if source.is_none_or(is_temporary_workspace) || target.is_none() {
                return false;
            }
            reorder_items(&mut doc.workspaces, workspace_id, target_workspace_id, after, |workspace| &workspace.id);
            with_temporary_workspace_last(&mut doc.workspaces);
            true
        });
    }

    /// `setWorkspaceDefaultPreset`: sets or clears the project's default preset; clearing is "Last used", which follows
    /// the project's remembered settings. The project's draft, while it still holds what the project resolved it to,
    /// follows the change at once; one whose settings the user has changed is theirs and keeps them. A draft kept in the
    /// document after a restart is always one of those. Its opening messages are relaid only when nobody has touched them.
    pub fn set_workspace_default_preset(&mut self, workspace_id: &str, preset_id: &str) {
        let Some(mut workspace) = self.workspace(workspace_id).filter(|workspace| workspace.default_conversation_preset_id != preset_id).map(Workspace::shell_clone) else { return };
        self.update_document_if(|doc| {
            let Some(item) = doc.workspaces.iter_mut().find(|item| item.id == workspace_id) else { return false };
            item.default_conversation_preset_id = preset_id.to_owned();
            true
        });
        workspace.default_conversation_preset_id = preset_id.to_owned();
        self.draft_follow_default_preset(&workspace);
    }

    /// `deleteWorkspace`. Closes what's open in the project (terminals, browser pages, its draft's shells), releases its
    /// worktrees while the host's saved document still records them, then removes it. Does nothing to a reserved project,
    /// one already being deleted, or one with a conversation busy; the checks repeat after each wait.
    pub fn delete_workspace(&mut self, workspace_id: &str) {
        let Some(workspace) = self.workspace(workspace_id) else { return };
        if is_reserved_workspace(workspace) || self.workspace_is_deleting(workspace_id) || self.workspace_has_active_conversation(workspace_id) {
            return;
        }
        let conversation_ids_before: Vec<String> = self.workspace(workspace_id).map(|workspace| workspace.conversations.iter().map(|conversation| conversation.id.clone()).collect()).unwrap_or_default();
        self.local::<WorkspacesState>().deleting_workspace_ids.insert(workspace_id.to_owned());
        crate::prelude::repaint();
        let id = workspace_id.to_owned();
        let finish = |task: &crate::Task, id: String| {
            task.post(move |app| {
                app.local::<WorkspacesState>().deleting_workspace_ids.remove(&id);
                crate::prelude::repaint();
            });
        };
        let closes = {
            let mut closes = self.conversation_surface_closers(&conversation_ids_before);
            // The project's draft goes with it, its shells and page with everyone else's. A running
            // command of its own would otherwise hold up the deletion; the draft itself is forgotten
            // only once the project is gone.
            closes.extend(self.draft_surface_closers(workspace_id));
            closes
        };
        self.spawn(move |task| {
            let results: Vec<Result<(), String>> = crate::doc::run_all(closes, &task);
            let proceed = results.iter().all(Result::is_ok);
            let before = conversation_ids_before.clone();
            let check = id.clone();
            let still_same = task.update(move |app| {
                // A conversation added or moved meanwhile isn't what the user agreed to delete.
                !app.workspace_has_active_conversation(&check) && app.workspace(&check).is_some_and(|workspace| workspace.conversations.len() == before.len() && workspace.conversations.iter().zip(&before).all(|(conversation, id)| conversation.id == *id))
            });
            if !proceed || !still_same {
                return finish(&task, id);
            }
            // The project's worktrees go first, while the host's saved document still records them.
            let kept = crate::doc::release_deleted_worktrees(&task, &conversation_ids_before);
            let remove = id.clone();
            task.update(move |app| app.finish_delete_workspace(&remove, kept));
            finish(&task, id);
        });
    }

    fn finish_delete_workspace(&mut self, workspace_id: &str, kept: Vec<crate::doc::KeptWorktree>) {
        if self.workspace(workspace_id).is_none() || self.workspace_has_active_conversation(workspace_id) {
            return;
        }
        let fallback = self.doc.workspaces.iter().find(|item| item.id != workspace_id).map(|item| item.id.clone());
        let removed: Vec<Conversation> = self.workspace(workspace_id).map(|workspace| workspace.conversations.clone()).unwrap_or_default();
        let removed_ids: Vec<String> = removed.iter().map(|conversation| conversation.id.clone()).collect();
        self.invalidate_composer_images(&removed_ids);
        let queued: Vec<String> = removed.iter().flat_map(|conversation| conversation.queued_messages.iter().map(|message| message.id.clone())).collect();
        self.composer().forget_queued_messages(&queued);
        self.update_document(|doc| doc.workspaces.retain(|item| item.id != workspace_id));
        for id in &removed_ids {
            self.unloaded.remove(id);
        }
        self.forget_draft(workspace_id);
        for id in &removed_ids {
            self.forget_conversation_state(id);
        }
        if self.active_workspace_id().as_deref() == Some(workspace_id) {
            // After deletion, open the new task of the first remaining project.
            self.open_draft_conversation(Some(fallback.as_deref().unwrap_or(TEMPORARY_WORKSPACE_ID)));
            self.close_editors();
        }
        self.report_kept_worktrees(kept);
    }

    fn workspace_has_active_conversation(&mut self, workspace_id: &str) -> bool {
        let ids: Vec<String> = self.workspace(workspace_id).map(|workspace| workspace.conversations.iter().map(|conversation| conversation.id.clone()).collect()).unwrap_or_default();
        ids.iter().any(|id| self.runs().controller.run(id).is_some() || self.conversation_operation_is_active(id) || self.conversation_is_deleting(id))
    }
}

/// A workspace without its conversations, for the settings resolution that reads only its own fields.
trait ShellClone {
    fn shell_clone(&self) -> Workspace;
}

impl ShellClone for Workspace {
    fn shell_clone(&self) -> Workspace {
        Workspace {
            id: self.id.clone(),
            name: self.name.clone(),
            kind: self.kind,
            path: self.path.clone(),
            machine: self.machine.clone(),
            additional_workspaces: self.additional_workspaces.clone(),
            created_at: self.created_at.clone(),
            default_conversation_preset_id: self.default_conversation_preset_id.clone(),
            last_conversation_settings: self.last_conversation_settings.clone(),
            draft_conversation: self.draft_conversation.clone(),
            conversations: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::test_support::conversation;
    use crate::model::{ToolCategory, WorkspaceKind};

    fn project(id: &str, path: &str) -> Workspace {
        Workspace { id: id.into(), name: id.into(), kind: WorkspaceKind::Directory, path: path.into(), ..create_temporary_workspace(Vec::new()) }
    }

    fn ids(list: &[Workspace]) -> Vec<&str> {
        list.iter().map(|workspace| workspace.id.as_str()).collect()
    }

    #[test]
    fn treats_only_the_canonical_temporary_workspace_as_reserved() {
        assert!(is_reserved_workspace(&create_temporary_workspace(Vec::new())));
        assert!(!is_reserved_workspace(&project("ws_mewrk", "C:\\test\\Mewrk")));
    }

    #[test]
    fn creates_a_canonical_temporary_workspace() {
        let workspace = create_temporary_workspace(Vec::new());
        assert_eq!(workspace.id, "__temporary__");
        assert_eq!(workspace.name, "临时工作区");
        assert_eq!(workspace.kind, WorkspaceKind::Temporary);
        assert_eq!(workspace.path, "");
        assert!(workspace.conversations.is_empty());
        assert!(is_temporary_workspace(&workspace));
    }

    #[test]
    fn keeps_the_temporary_workspace_last_and_leaves_an_ordered_list_untouched() {
        let temporary = create_temporary_workspace(Vec::new());
        let mut ordered = vec![project("ws_a", "C:\\a"), project("ws_b", "C:\\b"), temporary.clone()];
        with_temporary_workspace_last(&mut ordered);
        assert_eq!(ids(&ordered), vec!["ws_a", "ws_b", "__temporary__"]);
        let mut first = vec![temporary.clone(), project("ws_a", "C:\\a"), project("ws_b", "C:\\b")];
        with_temporary_workspace_last(&mut first);
        assert_eq!(ids(&first), vec!["ws_a", "ws_b", "__temporary__"]);
        let mut middle = vec![project("ws_a", "C:\\a"), temporary, project("ws_b", "C:\\b")];
        with_temporary_workspace_last(&mut middle);
        assert_eq!(ids(&middle), vec!["ws_a", "ws_b", "__temporary__"]);
        let mut without = vec![project("ws_a", "C:\\a")];
        with_temporary_workspace_last(&mut without);
        assert_eq!(ids(&without), vec!["ws_a"]);
    }

    fn descriptor(name: &str, category: ToolCategory) -> ToolDescriptor {
        serde_json::from_value(json!({ "name": name, "label": name, "description": "", "category": category, "dangerous": false, "parameters": [{ "name": "path", "label": "path", "type": "string", "required": true }] })).unwrap()
    }

    fn tools() -> Vec<ToolDescriptor> {
        vec![descriptor("read", ToolCategory::Filesystem), descriptor("bash", ToolCategory::Shell), descriptor("pwsh", ToolCategory::Shell), descriptor("powershell", ToolCategory::Shell), descriptor("web_fetch", ToolCategory::Web)]
    }

    fn local() -> AttachedWorkspace {
        AttachedWorkspace { machine: None, path: "C:\\src\\app".into() }
    }

    fn remote() -> AttachedWorkspace {
        AttachedWorkspace { machine: Some(RunTarget::Ssh { machine_id: "m1".into() }), path: "/srv/app".into() }
    }

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    // This machine is Windows with both PowerShell editions and Git Bash; the SSH machine has bash.
    fn windows_host(machine: Option<&RunTarget>) -> Vec<String> {
        if machine.is_some() { strings(&["bash"]) } else { strings(&["pwsh", "powershell", "bash"]) }
    }

    fn posix_host(machine: Option<&RunTarget>) -> Vec<String> {
        if machine.is_some() { strings(&["bash"]) } else { strings(&["zsh", "bash", "sh"]) }
    }

    fn names(tool: &ToolDescriptor) -> Vec<&str> {
        tool.parameters.iter().map(|parameter| parameter.name.as_str()).collect()
    }

    #[test]
    fn leaves_every_descriptor_alone_while_the_conversation_has_one_workspace() {
        assert_eq!(with_workspace_argument(&tools(), &[local()], windows_host, "Workspace"), tools());
    }

    #[test]
    fn offers_the_hosts_numbers_to_the_workspace_scoped_tools_only() {
        let result = with_workspace_argument(&tools(), &[local(), remote()], windows_host, "Workspace");
        assert_eq!(names(&result[0]), vec!["path", "workspace"]);
        assert_eq!(names(&result[1]), vec!["path", "workspace"]);
        assert_eq!(names(&result[4]), vec!["path"]);
        let argument = result[0].parameters.last().unwrap();
        assert_eq!(argument.parameter_type, ToolParameterType::Number);
        assert!(!argument.required);
        assert_eq!(argument.placeholder.as_deref(), Some("1 | 2"));
        assert_eq!(argument.default_value, None);
        // Each PowerShell edition runs only where a machine has it: this one.
        assert_eq!(result[2].parameters.last().unwrap().placeholder.as_deref(), Some("1"));
        assert_eq!(result[3].parameters.last().unwrap().placeholder.as_deref(), Some("1"));
    }

    #[test]
    fn withdraws_the_argument_from_a_shell_tool_where_no_machine_has_the_shell() {
        let result = with_workspace_argument(&tools(), &[remote(), remote()], windows_host, "Workspace");
        assert_eq!(names(&result[2]), vec!["path"]);
        assert_eq!(names(&result[3]), vec!["path"]);
        let posix = with_workspace_argument(&tools(), &[local(), remote()], posix_host, "Workspace");
        assert_eq!(names(&posix[2]), vec!["path"]);
        assert_eq!(names(&posix[3]), vec!["path"]);
    }

    #[test]
    fn treats_powershell_7_and_windows_powershell_as_two_shells_each_addressed_on_its_own_machines() {
        let second = AttachedWorkspace { machine: Some(RunTarget::Ssh { machine_id: "m2".into() }), path: "C:\\two".into() };
        let editions = |machine: Option<&RunTarget>| if machine.is_some() { strings(&["pwsh"]) } else { strings(&["powershell"]) };
        let result = with_workspace_argument(&tools(), &[local(), second], editions, "Workspace");
        assert_eq!(result[2].parameters.last().unwrap().placeholder.as_deref(), Some("2"));
        assert_eq!(result[3].parameters.last().unwrap().placeholder.as_deref(), Some("1"));
    }

    #[test]
    fn does_not_double_a_workspace_argument_a_descriptor_already_declares() {
        let mut declared = descriptor("read", ToolCategory::Filesystem);
        declared.parameters = serde_json::from_value(json!([{ "name": "workspace", "label": "ws", "type": "number", "required": false }])).unwrap();
        let result = with_workspace_argument(std::slice::from_ref(&declared), &[local(), remote()], windows_host, "Workspace");
        assert_eq!(result[0], declared);
    }

    fn platform() -> Workspace {
        let mut workspace = project("ws_platform", "C:\\platform");
        workspace.additional_workspaces = vec![AttachedWorkspace { machine: Some(RunTarget::Ssh { machine_id: "m1".into() }), path: "/srv/api".into() }, AttachedWorkspace { machine: None, path: "D:\\shared\\lib".into() }];
        workspace
    }

    fn worktree(path: &str, workspace: Option<AttachedWorkspace>) -> ConversationWorktree {
        ConversationWorktree { path: path.into(), branch: "b".into(), base_oid: "o".into(), base_branch: None, workspace }
    }

    #[test]
    fn lists_the_projects_first_directory_and_then_the_ones_added_after_it() {
        let list = project_workspaces(Some(&platform()), &[]);
        assert_eq!(list.iter().map(|entry| entry.path.as_str()).collect::<Vec<_>>(), vec!["C:\\platform", "/srv/api", "D:\\shared\\lib"]);
        assert_eq!(list[1].machine, Some(RunTarget::Ssh { machine_id: "m1".into() }));
        assert!(project_workspaces(Some(&create_temporary_workspace(Vec::new())), &[]).is_empty());
    }

    #[test]
    fn lets_a_legacy_worktree_record_stand_in_for_the_first_directory_only() {
        let legacy = [worktree("C:\\platform\\.mewrk\\worktrees\\c1", None)];
        let list = project_workspaces(Some(&platform()), &legacy);
        assert_eq!(list[0], AttachedWorkspace { machine: None, path: legacy[0].path.clone() });
        assert_eq!(list[1].path, "/srv/api");
    }

    #[test]
    fn lets_each_workspaces_own_worktree_stand_in_for_it_on_its_own_machine() {
        let registered = project_workspaces(Some(&platform()), &[])[1].clone();
        let records = [worktree("/srv/api/.mewrk/worktrees/conversations/c1", Some(registered.clone()))];
        let resolved = project_workspaces(Some(&platform()), &records);
        assert_eq!(resolved[0].path, "C:\\platform");
        assert_eq!(resolved[1], AttachedWorkspace { machine: registered.machine.clone(), path: records[0].path.clone() });
        assert_eq!(worktree_for(&records, 2, &registered), Some(&records[0]));
        // The same path on another machine is another workspace.
        assert_eq!(worktree_for(&records, 2, &AttachedWorkspace { machine: None, path: "/srv/api".into() }), None);
    }

    #[test]
    fn numbers_the_conversations_attached_workspaces_after_every_workspace_of_the_project() {
        let mut c = conversation("c");
        c.attached_workspaces = vec![AttachedWorkspace { machine: None, path: "E:\\notes".into() }];
        let numbered = conversation_workspaces(Some(&platform()), Some(&c));
        assert_eq!(numbered.iter().map(|entry| entry.path.as_str()).collect::<Vec<_>>(), vec!["C:\\platform", "/srv/api", "D:\\shared\\lib", "E:\\notes"]);
        // A temporary project still takes number 1, for the scratch directory the host gives it.
        assert_eq!(conversation_workspaces(Some(&create_temporary_workspace(Vec::new())), Some(&c)).len(), 2);
    }

    #[test]
    fn labels_a_directory_by_its_last_segment_on_any_machine() {
        assert_eq!(workspace_directory_label("C:\\platform\\"), "platform");
        assert_eq!(workspace_directory_label("~/code/api"), "api");
        assert_eq!(workspace_directory_label("/"), "/");
    }

    #[test]
    fn keys_a_workspaces_variables_by_its_machine_and_its_path() {
        assert_eq!(workspace_env_key(None, "C:\\platform"), "local|C:\\platform");
        assert_eq!(workspace_env_key(Some(&RunTarget::Wsl { distro: "Ubuntu".into() }), "/home/dev/app"), "wsl:Ubuntu|/home/dev/app");
        // The same spelling on two machines is two workspaces, with two tables.
        assert_ne!(workspace_env_key(Some(&RunTarget::Ssh { machine_id: "m1".into() }), "/srv/app"), workspace_env_key(Some(&RunTarget::Ssh { machine_id: "m2".into() }), "/srv/app"));
    }

    #[test]
    fn counts_projects_by_any_of_their_workspaces_and_conversations_by_their_attached_ones() {
        let devbox = RunTarget::Ssh { machine_id: "m1".into() };
        let mut a = project("a", "/srv/a");
        a.machine = Some(devbox.clone());
        let mut b = project("b", "C:\\b");
        b.additional_workspaces = vec![AttachedWorkspace { machine: Some(devbox.clone()), path: "/srv/b".into() }];
        let mut c = project("c", "C:\\c");
        let mut on_devbox = conversation("x");
        on_devbox.attached_workspaces = vec![AttachedWorkspace { machine: Some(devbox.clone()), path: "~/x".into() }];
        let mut on_local = conversation("y");
        on_local.attached_workspaces = vec![AttachedWorkspace { machine: None, path: "D:\\y".into() }];
        c.conversations = vec![on_devbox, on_local];
        // The temporary project has no directory of its own and never counts as on this machine.
        let workspaces = vec![a, b, c, create_temporary_workspace(Vec::new())];
        assert_eq!(machine_usage(&workspaces, Some(&devbox)), MachineUsage { projects: 2, conversations: 1 });
        assert_eq!(machine_usage(&workspaces, None), MachineUsage { projects: 2, conversations: 1 });
        assert_eq!(machine_usage(&workspaces, Some(&RunTarget::Ssh { machine_id: "gone".into() })), MachineUsage::default());
        assert_eq!(machine_usage(&[], None), MachineUsage::default());
    }

    #[test]
    fn calls_an_ssh_machine_deleted_from_the_catalog_a_deleted_machine() {
        let devbox = SshMachineConfig { id: "m1".into(), name: "devbox".into(), host: "dev@devbox".into(), ..Default::default() };
        let gone = RunTarget::Ssh { machine_id: "m-gone".into() };
        let catalog = std::slice::from_ref(&devbox);
        assert!(is_deleted_machine(Some(&gone), catalog));
        assert!(!is_deleted_machine(Some(&RunTarget::Ssh { machine_id: "m1".into() }), catalog));
        assert!(!is_deleted_machine(None, &[]));
        assert!(!is_deleted_machine(Some(&RunTarget::Wsl { distro: "Ubuntu".into() }), &[]));
        // Both languages: the test runs in whichever `set_english` left, and other tests share it.
        let label = workspace_machine_label(Some(&gone), catalog).unwrap();
        assert!(label == "Deleted machine" || label == "已删除的机器");
        assert_eq!(workspace_machine_label(Some(&RunTarget::Ssh { machine_id: "m1".into() }), catalog).as_deref(), Some("SSH: devbox"));
        assert_eq!(workspace_location_title("/srv/app", Some(&gone), catalog), format!("/srv/app ({label})"));
    }

    #[test]
    fn capability_keys_round_trip_and_trim_separators() {
        assert_eq!(capability_workspace_key(None, "/srv/app/"), "local|/srv/app");
        assert_eq!(capability_workspace_key(None, "/"), "local|/");
        assert_eq!(parse_capability_workspace_key("ssh:m1|/srv"), (Some(RunTarget::Ssh { machine_id: "m1".into() }), "/srv".to_owned()));
        assert_eq!(parse_capability_workspace_key("wsl:U|/x"), (Some(RunTarget::Wsl { distro: "U".into() }), "/x".to_owned()));
        assert_eq!(parse_capability_workspace_key("/bare"), (None, "/bare".to_owned()));
    }

    #[test]
    fn capability_workspaces_number_as_the_model_does_and_skip_duplicates() {
        let mut p = project("p", "/one");
        p.additional_workspaces = vec![AttachedWorkspace { machine: None, path: "/two".into() }];
        let attached = [AttachedWorkspace { machine: None, path: "/one/".into() }, AttachedWorkspace { machine: None, path: "/three".into() }];
        let list = capability_workspaces(Some(&p), &attached);
        assert_eq!(list.iter().map(|entry| entry.number).collect::<Vec<_>>(), vec![1, 2, 4]);
        // The temporary project's scratch folder takes a number and has no level.
        let temporary = capability_workspaces(Some(&create_temporary_workspace(Vec::new())), &attached[1..]);
        assert_eq!(temporary.iter().map(|entry| entry.number).collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn normalizes_windows_spellings_only() {
        assert_eq!(normalized_workspace_path(" C:\\Foo\\ "), "c:\\foo");
        assert_eq!(normalized_workspace_path("/Srv/App/"), "/Srv/App");
        assert!(is_absolute_workspace_path("/x") && is_absolute_workspace_path("C:\\x") && is_absolute_workspace_path("\\\\host\\x") && !is_absolute_workspace_path("rel/x"));
    }

    #[test]
    fn reorders_before_and_after_the_target() {
        let id = |s: &&str| -> String { (*s).to_owned() };
        let mut items = vec!["a", "b", "c"];
        assert!(reorder_items(&mut items, "a", "c", true, |item| item));
        assert_eq!(items.iter().map(id).collect::<Vec<_>>(), vec!["b", "c", "a"]);
        assert!(reorder_items(&mut items, "a", "b", false, |item| item));
        assert_eq!(items.iter().map(id).collect::<Vec<_>>(), vec!["a", "b", "c"]);
        assert!(!reorder_items(&mut items, "a", "a", true, |item| item));
        assert!(!reorder_items(&mut items, "a", "z", true, |item| item));
    }
}
