//! The chips above the composer: where the conversation runs. Upstream's `ProjectChips.tsx` and the `composer-context` row of
//! `App.tsx`. They sit above the composer rather than inside it, since they describe the conversation and not the message.

use crate::{Active, ComposerProps};
use mewrk_client::model::{AttachedWorkspace, RunTarget, SshMachineConfig, Workspace, WorkspaceKind};
use mewrk_client::prelude::*;
use mewrk_client::workspaces::{is_temporary_workspace, run_env_key, same_machine, workspace_location_title, workspace_machine_label, TEMPORARY_WORKSPACE_ID};
use mewrk_pane_git as git;
use mewrk_ui_common::popover_menu::probe::say;
use mewrk_ui_common::path_text::{path_text_with, PathTextOptions};
use mewrk_ui_common::popover_menu::{PopoverMenu, PopoverMenuAction, PopoverMenuItem, PopoverMenuSection};

pub const PROJECT_MENU: &str = "composer-project-menu";
pub const WORKSPACE_MENU: &str = "composer-workspace-menu";
pub const ATTACH_MENU: &str = "composer-attach-menu";

/// `.composer-chip`'s and `.composer-chip--path`'s widest.
fn chip_max_width(path: bool) -> f32 {
    match (path, crate::narrow_window()) {
        (false, false) => 210.0,
        (true, false) => 320.0,
        (false, true) => 140.0,
        (true, true) => 200.0,
    }
}

/// `.composer-chip`'s, `.composer-worktree`'s and `.composer-option`'s type: 10.5 px, and 9.5 at 720 and under.
pub(crate) fn chip_type() -> TypeStyle {
    if crate::narrow_window() { TypeStyle { size: 9.5, ..FINE } } else { FINE }
}

/// `.composer-chip`: its look, told whether its menu is open.
fn chip_box(open: bool, disabled: bool, path: bool) -> E {
    let t_ = theme();
    let chip: E = row().max_w(chip_max_width(path)).min_w(0.0).h(25.0).items_center().gap(5.0).px(7.0).radius(7.0).border(t_.card_stroke).bg(if open { t_.subtle_hover } else { t_.layer });
    if disabled { chip } else { chip.hover_bg(t_.subtle_hover) }
}

fn chip_text_color(disabled: bool) -> Color {
    let t_ = theme();
    if disabled { t_.text_disabled } else { t_.text_secondary }
}

/// The chip's own title says the machine as well, so the path inside it doesn't speak over it.
fn no_title() -> PathTextOptions {
    PathTextOptions { title: Some(String::new()), ..PathTextOptions::default() }
}

/// The icon that says which machine a directory is on: this one, a WSL distribution, an SSH machine.
pub fn machine_icon(machine: Option<&RunTarget>) -> &'static str {
    match machine {
        None => "Folder",
        Some(RunTarget::Wsl { .. }) => "SquareTerminal",
        Some(RunTarget::Ssh { .. }) => "Server",
    }
}

/// Every directory of a project, one per line, each with its machine when that is not this one.
fn project_title(project: &Workspace, ssh_machines: &[SshMachineConfig]) -> String {
    let workspaces = mewrk_client::workspaces::registered_project_workspaces(Some(project));
    let lines: Vec<String> = workspaces.iter().map(|workspace| workspace_location_title(&workspace.path, workspace.machine.as_ref(), ssh_machines)).collect();
    if lines.is_empty() { project.name.clone() } else { lines.join("\n") }
}

/// A project as the chip's menu lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectRow {
    pub id: String,
    pub name: String,
    pub title: String,
    pub extra_workspaces: usize,
    pub deleting: bool,
}

/// What the project chip shows, and what its menu lists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectSelectorModel {
    pub projects: Vec<ProjectRow>,
    pub active_id: Option<String>,
    pub active_is_temporary: bool,
    pub active_name: String,
    pub title: String,
    /// The chip is held, by its conversation or its project going away.
    pub source_deleting: bool,
    pub temporary_deleting: bool,
}

