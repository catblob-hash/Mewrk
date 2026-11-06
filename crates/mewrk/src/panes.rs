//! `renderPane` of `App.tsx`: every kind of pane, wired to its crate. Every pane is a `SidePane` pseudo-window, so the title
//! bar, the close button and the focus reporting are written once here. A pane whose subject has gone renders nothing; the
//! effects close it, and returning `None` keeps the frame from drawing an empty window in the gap.

use std::collections::HashMap;

use mewrk_client::model::RunTarget;
use mewrk_client::prelude::*;
use mewrk_client::runs::PromptKind;
use mewrk_client::side_panes::{is_primary_preview_session, pane_is_open, pane_kind, pane_target, preview_sessions_for, preview_workspace_of, shown_subagent, side_pane_layout_for, subagent_history_pane_id, SidePaneKind, SidePaneLayout, SidePanesAction, TasksPaneTab};
use mewrk_client::workspaces::{host_is_windows, workspace_location_title, worktree_for, worktree_name};
use mewrk_pane_browser::controller::BrowserController;
use mewrk_pane_browser::preview::{preview_viewer_server_of_page, remote_control_detail};
use mewrk_pane_browser::{browser_pane, BrowserPaneProps};
use mewrk_pane_files::pane::{FilesPaneProps, FilesPaneWorkspace};
use mewrk_pane_files::pane_view::files_pane;
use mewrk_pane_git::{git_controller, git_review_page_label, git_review_panel, git_review_snapshot_cache_key, git_snapshot_broadcast_ids, git_snapshots_after_workspace_mutation, GitReviewCheckout, GitReviewHooks, GitReviewProps, ReviewInputs};
use mewrk_pane_tasks::fork_tray;
use mewrk_pane_tasks::history_pane::{history_pane, HistoryPaneProps};
use mewrk_pane_tasks::shell_tasks::ShellTaskStore;
use mewrk_pane_tasks::subagent_panel::{streamed_subagent_panel, subagent_tab_bar, StreamedSubagentPanelProps};
use mewrk_pane_tasks::subagents::find_openable_subagent_view;
use mewrk_pane_tasks::task_container::{BrowserSession, TaskSources};
use mewrk_pane_tasks::tasks_pane::{tasks_pane, tasks_pane_tab_bar, tasks_pane_tab_label, workflow_progress_by_run, workflow_run_ids_by_run, StoppingTasks, TasksPaneProps};
use mewrk_pane_tasks::{sources as task_sources, workflow_panel};
use mewrk_pane_terminal::{terminal_pane, terminal_workspace, NewTerminalMenu, ShellTaskOutcome, ShellTaskView, TerminalPaneProps, TerminalPhase};
use mewrk_ui_common::page_tabs::{page_tabs, PageTab, PageTabs};

use crate::actions::{self, close_pane, open_subagent_panel, show_tasks_tab, toggle_pane};
use crate::effects::{self, active_subagents, begin_git_mutation, git_mutation_disabled_reason, terminal_workspaces, workspace_git_mutation_running, ReviewPage};
use crate::menu::{MenuItem, MenuSection, PopoverMenu};
use crate::side_pane::{side_pane, SidePane};
use crate::state::shell;
use crate::widgets::{machine_icon_name, spinner, tool_button};

/// What every pane is told about the conversation.
pub struct PaneContext {
    pub conversation_id: String,
    pub host_id: String,
    pub draft: bool,
    pub draft_workspace_id: Option<String>,
    pub layout: SidePaneLayout,
    pub expanded: Option<String>,
}

impl PaneContext {
    pub fn of(app: &mut App) -> Option<Self> {
        let conversation_id = app.active_conversation_id()?;
        let host_id = app.host_conversation_id(&conversation_id);
        let draft = app.draft_is_active();
        let draft_workspace_id = if draft { Some(app.draft_of(&conversation_id).map_or_else(|| mewrk_client::workspaces::TEMPORARY_WORKSPACE_ID.to_owned(), |d| d.workspace_id)) } else { None };
        let layout = side_pane_layout_for(app.side_panes(), Some(&conversation_id)).clone();
        let expanded = layout.expanded.clone();
        Some(Self { conversation_id, host_id, draft, draft_workspace_id, layout, expanded })
    }

    fn on_focus(&self, pane: &str) -> Handler<App> {
        let (conversation_id, pane) = (self.conversation_id.clone(), pane.to_owned());
        Rc::new(move |app: &mut App| {
            app.dispatch_side_panes(SidePanesAction::Focus { conversation_id: conversation_id.clone(), pane: pane.clone() });
        })
    }

    fn on_toggle_expand(&self, pane: &str) -> Handler<App> {
        let (conversation_id, pane) = (self.conversation_id.clone(), pane.to_owned());
        Rc::new(move |app: &mut App| {
            app.dispatch_side_panes(SidePanesAction::ToggleExpand { conversation_id: conversation_id.clone(), pane: pane.clone() });
            repaint();
        })
    }

    fn on_close(&self, pane: &str) -> Handler<App> {
        let pane = pane.to_owned();
        Rc::new(move |app: &mut App| close_pane(app, &pane))
    }

    fn frame(&self, pane: &str, title: String) -> SidePane {
        SidePane {
            id: pane.to_owned(),
            title,
            header: None,
            subheader: None,
            trailing: None,
            menu_sections: Vec::new(),
            expanded: self.expanded.as_deref() == Some(pane),
            on_toggle_expand: Some(self.on_toggle_expand(pane)),
            on_close: self.on_close(pane),
            on_focus: Some(self.on_focus(pane)),
        }
    }
}

