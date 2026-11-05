//! The terminal pane as a whole: the part of upstream's `App.tsx` that draws it and drives its tabs.

use std::collections::HashSet;

use mewrk_client::prelude::*;

use crate::api::{terminal_shell_label, terminal_shell_wire_name, TerminalLaunchChoice, TerminalPhase, TerminalShell};
use crate::controller::terminal_session_key;
use crate::menu::{MenuEntry, MenuSection};
use crate::panel::{close_panel, keep_panel, store, terminal_panel, PanelHooks, PanelProps};
use crate::path::path_text;
use crate::shell_task::{keep_shell_task_panel, shell_task_panel, ShellTaskView};
use crate::tab_bar::{terminal_tab_bar, TerminalTabBarProps, TerminalTabDescriptor};
use crate::tabs::{terminal_strip_ids, TerminalTab, TerminalTabsAction, READ_ONLY_TERMINAL_TAB_ID};

/// One place a new terminal can start: a workspace of the conversation, by its 1-based number.
pub struct TerminalWorkspace {
    pub path: String,
    /// `workspaceLocationTitle`: the path and, when it is not this machine, where it is.
    pub title: Option<String>,
    /// `machineIcon`: `Folder` for this machine, `SquareTerminal` for WSL, `Server` for SSH.
    pub machine_icon: &'static str,
    /// What a terminal there can start, most preferred first: `mewrk_client::machine_shells::terminal_shells_for`.
    pub shells: Vec<TerminalShell>,
}

/// A workspace of the conversation as the `+` offers it, on its machine with what that machine was probed to have.
pub fn terminal_workspace(app: &mut App, workspace: &model::AttachedWorkspace) -> TerminalWorkspace {
    let machine = workspace.machine.as_ref();
    let ssh_machines = app.doc.assets.execution_environments.ssh_machines.clone();
    let machine_icon = match machine {
        None => "Folder",
        Some(model::RunTarget::Wsl { .. }) => "SquareTerminal",
        Some(model::RunTarget::Ssh { .. }) => "Server",
    };
    TerminalWorkspace { path: workspace.path.clone(), title: Some(mewrk_client::workspaces::workspace_location_title(&workspace.path, machine, &ssh_machines)), machine_icon, shells: app.terminal_shells_on(machine) }
}

/// What the terminal's `+` offers: upstream's `newTerminalMenu`.
pub struct NewTerminalMenu {
    /// The conversation's workspaces, in its numbering.
    pub workspaces: Vec<TerminalWorkspace>,
    /// Which of them the composer has selected, 1-based.
    pub selected: u32,
    /// A workspace lifecycle operation is running, so nothing can start now.
    pub disabled: bool,
}

/// What upstream's `renderPane("terminal")` reads from `App`.
pub struct TerminalPaneProps<'a> {
    /// The terminals' host identity: the conversation on screen, or for a draft the id it will materialize as.
    pub owner_id: &'a str,
    /// Set only while the terminal belongs to the draft: the project it is aimed at.
    pub draft_workspace_id: Option<&'a str>,
    /// The pane is showing. While not, call this anyway: the shells stay mounted behind it.
    pub pane_open: bool,
    /// `terminalInputDisabledReason`: a Git write is rewriting the checkout the shell is sitting in.
    pub input_disabled_reason: Option<&'a str>,
    /// The command the read-only page shows, found among the conversation's shell tasks. `None` when it has let go.
    pub read_only_task: Option<ShellTaskView>,
    pub read_only_stopping: bool,
    pub on_stop_task: Option<Rc<dyn Fn(&mut App, &ShellTaskView)>>,
    /// The rest of `beginTerminalCommand`: the checks only the whole app can make (no Git write in progress in this
    /// workspace, the conversation is not being deleted). The session check and the marking happen here.
    pub command_gate: Rc<dyn Fn(&mut App) -> bool>,
    /// For the draft: `awaitWorkspaceAtHost`.
    pub before_open: Option<Arc<dyn Fn(&Task) + Send + Sync>>,
    /// The last tab left, so the pane goes with it: close it and give the composer focus back.
    pub on_last_tab_closed: Rc<dyn Fn(&mut App)>,
    pub new_terminal: NewTerminalMenu,
}

/// What the pane draws. `SidePane` holds them: the bar takes its `header` slot, and the rest its body.
pub struct TerminalPaneView {
    pub header: E,
    pub body: E,
}

/// A tab is named after the shell it runs, the way the reference shell names its tabs after the program running in them.
pub fn terminal_tab_label(tab: &TerminalTab) -> String {
    if let Some(name) = &tab.name {
        return name.clone();
    }
    match tab.launch.and_then(|l| l.shell) {
        Some(shell) => terminal_shell_label(shell).to_owned(),
        None => t("终端", "Terminal").to_owned(),
    }
}