/// `ProjectSelector`'s reading of the document. `active` is `None` when no project is chosen yet; sending then uses the temporary one.
pub fn project_selector_model(app: &mut App, active: Option<&str>, disabled: bool, disabled_reason: &str) -> ProjectSelectorModel {
    let deleting: Vec<(String, bool)> = app.doc.workspaces.iter().map(|workspace| workspace.id.clone()).collect::<Vec<_>>().into_iter().map(|id| (id.clone(), app.workspace_is_deleting(&id))).collect();
    let is_deleting = |id: &str| deleting.iter().any(|(known, on)| known == id && *on);
    let ssh = &app.doc.assets.execution_environments.ssh_machines;
    let active_project = active.and_then(|id| app.doc.workspaces.iter().find(|workspace| workspace.id == id));
    let projects: Vec<ProjectRow> = app.doc.workspaces.iter().filter(|project| project.kind == WorkspaceKind::Directory).map(|project| ProjectRow { id: project.id.clone(), name: project.name.clone(), title: project_title(project, ssh), extra_workspaces: project.additional_workspaces.len(), deleting: is_deleting(&project.id) }).collect();
    let active_is_temporary = active_project.is_some_and(is_temporary_workspace);
    let active_name = match active_project {
        None => t("选择项目", "Choose project").to_owned(),
        Some(_) if active_is_temporary => t("临时项目", "Temporary project").to_owned(),
        Some(project) => project.name.clone(),
    };
    let source_deleting = disabled || active_project.is_some_and(|project| is_deleting(&project.id));
    let title = if source_deleting && !disabled_reason.is_empty() {
        disabled_reason.to_owned()
    } else {
        match active_project {
            Some(project) if !active_is_temporary => project_title(project, ssh),
            _ => active_name.clone(),
        }
    };
    ProjectSelectorModel { projects, active_id: active_project.map(|project| project.id.clone()), active_is_temporary, active_name, title, source_deleting, temporary_deleting: is_deleting(TEMPORARY_WORKSPACE_ID) }
}

/// The sections of the project menu, with what choosing and creating do handed in.
pub fn project_menu_sections(model: &ProjectSelectorModel, choose: Rc<dyn Fn(&mut App, &str)>, create: Rc<dyn Fn(&mut App)>) -> Vec<PopoverMenuSection> {
    let held = model.source_deleting;
    let choice = |id: &str, deleting: bool| {
        let (id, choose) = (id.to_owned(), choose.clone());
        move |app: &mut App| {
            if !held && !deleting {
                choose(app, &id);
            }
        }
    };
    let items: Vec<PopoverMenuItem> = if model.projects.is_empty() {
        vec![PopoverMenuItem::new("empty", t("还没有项目", "No projects yet")).disabled(true)]
    } else {
        model.projects.iter().map(|project| {
            PopoverMenuItem::new(project.id.clone(), project.name.clone())
                .title(if project.deleting { t("项目正在删除", "Project is being deleted").to_owned() } else { project.title.clone() })
                .maybe_hint((project.extra_workspaces > 0).then(|| (project.extra_workspaces + 1).to_string()))
                .checked(model.active_id.as_deref() == Some(project.id.as_str()))
                .disabled(held || project.deleting)
                .on(choice(&project.id, project.deleting))
        }).collect()
    };
    vec![
        PopoverMenuSection::new("projects").label(t("项目", "Projects")).items(items),
        PopoverMenuSection::new("actions")
            .item(PopoverMenuItem::new(TEMPORARY_WORKSPACE_ID, t("临时项目", "Temporary project")).checked(model.active_is_temporary).disabled(held || model.temporary_deleting).on(choice(TEMPORARY_WORKSPACE_ID, model.temporary_deleting)))
            .item(PopoverMenuItem::new("create", t("新建项目…", "New project…")).disabled(held).on(move |app: &mut App| create(app))),
    ]
}

/// The project chip: which project a new task belongs to. Choosing one moves the task there. It is only offered before the task
/// starts: once a conversation has content its project is settled, and the caller stops drawing it.
pub fn project_selector(model: ProjectSelectorModel, choose: Rc<dyn Fn(&mut App, &str)>, create: Rc<dyn Fn(&mut App)>) -> E {
    let t_ = theme();
    let label = tf("项目：{name}", "Project: {name}", &[("name", &model.active_name)]);
    let (name, temporary, held, title) = (model.active_name.clone(), model.active_is_temporary, model.source_deleting, model.title.clone());
    say(&name);
    PopoverMenu::new(PROJECT_MENU, label, move |open| {
        chip_box(open, held, false).child(icon(if temporary { "FolderClock" } else { "FolderKanban" }, 13.0).color(chip_text_color(held))).child(text(name).ty(chip_type()).color(chip_text_color(held)).ellipsis().min_w(0.0)).child(icon("ChevronDown", 11.0).color(t_.text_tertiary))
    })
    .title(title.clone())
    .disabled(held)
    .disabled_reason(held.then_some(title))
    .menu_label(t("选择项目", "Select project"))
    .width(252.0)
    .above()
    .sections(move || project_menu_sections(&model, choose, create))
    .build()
}

