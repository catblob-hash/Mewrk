//! What `App.tsx` did in `useEffect`: the wiring between surfaces, run once at startup, and what has to follow the
//! document, the selection and the runs each frame.

use std::collections::HashMap;
use std::rc::Rc as StdRc;
use std::sync::mpsc;

use mewrk_client::doc::SurfaceCloser;
use mewrk_client::model::{AttachedWorkspace, ConversationWorktree, Workspace, WorkspaceKind};
use mewrk_client::prelude::*;
use mewrk_client::runs::ToolPromptNavigation;
use mewrk_client::side_panes::{expanded_pane, is_primary_preview_session, open_preview_session, pane_is_open, pane_kind, pane_target, preview_pane_id, preview_sessions_for, preview_workspace_of, shown_subagent, side_pane_layout_for, subagent_history_pane_id, SidePaneKind, SidePanesAction};
use mewrk_client::workspaces::{registered_project_workspaces, run_env_key, worktree_for};
use mewrk_pane_browser::browser::BrowserStatus;
use mewrk_pane_browser::controller::BrowserController;
use mewrk_pane_browser::preview::{note_viewer_network_cleared, PreviewServerSnapshot, PreviewServerStatus};
use mewrk_pane_browser::{hide_built_in_browser, page_is_served_by_server, preview_page_is_served_by, preview_viewer_server_of_page, request_browser_session_close};
use mewrk_pane_git::{git_checkouts_are_distinct, git_controller, git_conversation_target, git_peer_blocks_mutation, git_refresh, git_snapshot_for_workspace, git_snapshot_key, git_surface_key, git_surface_polling, git_workspace_target, GitCheckoutRef, GitPollRefresh, GitPollSurface, GitTarget, GitWorkspaceSnapshot};
use mewrk_pane_tasks::context::Ctx;
use mewrk_pane_tasks::subagent_panel::{ensure_external_step_body, rendered_contexts};
use mewrk_pane_tasks::subagents::{derive_subagent_views, find_openable_subagent_view, find_subagent_view, subagent_view_messages, SubagentView};
use mewrk_pane_terminal::{conversation_terminal_ids, destroy_terminal_session, forget_read_only_terminal, remove_terminal_conversation};

use crate::state::{shell, with_ssh_prompt, without_ssh_prompt, SshPrompt};

// ---------------------------------------------------------------- startup

/// `root::init`: every surface's init, the document's hooks, and the push events no other crate handles.
pub fn init(app: &mut App) {
    if std::mem::replace(&mut shell(app).initialized, true) {
        return;
    }
    mewrk_client::doc::init(app);
    mewrk_pane_tasks::shell_tasks::install(app);
    mewrk_pane_tasks::fork_tray::install(app);
    mewrk_pane_git::set_file_icon(mewrk_pane_files::icons::file_icon);
    mewrk_pane_files::path_open::set_path_open_handler(Rc::new(crate::actions::handle_path_open));
    mewrk_pane_files::path_open::set_path_prefetch_handler(Some(Rc::new(crate::dialogs::path_choice::prefetch_path)));
    mewrk_settings_conv::ops::live(app).on_remote_reveal = Some(Rc::new(|app: &mut App, machine, path| crate::actions::show_in_files(app, Some(machine), path, None, false, true)));
    app.local::<mewrk_pane_tasks::shell_tasks::ShellTaskStore>().on_evicted = Some(Rc::new(on_shell_task_evicted));
    register_doc_hooks(app);
    mewrk_pane_browser::attach_window(app);

    app.on_push_event(|app, event| match event["type"].as_str() {
        Some("openSettings") => crate::actions::open_global_settings(app, "providers"),
        Some("sshPromptRequested") => {
            if let Ok(prompt) = serde_json::from_value::<SshPrompt>(event.clone()) {
                with_ssh_prompt(&mut shell(app).ssh_prompts, prompt);
                repaint();
            }
        }
        Some("sshPromptSettled") => {
            if let Some(id) = event["id"].as_str() {
                without_ssh_prompt(&mut shell(app).ssh_prompts, id);
                repaint();
            }
        }
        Some("previewServersChanged") => refresh_preview_servers(app),
        _ => {}
    });
    app.invoke::<Vec<SshPrompt>>("list_ssh_prompts", json!({}), |app, result| {
        if let Ok(listed) = result {
            for prompt in listed {
                with_ssh_prompt(&mut shell(app).ssh_prompts, prompt);
            }
            repaint();
        }
    });
    mewrk_pane_tasks::fork_tray::list_pending_fork_requests(app);
}

/// `shellTaskEvicted`: the read-only page closes like any other tab, and the pane goes with its last tab.
fn on_shell_task_evicted(app: &mut App, conversation: &str, task: &str) {
    if forget_read_only_terminal(app, conversation, task) {
        app.dispatch_side_panes(SidePanesAction::Close { conversation_id: conversation.to_owned(), pane: "terminal".to_owned() });
    }
}

/// Waits on the UI thread's answer from a task thread, which is how a closer sits on another surface's lifecycle.
fn closer(start: impl FnOnce(&mut App, Box<dyn FnOnce(&mut App, Result<(), String>) + Send>) + Send + 'static) -> SurfaceCloser {
    Box::new(move |task: &Task| {
        let (sender, receiver) = mpsc::channel::<Result<(), String>>();
        task.post(move |app| {
            start(
                app,
                Box::new(move |_, result| {
                    let _ = sender.send(result);
                }),
            );
        });
        receiver.recv().unwrap_or_else(|_| Err("the app went away".to_owned()))
    })
}

fn terminal_closers(app: &mut App, owner: &str) -> Vec<SurfaceCloser> {
    conversation_terminal_ids(app, owner)
        .into_iter()
        .map(|terminal| {
            let owner = owner.to_owned();
            closer(move |app, done| {
                destroy_terminal_session(app, &owner, &terminal, move |app, ok| done(app, if ok { Ok(()) } else { Err("the terminal did not close".to_owned()) }));
            })
        })
        .collect()
}