/// `terminalShellMenuItems`: the shells for a workspace on one machine. A machine with none gets a row that says so, so
/// a menu never opens empty.
pub fn terminal_shell_menu_items(shells: &[TerminalShell], disabled: bool, on_select: Rc<dyn Fn(&mut App, TerminalShell)>) -> Vec<MenuEntry> {
    if shells.is_empty() {
        return vec![MenuEntry { icon: Some("SquareTerminal"), disabled: true, ..MenuEntry::new("none", t("未探测到可用的 shell", "No shells detected")) }];
    }
    shells
        .iter()
        .map(|&shell| {
            let f = on_select.clone();
            MenuEntry { icon: Some("SquareTerminal"), disabled, on_select: Some(Rc::new(move |app: &mut App| f(app, shell))), ..MenuEntry::new(terminal_shell_wire_name(shell), terminal_shell_label(shell)) }
        })
        .collect()
}

/// Where a new terminal can start: the shells a workspace's machine was probed to have, and with more than one
/// workspace, the workspace first and its shells beside it.
pub fn new_terminal_sections(menu: NewTerminalMenu, owner_id: &str, open_pane: Rc<dyn Fn(&mut App)>) -> Vec<MenuSection> {
    let NewTerminalMenu { workspaces, selected, disabled } = menu;
    let opener = |member: u32| -> Rc<dyn Fn(&mut App, TerminalShell)> {
        let (owner, open_pane) = (owner_id.to_owned(), open_pane.clone());
        Rc::new(move |app: &mut App, shell| {
            open_terminal_in(app, &owner, member, shell);
            open_pane(app);
        })
    };
    if workspaces.len() > 1 {
        let items = workspaces
            .iter()
            .enumerate()
            .map(|(i, w)| {
                let children = terminal_shell_menu_items(&w.shells, false, opener(i as u32 + 1));
                MenuEntry { title: w.title.as_deref().map(Arc::from), icon: Some(w.machine_icon), disabled, is_path: true, children, ..MenuEntry::new(format!("workspace-{}", i + 1), w.path.as_str()) }
            })
            .collect();
        return vec![MenuSection { id: "workspaces".to_owned(), label: Some(Arc::from(t("在哪个工作区打开", "Open in which workspace"))), items }];
    }
    let current = workspaces.get(selected.saturating_sub(1) as usize).or_else(|| workspaces.first());
    let label = current.map(|w| Arc::from(tf("在 {name} 打开", "Open in {name}", &[("name", &w.path)]).as_str()));
    let shells = current.map_or(&[][..], |w| &w.shells[..]);
    vec![MenuSection { id: "shells".to_owned(), label, items: terminal_shell_menu_items(shells, disabled, opener(selected.max(1))) }]
}

/// Opens a new terminal in the conversation's workspace `member` (1-based), in `shell`. The caller opens the pane.
pub fn open_terminal_in(app: &mut App, owner_id: &str, member: u32, shell: TerminalShell) {
    store(app).tabs.apply(TerminalTabsAction::Add { conversation_id: owner_id.to_owned(), launch: Some(TerminalLaunchChoice { workspace: Some(member), shell: Some(shell) }) });
    repaint();
}

/// A terminal pane with no tabs has nothing to show, so asking for the pane asks for a terminal to put in it: call this
/// when the pane is opened. `launch` is what a terminal nobody chose a shell for starts: the workspace the composer has
/// selected, in that workspace's first shell.
pub fn ensure_terminal_tab(app: &mut App, owner_id: &str, launch: Option<TerminalLaunchChoice>) {
    store(app).tabs.apply(TerminalTabsAction::Ensure { conversation_id: owner_id.to_owned(), launch });
    repaint();
}

/// A command's row asks for its output: shown first, so that opening the pane finds a page to show and starts no shell.
pub fn show_read_only_terminal(app: &mut App, owner_id: &str, shell_task_id: &str) {
    store(app).tabs.apply(TerminalTabsAction::ShowReadOnly { conversation_id: owner_id.to_owned(), shell_task_id: shell_task_id.to_owned() });
    repaint();
}

/// The host let go of a command's output (`shellTaskEvicted`). Answers whether that took the pane's last tab, in which
/// case the caller closes the pane.
pub fn forget_read_only_terminal(app: &mut App, owner_id: &str, shell_task_id: &str) -> bool {
    let tabs = &mut store(app).tabs;
    let layout = tabs.layout(owner_id);
    let last = layout.read_only.as_deref() == Some(shell_task_id) && terminal_strip_ids(&layout).len() <= 1;
    tabs.apply(TerminalTabsAction::ForgetReadOnly { conversation_id: owner_id.to_owned(), shell_task_id: shell_task_id.to_owned() });
    repaint();
    last
}