/// The sections of the workspace menu: one per machine, in the order the machines first appear, so the workspaces still read in
/// project order within each group. Each heading carries a gear for that machine's settings, and each workspace a gear for its
/// own: its sandbox and variables, which belong to the workspace, not to its machine.
pub fn workspace_menu_sections(workspaces: &[AttachedWorkspace], selected: usize, ssh_machines: &[SshMachineConfig], on_select: Rc<dyn Fn(&mut App, usize)>, on_machine: Rc<dyn Fn(&mut App, Option<RunTarget>)>, on_workspace: Rc<dyn Fn(&mut App, usize)>) -> Vec<PopoverMenuSection> {
    let mut groups: Vec<(String, Option<RunTarget>, Vec<PopoverMenuItem>)> = Vec::new();
    for (index, workspace) in workspaces.iter().enumerate() {
        let member = index + 1;
        let key = run_env_key(workspace.machine.as_ref());
        let at = groups.iter().position(|group| group.0 == key).unwrap_or_else(|| {
            groups.push((key, workspace.machine.clone(), Vec::new()));
            groups.len() - 1
        });
        let (select, configure) = (on_select.clone(), on_workspace.clone());
        groups[at].2.push(
            PopoverMenuItem::new(member.to_string(), workspace.path.clone())
                .path_label()
                .title(workspace_location_title(&workspace.path, workspace.machine.as_ref(), ssh_machines))
                .checked(member == selected)
                .on(move |app: &mut App| select(app, member))
                .action(PopoverMenuAction::new(tf("{name} 的设置", "Settings for {name}", &[("name", &workspace.path)]), "Settings", move |app: &mut App| configure(app, member))),
        );
    }
    groups
        .into_iter()
        .map(|(key, machine, items)| {
            let name = workspace_machine_label(machine.as_ref(), ssh_machines).unwrap_or_else(|| t("本机", "This machine").to_owned());
            let configure = on_machine.clone();
            PopoverMenuSection::new(key).label(name.clone()).action(PopoverMenuAction::new(tf("{name} 的设置", "Settings for {name}", &[("name", &name)]), "Settings", move |app: &mut App| configure(app, machine.clone()))).items(items)
        })
        .collect()
}

/// The workspace chip. Choosing one changes nothing in the conversation: every workspace of the project stays reachable by its
/// number. It only decides which directory the Git chip beside it describes and where the terminal pane starts a shell.
pub fn workspace_member_selector(workspaces: Vec<AttachedWorkspace>, selected: usize, ssh_machines: Vec<SshMachineConfig>, on_select: Rc<dyn Fn(&mut App, usize)>, on_machine: Rc<dyn Fn(&mut App, Option<RunTarget>)>, on_workspace: Rc<dyn Fn(&mut App, usize)>) -> Option<E> {
    let t_ = theme();
    let current = workspaces.get(selected.saturating_sub(1)).or_else(|| workspaces.first())?.clone();
    say(&current.path);
    let label = tf("工作区：{name}", "Workspace: {name}", &[("name", &current.path)]);
    let title = workspace_location_title(&current.path, current.machine.as_ref(), &ssh_machines);
    let shown = current.clone();
    Some(
        PopoverMenu::new(WORKSPACE_MENU, label, move |open| {
            chip_box(open, false, true).child(icon(machine_icon(shown.machine.as_ref()), 13.0).color(t_.text_secondary)).child(path_text_with("composer-workspace-chip", shown.path.clone(), chip_type(), t_.text_secondary, no_title())).child(icon("ChevronDown", 11.0).color(t_.text_tertiary))
        })
        .title(title)
        .menu_label(t("选择工作区", "Select workspace"))
        .width(340.0)
        .above()
        .sections(move || workspace_menu_sections(&workspaces, selected, &ssh_machines, on_select, on_machine, on_workspace))
        .build(),
    )
}

