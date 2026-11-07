//! The conversation tile: `StreamedConversationView` with everything `App.tsx` passes it. The timeline and its handlers are the
//! chat crate's; what only the shell can say (why a rewind is held back, what opens panes, the agents) is added here.

use std::hash::{Hash, Hasher};

use mewrk_client::machine_shells::{available_shell_backends, host_platform, known_shells, tools_for_shell_backends};
use mewrk_client::model::{ContextItem, ToolDescriptor};
use mewrk_client::prelude::*;
use mewrk_client::tool_defaults::localize_tool_descriptor;
use mewrk_client::workspaces::{with_workspace_argument, worktree_for};
use mewrk_pane_git::{git_controller, git_snapshot_for_workspace, git_snapshot_key, git_surface_key};
use mewrk_pane_tasks::subagent_panel::TranscriptArgs;
use mewrk_pane_tasks::subagents::SubagentView;
use mewrk_pane_tasks::task_container::{count_running_tasks, derive_task_items, task_container_messages};
use mewrk_ui_chat::conversation::{streamed_conversation_view, transcript_view};
use mewrk_ui_chat::handlers::{handle_context_edit, open_rewind_dialog, timeline_props, TimelineEdits};
use mewrk_ui_chat::props::{ConversationViewProps, OnId};
use mewrk_ui_chat::rewind::rewind_range;

use crate::effects::{active_subagents, active_workspace_deleting, workspace_git_mutation_running};

/// `timelinePathBaseDir`: the directory a transcript's relative paths are written against.
pub fn timeline_path_base_dir(app: &mut App) -> Option<String> {
    let workspace_id = app.active_workspace_id()?;
    let conversation_id = app.active_conversation_id();
    // Read in place: the workspace holds every conversation in it, messages and all.
    let (worktree_path, local, path) = {
        let workspace = app.workspace(&workspace_id)?;
        let registered = mewrk_client::workspaces::registered_project_workspaces(Some(workspace));
        let worktrees = conversation_id.as_ref().and_then(|id| app.find_conversation(&workspace.id, id)).map_or(&[][..], |c| c.worktrees.as_slice());
        let worktree_path = registered.first().and_then(|primary| worktree_for(worktrees, 1, primary)).map(|worktree| worktree.path.clone());
        (worktree_path, workspace.machine.is_none(), workspace.path.clone())
    };
    if worktree_path.is_some() {
        return worktree_path;
    }
    if local
        && let Some(id) = &conversation_id
    {
        let controller = git_controller(app);
        let surface = git_surface_key(&workspace_id, 1);
        if let Some(root) = git_snapshot_for_workspace(controller.snapshots.get(&git_snapshot_key(id, 1)), Some(surface.as_str())).flatten().map(|snapshot| snapshot.worktree_root.clone()).filter(|root| !root.is_empty()) {
            return Some(root);
        }
    }
    Some(path)
}

/// `activeTimelineTools` and `activeEnabledTools`. Kept as long as what they are read from stays: a view that gets new tools
/// starts over on what it derived from them, and the catalog is localized from scratch to build them.
#[derive(Default)]
struct ToolsMemo {
    key: u64,
    tools: Rc<Vec<ToolDescriptor>>,
    enabled: Rc<Vec<String>>,
}

fn timeline_tools(app: &mut App, conversation_id: &str) -> (Rc<Vec<ToolDescriptor>>, Rc<Vec<String>>) {
    let workspaces = app.active_conversation_workspaces();
    let platform = host_platform();
    let probes = app.machine_shells().clone();
    let shells_at = |machine: Option<&mewrk_client::model::RunTarget>| -> Vec<String> { known_shells(machine, &probes, platform).backends.iter().map(|backend| backend.as_str().to_owned()).collect() };
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    conversation_id.hash(&mut hasher);
    (app.doc.global_settings.resolved_app_language as u8).hash(&mut hasher);
    app.doc.tools.len().hash(&mut hasher);
    for workspace in &workspaces {
        workspace.path.hash(&mut hasher);
        shells_at(workspace.machine.as_ref()).hash(&mut hasher);
    }
    shells_at(None).hash(&mut hasher);
    if let Some(conversation) = app.conversation(conversation_id) {
        conversation.settings.enabled_tools.hash(&mut hasher);
    }
    let key = hasher.finish();
    let memo = app.local::<ToolsMemo>();
    if memo.key == key {
        return (memo.tools.clone(), memo.enabled.clone());
    }
    // `activeConversationTools`: the localized catalog, narrowed to the shells the conversation's machines have.
    let language = app.doc.global_settings.resolved_app_language;
    let catalog: Vec<ToolDescriptor> = app.doc.tools.iter().map(|tool| localize_tool_descriptor(tool, language)).collect();
    let catalog = tools_for_shell_backends(&catalog, |tool| tool.name.as_str(), &available_shell_backends(&workspaces, &probes, platform));
    let enabled: Vec<String> = app.conversation(conversation_id).map(|conversation| conversation.settings.enabled_tools.iter().filter(|name| catalog.iter().any(|tool| &tool.name == *name)).cloned().collect()).unwrap_or_default();
    let tools = Rc::new(with_workspace_argument(&catalog, &workspaces, shells_at, t("工作区编号", "Workspace number")));
    let enabled = Rc::new(enabled);
    let memo = app.local::<ToolsMemo>();
    *memo = ToolsMemo { key, tools: tools.clone(), enabled: enabled.clone() };
    (tools, enabled)
}