/// `newTerminalMenu`, as the terminal crate takes it.
pub fn new_terminal_menu(app: &mut App) -> NewTerminalMenu {
    let workspaces = terminal_workspaces(app);
    let workspaces = workspaces.iter().map(|workspace| terminal_workspace(app, workspace)).collect();
    NewTerminalMenu { workspaces, selected: app.active_workspace_member() as u32, disabled: effects::lifecycle_operation_running(app) }
}

/// The terminal stays mounted while its pane is closed. Unmounting it would run the panel's teardown, which reports the
/// session idle, and a still-running shell would vanish from the task rows and from every terminal-busy guard that reads
/// them. Called every frame; the elements it returns are only used while the pane is open.
fn terminal_view(app: &mut App, cx: &PaneContext) -> mewrk_pane_terminal::TerminalPaneView {
    let pane_open = pane_is_open(&cx.layout, "terminal");
    let read_only_task = active_read_only_task(app, cx);
    let stopping = read_only_task.as_ref().is_some_and(|task| app.local::<StoppingTasks>().for_conversation(&cx.conversation_id).contains(&task.shell_task_id));
    let input_disabled_reason = workspace_git_mutation_running(app).then(|| t("Git 写操作进行中，完成后才能在终端输入", "A Git write is running. Wait for it to finish before typing in the terminal."));
    let conversation_id = cx.conversation_id.clone();
    let new_terminal = new_terminal_menu(app);
    let draft_workspace = cx.draft_workspace_id.clone();
    terminal_pane(
        app,
        TerminalPaneProps {
            owner_id: &cx.host_id,
            draft_workspace_id: cx.draft_workspace_id.as_deref(),
            pane_open,
            input_disabled_reason,
            read_only_task,
            read_only_stopping: stopping,
            on_stop_task: Some(Rc::new(|app: &mut App, task: &ShellTaskView| {
                mewrk_pane_tasks::shell_tasks::stop_shell_task(app, &task.conversation_id, &task.shell_task_id, |_, result| {
                    if let Err(error) = result {
                        eprintln!("Failed to stop task: {error}");
                    }
                });
            })),
            command_gate: {
                let conversation = conversation_id.clone();
                Rc::new(move |app: &mut App| !workspace_git_mutation_running(app) && !app.conversation_deletion_is_active(&conversation))
            },
            before_open: draft_workspace.map(|workspace| -> Arc<dyn Fn(&Task) + Send + Sync> {
                Arc::new(move |task: &Task| {
                    // A project the draft was just aimed at may still be on its way to the host.
                    let workspace = workspace.clone();
                    task.update(move |app| app.await_workspace_at_host(&workspace, |_, _| {}));
                })
            }),
            on_last_tab_closed: Rc::new(|app: &mut App| {
                close_pane(app, "terminal");
                mewrk_ui_composer::focus_composer();
            }),
            new_terminal,
        },
    )
}

/// The command the read-only page shows, found among the conversation's shell tasks.
fn active_read_only_task(app: &mut App, cx: &PaneContext) -> Option<ShellTaskView> {
    let read_only = mewrk_pane_terminal::store(app).tabs.layout(&cx.host_id).read_only?;
    let task = app.local::<ShellTaskStore>().tasks.iter().find(|task| task.shell_task_id == read_only)?.clone();
    let outcome = task.outcome.map(|outcome| match outcome {
        mewrk_pane_tasks::shell_tasks::ShellTaskOutcome::Succeeded => ShellTaskOutcome::Succeeded,
        mewrk_pane_tasks::shell_tasks::ShellTaskOutcome::Failed => ShellTaskOutcome::Failed,
        mewrk_pane_tasks::shell_tasks::ShellTaskOutcome::Stopped => ShellTaskOutcome::Stopped,
    });
    Some(ShellTaskView { shell_task_id: task.shell_task_id, conversation_id: task.conversation_id, tool_name: task.tool_name, command: task.command, stopping: task.stopping, outcome, exit_code: task.exit_code.map(i64::from), workspace_root: task.workspace_root, cwd: task.cwd })
}

/// Called every frame a conversation is on screen, whether or not the terminal pane is open.
pub fn keep_terminal_mounted(app: &mut App, cx: &PaneContext) -> mewrk_pane_terminal::TerminalPaneView {
    terminal_view(app, cx)
}

/// `renderPane`.
pub fn render_pane(app: &mut App, cx: &PaneContext, pane: &str, terminal: &mut Option<mewrk_pane_terminal::TerminalPaneView>) -> Option<E> {
    match pane_kind(pane) {
        SidePaneKind::Terminal => {
            let view = terminal.take()?;
            let mut frame = cx.frame(pane, t("终端", "Terminal").to_owned());
            // The tabs are this pane's whole chrome, so they take the title bar and the pane draws no title of its own.
            frame.header = Some(view.header);
            Some(side_pane(frame, view.body))
        }
        SidePaneKind::Preview => preview_pane(app, cx, pane),
        SidePaneKind::Review => review_pane(app, cx, pane),
        SidePaneKind::Files => Some(files_pane_view(app, cx, pane)),
        SidePaneKind::Tasks => Some(tasks_pane_view(app, cx, pane)),
        SidePaneKind::History => history_agent_pane(app, cx, pane),
        SidePaneKind::Plan => plan_pane(app, cx, pane),
        SidePaneKind::Subagent => subagent_pane(app, cx, pane),
        SidePaneKind::Settings => {
            let body = mewrk_settings_conv::conversation_settings_pane(app)?;
            Some(side_pane(cx.frame(pane, t("对话设置", "Conversation settings").to_owned()), body))
        }
        SidePaneKind::Unknown => None,
    }
}