fn browser_closers(app: &mut App, conversation: &str) -> Vec<SurfaceCloser> {
    let sessions: Vec<String> = preview_sessions_for(app.side_panes(), Some(conversation)).to_vec();
    sessions
        .into_iter()
        .map(|session| {
            closer(move |app, done| {
                request_browser_session_close(app, &session, move |app, result| done(app, result));
            })
        })
        .collect()
}

/// `conversationHasLiveActivity`'s other half, and the rest of what the document's logic asks the panes about.
fn register_doc_hooks(app: &mut App) {
    let hooks = app.doc_hooks();
    hooks.git_mutation_active.push(Rc::new(|app, conversation| git_controller(app).mutation_is_active(conversation)));
    hooks.live_activity.push(Rc::new(|app, conversation| {
        let tasks = app.local::<mewrk_pane_tasks::shell_tasks::ShellTaskStore>();
        if tasks.tasks.iter().any(|task| task.conversation_id == conversation && task.outcome.is_none()) {
            return true;
        }
        mewrk_pane_terminal::store(app).controller.current().values().any(|session| session.conversation_id == conversation && session.phase == mewrk_pane_terminal::TerminalPhase::Running && session.busy)
    }));
    hooks.surface_closers.push(Rc::new(|app, conversation| {
        let owner = app.host_conversation_id(conversation);
        let mut closers = terminal_closers(app, &owner);
        closers.extend(browser_closers(app, conversation));
        closers
    }));
    hooks.terminal_closers.push(Rc::new(|app, conversation| {
        let owner = app.host_conversation_id(conversation);
        terminal_closers(app, &owner)
    }));
    hooks.draft_server_closers.push(Rc::new(|_app, owner, workspace| {
        let (owner, workspace) = (owner.to_owned(), workspace.to_owned());
        let stop: SurfaceCloser = Box::new(move |task: &Task| {
            let listed: Vec<PreviewServerSnapshot> = task.invoke("preview_list_servers", json!({ "target": { "conversationId": owner, "draftWorkspaceId": workspace } })).unwrap_or_default();
            for server in listed.iter().filter(|server| server.session_id.as_deref() == Some(owner.as_str())) {
                let _ = task.invoke::<Value>("preview_stop_server", json!({ "handle": server.handle }));
            }
            Ok(())
        });
        vec![stop]
    }));
    hooks.conversation_forgotten.push(Rc::new(|app, conversation| {
        remove_terminal_conversation(app, conversation);
        git_controller(app).update_snapshots(|snapshots| {
            let before = snapshots.len();
            snapshots.retain(|key, _| mewrk_pane_git::git_snapshot_key_conversation(key) != conversation);
            before != snapshots.len()
        });
    }));
    hooks.draft_adopted.push(Rc::new(|app, draft, conversation, worktree_members| {
        git_controller(app).update_snapshots(|snapshots| {
            let mut changed = false;
            let keys: Vec<String> = snapshots.keys().filter(|key| mewrk_pane_git::git_snapshot_key_conversation(key) == draft).cloned().collect();
            for key in keys {
                let member = key.rsplit_once('#').and_then(|(_, m)| m.parse::<u32>().ok()).unwrap_or(1);
                let Some(entry) = snapshots.remove(&key) else { continue };
                changed = true;
                // A snapshot of a member with a worktree is the root checkout's, not what the conversation now runs on.
                if !worktree_members.contains(&member) {
                    snapshots.insert(git_snapshot_key(conversation, member), entry);
                }
            }
            changed
        });
    }));
    hooks.draft_forgotten.push(Rc::new(|app, draft, materializes_as| {
        remove_terminal_conversation(app, materializes_as);
        git_controller(app).update_snapshots(|snapshots| {
            let before = snapshots.len();
            snapshots.retain(|key, _| mewrk_pane_git::git_snapshot_key_conversation(key) != draft);
            before != snapshots.len()
        });
    }));
    hooks.timeline_notice.push(Rc::new(|app, message, duration| app.show_timeline_notice(None, message.to_owned(), None, duration)));
    hooks.git_refresh.push(Rc::new(|app, conversation, workspace, member| {
        let target = git_conversation_target(conversation, Some(member));
        let (key, surface) = (git_snapshot_key(conversation, member), git_surface_key(workspace, member));
        git_refresh(app, &key, &surface, &target, |_, _| {});
    }));
}

/// `setEditor(null)` through the hooks, since the chat crate holds the editors.
pub fn close_editors(app: &mut App) {
    let hooks = app.doc_hooks().editors_closed.clone();
    for close in hooks {
        close(app);
    }
}

// ---------------------------------------------------------------- Git

/// One workspace of the project with a Git surface: `activeGitMembers`.
#[derive(Clone, Debug)]
pub struct GitMember {
    pub member: usize,
    pub registered: AttachedWorkspace,
    pub worktree: Option<ConversationWorktree>,
    pub key: String,
    pub surface_key: String,
    pub target: GitTarget,
}

/// A review page: a member Git has said is a repository.
#[derive(Clone, Debug)]
pub struct ReviewPage {
    pub member: usize,
    pub registered: AttachedWorkspace,
    pub worktree: Option<ConversationWorktree>,
    pub key: String,
    pub surface_key: String,
    pub target: GitTarget,
    pub snapshot: GitWorkspaceSnapshot,
}

/// `gitTargetForMember`: the conversation's checkout of its project workspace `member`, wherever it is.
pub fn git_target_for_member(app: &mut App, member: u32) -> Option<GitTarget> {
    let conversation = app.active_conversation_id()?;
    if !app.draft_is_active() {
        return Some(git_conversation_target(&conversation, Some(member)));
    }
    let workspace = app.active_workspace()?;
    if workspace.kind != WorkspaceKind::Directory {
        return None;
    }
    let id = workspace.id.clone();
    Some(git_workspace_target(&id, Some(member)))
}