/// The roster as one `Rc`, for as long as it is the same roster: the view keys what it derives from it on the address.
#[derive(Default)]
struct AgentsMemo {
    revision: u64,
    agents: Rc<Vec<Rc<SubagentView>>>,
}

fn agents_of(app: &mut App, conversation_id: &str) -> Rc<Vec<Rc<SubagentView>>> {
    let (_, revision, agents) = active_subagents(app, conversation_id);
    let memo = app.local::<AgentsMemo>();
    if memo.revision != revision {
        *memo = AgentsMemo { revision, agents: Rc::new(agents) };
    }
    memo.agents.clone()
}

/// `rewindDisabledReason`: a rewind changes the timeline and the files under it, so it waits for everything that writes either.
fn rewind_disabled_reason(app: &mut App, conversation: &str) -> Option<String> {
    if app.model_run_is_active(conversation) {
        return Some(t("模型回合进行中，完成或停止后才能回退", "Wait for the model turn to finish or stop before rewinding").to_owned());
    }
    if active_workspace_deleting(app) {
        return Some(t("项目正在删除，无法回退", "Cannot rewind while the project is being deleted").to_owned());
    }
    if workspace_git_mutation_running(app) {
        return Some(t("Git 写操作进行中，完成后才能回退", "Wait for the Git operation to finish before rewinding").to_owned());
    }
    None
}

/// `conversation-view__body-loading`: the conversation's body is not in memory yet.
fn body_loading(app: &mut App, conversation_id: &str) -> E {
    let t_ = theme();
    let mut view: E = row().key(("conversation-body-loading", conversation_id)).role(Role::Status).flex1().min_h(0.0).center().gap(8.0);
    match app.body_load_failure(conversation_id) {
        Some(error) => {
            let id = conversation_id.to_owned();
            view = view.child(text(tf("对话内容载入失败：{error}", "The conversation could not be loaded: {error}", &[("error", &error)])).ty(CAPTION).color(t_.critical)).child(crate::widgets::dialog_button(crate::widgets::Look::Standard, t("重试", "Retry")).key(("conversation-body-retry", conversation_id)).on_click(move |app: &mut App| app.load_active_body(&id)));
        }
        None => view = view.child(text(t("正在载入对话…", "Loading the conversation…")).ty(CAPTION).color(t_.text_secondary)),
    }
    view
}

/// `onOpenWorkflowRun`: the tasks pane on show, then the run's panel in view, once the pane has drawn it.
fn focus_workflow_run(app: &mut App, run_id: &str) {
    crate::actions::show_tasks_tab(app, mewrk_client::side_panes::TasksPaneTab::Tasks);
    if !mewrk_pane_tasks::tasks_pane::focus_workflow_run_panel(run_id) {
        let run_id = run_id.to_owned();
        crate::pane_tiles::fluent_defer(move |_: &mut App| {
            mewrk_pane_tasks::tasks_pane::focus_workflow_run_panel(&run_id);
        });
    }
}