/// The workspaces a page may be opened for: `previewWorkspaceMenuItems`.
pub fn preview_workspace_menu_items(app: &mut App, disabled: bool) -> Vec<MenuItem> {
    let ssh_machines = app.doc.assets.execution_environments.ssh_machines.clone();
    terminal_workspaces(app)
        .iter()
        .enumerate()
        .map(|(index, workspace)| {
            let member = index as u32 + 1;
            let mut item = MenuItem::new(format!("workspace:{member}"), workspace.path.clone()).icon(machine_icon_name(workspace.machine.as_ref())).disabled(disabled);
            // A disabled row's title is taken for why it is disabled, and this one only says where the workspace is.
            if !disabled {
                item = item.title(workspace_location_title(&workspace.path, workspace.machine.as_ref(), &ssh_machines));
            }
            item.label_is_path = true;
            item.on(move |app: &mut App| actions::open_preview_page(app, member))
        })
        .collect()
}

fn preview_page_tabs(app: &mut App, cx: &PaneContext, active: &str) -> E {
    let sessions = actions::preview_tabs(app, &cx.conversation_id);
    let workspaces = terminal_workspaces(app);
    let multiple = workspaces.len() > 1;
    let ssh_machines = app.doc.assets.execution_environments.ssh_machines.clone();
    let servers = shell(app).preview_servers.clone();
    let mut tabs = Vec::new();
    for session in &sessions {
        let member = preview_workspace_of(app.side_panes(), session);
        let workspace = workspaces.get(member.saturating_sub(1) as usize);
        let status = app.local::<BrowserController>().statuses.get(session).cloned();
        let url = app.local::<BrowserController>().page_url(session).unwrap_or_default().to_owned();
        let address = url_host(&url);
        let directory = workspace.map_or_else(|| t("预览", "Preview").to_owned(), |workspace| workspace.path.clone());
        // A page showing a remote-control viewer is named after the device: its address is a loopback port and a token.
        let viewer = preview_viewer_server_of_page(Some(&url), &servers);
        let viewer_remote = viewer.and_then(|server| server.remote.as_ref());
        let label = match (viewer, &address) {
            (Some(server), _) => server.name.clone(),
            (None, Some(host)) => status.as_ref().and_then(|status| status.title.as_ref()).map(|title| title.trim().to_owned()).filter(|title| !title.is_empty()).unwrap_or_else(|| host.clone()),
            (None, None) => directory,
        };
        let mut tab = PageTab::new(session.clone(), label);
        tab.title = match (viewer_remote, &address, workspace) {
            (Some(remote), _, _) => remote_control_detail(None, Some(remote)),
            (None, Some(_), _) => Some(url.clone()),
            (None, None, Some(workspace)) => Some(workspace_location_title(&workspace.path, workspace.machine.as_ref(), &ssh_machines)),
            _ => None,
        };
        if address.is_none()
            && let Some(workspace) = workspace
        {
            tab.content = Some(mewrk_pane_terminal::path_text(workspace.path.clone(), "", CAPTION, None));
        }
        tab.icon = if viewer_remote.is_some() { Some("MonitorSmartphone".into()) } else { workspace.and_then(|workspace| workspace.machine.as_ref()).map(|machine| machine_icon_name(Some(machine)).into()) };
        tab.badge = multiple.then(|| member.to_string());
        tabs.push(tab);
    }
    let add_label = t("新建预览页面", "New preview page");
    let trailing: E = if multiple {
        let disabled = effects::lifecycle_operation_running(app);
        let items = preview_workspace_menu_items(app, disabled);
        let trigger_key_id = cx.conversation_id.clone();
        PopoverMenu::new(("preview-add-menu", cx.conversation_id.as_str()), add_label, move |_| tool_button(("preview-add", trigger_key_id.as_str()), "Plus", add_label, 24.0, 14.0))
            .menu_label(t("为哪个工作区打开预览", "Open a preview for which workspace"))
            .sections(move || vec![MenuSection::new("workspaces").label(t("在哪个工作区打开", "Open in which workspace")).items(items)])
            .align_end()
            .dense()
            .width(240.0)
            .build()
    } else {
        tool_button(("preview-add", cx.conversation_id.as_str()), "Plus", add_label, 24.0, 14.0).on_click(|app: &mut App| actions::open_preview_page(app, 1))
    };
    // With no page there is no tab to name the pane, and the page it would add is the start page already on show.
    if tabs.is_empty() {
        return row().flex1().min_w(0.0).items_center().gap(6.0).child(text(t("预览", "Preview")).ty(CAPTION).color(theme().text_secondary).pl(4.0)).child(multiple.then_some(trailing));
    }
    let conversation = cx.conversation_id.clone();
    page_tabs(PageTabs {
        key: format!("preview-pages:{}", cx.conversation_id),
        tabs,
        active_id: Some(active.to_owned()),
        aria_label: Arc::from(t("预览页面", "Preview pages")),
        more_label: Arc::from(t("更多页面", "More pages")),
        on_select: Rc::new(|app: &mut App, session: &str| actions::open_browser_tab(app, Some(session), None, |_, _| {})),
        on_close: Some(Rc::new(|app: &mut App, session: &str| actions::close_preview_page(app, session))),
        close_label: Some(Rc::new(|tab: &PageTab| tf("关闭页面 {name}", "Close page {name}", &[("name", &tab.label)]))),
        close_on_delete_key: false,
        on_reorder: Some(Rc::new(move |app: &mut App, session_ids| app.dispatch_side_panes(SidePanesAction::ReorderPreviews { conversation_id: conversation.clone(), session_ids }))),
        max_tab_width: 180.0,
        on_tab_context_menu: None,
        on_tab_double_click: None,
        editor: None,
        trailing: Some(trailing),
        trailing_width: 24.0,
    })
}