/// `activeGitMembers`.
pub fn git_members(app: &mut App) -> Vec<GitMember> {
    let Some(conversation) = app.active_conversation_id() else { return Vec::new() };
    let draft = app.draft_is_active();
    let Some(workspace) = app.active_workspace().cloned() else { return Vec::new() };
    let registered: Vec<AttachedWorkspace> = if workspace.kind == WorkspaceKind::Directory {
        registered_project_workspaces(Some(&workspace))
    } else if draft {
        Vec::new()
    } else {
        vec![AttachedWorkspace { machine: None, path: workspace.path.clone() }]
    };
    let worktrees: Vec<ConversationWorktree> = if draft { Vec::new() } else { app.find_conversation(&workspace.id, &conversation).map(|c| c.worktrees.clone()).unwrap_or_default() };
    registered
        .into_iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            let member = index + 1;
            let target = git_target_for_member(app, member as u32)?;
            let worktree = worktree_for(&worktrees, member, &entry).cloned();
            Some(GitMember { member, registered: entry, worktree, key: git_snapshot_key(&conversation, member as u32), surface_key: git_surface_key(&workspace.id, member as u32), target })
        })
        .collect()
}

/// `activeReviewPages`: every project workspace that is a Git repository, in the order the reader dragged them into, with
/// the project's order for the rest.
pub fn review_pages(app: &mut App) -> Vec<ReviewPage> {
    let members = git_members(app);
    let conversation = app.active_conversation_id();
    let controller = git_controller(app);
    let mut pages: Vec<ReviewPage> = members
        .into_iter()
        .filter_map(|m| {
            let snapshot = git_snapshot_for_workspace(controller.snapshots.get(&m.key), Some(m.surface_key.as_str())).flatten()?.clone();
            Some(ReviewPage { member: m.member, registered: m.registered, worktree: m.worktree, key: m.key, surface_key: m.surface_key, target: m.target, snapshot })
        })
        .collect();
    let order = conversation.map_or_else(Vec::new, |id| app.review_page_order(&id));
    let rank = |member: usize| order.iter().position(|m| *m == member).unwrap_or(order.len() + member);
    pages.sort_by_key(|page| rank(page.member));
    pages
}

/// `activeReviewMember`: the page the reader opened, or the one the composer's chip has selected.
pub fn active_review_member(app: &mut App, pages: &[ReviewPage]) -> usize {
    let chosen = app.active_conversation_id().and_then(|id| app.review_page_member(&id));
    match chosen {
        // A page Git has not answered for yet is still the one the reader chose.
        Some(member) if pages.iter().any(|page| page.member == member) || git_members(app).iter().any(|entry| entry.member == member) => member,
        _ => app.active_workspace_member(),
    }
}

/// `beginGitMutation`.
pub fn begin_git_mutation(app: &mut App, conversation: &str, member: u32) -> bool {
    if app.runs().controller.summaries().contains_key(conversation) || app.context_mutation_is_blocked(conversation) {
        return false;
    }
    let workspace: Option<Workspace> = match app.draft_of(conversation) {
        Some(draft) => app.workspace(&draft.workspace_id).cloned(),
        None => app.workspace_of_conversation(conversation).cloned(),
    };
    let Some(workspace) = workspace else { return false };
    let registered = registered_project_workspaces(Some(&workspace)).get(member.saturating_sub(1) as usize).cloned();
    let surface = git_surface_key(&workspace.id, member);
    let isolated = |worktrees: &[ConversationWorktree]| registered.as_ref().is_some_and(|entry| worktree_for(worktrees, member as usize, entry).is_some());
    let draft_id = mewrk_client::drafts::draft_conversation_id(&workspace.id);
    let mut peers: Vec<(String, Vec<ConversationWorktree>)> = workspace.conversations.iter().map(|c| (c.id.clone(), c.worktrees.clone())).collect();
    // The project's draft is a peer at the root checkout too, though no list holds it.
    if app.draft_works_in(&workspace.id) {
        peers.push((draft_id, Vec::new()));
    }
    let run_active: HashMap<String, bool> = peers.iter().map(|(id, _)| (id.clone(), app.model_run_is_active(id))).collect();
    let acting_worktrees = workspace.conversations.iter().find(|c| c.id == conversation).map(|c| c.worktrees.clone()).unwrap_or_default();
    let controller = git_controller(app);
    let acting_entry = controller.snapshots.get(&git_snapshot_key(conversation, member));
    let acting = GitCheckoutRef { snapshot: git_snapshot_for_workspace(acting_entry, Some(surface.as_str())), isolated: isolated(&acting_worktrees) };
    let blocked = peers.iter().any(|(id, worktrees)| {
        id != conversation && git_peer_blocks_mutation(GitCheckoutRef { snapshot: acting.snapshot, isolated: acting.isolated }, GitCheckoutRef { snapshot: git_snapshot_for_workspace(controller.snapshots.get(&git_snapshot_key(id, member)), Some(surface.as_str())), isolated: isolated(worktrees) }, run_active.get(id).copied().unwrap_or(false), controller.mutation_is_active(id))
    });
    let _ = git_checkouts_are_distinct;
    if blocked {
        return false;
    }
    let workspace_id = workspace.id.clone();
    let sessions: Vec<(String, bool)> = mewrk_pane_terminal::store(app).controller.current().values().map(|session| (session.conversation_id.clone(), session.busy)).collect();
    if sessions.iter().any(|(owner, busy)| *busy && app.owner_works_in(owner, &workspace_id)) {
        return false;
    }
    let mut ids: Vec<String> = peers.into_iter().map(|(id, _)| id).collect();
    if !ids.iter().any(|id| id == conversation) {
        ids.push(conversation.to_owned());
    }
    git_controller(app).acquire_mutation_lease(conversation, &ids);
    true
}