/// `setConversationAttachedWorkspaces`: on the same terms as the run target, a draft keeps them in its own state until materialization.
fn update_attached(app: &mut App, change: impl FnOnce(&mut Vec<AttachedWorkspace>)) {
    let Some(active) = app.active_conversation_id() else { return };
    if let Some(draft) = app.draft_of(&active) {
        let mut attached = draft.attached_workspaces.clone();
        change(&mut attached);
        app.update_draft(&draft.workspace_id, move |current| {
            let mut next = current.clone();
            next.attached_workspaces = attached;
            Some(next)
        });
        return;
    }
    let Some(workspace_id) = app.active_workspace_id() else { return };
    app.update_conversation(&workspace_id, &active, |conversation| {
        change(&mut conversation.attached_workspaces);
        conversation.updated_at = mewrk_client::doc::now_iso();
    });
}

/// `attachWorkspace`: appends one workspace, ignoring a machine-and-path pair already attached. The shell's remote directory
/// browser calls this with what it picked.
pub fn attach_workspace(app: &mut App, machine: Option<RunTarget>, path: String) {
    update_attached(app, move |attached| {
        if !attached.iter().any(|entry| same_machine(entry.machine.as_ref(), machine.as_ref()) && entry.path == path) {
            attached.push(AttachedWorkspace { machine, path });
        }
    });
}

/// `detachWorkspace`.
pub fn detach_workspace(app: &mut App, workspace: &AttachedWorkspace) {
    update_attached(app, |attached| attached.retain(|entry| !(same_machine(entry.machine.as_ref(), workspace.machine.as_ref()) && entry.path == workspace.path)));
}

/// `attachLocalWorkspace`: the picker is what authorizes the path (the host will refuse to save a document naming a directory it
/// never returned), so there is no text-entry path here, and nothing to do when the user cancels.
fn attach_local_workspace(app: &mut App) {
    app.pick_workspace_directory(|app, picked| {
        if let Ok(Some(path)) = picked {
            attach_workspace(app, None, path);
        }
    });
}

pub fn attached_settings_key(path: &str) -> u64 {
    hash_key(("attached-settings", path))
}

pub fn attached_remove_key(path: &str) -> u64 {
    hash_key(("attached-remove", path))
}

fn chip_tool(key: u64, name: &'static str, label: String, enabled: bool, on_click: impl Fn(&mut App) + 'static) -> E {
    let t_ = theme();
    say(&label);
    let tool: E = row().key(key).center().size(15.0, 15.0).radius(4.0).shrink0().tooltip(label).child(icon(name, 11.0).color(t_.text_tertiary));
    if enabled { tool.hover_bg(t_.divider).on_click(on_click) } else { tool.disabled(true).on_click(|_: &mut App| {}) }
}

/// Attached workspaces sit with the other "where this runs" chips. Both these and the picker are frozen while a turn runs: the host
/// snapshots the granted set when it builds the request, so a grant given or taken back mid-run would show here without reaching the
/// calls that turn is still making.
fn attached_chip(workspace: &AttachedWorkspace, ssh: &[SshMachineConfig], running: bool, props: &ComposerProps) -> E {
    let t_ = theme();
    say(&workspace.path);
    let settings_label = tf("{name} 的设置", "Settings for {name}", &[("name", &workspace.path)]);
    let remove_label = tf("移除工作区：{path}", "Remove workspace: {path}", &[("path", &workspace.path)]);
    let (machine, path) = (workspace.machine.clone(), workspace.path.clone());
    let configure = props.on_configure_workspace.clone();
    let removing = workspace.clone();
    row()
        .key(("attached-chip", run_env_key(workspace.machine.as_ref()), workspace.path.as_str()))
        .max_w(chip_max_width(true))
        .min_w(0.0)
        .h(25.0)
        .items_center()
        .gap(5.0)
        .pl(7.0)
        .pr(3.0)
        .radius(7.0)
        .border(t_.card_stroke)
        .bg(t_.layer)
        .full_text(workspace_location_title(&workspace.path, workspace.machine.as_ref(), ssh))
        .child(icon(machine_icon(workspace.machine.as_ref()), 13.0).color(t_.text_secondary))
        .child(path_text_with(("composer-attached-chip", workspace.path.as_str()), workspace.path.clone(), chip_type(), t_.text_secondary, no_title()))
        .child(chip_tool(attached_settings_key(&workspace.path), "Settings", settings_label, true, move |app: &mut App| {
            if let Some(configure) = &configure {
                configure(app, machine.clone(), path.clone());
            }
        }))
        .child(chip_tool(attached_remove_key(&workspace.path), "X", remove_label, !running, move |app: &mut App| detach_workspace(app, &removing)))
}