/// `splitBrowserAddress`'s host.
fn url_host(url: &str) -> Option<String> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    (!host.is_empty()).then(|| host.to_owned())
}

fn preview_pane(app: &mut App, cx: &PaneContext, pane: &str) -> Option<E> {
    // A session another conversation minted has no pane here; this guard covers the window where the layout and the
    // registry disagree.
    let target = pane_target(pane)?.to_owned();
    if !preview_sessions_for(app.side_panes(), Some(&cx.conversation_id)).iter().any(|session| *session == target) {
        return None;
    }
    let summary = app.model_run_summaries().into_iter().find(|(id, _)| *id == cx.conversation_id).map(|(_, summary)| summary);
    let automation_tool = summary.and_then(|summary| summary.browser_automation_tool.clone());
    let stopping = shell(app).automation_stopping.contains(&cx.conversation_id);
    let automation_holds = is_primary_preview_session(&target, &cx.host_id) && (automation_tool.is_some() || stopping);
    let page_workspace = preview_workspace_of(app.side_panes(), &target);
    let workspaces = terminal_workspaces(app);
    let page_machine = workspaces.get(page_workspace.saturating_sub(1) as usize).and_then(|workspace| workspace.machine.clone());
    let preview_target = actions::preview_target_for_page(app, &target);
    let link_notice = app.remote_link_notice(page_machine.as_ref());
    let initial = app.local::<BrowserController>().statuses.get(&target).cloned();
    let active = cx.expanded.is_none() || cx.expanded.as_deref() == Some(pane);
    let file_target = primary_git_target_value(app);
    let tabs_cx = PaneContext { conversation_id: cx.conversation_id.clone(), host_id: cx.host_id.clone(), draft: cx.draft, draft_workspace_id: cx.draft_workspace_id.clone(), layout: cx.layout.clone(), expanded: cx.expanded.clone() };
    let tabs_active = target.clone();
    let tabs: Rc<dyn Fn(&mut App) -> E> = Rc::new(move |app: &mut App| preview_page_tabs(app, &tabs_cx, &tabs_active));
    let background_sessions = {
        let sessions: Vec<String> = preview_sessions_for(app.side_panes(), Some(&cx.conversation_id)).to_vec();
        let controller = app.local::<BrowserController>();
        sessions.into_iter().filter(|session| *session != target && (is_primary_preview_session(session, &cx.host_id) || !controller.statuses.contains_key(session))).collect()
    };
    let conversation = cx.conversation_id.clone();
    let conversation_for_attach = conversation.clone();
    let opener = target.clone();
    let view = browser_pane(
        app,
        BrowserPaneProps {
            session_id: Some(target.clone()),
            background_sessions,
            active,
            target: preview_target,
            file_target,
            tabs: Some(tabs),
            link_notice,
            trusted_overlay_rect: None,
            initial_status: initial,
            on_attach_image: Some(Rc::new(move |app: &mut App, name: String, png: Vec<u8>| crate::composer::attach_image(app, &conversation_for_attach, name, png))),
            on_element_picked: Some(Rc::new(move |app: &mut App, element| crate::composer::element_picked(app, &conversation, element))),
            on_open_page: Some(Rc::new(move |app: &mut App, url, done| actions::open_browser_tab(app, Some(&opener), url, done))),
            bottom_corner_radius: 10.0,
        },
    );
    let mut frame = cx.frame(pane, t("预览", "Preview").to_owned());
    frame.header = Some(view.header);
    frame.subheader = view.subheader;
    if automation_holds {
        let label = if stopping { t("正在停止页面操作", "Stopping page automation") } else { t("停止页面操作", "Stop page automation") };
        let conversation = cx.conversation_id.clone();
        frame.trailing = Some(if stopping { row().size(24.0, 24.0).center().tooltip(label).child(spinner(12.0)) } else { tool_button(("preview-stop-automation", cx.conversation_id.as_str()), "Square", label, 24.0, 9.0).on_click(move |app: &mut App| actions::stop_automation(app, &conversation)) });
    }
    Some(side_pane(frame, view.body))
}

/// `activePrimaryGitTarget`: the conversation's own checkout when it is on this host.
fn primary_git_target_value(app: &mut App) -> Option<Value> {
    if app.active_workspace().is_some_and(|workspace| workspace.machine.is_some()) {
        return None;
    }
    effects::git_target_for_member(app, 1).and_then(|target| serde_json::to_value(target).ok())
}