/// `gitMutationDisabledReasonFor`.
pub fn git_mutation_disabled_reason(app: &mut App, conversation: &str, member: u32) -> Option<String> {
    if app.model_run_is_active(conversation) {
        return Some(t("模型或子代理正在使用工作区，结束后才能执行 Git 写操作", "A model or subagent is using the workspace. Wait for it to finish before changing Git state.").to_owned());
    }
    if git_peer_operation_running(app, conversation, member) {
        return Some(t("同一项目中的另一项任务正在运行，暂不能执行 Git 写操作", "Another task in this project is running, so Git changes are temporarily unavailable.").to_owned());
    }
    if workspace_terminal_busy(app) {
        return Some(t("内置终端正在执行命令，结束后才能执行 Git 写操作", "The built-in terminal is running a command. Wait before changing Git state.").to_owned());
    }
    if active_workspace_deleting(app) {
        return Some(t("项目正在删除，无法执行 Git 写操作", "Git changes are unavailable while the project is being deleted.").to_owned());
    }
    None
}

/// `gitPeerOperationRunningFor`.
fn git_peer_operation_running(app: &mut App, conversation: &str, member: u32) -> bool {
    let Some(workspace) = app.active_workspace().cloned() else { return false };
    let registered = registered_project_workspaces(Some(&workspace)).get(member.saturating_sub(1) as usize).cloned();
    let surface = git_surface_key(&workspace.id, member);
    let isolated = |worktrees: &[ConversationWorktree]| registered.as_ref().is_some_and(|entry| worktree_for(worktrees, member as usize, entry).is_some());
    let acting_worktrees = workspace.conversations.iter().find(|c| c.id == conversation).map(|c| c.worktrees.clone()).unwrap_or_default();
    let mut peers: Vec<(String, Vec<ConversationWorktree>)> = workspace.conversations.iter().map(|c| (c.id.clone(), c.worktrees.clone())).collect();
    if app.draft_of_workspace(&workspace.id).is_some() {
        peers.push((mewrk_client::drafts::draft_conversation_id(&workspace.id), Vec::new()));
    }
    let summaries: Vec<String> = app.runs().controller.summaries().keys().cloned().collect();
    let controller = git_controller(app);
    let acting_entry = controller.snapshots.get(&git_snapshot_key(conversation, member));
    let acting_snapshot = git_snapshot_for_workspace(acting_entry, Some(surface.as_str()));
    peers.iter().any(|(id, worktrees)| {
        id != conversation && git_peer_blocks_mutation(GitCheckoutRef { snapshot: acting_snapshot, isolated: isolated(&acting_worktrees) }, GitCheckoutRef { snapshot: git_snapshot_for_workspace(controller.snapshots.get(&git_snapshot_key(id, member)), Some(surface.as_str())), isolated: isolated(worktrees) }, summaries.contains(id), controller.mutation_is_active(id))
    })
}

/// `activeWorkspaceTerminalBusy`.
pub fn workspace_terminal_busy(app: &mut App) -> bool {
    let Some(workspace) = app.active_workspace().cloned() else { return false };
    let mut owners: Vec<String> = workspace.conversations.iter().map(|c| c.id.clone()).collect();
    if let Some(draft) = app.draft_of_workspace(&workspace.id) {
        owners.push(draft.materializes_as);
    }
    let refs: Vec<&str> = owners.iter().map(String::as_str).collect();
    mewrk_pane_terminal::terminal_busy_in(app, &refs)
}

/// `activeWorkspaceDeletionRunning`.
pub fn active_workspace_deleting(app: &mut App) -> bool {
    app.active_workspace_id().is_some_and(|id| app.workspace_is_deleting(&id))
}

/// `activeWorkspaceGitMutationRunning`.
pub fn workspace_git_mutation_running(app: &mut App) -> bool {
    let Some(workspace) = app.active_workspace().cloned() else { return false };
    let mut ids: Vec<String> = workspace.conversations.iter().map(|c| c.id.clone()).collect();
    if app.draft_of_workspace(&workspace.id).is_some() {
        ids.push(mewrk_client::drafts::draft_conversation_id(&workspace.id));
    }
    let controller = git_controller(app);
    ids.iter().any(|id| controller.mutation_is_active(id))
}

/// `activeWorkspaceLifecycleOperationRunning`.
pub fn lifecycle_operation_running(app: &mut App) -> bool {
    active_workspace_deleting(app) || workspace_git_mutation_running(app)
}

fn poll_git(app: &mut App) {
    let members = git_members(app);
    let pages_open = app.active_conversation_id().is_some_and(|id| pane_is_open(side_pane_layout_for(app.side_panes(), Some(&id)), "review"));
    let member = app.active_workspace_member();
    let review_member = app.active_conversation_id().and_then(|id| app.review_page_member(&id)).filter(|m| members.iter().any(|entry| entry.member == *m)).unwrap_or(member);
    let surfaces: Vec<GitPollSurface> = members.iter().map(|entry| GitPollSurface { key: entry.key.clone(), surface_key: entry.surface_key.clone(), target: entry.target.clone(), foreground: entry.member == member || (pages_open && entry.member == review_member) }).collect();
    let refresh: GitPollRefresh = Rc::new(|app: &mut App, surface: &GitPollSurface, done: Box<dyn FnOnce(&mut App, bool) + Send>| {
        let key = surface.key.clone();
        git_refresh(app, &surface.key, &surface.surface_key, &surface.target, move |app, result| {
            let failures = &mut shell(app).git_surface_failures;
            match result {
                Ok(_) => {
                    failures.remove(&key);
                    done(app, true);
                }
                Err(reason) => {
                    failures.insert(key, if reason.trim().is_empty() { t("无法读取 Git 状态", "Could not read the Git status").to_owned() } else { reason });
                    repaint();
                    done(app, false);
                }
            }
        });
    });
    git_surface_polling(app, &surfaces, refresh, true);
    // A conversation none of whose workspaces is a Git repository has no review pane to show. Leaving it up would strand the
    // user on a pane whose panel has nothing to render.
    if pages_open
        && let Some(conversation) = app.active_conversation_id()
    {
        let controller = git_controller(app);
        let no_repository = members.iter().all(|entry| matches!(git_snapshot_for_workspace(controller.snapshots.get(&entry.key), Some(entry.surface_key.as_str())), Some(None)));
        if no_repository {
            app.dispatch_side_panes(SidePanesAction::Close { conversation_id: conversation, pane: "review".to_owned() });
        }
    }
}

