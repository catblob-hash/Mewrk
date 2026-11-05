//! The handlers of `App.tsx` that belong to the shell: opening and closing panes, preview pages, task rows.

use mewrk_client::drafts::draft_workspace_id_of;
use mewrk_client::prelude::*;
use mewrk_client::side_panes::{focused_pane, open_preview_session, new_preview_page_session_id, pane_is_open, pane_kind, preview_pane_id, preview_sessions_for, preview_tab_after_closing, preview_workspace_of, shown_tasks_tab, side_pane_layout_for, SidePaneKind, SidePanesAction, TasksPaneTab};
use mewrk_pane_browser::controller::BrowserController;
use mewrk_pane_browser::preview::take_viewer_network_cleared;
use mewrk_pane_browser::{hide_built_in_browser, note_preview_pane_closed, present_page, request_browser_session_close, PreviewTarget};
use mewrk_pane_files::links::PathOpenRequest;
use mewrk_pane_tasks::task_container::{TaskItem, TaskKind};
use mewrk_pane_tasks::tasks_pane::{stop_task_item, StopHooks};
use mewrk_pane_terminal::{ensure_terminal_tab, show_read_only_terminal, TerminalLaunchChoice};
use mewrk_settings_global::SettingsView;

use crate::state::shell;

/// `activeConversationIdRef.current`.
pub fn active_id(app: &mut App) -> Option<String> {
    app.active_conversation_id()
}

/// `openGlobalSettings(view)`: takes upstream's view ids.
pub fn open_global_settings(app: &mut App, view: &str) {
    mewrk_settings_global::open(app, SettingsView::from_wire(view));
}

/// `defaultTerminalLaunchRef`: what a terminal nobody chose a shell for starts: the workspace the composer's chip has
/// selected, in the first shell that machine was probed to have.
pub fn default_terminal_launch(app: &mut App) -> Option<TerminalLaunchChoice> {
    let member = app.active_workspace_member();
    let workspaces = app.active_project_workspaces();
    let machine = workspaces.get(member.saturating_sub(1)).and_then(|workspace| workspace.machine.clone()).or_else(|| app.active_workspace().and_then(|workspace| workspace.machine.clone()));
    let shell = app.terminal_shells_on(machine.as_ref()).first().copied();
    Some(TerminalLaunchChoice { workspace: Some(member as u32), shell })
}

/// `openPane`.
pub fn open_pane(app: &mut App, pane: &str) {
    let Some(conversation_id) = active_id(app) else { return };
    // A terminal pane with no tabs has nothing to show, so asking for the pane asks for a terminal to put in it.
    if pane == "terminal" {
        let owner = app.host_conversation_id(&conversation_id);
        let launch = default_terminal_launch(app);
        ensure_terminal_tab(app, &owner, launch);
    }
    app.dispatch_side_panes(SidePanesAction::Open { conversation_id, pane: pane.to_owned() });
    repaint();
}

/// `closePane`. Closing a preview pane releases the native surface; the session itself keeps running.
pub fn close_pane(app: &mut App, pane: &str) {
    let Some(conversation_id) = active_id(app) else { return };
    if pane_kind(pane) == SidePaneKind::Files {
        mewrk_pane_files::drop_files_pane(app, &format!("{pane}:{conversation_id}"));
    }
    app.dispatch_side_panes(SidePanesAction::Close { conversation_id, pane: pane.to_owned() });
    if pane_kind(pane) == SidePaneKind::Preview {
        note_preview_pane_closed();
        hide_built_in_browser(app);
    }
    repaint();
}

/// `togglePane`. Reads the current state, not a frame's, so two toggles in one tick observe each other.
pub fn toggle_pane(app: &mut App, pane: &str) {
    let Some(conversation_id) = active_id(app) else { return };
    if pane_is_open(side_pane_layout_for(app.side_panes(), Some(&conversation_id)), pane) { close_pane(app, pane) } else { open_pane(app, pane) }
}

/// `showTasksTab`.
pub fn show_tasks_tab(app: &mut App, tab: TasksPaneTab) {
    let Some(conversation_id) = active_id(app) else { return };
    app.dispatch_side_panes(SidePanesAction::ShowTasksTab { conversation_id, tab });
    repaint();
}