fn review_pane(app: &mut App, cx: &PaneContext, pane: &str) -> Option<E> {
    app.active_workspace_id()?;
    let pages = effects::review_pages(app);
    if pages.is_empty() {
        return None;
    }
    let member = effects::active_review_member(app, &pages);
    let page = pages.iter().find(|entry| entry.member == member).unwrap_or(&pages[0]).clone();
    let conversation = cx.conversation_id.clone();
    let page_tabs_el = (pages.len() > 1).then(|| review_page_tabs(app, &pages, page.member, &conversation));
    let reveal_request = shell(app).review_request.as_ref().filter(|(id, entry_member, _)| *id == conversation && *entry_member == page.member).map(|(_, _, request)| request.clone());
    let mutation_disabled_reason = git_mutation_disabled_reason(app, &conversation, page.member as u32);
    let checkout = GitReviewCheckout { worktree_name: page.worktree.as_ref().map(worktree_name), base_branch: page.worktree.as_ref().and_then(|w| w.base_branch.clone()), base_oid: page.worktree.as_ref().map(|w| w.base_oid.clone()).filter(|oid| !oid.is_empty()) };
    let key = format!("{}:{}:{}", conversation, page.member, git_review_snapshot_cache_key(&page.snapshot));
    let hooks = {
        let member = page.member as u32;
        let (snapshot_conversation, surface_key, has_worktree, registered) = (conversation.clone(), page.surface_key.clone(), page.worktree.is_some(), page.registered.clone());
        let (start_conversation, end_conversation, mention_conversation) = (conversation.clone(), conversation.clone(), conversation.clone());
        GitReviewHooks {
            on_snapshot_change: Rc::new(move |app: &mut App, next| {
                // Broadcast Git snapshots only to conversations using the same checkout. Drafts use the workspace root; worktree
                // conversations do not.
                let mut peers: Vec<(String, bool)> = app.active_workspace().map(|workspace| workspace.conversations.iter().map(|peer| (peer.id.clone(), worktree_for(&peer.worktrees, member as usize, &registered).is_some())).collect()).unwrap_or_default();
                if let Some(workspace) = app.active_workspace_id()
                    && app.draft_of_workspace(&workspace).is_some()
                {
                    peers.push((mewrk_client::drafts::draft_conversation_id(&workspace), false));
                }
                let ids: Vec<String> = git_snapshot_broadcast_ids(&peers, &snapshot_conversation, has_worktree).into_iter().map(|id| mewrk_pane_git::git_snapshot_key(&id, member)).collect();
                let surface = surface_key.clone();
                git_controller(app).update_snapshots(move |current| {
                    git_snapshots_after_workspace_mutation(current, &ids, &surface, next.as_ref());
                    true
                });
                repaint();
            }),
            on_mutation_start: Rc::new(move |app: &mut App| begin_git_mutation(app, &start_conversation, member)),
            on_mutation_end: Rc::new(move |app: &mut App| git_controller(app).release_mutation_lease(&end_conversation)),
            on_reveal_handled: Rc::new(|app: &mut App, nonce| {
                let state = shell(app);
                if state.review_request.as_ref().is_some_and(|(_, _, request)| request.nonce == nonce) {
                    state.review_request = None;
                }
            }),
            on_mention_line: Some(Rc::new(move |app: &mut App, path: &str, line| actions::mention_file_in_composer(app, &mention_conversation, &mewrk_client::file_mention::FileMention { path: path.to_owned(), workspace: Some(member), line }))),
        }
    };
    let view = git_review_panel(app, GitReviewProps { key, inputs: ReviewInputs { target: page.target.clone(), snapshot: page.snapshot.clone(), active: true, checkout, reveal_request, mutation_disabled_reason }, hooks, page_tabs: page_tabs_el });
    let mut frame = cx.frame(pane, t("审阅", "Review").to_owned());
    frame.header = Some(view.header);
    frame.menu_sections = view.menu_sections.into_iter().map(convert_git_section).collect();
    Some(side_pane(frame, view.body))
}

fn convert_git_section(section: mewrk_pane_git::MenuSection) -> MenuSection {
    let mut converted = MenuSection::new(section.id);
    converted.label = section.label;
    converted.items = section
        .items
        .into_iter()
        .map(|item| {
            let mut row = MenuItem::new(item.id, item.label);
            row.description = item.description;
            row.hint = item.hint;
            row.icon = item.icon.map(Into::into);
            row.checked = item.checked;
            row.disabled = item.disabled;
            row.on_select = item.on_select;
            row
        })
        .collect();
    converted
}

/// A page is named after what it reviews: the workspace, the branch (the one a worktree was forked from, which is what its
/// changes are read against) and the worktree itself.
fn review_page_tabs(app: &mut App, pages: &[ReviewPage], active: usize, conversation: &str) -> E {
    let ssh_machines = app.doc.assets.execution_environments.ssh_machines.clone();
    let failures = shell(app).git_surface_failures.clone();
    let tabs: Vec<PageTab> = pages
        .iter()
        .map(|entry| {
            let branch = entry.worktree.as_ref().and_then(|w| w.base_branch.clone()).or_else(|| entry.snapshot.branch.clone()).or_else(|| entry.snapshot.head.as_ref().map(|head| head.chars().take(8).collect())).unwrap_or_else(|| t("尚无提交", "No commits yet").to_owned());
            let worktree = entry.worktree.as_ref().map(worktree_name);
            let label = [Some(entry.registered.path.clone()), Some(branch.clone()), worktree.clone()].into_iter().flatten().collect::<Vec<_>>().join(" → ");
            let failure = failures.get(&entry.key);
            let mut title = vec![workspace_location_title(&entry.registered.path, entry.registered.machine.as_ref(), &ssh_machines)];
            title.extend(entry.worktree.as_ref().map(|w| w.path.clone()));
            title.extend(failure.map(|reason| tf("最近一次读取失败，显示的是上次的状态：{reason}", "The last read failed; this is the state from before: {reason}", &[("reason", reason)])));
            let mut tab = PageTab::new(entry.member.to_string(), label);
            tab.content = Some(git_review_page_label(&entry.registered.path, &branch, worktree.as_deref()));
            tab.title = Some(title.join("\n"));
            tab.icon = if failure.is_some() { Some("CircleAlert".into()) } else { entry.registered.machine.as_ref().map(|machine| machine_icon_name(Some(machine)).into()) };
            tab.closable = false;
            tab
        })
        .collect();
    let (select_conversation, reorder_conversation) = (conversation.to_owned(), conversation.to_owned());
    page_tabs(PageTabs {
        key: format!("review-pages:{conversation}"),
        tabs,
        active_id: Some(active.to_string()),
        aria_label: Arc::from(t("审阅的工作区", "Workspaces under review")),
        more_label: Arc::from(t("更多工作区", "More workspaces")),
        on_select: Rc::new(move |app: &mut App, id: &str| {
            if let Ok(member) = id.parse() {
                app.set_review_page_member(&select_conversation, member);
            }
        }),
        on_close: None,
        close_label: None,
        close_on_delete_key: false,
        on_reorder: Some(Rc::new(move |app: &mut App, ids: Vec<String>| app.set_review_page_order(&reorder_conversation, ids.iter().filter_map(|id| id.parse().ok()).collect()))),
        max_tab_width: 220.0,
        on_tab_context_menu: None,
        on_tab_double_click: None,
        editor: None,
        trailing: None,
        trailing_width: 0.0,
    })
}