// ---------------------------------------------------------------- previews

/// A page as the pairing rule reads it.
pub struct OpenPage {
    pub session_id: String,
    pub url: Option<String>,
    pub machine: String,
}

impl OpenPage {
    pub fn of(app: &mut App, session_id: &str) -> Self {
        let url = app.local::<BrowserController>().page_url(session_id).map(str::to_owned);
        Self { session_id: session_id.to_owned(), url, machine: page_machine_key(app, session_id) }
    }
}

/// `previewPageMachineKeyRef`: which machine a page is on: its workspace's. A page reaches that machine's `localhost`, so
/// a server and a page only belong together when their machines match.
pub fn page_machine_key(app: &mut App, session_id: &str) -> String {
    let member = preview_workspace_of(app.side_panes(), session_id);
    let workspaces = terminal_workspaces(app);
    run_env_key(workspaces.get(member.saturating_sub(1) as usize).and_then(|workspace| workspace.machine.as_ref()))
}

/// `previewServerMachineKeyRef`.
pub fn server_machine_key(app: &mut App, server: &PreviewServerSnapshot) -> String {
    match (server.workspace, &server.machine) {
        (Some(workspace), _) => {
            let workspaces = terminal_workspaces(app);
            run_env_key(workspaces.get(workspace.saturating_sub(1) as usize).and_then(|entry| entry.machine.as_ref()))
        }
        (None, Some(machine)) => format!("machine:{machine}"),
        (None, None) => run_env_key(None),
    }
}

/// `activeTerminalWorkspaces`: the workspaces a terminal can be opened in, in the conversation's numbering. The host knows
/// a draft's project workspaces and nothing it attached, so those wait until it is sent.
pub fn terminal_workspaces(app: &mut App) -> Vec<AttachedWorkspace> {
    let all = app.active_conversation_workspaces();
    if app.draft_is_active() {
        let count = app.active_project_workspaces().len().max(1);
        all.into_iter().take(count).collect()
    } else {
        all
    }
}

/// `stopServersOfClosedPage`: closing the page a server was serving stops that server, so a tab the user closed does not
/// leave a process running behind nothing. One another page of this conversation still shows, and one this conversation
/// does not own, are left alone.
pub fn stop_servers_of_closed_page(app: &mut App, conversation: &str, closed: OpenPage) {
    if closed.url.is_none() || app.active_conversation_id().as_deref() != Some(conversation) {
        return;
    }
    let owner = app.host_conversation_id(conversation);
    let servers = shell(app).preview_servers.clone();
    let sessions: Vec<String> = preview_sessions_for(app.side_panes(), Some(conversation)).to_vec();
    let others: Vec<OpenPage> = sessions.iter().filter(|session| **session != closed.session_id).map(|session| OpenPage::of(app, session)).collect();
    for server in servers {
        if server.session_id.as_deref() != Some(owner.as_str()) {
            continue;
        }
        let machine = server_machine_key(app, &server);
        let doomed = page_is_served_by_server(closed.url.as_deref(), &closed.machine, &server, &machine) && !others.iter().any(|page| page_is_served_by_server(page.url.as_deref(), &page.machine, &server, &machine));
        if doomed {
            app.invoke_logged("preview_stop_server", json!({ "handle": server.handle }));
        }
    }
}

/// The tabs showing what `gone` served. A page some server in `live` answers for stays: that is the entry restarted.
fn pages_left_by(app: &mut App, conversation: &str, gone: &[PreviewServerSnapshot], live: &[PreviewServerSnapshot]) -> Vec<String> {
    let serve = |app: &mut App, page: &OpenPage, servers: &[PreviewServerSnapshot]| servers.iter().any(|server| {
        let machine = server_machine_key(app, server);
        page_is_served_by_server(page.url.as_deref(), &page.machine, server, &machine)
    });
    let sessions: Vec<String> = preview_sessions_for(app.side_panes(), Some(conversation)).to_vec();
    sessions.into_iter().filter(|session| {
        let page = OpenPage::of(app, session);
        serve(app, &page, gone) && !serve(app, &page, live)
    }).collect()
}

/// `closePagesServedAt`: a server's tabs go with it.
pub fn close_pages_served_at(app: &mut App, conversation: &str, handle: &str) {
    let Some(server) = shell(app).preview_servers.iter().find(|server| server.handle == handle).cloned() else { return };
    if app.active_conversation_id().as_deref() != Some(conversation) {
        return;
    }
    let pages = pages_left_by(app, conversation, &[server], &[]);
    close_pages_of_gone_server(app, conversation, pages);
}

/// A run that has used the page holds it until the run ends, and a page can't be closed under the run. Its tab waits for that.
fn close_pages_of_gone_server(app: &mut App, conversation: &str, pages: Vec<String>) {
    let held = automation_holds(app, conversation);
    let (waiting, now): (Vec<String>, Vec<String>) = pages.into_iter().partition(|session| held && is_primary_preview_session(session, conversation));
    for session in waiting {
        let url = app.local::<BrowserController>().page_url(&session).map(str::to_owned);
        shell(app).doomed_pages.push((conversation.to_owned(), session, url));
    }
    crate::actions::close_preview_pages(app, &now);
}

/// Closes the tabs that were waiting for a run to let go. One the run moved somewhere else is the run's page now, and stays.
fn close_released_pages(app: &mut App, conversation: &str) {
    if shell(app).doomed_pages.is_empty() || automation_holds(app, conversation) {
        return;
    }
    let (mine, others): (Vec<_>, Vec<_>) = std::mem::take(&mut shell(app).doomed_pages).into_iter().partition(|(owner, _, _)| owner == conversation);
    shell(app).doomed_pages = others;
    let pages: Vec<String> = mine.into_iter().filter(|(_, session, url)| app.local::<BrowserController>().page_url(session) == url.as_deref()).map(|(_, session, _)| session).collect();
    crate::actions::close_preview_pages(app, &pages);
}