/// A WSL distribution, as `list_wsl_distros` names it.
#[derive(Default)]
struct MachineMenu {
    /// `None` before the menu has been opened. Enumerated on each open rather than cached: a distribution can be installed or removed
    /// while the app is running, and the menu is the moment the answer matters.
    distros: Option<Vec<String>>,
}

fn attach_button(app: &mut App, ssh: Vec<SshMachineConfig>, running: bool, props: &ComposerProps) -> E {
    let t_ = theme();
    let distros = app.local::<MachineMenu>().distros.clone().unwrap_or_default();
    let browse = props.on_browse_machine.clone();
    PopoverMenu::new(ATTACH_MENU, t("附加工作区", "Attach a workspace"), move |open| {
        row().center().size(25.0, 25.0).radius(7.0).border(t_.card_stroke).bg(if open { t_.subtle_hover } else { t_.layer }).when(!running, |e| e.hover_bg(t_.subtle_hover)).child(icon("FolderPlus", 13.0).color(if running { t_.text_disabled } else { t_.text_secondary }))
    })
    .disabled(running)
    .menu_label(t("在哪台机器上选目录", "Which machine to pick a directory on"))
    .width(244.0)
    .above()
    .empty_label(t("没有可选的机器", "No machines to choose from"))
    .on_open(|app: &mut App| {
        app.list_wsl_distros(|app, listed| {
            let names = listed.unwrap_or_default().iter().filter_map(|distro| distro.get("name").and_then(Value::as_str).map(str::to_owned)).collect();
            app.local::<MachineMenu>().distros = Some(names);
            repaint();
        });
    })
    .sections(move || {
        let browse_to = |machine: RunTarget, name: String, browse: Option<Rc<dyn Fn(&mut App, RunTarget, String)>>| {
            move |app: &mut App| {
                if let Some(browse) = &browse {
                    browse(app, machine.clone(), name.clone());
                }
            }
        };
        let mut items = vec![PopoverMenuItem::new("local", t("本机", "This machine")).on(attach_local_workspace)];
        items.extend(distros.iter().map(|distro| PopoverMenuItem::new(format!("wsl:{distro}"), distro.clone()).on(browse_to(RunTarget::Wsl { distro: distro.clone() }, distro.clone(), browse.clone()))));
        items.extend(ssh.iter().map(|machine| PopoverMenuItem::new(format!("ssh:{}", machine.id), machine.name.clone()).on(browse_to(RunTarget::Ssh { machine_id: machine.id.clone() }, machine.name.clone(), browse.clone()))));
        vec![PopoverMenuSection::new("machines").items(items)]
    })
    .build()
}