/// `filesPaneWorkspaces`: the conversation's workspaces as the file pane offers them, numbered the way the model addresses
/// them. A draft knows its project's workspaces and nothing it attached; one aimed at no project has none yet.
pub fn files_pane_workspaces(app: &mut App) -> Vec<FilesPaneWorkspace> {
    if app.draft_is_active() && effects::git_target_for_member(app, 1).is_none() {
        return Vec::new();
    }
    terminal_workspaces(app)
        .into_iter()
        .enumerate()
        // A temporary project's scratch directory is the host's to name; the renderer has no path for it.
        .filter(|(_, workspace)| !workspace.path.trim().is_empty())
        .map(|(index, workspace)| FilesPaneWorkspace { number: index as u32 + 1, machine: workspace.machine, path: workspace.path })
        .collect()
}

fn files_pane_view(app: &mut App, cx: &PaneContext, pane: &str) -> E {
    let conversation = cx.conversation_id.clone();
    let workspaces = files_pane_workspaces(app);
    let ssh_machines = app.doc.assets.execution_environments.ssh_machines.clone();
    let open_request = shell(app).files_request.as_ref().filter(|(id, _)| *id == conversation).map(|(_, request)| request.clone());
    let mention_conversation = conversation.clone();
    let view = files_pane(
        app,
        FilesPaneProps {
            pane_id: format!("{pane}:{conversation}"),
            workspaces,
            ssh_machines,
            host_windows: host_is_windows(mewrk_client::machine_shells::host_platform()),
            active: true,
            open_request,
            on_open_request_handled: Some(Rc::new(|app: &mut App, nonce| {
                let state = shell(app);
                if state.files_request.as_ref().is_some_and(|(_, request)| request.nonce == nonce) {
                    state.files_request = None;
                }
            })),
            on_mention: Some(Rc::new(move |app: &mut App, mention| actions::mention_file_in_composer(app, &mention_conversation, &mention))),
        },
    );
    let mut frame = cx.frame(pane, t("文件", "Files").to_owned());
    // The title and the tabs are this pane's own chrome, and its `⋮` rows are its own too.
    frame.header = Some(view.header);
    frame.menu_sections = view.menu_sections;
    side_pane(frame, view.body)
}

/// `taskSources`.
pub fn task_sources_of(app: &mut App, cx: &PaneContext, agents: Vec<Rc<mewrk_pane_tasks::subagents::SubagentView>>) -> TaskSources {
    let conversation = cx.conversation_id.clone();
    let terminals: Vec<task_sources::TerminalSessionState> = mewrk_pane_terminal::store(app).controller.current().values().filter(|session| session.conversation_id == cx.host_id && session.phase == TerminalPhase::Running && (session.busy || session.has_history)).cloned().collect();
    let shell_tasks = app.local::<ShellTaskStore>().for_conversation(&conversation);
    let sessions: Vec<String> = preview_sessions_for(app.side_panes(), Some(&conversation)).to_vec();
    let browser_sessions: Vec<BrowserSession> = sessions.iter().filter_map(|session| app.local::<BrowserController>().statuses.get(session).map(|status| BrowserSession { session_id: session.clone(), status: status.clone() })).collect();
    let preview_servers: Vec<task_sources::PreviewServerSnapshot> = shell(app).preview_servers.clone();
    let summary = app.model_run_summaries().into_iter().find(|(id, _)| *id == conversation).map(|(_, summary)| summary);
    let plan = app.conversation_plan(&conversation);
    let prompt = app.active_tool_prompt(&conversation);
    let user_aborted_tasks = app.conversation(&conversation).map(|c| c.user_aborted_tasks.clone()).unwrap_or_default();
    TaskSources {
        conversation_id: Some(conversation.clone()),
        agents,
        terminals,
        shell_tasks,
        browser: None,
        preview_servers,
        browser_session_id: Some(conversation.clone()),
        browser_sessions: Some(browser_sessions),
        browser_automation_tool: summary.as_ref().and_then(|summary| summary.browser_automation_tool.clone()),
        browser_automation_stopping: shell(app).automation_stopping.contains(&conversation),
        model_request_id: summary.map(|summary| summary.request_id.clone()),
        user_aborted_tasks,
        plan,
        plan_awaiting_approval: prompt.is_some_and(|(prompt, _, _)| prompt.kind == PromptKind::PlanExit),
        plan_drafting: false,
        fork_decisions: app.fork_decisions(&conversation),
        inherited_model_id: mewrk_ui_composer::active_model_choice(app).map(|choice| choice.model.id),
        tool_explanation: None,
        now: None,
    }
}