/// Still answering, or a remote session kept in the list to say why it gave up.
fn server_is_live(server: &PreviewServerSnapshot) -> bool {
    match server.status {
        PreviewServerStatus::Starting | PreviewServerStatus::Running => true,
        PreviewServerStatus::Failed => server.remote.is_some(),
        PreviewServerStatus::Stopped => false,
    }
}

/// `refreshPreviewServers`: re-reads the dev-server list. A server that went away takes its tabs with it, whether somebody
/// stopped it or it exited on its own.
pub fn refresh_preview_servers(app: &mut App) {
    let (Some(conversation), Some(target)) = (app.active_conversation_id(), crate::actions::preview_owner_target(app)) else { return };
    let owner = target.conversation_id.clone();
    app.invoke::<Vec<PreviewServerSnapshot>>("preview_list_servers", json!({ "target": target }), move |app, result| {
        // A workspace that has gone away answers again on the next event; the bar keeps what it has.
        let Ok(listed) = result else { return };
        if app.active_conversation_id().as_deref() != Some(conversation.as_str()) {
            return;
        }
        let mine: Vec<PreviewServerSnapshot> = listed.into_iter().filter(|server| server.session_id.as_deref().is_none_or(|id| id.is_empty() || id == owner)).collect();
        let previous = std::mem::replace(&mut shell(app).preview_servers, mine.clone());
        let live: Vec<PreviewServerSnapshot> = mine.into_iter().filter(server_is_live).collect();
        let gone: Vec<PreviewServerSnapshot> = previous.into_iter().filter(|server| server_is_live(server) && !live.iter().any(|now| now.handle == server.handle)).collect();
        if !gone.is_empty() {
            let pages = pages_left_by(app, &conversation, &gone, &live);
            close_pages_of_gone_server(app, &conversation, pages);
        }
        repaint();
    });
}

/// A session showing nothing that is not on screen is what's left of a start page the user moved on from. The conversation's
/// own page stays on the roster: it is what the model's tools fill, and it costs nothing with nothing on it.
fn reclaim_blank_pages(app: &mut App, conversation: &str) {
    let owner = app.host_conversation_id(conversation);
    let open = open_preview_session(side_pane_layout_for(app.side_panes(), Some(conversation))).map(str::to_owned);
    let sessions: Vec<String> = preview_sessions_for(app.side_panes(), Some(conversation)).to_vec();
    for session in sessions {
        let controller = app.local::<BrowserController>();
        let settled_blank = controller.statuses.contains_key(&session) && controller.page_is_blank(&session) && !controller.close_in_flight(&session) && controller.pending_opens(&session) == 0;
        if settled_blank && open.as_deref() != Some(session.as_str()) && !is_primary_preview_session(&session, &owner) {
            crate::actions::close_preview_session(app, conversation, &session);
        }
    }
}

/// `automationHolds` beside the stopping set: whether the model is driving the conversation's own page right now.
pub fn automation_holds(app: &mut App, conversation: &str) -> bool {
    app.model_run_summaries().iter().any(|(id, summary)| id == conversation && summary.browser_automation_tool.is_some())
}

/// Keeps each page filed under the workspace it is actually showing, and remembers which pages show a remote-control
/// viewer: the two effects over `activeBrowserSessions`.
fn follow_page_networks(app: &mut App, conversation: &str) {
    let sessions: Vec<String> = preview_sessions_for(app.side_panes(), Some(conversation)).to_vec();
    if sessions.is_empty() {
        return;
    }
    let servers = shell(app).preview_servers.clone();
    let workspaces = terminal_workspaces(app);
    let statuses: Vec<(String, BrowserStatus)> = sessions.iter().filter_map(|session| app.local::<BrowserController>().statuses.get(session).map(|status| (session.clone(), status.clone()))).collect();
    for (session, status) in statuses {
        if !status.has_page {
            continue;
        }
        // A remote-control viewer is served on this computer whatever machine the device is on: no sign of a workspace.
        if preview_viewer_server_of_page(Some(&status.url), &servers).is_some() {
            note_viewer_network_cleared(&session);
            continue;
        }
        if workspaces.len() < 2 {
            continue;
        }
        let machine_of = |member: u32| run_env_key(workspaces.get(member.saturating_sub(1) as usize).and_then(|entry| entry.machine.as_ref()));
        let current = preview_workspace_of(app.side_panes(), &session);
        let reported = status.network_machine.clone();
        let network = reported.clone().unwrap_or_else(|| run_env_key(None));
        let serving: Vec<&PreviewServerSnapshot> = servers.iter().filter(|server| server.workspace.is_some_and(|workspace| machine_of(workspace) == network) && preview_page_is_served_by(Some(&status.url), server)).collect();
        let mut next = current;
        if !serving.is_empty() && !serving.iter().any(|server| server.workspace == Some(current)) {
            next = serving[0].workspace.unwrap_or(current);
        } else if let Some(reported) = &reported
            && machine_of(current) != *reported
            && let Some(index) = workspaces.iter().position(|workspace| run_env_key(workspace.machine.as_ref()) == network)
        {
            next = index as u32 + 1;
        }
        if next != current {
            app.dispatch_side_panes(SidePanesAction::SetPreviewWorkspace { conversation_id: conversation.to_owned(), session_id: session, workspace: next });
        }
    }
}