/// The conversation is gone: its strip goes too.
pub fn remove_terminal_conversation(app: &mut App, conversation_id: &str) {
    store(app).tabs.apply(TerminalTabsAction::RemoveConversation { conversation_id: conversation_id.to_owned() });
}

/// Ends one terminal and takes its tab with it.
///
/// A tab is its shell's only place on screen, so the two leave together, and the pane goes with the last of them the way
/// a browser window closes with its last page. The pane's own × is the opposite act: it puts the window away and leaves
/// every shell running behind it.
///
/// The mounted panel owns the kill so input stays gated and the body stays veiled until the host confirms it; only a tab
/// with no panel, one belonging to another conversation, goes straight to the session store.
pub fn close_terminal_tab(app: &mut App, owner_id: &str, terminal_id: &str, on_last_tab_closed: Rc<dyn Fn(&mut App)>) {
    let was_last = terminal_strip_ids(&store(app).tabs.layout(owner_id)).len() <= 1;
    let (owner, terminal) = (owner_id.to_owned(), terminal_id.to_owned());
    let finish: Box<dyn FnOnce(&mut App)> = Box::new(move |app| {
        store(app).tabs.apply(TerminalTabsAction::Close { conversation_id: owner, terminal_id: terminal });
        repaint();
        if was_last {
            on_last_tab_closed(app);
        }
    });
    // The read-only page has no shell behind it: closing it only takes the page away, and the command it showed runs on.
    if terminal_id == READ_ONLY_TERMINAL_TAB_ID {
        finish(app);
        return;
    }
    close_panel(app, owner_id, terminal_id, finish);
}

/// Ends a terminal from outside the pane, a moved or deleted conversation's: `destroyTerminalSession`. `done` says whether
/// the host took it.
pub fn destroy_terminal_session(app: &mut App, owner_id: &str, terminal_id: &str, done: impl FnOnce(&mut App, bool) + 'static) {
    crate::panel::request_close(app, owner_id, terminal_id, Box::new(move |app, result| done(app, result.is_ok())));
}

/// Every terminal id a conversation has here, tabs and sessions alike: what ending the conversation has to close.
pub fn conversation_terminal_ids(app: &mut App, owner_id: &str) -> Vec<String> {
    let store_ = store(app);
    let mut ids: Vec<String> = store_.tabs.layout(owner_id).tabs.iter().map(|tab| tab.id.clone()).collect();
    for session in store_.controller.current().values() {
        if session.conversation_id == owner_id && !ids.contains(&session.terminal_id) {
            ids.push(session.terminal_id.clone());
        }
    }
    ids
}

/// The active conversation's read-only page, for marking its task row: `activeReadOnlyShellTaskId`.
pub fn active_read_only_shell_task(app: &mut App, owner_id: &str) -> Option<String> {
    let layout = store(app).tabs.layout(owner_id);
    if layout.active_id.as_deref() == Some(READ_ONLY_TERMINAL_TAB_ID) { layout.read_only } else { None }
}

/// Whether any terminal the renderer can see in these conversations is running a command: `activeWorkspaceTerminalBusy`.
pub fn terminal_busy_in(app: &mut App, owner_ids: &[&str]) -> bool {
    store(app).controller.current().values().any(|session| session.busy && owner_ids.contains(&session.conversation_id.as_str()))
}