fn tasks_pane_view(app: &mut App, cx: &PaneContext, pane: &str) -> E {
    let tab = cx.layout.tasks_tab;
    let conversation = cx.conversation_id.clone();
    let mut frame = cx.frame(pane, tasks_pane_tab_label(tab).to_owned());
    // The tabs take the title bar; the pane's region goes by the page on show.
    frame.header = Some(tasks_pane_tab_bar(tab, |app: &mut App, next| show_tasks_tab(app, next)));
    let streaming = app.model_run_is_active(&conversation);
    let body = match tab {
        TasksPaneTab::History => {
            let revision = app.conversation(&conversation).map_or(0, |c| c.contexts.len() as u64);
            history_pane(app, HistoryPaneProps::new(conversation.clone(), revision, streaming))
        }
        TasksPaneTab::Tasks => {
            let (_, _, agents) = active_subagents(app, &conversation);
            let sources = task_sources_of(app, cx, agents.clone());
            let mut props = TasksPaneProps::new(sources, |app: &mut App, agent: &str| open_subagent_panel(app, agent), |app: &mut App, item| actions::stop_item(app, item));
            props.selected_agent_id = shown_subagent(&cx.layout).map(str::to_owned);
            props.selected_row_id = selected_task_row(app, cx);
            props.stopping_ids = app.local::<StoppingTasks>().for_conversation(&conversation);
            let (progress, run_ids) = workflow_tables(app, &conversation, &agents);
            props.workflow_progress = progress;
            props.workflow_run_ids = run_ids;
            let step_conversation = conversation.clone();
            props.on_workflow_step_control = Some(Rc::new(move |app: &mut App, run: &str, step, action| workflow_panel::control_workflow_step(app, &step_conversation, run, step, action)));
            props.on_open_item = Some(Rc::new(|app: &mut App, item| actions::open_task_item_page(app, item)));
            tasks_pane(app, props)
        }
    };
    side_pane(frame, body)
}

/// `workflowProgressByRun` and `workflowRunIdsByRun`: from the run, or with no run from what a workflow that outlived the last
/// one has streamed since.
fn workflow_tables(app: &mut App, conversation: &str, agents: &[Rc<mewrk_pane_tasks::subagents::SubagentView>]) -> (HashMap<String, mewrk_pane_tasks::workflow_progress::WorkflowProgressView>, HashMap<String, String>) {
    if let Some(run) = app.run_view(conversation) {
        return (workflow_progress_by_run(agents, run.workflow_progress_by_call.as_ref()), workflow_run_ids_by_run(agents, &run.workflow_run_id_by_call));
    }
    match app.task_overlay(conversation) {
        Some(overlay) => (workflow_progress_by_run(agents, overlay.workflow_progress_by_call.as_ref()), workflow_run_ids_by_run(agents, &overlay.workflow_run_id_by_call)),
        None => (HashMap::new(), HashMap::new()),
    }
}

/// `selectedTaskRowId`: the task row the focused pane is currently showing, so the task list marks it as current.
fn selected_task_row(app: &mut App, cx: &PaneContext) -> Option<String> {
    let focused = cx.layout.focused.as_deref()?;
    match pane_kind(focused) {
        SidePaneKind::Subagent => shown_subagent(&cx.layout).map(str::to_owned),
        SidePaneKind::Preview => Some(focused.to_owned()),
        // A command's row is marked while the terminal pane shows its output.
        SidePaneKind::Terminal => mewrk_pane_terminal::active_read_only_shell_task(app, &cx.host_id),
        SidePaneKind::Plan => Some("plan".to_owned()),
        // A targeted history pane is that agent's surface too.
        SidePaneKind::History => pane_target(focused).map(str::to_owned),
        _ => None,
    }
}

fn history_agent_pane(app: &mut App, cx: &PaneContext, pane: &str) -> Option<E> {
    let conversation = cx.conversation_id.clone();
    let (_, _, agents) = active_subagents(app, &conversation);
    let view = find_openable_subagent_view(&agents, pane_target(pane))?.clone();
    let streaming = app.model_run_is_active(&conversation);
    let revision = app.conversation(&conversation).map_or(0, |c| c.contexts.len() as u64);
    let mut props = HistoryPaneProps::new(conversation, revision, streaming);
    // A spawned agent's entries are keyed by its name; a workflow step's by the run-scoped address the host publishes on its
    // shell. A view that carries neither asks for nothing rather than falling back to the conversation's own entries.
    props.owners = Some(view.ledger_owner.clone().into_iter().collect());
    Some(side_pane(cx.frame(pane, tf("{label} 的历史记录", "{label} · history", &[("label", &view.label)])), history_pane(app, props)))
}

/// The approval card, for a pane that owns it (`approvalDockOwner`).
fn owned_dock(app: &mut App, conversation: &str, owner: effects::DockOwner) -> Option<E> {
    if effects::approval_dock_owner(app, conversation) != owner {
        return None;
    }
    let (prompt, index, total) = app.active_tool_prompt(conversation)?;
    // `.tool-approval-dock`'s `calc(100% - 48px)`, which only the composer's own padding stands in for. Its `margin-bottom` is 10 px
    // under an agent's transcript.
    let foot = if owner == effects::DockOwner::Subagent { 8.0 } else { 0.0 };
    mewrk_ui_composer::approval_dock::tool_approval_dock(app, conversation, Some(&prompt), Some(mewrk_ui_composer::approval_dock::DockStack { index, total })).map(|dock| div().w_full().px(24.0).pb(foot).child(dock))
}