/// `beginGitMutation`: whether the conversation may write to the checkout of project workspace `member` now, and the lease if so.
pub fn begin_git_mutation(app: &mut App, conversation_id: &str, member: u32, props: &ComposerProps) -> bool {
    let model_running = app.model_run_summaries().iter().any(|(id, _)| id == conversation_id);
    if model_running || app.context_mutation_is_blocked(conversation_id) {
        return false;
    }
    // Drafts are absent from workspace conversations, so resolve their project directly.
    let draft_workspace = app.draft_of(conversation_id).map(|draft| draft.workspace_id);
    let workspace = match &draft_workspace {
        Some(id) => app.doc.workspaces.iter().find(|workspace| &workspace.id == id),
        None => app.doc.workspaces.iter().find(|workspace| workspace.conversations.iter().any(|conversation| conversation.id == conversation_id)),
    };
    let Some(workspace) = workspace else { return false };
    let registered = mewrk_client::workspaces::registered_project_workspaces(Some(workspace));
    let registered = registered.get(member as usize - 1).cloned();
    let workspace_id = workspace.id.clone();
    let surface = git::git_surface_key(&workspace_id, member);
    // A peer running a model in its own checkout is not writing here, which is also how the host decides. The draft has no worktree
    // of its own. The project's draft is a peer at the root checkout too, though no list holds it.
    let mut peers: Vec<(String, bool)> = workspace
        .conversations
        .iter()
        .map(|conversation| (conversation.id.clone(), registered.as_ref().is_some_and(|registered| mewrk_client::workspaces::worktree_for(&conversation.worktrees, member as usize, registered).is_some())))
        .collect();
    if app.draft_works_in(&workspace_id) {
        peers.push((mewrk_client::drafts::draft_conversation_id(&workspace_id), false));
    }
    let acting_isolated = peers.iter().find(|(id, _)| id == conversation_id).is_some_and(|(_, isolated)| *isolated);
    let snapshot_of = |app: &mut App, id: &str| -> Option<Option<git::GitWorkspaceSnapshot>> {
        let controller = git::git_controller(app);
        git::git_snapshot_for_workspace(controller.snapshots.get(&git::git_snapshot_key(id, member)), Some(&surface)).map(|found| found.cloned())
    };
    let acting_snapshot = snapshot_of(app, conversation_id);
    for (peer, isolated) in &peers {
        if peer == conversation_id {
            continue;
        }
        let peer_snapshot = snapshot_of(app, peer);
        let peer_run_active = app.model_run_is_active(peer);
        let peer_mutation_active = git::git_controller(app).mutation_is_active(peer);
        let acting = git::GitCheckoutRef { snapshot: acting_snapshot.as_ref().map(Option::as_ref), isolated: acting_isolated };
        let other = git::GitCheckoutRef { snapshot: peer_snapshot.as_ref().map(Option::as_ref), isolated: *isolated };
        if git::git_peer_blocks_mutation(acting, other, peer_run_active, peer_mutation_active) {
            return false;
        }
    }
    if let Some(busy) = &props.workspace_terminal_busy
        && busy(app, &workspace_id)
    {
        return false;
    }
    // Every poll of this workspace must be invalidated, the draft's included.
    let mut ids: Vec<String> = peers.into_iter().map(|(id, _)| id).collect();
    if !ids.iter().any(|id| id == conversation_id) {
        ids.push(conversation_id.to_owned());
    }
    git::git_controller(app).acquire_mutation_lease(conversation_id, &ids);
    true
}

/// `refreshGitSnapshot`: nothing refreshes while a Git write holds the project, or a read that started before it could land after it.
pub fn refresh_git_snapshot(app: &mut App, snapshot_key: String, surface_key: String, target: git::GitTarget) {
    let project_id = surface_key.split('#').next().unwrap_or(&surface_key).to_owned();
    let conversations: Vec<String> = app.doc.workspaces.iter().find(|workspace| workspace.id == project_id).map(|workspace| workspace.conversations.iter().map(|conversation| conversation.id.clone()).collect()).unwrap_or_default();
    let controller = git::git_controller(app);
    if controller.mutation_is_active(git::git_snapshot_key_conversation(&snapshot_key)) || conversations.iter().any(|id| controller.mutation_is_active(id)) {
        return;
    }
    git::git_refresh(app, &snapshot_key, &surface_key, &target, |_, _| {});
}

pub(crate) fn git_target_for_member(active: &Active, member: u32) -> Option<git::GitTarget> {
    if !active.is_draft {
        return Some(git::git_conversation_target(&active.id, Some(member)));
    }
    let workspace_id = active.workspace_id.as_deref()?;
    (active.workspace_kind == Some(WorkspaceKind::Directory)).then(|| git::git_workspace_target(workspace_id, Some(member)))
}

/// What the row reads about Git for the selected workspace.
pub(crate) struct GitChipState {
    pub snapshot: Option<git::GitWorkspaceSnapshot>,
    pub target: Option<git::GitTarget>,
    pub key: String,
    pub surface: String,
}

pub(crate) fn git_chip_state(app: &mut App, active: &Active) -> Option<GitChipState> {
    let workspace_id = active.workspace_id.clone()?;
    let member = active.member as u32;
    let surface = git::git_surface_key(&workspace_id, member);
    let key = git::git_snapshot_key(&active.id, member);
    let snapshot = git::git_snapshot_for_workspace(git::git_controller(app).snapshots.get(&key), Some(&surface)).flatten().cloned();
    Some(GitChipState { snapshot, target: git_target_for_member(active, member), key, surface })
}