/// `toggleTasksTab`: a tab's menu row closes the pane when it is that tab on show, and otherwise shows that tab.
pub fn toggle_tasks_tab(app: &mut App, tab: TasksPaneTab) {
    let Some(conversation_id) = active_id(app) else { return };
    if shown_tasks_tab(side_pane_layout_for(app.side_panes(), Some(&conversation_id))) == Some(tab) { close_pane(app, "tasks") } else { show_tasks_tab(app, tab) }
}

/// `openSubagentPanel`: an agent's read-only transcript as a tab of the subagent pane.
pub fn open_subagent_panel(app: &mut App, subagent_id: &str) {
    let Some(conversation_id) = active_id(app) else { return };
    crate::effects::close_editors(app);
    app.dispatch_side_panes(SidePanesAction::OpenSubagent { conversation_id, subagent_id: subagent_id.to_owned() });
    repaint();
}

/// `closeLastPane`: leaves the focused pane, which is what the `panel.close` shortcut addresses.
pub fn close_last_pane(app: &mut App) {
    let Some(conversation_id) = active_id(app) else { return };
    if let Some(pane) = focused_pane(side_pane_layout_for(app.side_panes(), Some(&conversation_id))).map(str::to_owned) {
        close_pane(app, &pane);
    }
}

fn browser(app: &mut App) -> &mut BrowserController {
    app.local::<BrowserController>()
}

/// `openBrowserTab`: shows a page in the conversation's preview pane.
pub fn open_browser_tab(app: &mut App, session_id: Option<&str>, url: Option<String>, done: impl FnOnce(&mut App, Result<(), String>) + Send + 'static) {
    let Some(conversation_id) = active_id(app) else { return done(app, Ok(())) };
    let target = session_id.map_or_else(|| app.host_conversation_id(&conversation_id), str::to_owned);
    if app.conversation_deletion_is_active(&conversation_id) || browser(app).close_in_flight(&target) {
        return done(app, Ok(()));
    }
    // Two native pages share one panel rectangle, so the outgoing page must be hidden before the incoming one is shown.
    if browser(app).visible_session().is_some_and(|visible| visible != target) {
        hide_built_in_browser(app);
        if active_id(app).as_deref() != Some(conversation_id.as_str()) {
            return done(app, Ok(()));
        }
    }
    shell(app).last_preview_page.insert(conversation_id.clone(), target.clone());
    app.dispatch_side_panes(SidePanesAction::Open { conversation_id: conversation_id.clone(), pane: preview_pane_id(&target) });
    let session = target.clone();
    present_page(app, &target, url, move |app, result| {
        if active_id(app).as_deref() != Some(conversation_id.as_str()) {
            return done(app, Ok(()));
        }
        let opened = result.as_ref().copied().unwrap_or(false);
        let visible = browser(app).visible_session().map(str::to_owned);
        if visible.is_none() && !opened {
            // The host refused the open, and the refusal took the page off the visible slot. The page itself is still this
            // conversation's, so only the pane goes: left open it drew nothing and took two more clicks to open.
            app.dispatch_side_panes(SidePanesAction::Close { conversation_id, pane: preview_pane_id(&session) });
        } else if visible.as_deref() == Some(session.as_str()) && !opened {
            app.dispatch_side_panes(SidePanesAction::ForgetPreview { conversation_id, session_id: session });
        }
        repaint();
        done(app, result.map(|_| ()));
    });
}

/// `activePreviewOwnerTarget`.
pub fn preview_owner_target(app: &mut App) -> Option<PreviewTarget> {
    let conversation_id = active_id(app)?;
    let host = app.host_conversation_id(&conversation_id);
    // The draft's host identity has no document behind it yet, so the host reads its project off this.
    let draft_workspace_id = draft_workspace_id_of(&conversation_id).map(str::to_owned);
    Some(PreviewTarget { conversation_id: host, draft_workspace_id, workspace: None })
}

/// `previewTargetForPage`: where a page's start page reads its servers from and runs them: its own workspace.
pub fn preview_target_for_page(app: &mut App, session_id: &str) -> Option<PreviewTarget> {
    let owner = preview_owner_target(app)?;
    let workspace = preview_workspace_of(app.side_panes(), session_id);
    Some(if workspace > 1 { PreviewTarget { workspace: Some(workspace), ..owner } } else { owner })
}