fn plan_pane(app: &mut App, cx: &PaneContext, pane: &str) -> Option<E> {
    let t_ = theme();
    let plan = app.conversation_plan(&cx.conversation_id)?;
    let awaiting = app.active_tool_prompt(&cx.conversation_id).is_some_and(|(prompt, _, _)| prompt.kind == PromptKind::PlanExit);
    let mut frame = cx.frame(pane, mewrk_ui_chat::plan_page::plan_title(&plan.markdown).unwrap_or_else(|| t("实施计划", "Implementation plan").to_owned()));
    frame.trailing = Some(text(mewrk_ui_chat::plan_page::plan_status_label(Some(&plan), awaiting)).ty(FINE).color(t_.text_secondary).nowrap().mr(4.0));
    // The exit card is answered here, beside the plan it is about.
    let dock = owned_dock(app, &cx.conversation_id, effects::DockOwner::Plan);
    let single_dollar_math = app.doc.global_settings.appearance.single_dollar_math;
    Some(side_pane(frame, mewrk_ui_chat::plan_page::plan_pane(("plan", cx.conversation_id.as_str()), Some(&plan), single_dollar_math, dock)))
}

fn subagent_pane(app: &mut App, cx: &PaneContext, pane: &str) -> Option<E> {
    let conversation = cx.conversation_id.clone();
    let (contexts, revision, agents) = active_subagents(app, &conversation);
    let selected = shown_subagent(&cx.layout)?.to_owned();
    let view = find_openable_subagent_view(&agents, Some(&selected))?.clone();
    // A tab still filed under the call id its agent has since left resolves to the agent's view, and the effects move it
    // over; until then the two must not draw twice.
    let mut tab_agents: Vec<Rc<mewrk_pane_tasks::subagents::SubagentView>> = Vec::new();
    for id in &cx.layout.subagent_tabs {
        if let Some(agent) = find_openable_subagent_view(&agents, Some(id))
            && !tab_agents.iter().any(|existing| existing.id == agent.id)
        {
            tab_agents.push(agent.clone());
        }
    }
    let ledger_pane = subagent_history_pane_id(&view.id);
    let ledger_open = pane_is_open(&cx.layout, &ledger_pane);
    let t_ = theme();
    let close_conversation = conversation.clone();
    let reorder_conversation = conversation.clone();
    let mut frame = cx.frame(pane, view.label.clone());
    // Agents share this pane as its tabs, so the strip takes the title bar and names the shown one.
    frame.header = Some(subagent_tab_bar(&tab_agents, Some(&view.id), |app: &mut App, id: &str| open_subagent_panel(app, id), move |app: &mut App, id: &str| app.dispatch_side_panes(SidePanesAction::CloseSubagent { conversation_id: close_conversation.clone(), subagent_id: id.to_owned() }), move |app: &mut App, subagent_ids: Vec<String>| app.dispatch_side_panes(SidePanesAction::ReorderSubagents { conversation_id: reorder_conversation.clone(), subagent_ids })));
    // What happened in this agent, beside the transcript it settled into.
    let ledger_label = tf("{label} 的历史记录", "{label} · history", &[("label", &view.label)]);
    let ledger_target = ledger_pane.clone();
    frame.trailing = Some(row().key(("side-pane-ledger", pane)).center().size(24.0, 24.0).radius(6.0).hover_bg(t_.subtle_hover).tooltip(ledger_label).aria_pressed(ledger_open).child(icon("History", 13.0).color(if ledger_open { t_.accent_text } else { t_.text_secondary })).on_click(move |app: &mut App| toggle_pane(app, &ledger_target)));
    let path_base_dir = crate::chat::timeline_path_base_dir(app);
    let close_pane_id = pane.to_owned();
    let dock = owned_dock(app, &conversation, effects::DockOwner::Subagent);
    let body = streamed_subagent_panel(
        app,
        StreamedSubagentPanelProps {
            conversation_id: conversation,
            rendered_contexts: &contexts,
            revision,
            selected_subagent_id: Some(view.id.clone()),
            on_select_agent: Rc::new(|app: &mut App, id: &str| open_subagent_panel(app, id)),
            on_close: Rc::new(move |app: &mut App| close_pane(app, &close_pane_id)),
            chromeless: true,
            path_base_dir,
            // Approval cards on subagent panes, so their source-navigation target remains visible and actionable.
            dock,
            transcript: &crate::chat::subagent_transcript,
        },
    )?;
    Some(side_pane(frame, body))
}

/// Keeps a machine's icon for what the menus show. Local until the composer's chips are published.
pub fn machine_label(app: &App, machine: Option<&RunTarget>) -> String {
    app.machine_label(machine).unwrap_or_else(|| t("本机", "This machine").to_owned())
}

/// The fork tray, a child of the window's root.
pub fn fork_tray_view(app: &mut App) -> Option<E> {
    let requests = app.local::<fork_tray::ForkStore>().requests.clone();
    let on_decide: fork_tray::ForkDecide = Rc::new(|app: &mut App, id: &str, approved| fork_tray::decide_fork_request(app, id, approved));
    let on_open: fork_tray::ForkOpenSource = Rc::new(|app: &mut App, workspace: &str, conversation: &str| app.select_conversation(workspace, conversation));
    let tray = fork_tray::fork_request_tray(&requests, crate::chrome::TOPBAR_HEIGHT, on_decide, Some(on_open))?;
    // `useFloatingSurface`: lifted out of the tree, so a native browser page under it makes room as it does for a menu. The tray's
    // own width and offsets, and nothing a press or Escape could dismiss.
    let (window_w, _) = window_size();
    let width = 360f32.min(window_w - 24.0);
    Some(tray.overlay_at(window_w - width - 12.0, crate::chrome::TOPBAR_HEIGHT + 12.0))
}