/// `StreamedConversationView`, as `App.tsx` renders it.
pub fn chat_view(app: &mut App, conversation_id: &str, running_task_count: usize) -> E {
    let (tools, enabled) = timeline_tools(app, conversation_id);
    let agents = agents_of(app, conversation_id);
    let mut props: ConversationViewProps = timeline_props(app, conversation_id, tools, enabled);
    props.agents = agents;
    props.path_base_dir = timeline_path_base_dir(app);
    props.running_task_count = running_task_count;
    props.on_open_tasks = Some(Rc::new(|app: &mut App| crate::actions::show_tasks_tab(app, mewrk_client::side_panes::TasksPaneTab::Tasks)));
    let open_subagent: OnId = Rc::new(|app: &mut App, id: &str| crate::actions::open_subagent_panel(app, id));
    props.on_open_subagent = Some(open_subagent);
    props.on_open_workflow_run = Some(Rc::new(focus_workflow_run));

    // What only the shell can tell: Git writes and a project going away.
    let deleting = active_workspace_deleting(app);
    let rewind_reason = rewind_disabled_reason(app, conversation_id);
    if props.on_rewind_to.is_some() {
        let reason = rewind_reason.clone();
        props.on_rewind_to = Some(Rc::new(move |app: &mut App, item: &ContextItem| open_rewind_dialog(app, item, reason.as_deref())));
    }
    props.rewind_disabled_reason = rewind_reason;
    if deleting {
        props.branch_from_disabled_reason = Some(t("项目正在删除，无法创建分支", "Cannot create a branch while the project is being deleted").to_owned());
        props.fork_disabled_reason = Some(t("项目正在删除，无法分叉会话", "Cannot fork the conversation while the project is being deleted").to_owned());
    }
    // `branchSwitchDisabledReason`: the first of these that holds.
    props.branch_switch_disabled_reason = if workspace_git_mutation_running(app) {
        Some(t("Git 写操作进行中，完成后才能切换对话分支", "Wait for the Git operation to finish before switching conversation branches.").to_owned())
    } else if app.model_run_is_active(conversation_id) {
        Some(t("模型回合进行中，完成或停止后才能切换分支", "Wait for the model turn to finish or stop before switching branches").to_owned())
    } else if deleting {
        Some(t("项目正在删除，无法切换分支", "Cannot switch branches while the project is being deleted").to_owned())
    } else {
        None
    };

    // The slots.
    if app.active_body_unloaded() {
        let id = conversation_id.to_owned();
        props.timeline_placeholder = Some(Rc::new(move |app: &mut App| body_loading(app, &id)));
    }
    if app.composer_is_unsent_task() {
        props.empty_state = Some(Rc::new(mewrk_ui_composer::draft_landing));
    }
    let id = conversation_id.to_owned();
    props.composer = Some(Rc::new(move |app: &mut App| crate::composer::composer_view(app, &id)));
    streamed_conversation_view(app, props)
}

/// `countRunningTasks(deriveTaskItems(taskSources, taskMessages))`.
pub fn running_tasks(sources: &mewrk_pane_tasks::task_container::TaskSources) -> usize {
    count_running_tasks(&derive_task_items(sources, &task_container_messages()))
}

/// `SubagentPanel`'s `transcript`: the same surface, read-only.
pub fn subagent_transcript(app: &mut App, args: TranscriptArgs<'_>) -> E {
    let conversation = app.active_conversation_id().unwrap_or_default();
    let (tools, _) = timeline_tools(app, &conversation);
    let open_subagent: OnId = Rc::new(|app: &mut App, id: &str| crate::actions::open_subagent_panel(app, id));
    transcript_view(app, args, tools, Some(open_subagent))
}

/// `handleContextEdit`, for the shortcut: the timeline's editor on this message.
pub fn edit_context(app: &mut App, context_id: &str) {
    let Some(conversation) = app.active_conversation_id() else { return };
    let Some(item) = app.conversation(&conversation).and_then(|conversation| conversation.contexts.iter().find(|item| item.id() == context_id).cloned()) else { return };
    handle_context_edit(app, &item);
}

/// `rewindTargetGone`: without a range the message, or the conversation it was asked about, is no longer on screen.
pub fn follow_rewind_target(app: &mut App) {
    let Some((conversation_id, context_id)) = app.local::<TimelineEdits>().rewind.as_ref().map(|state| (state.conversation_id.clone(), state.context_id.clone())) else { return };
    let active = app.active_conversation_id();
    let in_view = active.as_deref() == Some(conversation_id.as_str()) && app.conversation(&conversation_id).is_some_and(|conversation| rewind_range(conversation, &context_id).is_some());
    if !in_view {
        app.local::<TimelineEdits>().rewind = None;
    }
}