/// Keeps the one native surface pointed at the pane that is asking for it. Switching conversations releases a page whose
/// pane is not on screen here, and re-presents this conversation's own preview pane.
fn keep_native_surface(app: &mut App, conversation: Option<&str>) {
    let layout = side_pane_layout_for(app.side_panes(), conversation).clone();
    let open = open_preview_session(&layout).map(str::to_owned);
    // Expanding another pane leaves the preview pane with a full-size box it no longer occupies.
    let covered = expanded_pane(&layout).is_some_and(|expanded| open.as_deref().is_none_or(|session| preview_pane_id(session) != expanded));
    let wanted = if covered { None } else { open };
    let presented = app.local::<BrowserController>().visible_session().map(str::to_owned);
    if presented.is_some() && presented != wanted {
        hide_built_in_browser(app);
    }
    if let Some(wanted) = wanted
        && presented.as_deref() != Some(wanted.as_str())
        && !app.local::<BrowserController>().close_in_flight(&wanted)
    {
        crate::actions::open_browser_tab(app, Some(&wanted), None, |_, _| {});
    }
}

// ---------------------------------------------------------------- subagents

struct SubagentMemo {
    key: (String, usize, String, u64, u64),
    contexts: StdRc<Vec<Ctx>>,
    revision: u64,
    views: Vec<Rc<SubagentView>>,
}

#[derive(Default)]
struct SubagentCache(Option<SubagentMemo>);

/// The rendered timeline and what `deriveSubagentViews` makes of it, for the conversation on screen. The derivation walks
/// whole transcripts, so it runs when the contexts change rather than on every frame.
pub fn active_subagents(app: &mut App, conversation_id: &str) -> (StdRc<Vec<Ctx>>, u64, Vec<Rc<SubagentView>>) {
    let run_revision = app.run_view(conversation_id).map(|run| run.revision);
    // With no run, what the conversation's background tasks streamed since the last one.
    let overlay_revision = if run_revision.is_none() { app.task_overlay(conversation_id).map_or(0, |overlay| overlay.revision) } else { 0 };
    let (len, stamp) = match app.conversation(conversation_id) {
        Some(conversation) => (conversation.contexts.len(), conversation.updated_at.clone()),
        None => (0, String::new()),
    };
    let key = (conversation_id.to_owned(), len, stamp, run_revision.unwrap_or(0), overlay_revision);
    if let Some(memo) = &app.local::<SubagentCache>().0
        && memo.key == key
    {
        return (memo.contexts.clone(), memo.revision, memo.views.clone());
    }
    let persisted: Vec<mewrk_client::model::ContextItem> = app.conversation(conversation_id).map(|conversation| conversation.contexts.clone()).unwrap_or_default();
    let previous_revision = app.local::<SubagentCache>().0.as_ref().map_or(0, |memo| memo.revision);
    let revision = previous_revision.wrapping_add(1);
    let (contexts, _) = rendered_contexts(app, conversation_id, &persisted, revision);
    let views = derive_subagent_views(&contexts, &subagent_view_messages());
    let contexts = StdRc::new(contexts);
    app.local::<SubagentCache>().0 = Some(SubagentMemo { key, contexts: contexts.clone(), revision, views: views.clone() });
    (contexts, revision, views)
}

/// Navigates once to the source agent or workflow step of an approval card; a source not there yet is tried again on later
/// frames. A plan-exit card is answered in the plan pane, because the answer is "is this plan right" and the plan is not in
/// the timeline.
fn follow_tool_prompts(app: &mut App, conversation: &str) {
    if app.tool_prompts(conversation).is_empty() {
        return;
    }
    let (_, _, views) = active_subagents(app, conversation);
    let next = app.next_tool_prompt_navigation(conversation, |prompt| mewrk_pane_tasks::subagents::approval_prompt_subagent_view(&views, prompt.source_agent.as_deref(), prompt.source_call_id.as_deref()).map(|view| view.id.clone()));
    match next {
        Some(ToolPromptNavigation::PlanPane) => crate::actions::open_pane(app, "plan"),
        Some(ToolPromptNavigation::Subagent(agent)) => crate::actions::open_subagent_panel(app, &agent),
        None => {}
    }
}

/// The subagent pane only keeps tabs for agents that still exist, and a tab opened while the call was streaming follows the
/// agent to its persisted record. An agent's history pane is addressed by the same id and follows it the same way.
fn follow_subagent_tabs(app: &mut App, conversation: &str) {
    let layout = side_pane_layout_for(app.side_panes(), Some(conversation)).clone();
    let has_subagent_panes = !layout.subagent_tabs.is_empty() || layout.panes.iter().any(|pane| pane_kind(pane) == SidePaneKind::History);
    if !has_subagent_panes {
        return;
    }
    let (contexts, _, views) = active_subagents(app, conversation);
    for subagent in &layout.subagent_tabs {
        match find_openable_subagent_view(&views, Some(subagent)) {
            None => app.dispatch_side_panes(SidePanesAction::CloseSubagent { conversation_id: conversation.to_owned(), subagent_id: subagent.clone() }),
            Some(matched) if matched.id != *subagent => app.dispatch_side_panes(SidePanesAction::RetargetSubagent { conversation_id: conversation.to_owned(), from: subagent.clone(), to: matched.id.clone() }),
            _ => {}
        }
    }
    for pane in &layout.panes {
        // The conversation's own history is a tab of the tasks pane; a history pane is always an agent's.
        if pane_kind(pane) != SidePaneKind::History {
            continue;
        }
        let Some(subagent) = pane_target(pane) else { continue };
        match find_openable_subagent_view(&views, Some(subagent)) {
            None => app.dispatch_side_panes(SidePanesAction::Close { conversation_id: conversation.to_owned(), pane: pane.clone() }),
            Some(matched) if matched.id != subagent => app.dispatch_side_panes(SidePanesAction::Replace { conversation_id: conversation.to_owned(), pane: pane.clone(), replacement: subagent_history_pane_id(&matched.id) }),
            _ => {}
        }
    }
    // Fetch an externalized workflow step's full record the first time the drawer opens it.
    if let Some(selected) = shown_subagent(&layout).and_then(|id| find_subagent_view(&views, Some(id))).cloned() {
        ensure_external_step_body(app, conversation, &contexts, &selected);
    }
}