/// `openPreviewPage`: opens a page for the conversation's workspace `member` and shows it. A page with nothing loaded yet
/// is that workspace's start page, which is exactly what a new page would be, so an idle one is brought forward rather
/// than joined by a second copy of itself.
pub fn open_preview_page(app: &mut App, member: u32) {
    let Some(conversation_id) = active_id(app) else { return };
    let roster: Vec<String> = preview_sessions_for(app.side_panes(), Some(&conversation_id)).to_vec();
    let idle = roster.iter().find(|session| {
        if preview_workspace_of(app.local::<mewrk_client::side_panes::SidePanesState>(), session) != member {
            return false;
        }
        browser(app).page_is_blank(session)
    }).cloned();
    let owner = preview_owner_target(app);
    let target_of = |owner: PreviewTarget| if member > 1 { PreviewTarget { workspace: Some(member), ..owner } } else { owner };
    if let Some(idle) = idle {
        if let Some(owner) = owner.filter(|_| take_viewer_network_cleared(&idle)) {
            // The idle tab last showed a remote-control viewer, which took it off its workspace's network. It is this
            // workspace's start page again, so it goes back to the workspace's machine before it is shown.
            let session = idle.clone();
            let conversation = conversation_id.clone();
            app.invoke::<Value>("browser_set_page_network", json!({ "sessionId": idle, "target": target_of(owner) }), move |app, _| {
                if active_id(app).as_deref() == Some(conversation.as_str()) {
                    open_browser_tab(app, Some(&session), None, |_, _| {});
                }
            });
            return;
        }
        return open_browser_tab(app, Some(&idle), None, |_, _| {});
    }
    let token = format!("tab_{:016x}{:08x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64), std::process::id());
    let host = app.host_conversation_id(&conversation_id);
    let session = new_preview_page_session_id(&roster, &host, &token);
    app.dispatch_side_panes(SidePanesAction::RegisterPreview { conversation_id: conversation_id.clone(), session_id: session.clone(), workspace: Some(member) });
    let Some(owner) = owner else {
        return open_browser_tab(app, Some(&session), None, |_, _| {});
    };
    // The page's network is its workspace's machine, and it has to be that before the page exists: its first request must
    // already leave from there. A binding that fails leaves the page on this computer, and the pane says why.
    let opening = session.clone();
    app.invoke::<Value>("browser_set_page_network", json!({ "sessionId": session, "target": target_of(owner) }), move |app, _| {
        if active_id(app).as_deref() == Some(conversation_id.as_str()) {
            open_browser_tab(app, Some(&opening), None, |_, _| {});
        }
    });
}

/// `showPreviewPanel`: brings the preview pane up on the page the user last had there. With no page at all, the pane opens
/// on the start page of the workspace the composer's chip has selected.
pub fn show_preview_panel(app: &mut App) {
    let Some(conversation_id) = active_id(app) else { return };
    let roster: Vec<String> = preview_sessions_for(app.side_panes(), Some(&conversation_id)).to_vec();
    if roster.is_empty() {
        let member = app.active_workspace_member() as u32;
        return open_preview_page(app, member);
    }
    let last = shell(app).last_preview_page.get(&conversation_id).cloned().filter(|page| roster.contains(page));
    let page = last.or_else(|| preview_tabs(app, &conversation_id).into_iter().next()).unwrap_or_else(|| roster[0].clone());
    open_browser_tab(app, Some(&page), None, |_, _| {});
}

/// The pages a conversation's strip lists, in its order. A session showing nothing is the pane's start page and has no tab.
pub fn preview_tabs(app: &mut App, conversation_id: &str) -> Vec<String> {
    let roster: Vec<String> = preview_sessions_for(app.side_panes(), Some(conversation_id)).to_vec();
    roster.into_iter().filter(|session| !browser(app).page_is_blank(session)).collect()
}

/// The preview toolbar button and `panel.browser.toggle`.
pub fn toggle_preview(app: &mut App) {
    let Some(conversation_id) = active_id(app) else { return };
    match open_preview_session(side_pane_layout_for(app.side_panes(), Some(&conversation_id))).map(str::to_owned) {
        Some(session) => close_pane(app, &preview_pane_id(&session)),
        None => show_preview_panel(app),
    }
}

/// Closes a page at the host. `settled` runs once it has gone, with what the page was showing.
fn close_native_page(app: &mut App, conversation_id: &str, session_id: &str, settled: impl FnOnce(&mut App, &str, &str) + Send + 'static) {
    // Only the primary session is the surface Agent tools drive, so only it has to wait for automation to stop.
    if mewrk_client::side_panes::is_primary_preview_session(session_id, conversation_id) && crate::effects::automation_holds(app, conversation_id) {
        return;
    }
    // Read before the close: settling it erases the page's status, and with it the only record of which server it showed.
    let closed = crate::effects::OpenPage::of(app, session_id);
    let (conversation, session) = (conversation_id.to_owned(), session_id.to_owned());
    request_browser_session_close(app, session_id, move |app, result| {
        if result.is_err() {
            return;
        }
        take_viewer_network_cleared(&session);
        let controller = browser(app);
        controller.clear_visible_session_if(&session);
        controller.forget_page(&session);
        if active_id(app).as_deref() == Some(conversation.as_str()) {
            browser(app).runtime_ready = false;
        }
        settled(app, &conversation, &session);
        crate::effects::stop_servers_of_closed_page(app, &conversation, closed);
        repaint();
    });
}

/// `closePreviewSession`: closes one page and its native page, and stops the dev server it was the last page of.
pub fn close_preview_session(app: &mut App, conversation_id: &str, session_id: &str) {
    close_native_page(app, conversation_id, session_id, |app, conversation, session| {
        app.dispatch_side_panes(SidePanesAction::ForgetPreview { conversation_id: conversation.to_owned(), session_id: session.to_owned() });
    });
}

/// The page on screen goes and no tab is left to take its place. The pane stays, on its start page: this session stays on
/// the roster with nothing on it, and `keep_native_surface` presents it again as soon as the close is through.
fn close_last_preview_page(app: &mut App, conversation_id: &str, session_id: &str) {
    close_native_page(app, conversation_id, session_id, |app, conversation, session| {
        // The close took the page's network with it, and a start page's servers are its workspace's.
        if active_id(app).as_deref() == Some(conversation)
            && let Some(target) = preview_target_for_page(app, session)
        {
            app.invoke_logged("browser_set_page_network", json!({ "sessionId": session, "target": target }));
        }
    });
}

/// `closePreviewPage`: closes pages by their tabs. A neighbour takes the on-screen page's place first, so the pane moves to
/// another page instead of blinking shut and open again; with none left the pane stays up on its start page.
pub fn close_preview_pages(app: &mut App, sessions: &[String]) {
    let Some(conversation_id) = active_id(app) else { return };
    let open = open_preview_session(side_pane_layout_for(app.side_panes(), Some(&conversation_id))).map(str::to_owned);
    for session in sessions.iter().filter(|session| Some(session.as_str()) != open.as_deref()) {
        close_preview_session(app, &conversation_id, session);
    }
    let Some(open) = open.filter(|open| sessions.contains(open)) else { return };
    let tabs = preview_tabs(app, &conversation_id);
    match preview_tab_after_closing(&tabs, &open, sessions) {
        Some(next) => {
            let (conversation, session) = (conversation_id, open);
            open_browser_tab(app, Some(&next), None, move |app, _| close_preview_session(app, &conversation, &session));
        }
        None => close_last_preview_page(app, &conversation_id, &open),
    }
}

/// One page, from its tab.
pub fn close_preview_page(app: &mut App, session_id: &str) {
    close_preview_pages(app, &[session_id.to_owned()]);
}

/// `openGitReview`. Deliberately unconditional: the status card's rows all point at the same pane, and returning early when
/// it was already open used to make a second click do nothing at all.
pub fn open_git_review(app: &mut App, member: Option<usize>) {
    let Some(conversation_id) = active_id(app) else { return };
    let pages = crate::effects::review_pages(app);
    let page = member.unwrap_or_else(|| crate::effects::active_review_member(app, &pages));
    if !pages.iter().any(|entry| entry.member == page) {
        return;
    }
    app.set_review_page_member(&conversation_id, page);
    open_pane(app, "review");
}

/// `openTaskItemPage`.
pub fn open_task_item_page(app: &mut App, item: &TaskItem) {
    let Some(conversation_id) = active_id(app) else { return };
    match &item.kind {
        TaskKind::Terminal { .. } => open_pane(app, "terminal"),
        TaskKind::Shell { shell } => {
            // Shown first, so that opening the pane finds a page to show and starts no shell for it.
            let owner = app.host_conversation_id(&conversation_id);
            show_read_only_terminal(app, &owner, &shell.shell_task_id);
            open_pane(app, "terminal");
        }
        TaskKind::Plan { .. } => open_pane(app, "plan"),
        TaskKind::Fork { decision } => {
            // Only an approved row is openable, and the child it points at may since have been deleted: then the row stays a
            // record and opens nothing.
            let Some(child) = decision.child_conversation_id.clone() else { return };
            let workspace = decision.workspace_id.clone();
            if app.find_conversation(&workspace, &child).is_some() {
                app.set_active(Some(&workspace), Some(&child));
            }
        }
        TaskKind::Browser { session_id, .. } => open_browser_tab(app, Some(session_id), None, |_, _| {}),
        // A dev server has no pane of its own: what there is to look at is the page it serves, so the row opens the
        // conversation's preview.
        TaskKind::Preview { .. } => open_browser_tab(app, None, None, |_, _| {}),
        _ => {}
    }
}

/// `stopBrowserAutomation`, with the set the pane's stop control and the task row read.
pub fn stop_automation(app: &mut App, conversation: &str) {
    shell(app).automation_stopping.insert(conversation.to_owned());
    let id = conversation.to_owned();
    app.stop_browser_automation(conversation, move |app, _| {
        shell(app).automation_stopping.remove(&id);
        repaint();
    });
}

/// `stopTaskItem`.
pub fn stop_item(app: &mut App, item: &TaskItem) {
    let hooks = StopHooks {
        close_browser_session: Some(Rc::new(|app: &mut App, session: &str| request_browser_session_close(app, session, |_, _| {}))),
        stop_browser_automation: Some(Rc::new(|app: &mut App, conversation: &str| stop_automation(app, conversation))),
        close_pages_served_at: Some(Rc::new(|app: &mut App, conversation: &str, server| crate::effects::close_pages_served_at(app, conversation, &server.handle))),
        destroy_terminal: Some(Rc::new(|app: &mut App, terminal| mewrk_pane_terminal::destroy_terminal_session(app, &terminal.conversation_id, &terminal.terminal_id, |_, _| {}))),
    };
    stop_task_item(app, item, &hooks);
}

/// `mentionFileInComposer`.
pub fn mention_file_in_composer(app: &mut App, conversation_id: &str, mention: &mewrk_client::file_mention::FileMention) {
    let several = app.active_conversation_workspaces().len() > 1;
    let mention = mewrk_client::file_mention::file_mention_text(mention, several);
    mewrk_ui_composer::put_mention_in_composer(app, conversation_id, &mention);
}

/// Opens a file in the Files pane, opening the pane if it was closed (`showInFiles`).
pub fn show_in_files(app: &mut App, machine: mewrk_pane_files::browser::BrowseMachine, path: String, line: Option<u32>, new_page: bool, folder: bool) {
    let Some(conversation_id) = active_id(app) else { return };
    let was_open = pane_is_open(side_pane_layout_for(app.side_panes(), Some(&conversation_id)), "files");
    let state = shell(app);
    state.request_nonce += 1;
    let nonce = state.request_nonce;
    state.files_request = Some((conversation_id, mewrk_pane_files::pane::FilesPaneOpenRequest { machine, path, line, nonce, collapse_tree: !was_open, new_page, folder }));
    open_pane(app, "files");
}

/// `showInReview`.
pub fn show_in_review(app: &mut App, member: usize, relative: String) {
    let Some(conversation_id) = active_id(app) else { return };
    let was_open = pane_is_open(side_pane_layout_for(app.side_panes(), Some(&conversation_id)), "review");
    app.set_review_page_member(&conversation_id, member);
    let state = shell(app);
    state.request_nonce += 1;
    let nonce = state.request_nonce;
    state.review_request = Some((conversation_id, member, mewrk_pane_git::GitReviewRevealRequest { path: relative, nonce, collapse_tree: !was_open }));
    open_pane(app, "review");
}

/// What `setPathOpenHandler` does: a file path clicked anywhere in a transcript opens in the Files pane, and a file a turn
/// changed opens on its diff in the review pane when Git tracks the change.
pub fn handle_path_open(app: &mut App, request: &PathOpenRequest) -> bool {
    crate::dialogs::path_choice::open_path(app, request)
}
