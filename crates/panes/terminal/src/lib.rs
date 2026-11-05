//! The terminal pane, upstream's `TerminalPanel`, `TerminalTabBar`, `ShellTaskPanel` and `lib/terminal*.ts`.
//!
//! Bytes come from the host's `TerminalEvent` channel into `mewrk_term::Terminal`, keystrokes go back through
//! `write_terminal`, and everything between is `core::PanelCore`, a state machine with no host in it.
//!
//! Public views:
//!
//! ```text
//! terminal_pane(app: &mut App, props: TerminalPaneProps<'_>) -> TerminalPaneView { header: E, body: E }
//!     The pane. `header` goes in `SidePane`'s header slot (it is the pane's whole chrome), `body` in its body. Call this
//!     every frame the conversation is on screen, with `pane_open: false` while the pane is closed: that keeps the shells
//!     mounted, as upstream keeps the panel mounted behind a closed pane, and the returned elements may be dropped.
//!     Stopping the calls detaches: the same as upstream's panel unmounting.
//! terminal_panel(app: &mut App, props: PanelProps<'_>) -> E                       One shell's panel, on its own.
//! keep_panel(app: &mut App, props: PanelProps<'_>)                                The same, mounted but not on screen.
//! shell_task_panel(app: &mut App, task: &ShellTaskView, stopping: bool,
//!                  on_stop: Option<Rc<dyn Fn(&mut App)>>) -> E                    The read-only page.
//! keep_shell_task_panel(app: &mut App, task: &ShellTaskView)                      The same, mounted but not on screen.
//! terminal_tab_bar(props: TerminalTabBarProps) -> E                               The strip with its `+`.
//! page_tabs::page_tabs(props: PageTabs) -> E                                       The shared page strip, `mewrk_ui_common::page_tabs`.
//! popover_menu(key, label, trigger: E, sections: Vec<MenuSection>, align: Align) -> E    The menu `+` opens.
//! path_text(path, prefix, ty: TypeStyle, color: Option<Color>) -> E               `PathText`.
//! ```
//!
//! What the rest of the app calls, in place of upstream's `dispatchTerminalTabs` and friends:
//!
//! ```text
//! ensure_terminal_tab(app, owner_id, launch)            Opening the pane finds something to show.
//! open_terminal_in(app, owner_id, member, shell)        A new terminal in a workspace.
//! show_read_only_terminal(app, owner_id, shell_task_id) A command's task row was clicked. Then open the pane.
//! forget_read_only_terminal(app, owner_id, shell_task_id) -> bool   `shellTaskEvicted`; true when the pane should close.
//! close_terminal_tab(app, owner_id, terminal_id, on_last_tab_closed)
//! destroy_terminal_session(app, owner_id, terminal_id, done)
//! conversation_terminal_ids(app, owner_id) -> Vec<String>
//! remove_terminal_conversation(app, conversation_id)
//! active_read_only_shell_task(app, owner_id) -> Option<String>
//! terminal_busy_in(app, &[owner_id]) -> bool            `activeWorkspaceTerminalBusy`.
//! store(app).controller.current()                       The live sessions, for task rows and guards.
//! live_terminal_count(app, conversation_id, done)
//! ```

pub mod api;
pub mod controller;
pub mod core;
pub mod menu;
pub mod pane;
pub mod panel;
pub mod path;
pub mod shell_task;
mod spinner;
pub mod tab_bar;
pub mod tabs;

pub use api::{live_terminal_count, terminal_shell_label, TerminalCommandState, TerminalEvent, TerminalLaunchChoice, TerminalPhase, TerminalSessionState, TerminalShell};
pub use controller::{terminal_session_key, TerminalController};
pub use core::terminal_panel_id;
pub use menu::{popover_menu, MenuEntry, MenuSection};
pub use mewrk_ui_common::page_tabs;
pub use mewrk_ui_common::page_tabs::{fit_page_tabs, page_tabs as page_strip, PageTab, PageTabs};
pub use pane::{active_read_only_shell_task, close_terminal_tab, conversation_terminal_ids, destroy_terminal_session, ensure_terminal_tab, forget_read_only_terminal, new_terminal_sections, open_terminal_in, remove_terminal_conversation, show_read_only_terminal, terminal_busy_in, terminal_pane, terminal_session_phase, terminal_shell_menu_items, terminal_tab_label, terminal_workspace, NewTerminalMenu, TerminalPaneProps, TerminalPaneView, TerminalWorkspace};
pub use panel::{close_panel, keep_panel, panel_state, panel_status, panel_terminal, retry_panel, store, terminal_panel, PanelHooks, PanelProps, TerminalStore};
pub use path::{elide_path, path_text};
pub use shell_task::{keep_shell_task_panel, shell_task_copy_text, shell_task_failure, shell_task_panel, shell_task_terminal, ShellTaskOutcome, ShellTaskView};
pub use mewrk_client::machine_shells::terminal_shells_for;
pub use tab_bar::{terminal_tab_bar, TerminalTabBarProps, TerminalTabDescriptor};
pub use tabs::{terminal_strip_ids, TerminalTab, TerminalTabsAction, TerminalTabsLayout, TerminalTabsState, READ_ONLY_TERMINAL_TAB_ID};