/// Draws the pane. Every tab stays mounted while this is called, shown or not.
pub fn terminal_pane(app: &mut App, props: TerminalPaneProps<'_>) -> TerminalPaneView {
    let TerminalPaneProps { owner_id, draft_workspace_id, pane_open, input_disabled_reason, read_only_task, read_only_stopping, on_stop_task, command_gate, before_open, on_last_tab_closed, new_terminal } = props;
    let layout = store(app).tabs.layout(owner_id);

    // The tab strip.
    let mut descriptors: Vec<TerminalTabDescriptor> = Vec::new();
    if let Some(task) = &read_only_task {
        // A command the host has already let go of has nothing left to show, so its page is not drawn.
        let directory = task.directory();
        descriptors.push(TerminalTabDescriptor {
            id: READ_ONLY_TERMINAL_TAB_ID.to_owned(),
            label: task.title(),
            title: Some(task.command.clone()),
            content: directory.map(|d| path_text(d, format!("{}:", task.tool_name), CAPTION, None).flex1()),
            read_only: true,
        });
    }
    for tab in &layout.tabs {
        descriptors.push(TerminalTabDescriptor { id: tab.id.clone(), label: terminal_tab_label(tab), title: None, content: None, read_only: false });
    }
    let closing_ids: HashSet<String> = {
        let store_ = store(app);
        layout.tabs.iter().filter(|tab| store_.controller.session(owner_id, &tab.id).is_some_and(|s| s.phase == TerminalPhase::Closing)).map(|tab| tab.id.clone()).collect()
    };
    let owner = owner_id.to_owned();
    let on_select: Rc<dyn Fn(&mut App, &str)> = {
        let owner = owner.clone();
        Rc::new(move |app: &mut App, id: &str| {
            store(app).tabs.apply(TerminalTabsAction::Activate { conversation_id: owner.clone(), terminal_id: id.to_owned() });
            repaint();
        })
    };
    let on_close: Rc<dyn Fn(&mut App, &str)> = {
        let (owner, last) = (owner.clone(), on_last_tab_closed.clone());
        Rc::new(move |app: &mut App, id: &str| close_terminal_tab(app, &owner, id, last.clone()))
    };
    let on_rename: Rc<dyn Fn(&mut App, &str, String)> = {
        let owner = owner.clone();
        Rc::new(move |app: &mut App, id: &str, name: String| {
            let tabs = &mut store(app).tabs;
            let layout = tabs.layout(&owner);
            // Submitting the name the terminal already shows is not a rename: it keeps following the derived one.
            let same = layout.tabs.iter().find(|tab| tab.id == id).is_some_and(|tab| name.trim() == terminal_tab_label(tab));
            tabs.apply(TerminalTabsAction::Rename { conversation_id: owner.clone(), terminal_id: id.to_owned(), name: if same { String::new() } else { name } });
            repaint();
        })
    };
    let on_reorder: Rc<dyn Fn(&mut App, Vec<String>)> = {
        let owner = owner.clone();
        Rc::new(move |app: &mut App, terminal_ids: Vec<String>| {
            store(app).tabs.apply(TerminalTabsAction::Reorder { conversation_id: owner.clone(), terminal_ids });
            repaint();
        })
    };
    let add = new_terminal_sections(new_terminal, owner_id, Rc::new(|_| {}));
    let header = terminal_tab_bar(TerminalTabBarProps { key: format!("terminal-tabs-{owner_id}"), tabs: descriptors, active_id: layout.active_id.clone(), closing_ids, on_select, on_close, on_rename, on_reorder: Some(on_reorder), add });

    // The body: one on screen, and the others kept mounted behind it.
    let read_only_open = pane_open && layout.active_id.as_deref() == Some(READ_ONLY_TERMINAL_TAB_ID);
    let mut body: E = col().key(("terminal-pane", owner_id)).flex1().min_w(0.0).min_h(0.0);
    if let Some(task) = &read_only_task {
        if read_only_open {
            let stop = on_stop_task.as_ref().map(|f| {
                let (f, task) = (f.clone(), task.clone());
                Rc::new(move |app: &mut App| f(app, &task)) as Rc<dyn Fn(&mut App)>
            });
            body = body.child(shell_task_panel(app, task, read_only_stopping, stop));
        } else {
            keep_shell_task_panel(app, task);
        }
    }
    for tab in &layout.tabs {
        let open = pane_open && layout.active_id.as_deref() == Some(tab.id.as_str());
        let label = terminal_tab_label(tab);
        let hooks = {
            let (owner, terminal, gate) = (owner.clone(), tab.id.clone(), command_gate.clone());
            let (close_owner, last) = (owner.clone(), on_last_tab_closed.clone());
            let close_terminal = tab.id.clone();
            PanelHooks {
                // `beginTerminalCommand`: no session, or the app says not now, and the command does not start.
                on_command_start: Rc::new(move |app: &mut App| {
                    if store(app).controller.session(&owner, &terminal).is_none() || !gate(app) {
                        return false;
                    }
                    store(app).controller.mark_command_started(&owner, &terminal)
                }),
                before_open: before_open.clone(),
                on_clean_exit: Rc::new(move |app: &mut App| close_terminal_tab(app, &close_owner, &close_terminal, last.clone())),
            }
        };
        let panel_props = PanelProps { conversation_id: owner_id, terminal_id: &tab.id, label: &label, open, launch: tab.launch, draft_workspace_id, input_disabled_reason, hooks };
        if open {
            body = body.child(terminal_panel(app, panel_props));
        } else {
            keep_panel(app, panel_props);
        }
    }
    TerminalPaneView { header, body }
}

/// For a tab's effect on the rest of the app: is this terminal's session known, and how does it stand?
pub fn terminal_session_phase(app: &mut App, owner_id: &str, terminal_id: &str) -> Option<TerminalPhase> {
    store(app).controller.current().get(&terminal_session_key(owner_id, terminal_id)).map(|s| s.phase)
}