// ---------------------------------------------------------------- each frame

/// What had to follow the document, the selection and the runs. Runs before the view is built.
pub fn frame(app: &mut App) {
    // What's left of `applyAppearance`: what the timeline reads for how to draw itself and whether to ask before a delete.
    let appearance = app.doc.global_settings.appearance.clone();
    if *app.appearance_snapshot() != appearance {
        app.publish_appearance(appearance);
    }
    crate::pane_tiles::run_deferred(app);
    app.runs_reconcile();
    drain_run_signals(app);

    let conversation = app.active_conversation_id();
    if shell(app).activated != conversation {
        let left = std::mem::replace(&mut shell(app).activated, conversation.clone());
        keep_timeline_scroll(app, left.as_deref(), conversation.as_deref());
        on_conversation_activated(app, conversation.as_deref());
    }
    poll_git(app);
    keep_native_surface(app, conversation.as_deref());
    if let Some(conversation) = &conversation {
        follow_tool_prompts(app, conversation);
        follow_subagent_tabs(app, conversation);
        follow_page_networks(app, conversation);
        close_released_pages(app, conversation);
        reclaim_blank_pages(app, conversation);
        // The plan pane only stays up while there is a plan. A plan not fetched yet is unknown, not absent.
        let plan_open = pane_is_open(side_pane_layout_for(app.side_panes(), Some(conversation)), "plan");
        if plan_open && matches!(app.runs().plans.get(conversation), Some(None)) {
            app.dispatch_side_panes(SidePanesAction::Close { conversation_id: conversation.clone(), pane: "plan".to_owned() });
        }
    }
    // A rewind target whose message is no longer on screen takes its window with it.
    crate::chat::follow_rewind_target(app);
}

/// `selectConversation`'s scroll: a timeline is left where it was, and comes back there. One never seen opens on its end.
fn keep_timeline_scroll(app: &mut App, left: Option<&str>, opened: Option<&str>) {
    if let Some(left) = left
        && let Some((top, _, _)) = scroll_state(hash_key(("context-scroll", left)))
    {
        app.set_context_scroll(left, top);
    }
    let Some(opened) = opened else { return };
    // At once, as setting `scrollTop` was: the shift carries it there and the request after it says where it now rests.
    let scroller = hash_key(("context-scroll", opened));
    let current = scroll_state(scroller).map_or(0.0, |state| state.0);
    match app.context_scroll(opened) {
        Some(top) => {
            scroll_request(scroller, ScrollRequest::Shift(top - current));
            scroll_request(scroller, ScrollRequest::To(top));
        }
        None => {
            scroll_request(scroller, ScrollRequest::Shift(f32::MAX / 2.0));
            scroll_request(scroller, ScrollRequest::ToEnd);
        }
    }
}

fn on_conversation_activated(app: &mut App, conversation: Option<&str>) {
    let state = shell(app);
    state.path_choice = None;
    // The list a conversation was left with is what its next read is compared to, so a server that ended while it was in
    // the background still takes its tabs when the conversation comes back.
    if let Some(left) = state.preview_servers_for.take() {
        state.preview_servers_of.insert(left, std::mem::take(&mut state.preview_servers));
    }
    state.preview_servers = conversation.and_then(|id| state.preview_servers_of.remove(id)).unwrap_or_default();
    state.preview_servers_for = conversation.map(str::to_owned);
    let Some(conversation) = conversation else { return };
    refresh_preview_servers(app);
    // Drafts do not exist at the host, so querying their tasks and plans would waste a call.
    if mewrk_client::drafts::is_draft_conversation_id(conversation) {
        return;
    }
    mewrk_pane_tasks::shell_tasks::reconcile(app, conversation);
    mewrk_pane_tasks::fork_tray::list_fork_decisions(app, conversation);
    app.load_conversation_plan(conversation);
}

fn drain_run_signals(app: &mut App) {
    // `branch_chip_error` stays where it is: the composer reads it, beside the branch chip's own.
    let signals = app.run_signals();
    let open_settings = std::mem::take(&mut signals.open_provider_settings);
    let scroll = std::mem::take(&mut signals.scroll_timeline_to_bottom);
    let register = std::mem::take(&mut signals.register_preview_sessions);
    if open_settings {
        crate::actions::open_global_settings(app, "providers");
    }
    if scroll && let Some(conversation) = app.active_conversation_id() {
        // `scrollTimelineToBottom`: after a send. Resting at the bottom is what puts the timeline back on following the output.
        scroll_request(("context-scroll", conversation.as_str()), ScrollRequest::ToEnd);
        repaint();
    }
    for conversation in register {
        // The model never gets to steal the surface: the session is registered without becoming the shown page.
        let session = app.host_conversation_id(&conversation);
        app.dispatch_side_panes(SidePanesAction::RegisterPreview { conversation_id: conversation, session_id: session, workspace: None });
    }
}

/// Who answers the pending card: exactly one surface owns it. `approvalDockOwner`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DockOwner {
    Composer,
    Plan,
    Subagent,
}

pub fn approval_dock_owner(app: &mut App, conversation: &str) -> DockOwner {
    let Some((prompt, _, _)) = app.active_tool_prompt(conversation) else { return DockOwner::Composer };
    let layout = side_pane_layout_for(app.side_panes(), Some(conversation)).clone();
    if prompt.kind == mewrk_client::runs::PromptKind::PlanExit && pane_is_open(&layout, "plan") {
        return DockOwner::Plan;
    }
    if prompt.source_agent.is_some() || prompt.source_call_id.is_some() {
        let (_, _, views) = active_subagents(app, conversation);
        if let Some(target) = mewrk_pane_tasks::subagents::approval_prompt_subagent_view(&views, prompt.source_agent.as_deref(), prompt.source_call_id.as_deref())
            && shown_subagent(&layout) == Some(target.id.as_str())
        {
            return DockOwner::Subagent;
        }
    }
    DockOwner::Composer
}