/// The row above the composer. The project is chosen before the task starts, and so are the branch and the worktree: once the
/// conversation has content it belongs there for good, and those chips go.
pub(crate) fn composer_context(app: &mut App, active: &Active, props: &ComposerProps) -> E {
    let t_ = theme();
    let ssh = app.doc.assets.execution_environments.ssh_machines.clone();
    let mut chips: E = row().w_full().max_w(820.0).min_h(25.0).px(2.0).items_center().wrap().gap(6.0);

    if !active.started {
        let deleting = app.conversation_is_deleting(&active.id);
        let model = project_selector_model(app, active.workspace_id.as_deref(), deleting, t("对话正在删除", "The conversation is being deleted"));
        // A new task is that project's own draft; a persisted conversation relocates.
        let is_draft = active.is_draft;
        let choose: Rc<dyn Fn(&mut App, &str)> = Rc::new(move |app, id| {
            if is_draft {
                app.open_draft_conversation(Some(id));
            } else {
                app.move_active_conversation(id, |_, _| {});
            }
        });
        let create: Rc<dyn Fn(&mut App)> = Rc::new(|app| app.set_workspace_dialog_open(true, true));
        chips = chips.child(project_selector(model, choose, create));
    }

    // Shown for a single workspace too: besides picking what Git shows, its menu is where a workspace's variables and its machine's
    // settings open.
    if !active.project_workspaces.is_empty() {
        let conversation = active.id.clone();
        let on_select: Rc<dyn Fn(&mut App, usize)> = Rc::new(move |app, member| {
            app.select_workspace_member(&conversation, member);
            crate::forget_member_memo(app);
        });
        let on_machine: Rc<dyn Fn(&mut App, Option<RunTarget>)> = {
            let configure = props.on_configure_machine.clone();
            Rc::new(move |app, machine| {
                if let Some(configure) = &configure {
                    configure(app, machine);
                }
            })
        };
        // The registered directory, not the worktree standing in for workspace 1: its settings belong to the directory the worktree
        // was checked out from.
        let registered = active.registered_workspaces.clone();
        let on_workspace: Rc<dyn Fn(&mut App, usize)> = {
            let configure = props.on_configure_workspace.clone();
            Rc::new(move |app, member| {
                if let (Some(configure), Some(workspace)) = (&configure, registered.get(member - 1)) {
                    configure(app, workspace.machine.clone(), workspace.path.clone());
                }
            })
        };
        chips = chips.child(workspace_member_selector(active.project_workspaces.clone(), active.member, ssh.clone(), on_select, on_machine, on_workspace));
    }

    let git_state = git_chip_state(app, active);
    if let Some(state) = &git_state
        && !active.started
        && let Some(snapshot) = &state.snapshot
    {
        let member = active.member as u32;
        let conversation = active.id.clone();
        let branch_label = active.worktree.as_ref().map(|worktree| worktree.branch.clone()).or_else(|| snapshot.branch.clone());
        let disabled = state.target.is_none() || active.model_running || active.lifecycle_running || git::git_controller(app).mutation_is_active(&active.id);
        let worktree_checked = if active.is_draft { active.draft_worktree_members.contains(&member) } else { active.worktree.is_some() };
        let (begin_props, begin_id) = (props.clone(), conversation.clone());
        let end_id = conversation.clone();
        let (refresh_key, refresh_surface, refresh_target) = (state.key.clone(), state.surface.clone(), state.target.clone());
        let registered = active.registered_workspaces.get(member as usize - 1).cloned();
        let (worktree_id, worktree_workspace) = (conversation.clone(), active.workspace_id.clone());
        let (flush_key, flush_surface, flush_target) = (state.key.clone(), state.surface.clone(), state.target.clone());
        let draft_workspace = active.workspace_id.clone();
        let hooks = git::BranchChipHooks {
            begin_mutation: Rc::new(move |app| begin_git_mutation(app, &begin_id, member, &begin_props)),
            end_mutation: Rc::new(move |app| git::git_controller(app).release_mutation_lease(&end_id)),
            refresh: Rc::new(move |app| {
                if let Some(target) = &refresh_target {
                    refresh_git_snapshot(app, refresh_key.clone(), refresh_surface.clone(), target.clone());
                }
            }),
            on_worktree_changed: Rc::new(move |app, worktree| {
                let (Some(workspace_id), Some(registered)) = (&worktree_workspace, &registered) else { return };
                app.update_conversation(workspace_id, &worktree_id, |conversation| {
                    conversation.worktrees = mewrk_client::workspaces::with_conversation_worktree(&conversation.worktrees, member as usize, registered, worktree);
                    conversation.updated_at = mewrk_client::doc::now_iso();
                });
                // The host resolves the checkout from the saved record, so the next read has to follow it.
                let (key, surface, target) = (flush_key.clone(), flush_surface.clone(), flush_target.clone());
                app.flush_latest_document(false, move |app, _| {
                    if let Some(target) = target {
                        refresh_git_snapshot(app, key, surface, target);
                    }
                });
            }),
            on_draft_worktree_toggled: active.is_draft.then(|| {
                Rc::new(move |app: &mut App, enabled: bool| {
                    app.local::<crate::SendWorktreeError>().0 = None;
                    let Some(workspace_id) = &draft_workspace else { return };
                    app.update_draft(workspace_id, |current| {
                        let mut next = current.clone();
                        next.worktree_members.retain(|candidate| *candidate != member);
                        if enabled {
                            next.worktree_members.push(member);
                            next.worktree_members.sort_unstable();
                        }
                        Some(next)
                    });
                }) as Rc<dyn Fn(&mut App, bool)>
            }),
        };
        chips = chips.child(row().h(25.0).items_center().radius(7.0).border(t_.card_stroke).bg(t_.layer).child(git::branch_chips(app, git::BranchChipProps { key: active.id.clone(), conversation_id: conversation, member, target: state.target.clone(), branch_label, disabled, has_worktree: active.worktree.is_some(), worktree_checked, offers_worktree: active.workspace_kind == Some(WorkspaceKind::Directory), is_draft: active.is_draft, hooks })));
    }

    for workspace in &active.attached_workspaces {
        chips = chips.child(attached_chip(workspace, &ssh, active.model_running, props));
    }
    if props.has_native_workspace_picker {
        chips = chips.child(attach_button(app, ssh, active.model_running, props));
    }
    if let Some(state) = &git_state
        && let Some(snapshot) = &state.snapshot
    {
        let open_review = props.on_open_git_review.clone();
        let member = active.member;
        // At the row's far end, where the cat keeps clear of it.
        chips = chips.child(div().ml_auto().child(git::status_card::git_status_card(("composer", active.id.as_str()), snapshot, props.git_review_open, Rc::new(move |app: &mut App| {
            if let Some(open) = &open_review {
                open(app, member);
            }
        }))));
    }

    // Floats above this row, whatever it holds: it belongs to the timeline over it, and says what the last edit there did.
    let notice = app.timeline_notice().filter(|notice| notice.conversation_id.as_deref().is_none_or(|id| id == active.id));
    let mut wrapper: E = row().key("composer-context").absolute().left(0.0).right(0.0).bottom_pct(1.0).mb(8.0).px(crate::wrap_padding()).justify_center().pass_through().child(chips);
    if let Some(notice) = notice {
        say(&notice.message);
        let mut line: E = row().key(("timeline-notice", notice.id.as_str())).role(Role::Status).absolute().bottom_pct(1.0).mb(8.0).h(26.0).px(10.0).items_center().gap(8.0).radius(7.0).border(t_.flyout_stroke).bg(t_.flyout).shadow(8.0, 3.0, t_.shadow).pass_through().child(text(notice.message.clone()).ty(CAPTION).color(t_.text_secondary).nowrap());
        if let Some((keys, action)) = &notice.hint {
            say(keys);
            say(action);
            line = line.child(div().px(5.0).py(2.0).radius(4.0).border(t_.card_stroke).bg(t_.card_secondary).child(text(keys.clone()).ty(FINE_STRONG).color(t_.text_primary))).child(text(action.clone()).ty(CAPTION).color(t_.text_secondary).nowrap());
        }
        wrapper = wrapper.child(line);
    }
    wrapper
}
