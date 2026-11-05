//! The tasks and history pane, upstream's TaskContainer, SubagentPanel, ShellTaskPanel, WorkflowRunPanel and HistoryPane,
//! with the usage statistics page and the fork request tray.
//!
//! # Public views
//!
//! Each is a function returning an element; the shell places it. What upstream's props carried is in a props struct.
//!
//! ```text
//! tasks_pane::tasks_pane(app: &mut App, props: TasksPaneProps) -> E
//! tasks_pane::tasks_pane_tab_bar(active: TasksPaneTab, on_select: impl Fn(&mut App, TasksPaneTab) + 'static) -> E
//! tasks_pane::tasks_pane_tab_label(tab: TasksPaneTab) -> &'static str
//! tasks_pane::stop_task_item(app: &mut App, item: &TaskItem, hooks: &StopHooks)        // what `on_stop_item` should do
//! tasks_pane::focus_workflow_run_panel(run_id: &str) -> bool
//!
//! workflow_panel::workflow_run_panel(props: WorkflowRunPanelProps) -> E
//! workflow_panel::control_workflow_step(app: &App, conversation_id, run_id, step_index, action)
//!
//! subagent_panel::subagent_panel(app: &mut App, props: SubagentPanelProps<'_>) -> E
//! subagent_panel::streamed_subagent_panel(app: &mut App, props: StreamedSubagentPanelProps<'_>) -> Option<E>
//! subagent_panel::subagent_tab_bar(agents: &[Rc<SubagentView>], active_id: Option<&str>, on_select, on_close, on_reorder) -> E
//! subagent_panel::subagent_status_badge(status: SubagentViewStatus) -> E
//!
//! shell_panel::shell_task_panel(app: &mut App, props: ShellTaskPanelProps) -> E
//!
//! history_pane::history_pane(app: &mut App, props: HistoryPaneProps) -> E
//!
//! usage_card::usage_settings(app: &mut App) -> E                                       // reads, backfills, draws
//! usage_card::usage_stats_card(statistics: &UsageStatistics, loading: bool) -> E
//!
//! fork_tray::fork_request_tray(requests: &[PendingForkRequest], caption_height: f32, on_decide: ForkDecide,
//!                              on_open_source: Option<ForkOpenSource>) -> Option<E>    // a child of the window's root
//! ```
//!
//! # State the shell starts
//!
//! - `shell_tasks::install(app)` follows `shellTask*` push events into `ShellTaskStore`; `shell_tasks::reconcile(app, id)`
//!   when a conversation becomes active. `ShellTaskStore::for_conversation(id)` fills `TaskSources::shell_tasks`, and its
//!   `on_evicted` closes a page showing a row the host let go of.
//! - `fork_tray::install(app)`, `list_pending_fork_requests(app)` after run adoption, `list_fork_decisions(app, id)` when a
//!   conversation becomes active, `decide_fork_request(app, id, approved)` as `on_decide`.
//! - `subagents::derive_subagent_views(contexts, &subagent_view_messages())` makes `TaskSources::agents`. It walks whole
//!   transcripts, so keep its result until the contexts change rather than calling it every frame.
//!
//! # Elements this crate does not draw
//!
//! `SubagentPanelProps::transcript` is the chat timeline, passed as a closure.

pub mod ansi_view;
pub mod context;
pub mod fork_tray;
pub mod history_pane;
pub mod history_record;
pub mod line_diff;
pub mod ojson;
pub mod shell_panel;
pub mod shell_tasks;
pub mod sources;
pub mod subagent_panel;
pub mod subagents;
pub mod task_container;
pub mod tasks_pane;
pub mod usage_card;
pub mod usage_statistics;
pub mod util;
pub mod widgets;
pub mod workflow_panel;
pub mod workflow_progress;
pub mod workflow_runs;
