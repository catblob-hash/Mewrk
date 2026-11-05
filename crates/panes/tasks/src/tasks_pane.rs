//! `components/TaskContainer.tsx` and `components/TasksPaneTabBar.tsx`.

use std::collections::HashMap;

use mewrk_client::prelude::*;

use fluent::{TypeStyle, Weight};

use crate::shell_tasks::{self, ShellTaskSnapshot};
use crate::sources::{PreviewServerSnapshot, RemoteControlPlatform, TerminalSessionState};
use crate::subagents::SubagentView;
use crate::task_container::*;
use crate::util::{format_elapsed, now_ms, to_fixed};
use crate::widgets::*;
use crate::workflow_panel::{workflow_run_panel, WorkflowRunPanelProps, WorkflowStepAction};
use crate::workflow_progress::{derive_workflow_progress, workflow_progress_messages, WorkflowProgressEntry, WorkflowProgressView};
use crate::workflow_runs::derive_workflow_run;

pub(crate) const LABEL_TY: TypeStyle = ts(11.5, 15.0, Weight::Semibold);
pub(crate) const LABEL_DONE_TY: TypeStyle = ts(11.5, 15.0, Weight::Regular);
pub(crate) const DETAIL_TY: TypeStyle = ts(10.0, 13.0, Weight::Regular);
pub(crate) const METRIC_TY: TypeStyle = mono(9.5, 13.0);

pub type StepControlHandler = Rc<dyn Fn(&mut App, &str, i64, WorkflowStepAction)>;

/// What upstream's `TasksPaneProps` carried.
pub struct TasksPaneProps {
    pub sources: TaskSources,
    /// Agent view currently shown in the main area, so its row reads as selected.
    pub selected_agent_id: Option<String>,
    /// Row whose page the message area is showing, when it is not a transcript. Falls back to `selected_agent_id`.
    pub selected_row_id: Option<String>,
    /// Ids whose abort has been requested and not yet observed as terminal.
    pub stopping_ids: Vec<String>,
    /// Live progress ledgers, keyed by the run's view id. A settled run simply has no entry.
    pub workflow_progress: HashMap<String, WorkflowProgressView>,
    pub workflow_run_ids: HashMap<String, String>,
    pub on_workflow_step_control: Option<StepControlHandler>,
    pub on_select_agent: Rc<dyn Fn(&mut App, &str)>,
    /// Opens a row that is not an agent in its page. Rows of those kinds stay inert without it.
    pub on_open_item: Option<Rc<dyn Fn(&mut App, &TaskItem)>>,
    pub on_stop_item: Rc<dyn Fn(&mut App, &TaskItem)>,
}

impl TasksPaneProps {
    pub fn new(sources: TaskSources, on_select_agent: impl Fn(&mut App, &str) + 'static, on_stop_item: impl Fn(&mut App, &TaskItem) + 'static) -> Self {
        Self { sources, selected_agent_id: None, selected_row_id: None, stopping_ids: Vec::new(), workflow_progress: HashMap::new(), workflow_run_ids: HashMap::new(), on_workflow_step_control: None, on_select_agent: Rc::new(on_select_agent), on_open_item: None, on_stop_item: Rc::new(on_stop_item) }
    }
}

fn task_item_icon(item: &TaskItem, size: f32, color: Color) -> E {
    let kind = match &item.kind {
        TaskKind::Aborted { source_kind, .. } => source_kind.as_str(),
        _ => item.kind_name(),
    };
    let name = match kind {
        "terminal" => "SquareTerminal",
        // Distinct from the terminal's framed glyph: a command the model started is not a shell the user opened.
        "shell" => "Terminal",
        "webSearch" | "browser" => "Globe2",
        "workflow" => "Workflow",
        // A dev server is a process, not a page; a remote-control row is a device.
        "preview" => match &item.kind {
            TaskKind::Preview { server, .. } => match server.remote.as_ref().map(|r| r.platform) {
                Some(RemoteControlPlatform::Desktop) => "Monitor",
                Some(_) => "Smartphone",
                None => "Server",
            },
            _ => "Server",
        },
        "plan" => "NotebookPen",
        "fork" => "GitFork",
        _ => "Bot",
    };
    icon(name, size).color(color)
}

/// 1_240 → "1.2k", 12_400 → "12k". Below 1000 the exact count fits.
pub fn format_tokens(tokens: i64) -> String {
    if tokens < 1000 {
        return tokens.to_string();
    }
    let thousands = tokens as f64 / 1000.0;
    if thousands < 10.0 { format!("{}k", to_fixed(thousands, 1)) } else { format!("{}k", (thousands + 0.5).floor() as i64) }
}

fn reports_agent_metrics(item: &TaskItem) -> bool {
    match &item.kind {
        TaskKind::Aborted { source_kind, .. } => source_kind == "subagent" || source_kind == "workflow",
        TaskKind::Subagent { .. } | TaskKind::Workflow { .. } => true,
        _ => false,
    }
}

/// The text and tooltip of each metric column a row shows. On an agent row a column it cannot report is an em dash;
/// a row that is not an agent has no such claim to make, so those columns are dropped outright.
pub fn task_metric_columns(item: &TaskItem) -> Vec<(&'static str, String, &'static str)> {
    let m = item.metrics;
    let dash = || "—".to_owned();
    let columns = [
        ("children", m.child_count.is_some(), m.child_count.map_or_else(dash, |n| tf("{count}子", "{count}c", &[("count", &n)])), t("子代数", "Child count")),
        ("tokens", m.tokens.is_some(), m.tokens.map_or_else(dash, format_tokens), t("token 数", "Tokens")),
        ("tools", m.tool_count.is_some(), m.tool_count.map_or_else(dash, |n| tf("{count}工具", "{count}t", &[("count", &n)])), t("工具调用数", "Tool calls")),
        ("elapsed", m.elapsed_ms.is_some(), m.elapsed_ms.map_or_else(dash, format_elapsed), t("运行时长", "Elapsed")),
    ];
    let agent = reports_agent_metrics(item);
    columns.into_iter().filter(|c| agent || c.1).map(|c| (c.0, c.2, c.3)).collect()
}

fn task_row(item: &TaskItem, p: &TasksPaneProps, stopping_ids: &[String]) -> E {
    let c = theme();
    let agent_row = matches!(item.kind, TaskKind::Subagent { .. });
    let page_row = p.on_open_item.is_some()
        && match &item.kind {
            TaskKind::Terminal { .. } | TaskKind::Preview { .. } | TaskKind::Browser { .. } | TaskKind::Shell { .. } | TaskKind::Plan { .. } => true,
            // A declined fork created nothing, so its row is a record with nowhere to go.
            TaskKind::Fork { decision } => decision.approved,
            _ => false,
        };
    let openable = agent_row || page_row;
    let selected = openable
        && match (&p.selected_row_id, &item.kind) {
            (Some(row), _) => &item.id == row,
            (None, TaskKind::Subagent { agent }) => p.selected_agent_id.as_deref() == Some(agent.id.as_str()),
            _ => false,
        };
    let stopping = stopping_ids.contains(&item.id);
    let running = item.state == TaskItemState::Running;
    let (label_color, label_ty, detail_color) = match item.state {
        TaskItemState::Running => (c.text_primary, LABEL_TY, c.text_tertiary),
        TaskItemState::Finished => (c.text_secondary, LABEL_DONE_TY, c.text_tertiary),
        TaskItemState::Failed => (c.critical, LABEL_TY, c.critical),
    };
    let icon_color = match item.state {
        TaskItemState::Running => c.accent_text,
        TaskItemState::Failed => c.critical,
        TaskItemState::Finished => c.text_tertiary,
    };
    let metric_color = match item.state {
        TaskItemState::Running => c.text_secondary,
        TaskItemState::Failed => c.critical,
        TaskItemState::Finished => c.text_tertiary,
    };
    let label: E = match &item.kind {
        // A shell's title is a path, so it gives way in the middle like every other path.
        TaskKind::Shell { shell } if shell_task_directory(shell).is_some() => {
            let directory = shell_task_directory(shell).expect("checked");
            let prefix = format!("{}:", shell.tool_name);
            let e = fitted_label(&prefix, Some(&directory), label_ty, label_color, running);
            if item.error.is_some() { e } else { div().min_w(0.0).tooltip(item.label.clone()).child(e) }
        }
        _ if running => fitted_label(&item.label, None, label_ty, label_color, true),
        _ => text(item.label.clone()).ty(label_ty).color(label_color).ellipsis(),
    };
    let mut main = row()
        .key(("task-row", item.kind_name(), item.id.as_str()))
        .items_center()
        .gap(7.0)
        .p(6.0)
        .radius(7.0)
        .when(selected, |e| e.bg(c.subtle_pressed))
        .when(openable, |e| e.hover_bg(c.subtle_hover))
        .tooltip(item.error.clone().unwrap_or_default())
        .child(task_item_icon(item, 13.0, icon_color))
        .child(col().flex1().gap(1.0).child(label).child(text(item.detail.clone()).ty(DETAIL_TY).color(detail_color).ellipsis()).child(item.error.clone().map(|error| text(error).ty(DETAIL_TY).color(c.critical).ellipsis())));
    if openable {
        let activate: Rc<dyn Fn(&mut App)> = {
            let (item, on_select, on_open) = (item.clone(), p.on_select_agent.clone(), p.on_open_item.clone());
            Rc::new(move |app: &mut App| match &item.kind {
                TaskKind::Subagent { agent } => on_select(app, &agent.id),
                _ => {
                    if let Some(open) = &on_open {
                        open(app, &item);
                    }
                }
            })
        };
        let on_key = activate.clone();
        main = main
            .role(Role::Button)
            .when(a11y_active(), |e| {
                let args: &[(&str, &dyn std::fmt::Display)] = &[("label", &item.label)];
                e.aria_label(if agent_row { tf("打开子代理 {label}", "Open subagent {label}", args) } else { tf("打开“{label}”", "Open “{label}”", args) })
            })
            .when(selected, |e| e.aria_current(AriaCurrent::True))
            .on_click(move |app| activate(app))
            .on_key(move |app, k| {
                if matches!(k.key, Key::Enter | Key::Char(' ')) {
                    on_key(app);
                    true
                } else {
                    false
                }
            });
    }
    let columns = task_metric_columns(item);
    if !columns.is_empty() {
        let mut metrics = row().items_center().gap(6.0).shrink0().aria_hidden(true);
        for (key, value, title) in columns {
            metrics = metrics.child(div().tooltip(title).child(rolling_number(("task-metric", item.id.as_str(), key), &value, METRIC_TY, metric_color)));
        }
        main = main.child(metrics);
    }
    // The plan is a document, not work in flight. A remote-control session that failed is kept by the host until it is
    // stopped, so its row keeps the control a finished row would not have.
    let stoppable = (running || (item.state == TaskItemState::Failed && matches!(item.kind, TaskKind::Preview { .. }))) && !matches!(item.kind, TaskKind::Plan { .. });
    if stoppable {
        let label = &item.label;
        let args: &[(&str, &dyn std::fmt::Display)] = &[("label", label)];
        let tip = if stopping {
            tf("正在中止“{label}”", "Stopping “{label}”", args)
        } else {
            match &item.kind {
                TaskKind::Preview { .. } => tf("停止“{label}”", "Stop “{label}”", args),
                TaskKind::Browser { automation_tool: None, .. } => tf("关闭“{label}”", "Close “{label}”", args),
                _ => tf("中止“{label}”", "Stop “{label}”", args),
            }
        };
        let mut stop = row().key(("task-stop", item.kind_name(), item.id.as_str())).center().size(22.0, 22.0).radius(5.0).shrink0().tooltip(tip);
        if stopping {
            // Pressing it does nothing, and does not fall through to the row underneath.
            stop = stop.on_click(|_| {}).disabled(true).child(spinner(11.0, c.text_secondary));
        } else {
            let (item, on_stop) = (item.clone(), p.on_stop_item.clone());
            stop = stop.hover_bg(c.critical_bg).child(stop_glyph(c.text_secondary)).on_click(move |app| on_stop(app, &item));
        }
        main = main.child(stop);
    }
    main
}

/// The conversation's running work as one list. Finished rows of every kind share one list under a single disclosure,
/// latest to finish first, so what just ended sits right under what is still running.
pub fn tasks_pane(app: &mut App, props: TasksPaneProps) -> E {
    let c = theme();
    let finished_open = use_state("tasks-pane-finished-open", || true);
    // Armed on running work, not on history: finished rows are frozen at a real duration.
    let clock_running = props.sources.shell_tasks.iter().any(|t| t.outcome.is_none()) || !props.sources.preview_servers.is_empty() || props.sources.agents.iter().any(|a| a.completed_at.is_none());
    let mut sources = props.sources.clone();
    sources.now = Some(sources.now.unwrap_or_else(now_ms));
    // A child's subtitle turns into the local helper model's title for its task when that arrives. Looked up here, where
    // the app is in reach, since a row derives without it.
    if sources.tool_explanation.is_none() {
        let titles: HashMap<String, String> = sources.agents.iter().flat_map(|agent| &agent.call_ids).filter_map(|call| app.tool_explanation(call, None, None).map(|title| (call.clone(), title))).collect();
        sources.tool_explanation = Some(Rc::new(move |call: &str| titles.get(call).cloned()));
    }
    let items = derive_task_items(&sources, &task_container_messages());
    let running = running_task_items(&items);
    let finished = finished_task_items(&items);

    let render_row = |item: &TaskItem| -> E {
        if let TaskKind::Workflow { .. } = item.kind {
            // A workflow is not a row: it draws its own panel, because that is what the run actually is.
            let on_stop_item = props.on_stop_item.clone();
            let stopped = item.clone();
            return workflow_run_panel(WorkflowRunPanelProps {
                view: derive_workflow_run(item, props.workflow_progress.get(&item.id)),
                selected_agent_id: props.selected_agent_id.clone(),
                stopping: props.stopping_ids.contains(&item.id),
                run_id: props.workflow_run_ids.get(&item.id).cloned(),
                on_open_agent: Some(props.on_select_agent.clone()),
                on_stop: Some(Rc::new(move |app| on_stop_item(app, &stopped))),
                on_step_control: props.on_workflow_step_control.clone(),
            })
            .key(("workflow-run", item.id.as_str()));
        }
        task_row(item, &props, &props.stopping_ids)
    };

    let mut pane = col().key("tasks-pane").flex1().scroll_y().pt(8.0).px(8.0).pb(16.0);
    if items.is_empty() {
        pane = pane.child(text(t("还没有任务", "No tasks yet")).ty(ts(11.5, 15.0, Weight::Regular)).color(c.text_tertiary).text_center().mx(12.0).my(24.0));
    }
    if !running.is_empty() {
        pane = pane.child(col().gap(2.0).children(running.iter().map(|i| render_row(i))));
    }
    if !finished.is_empty() {
        let open = finished_open.get();
        let toggle = finished_open.clone();
        let header = row()
            .key("tasks-finished-toggle")
            .items_center()
            .gap(6.0)
            .py(5.0)
            .px(8.0)
            .radius(6.0)
            .hover_bg(c.subtle_hover)
            .aria_expanded(open)
            .on_click(move |_| toggle.update(|v| *v = !*v))
            .child(icon("ChevronRight", 12.0).color(c.text_tertiary).rotate(if open { 90.0 } else { 0.0 }))
            .child(text(t("已完成", "Finished")).ty(FINE_STRONG).color(c.text_tertiary).nowrap())
            .child(text(finished.len().to_string()).ty(FINE_STRONG).color(c.text_tertiary).nowrap().ml_auto());
        let mut block = col().mt(10.0).child(header);
        if open {
            block = block.child(col().gap(2.0).children(finished.iter().map(|i| render_row(i))));
        }
        pane = pane.child(block);
    }
    // The clock sits outside the scroller: scrolled out of view it would not paint, and nothing would wake it.
    col().flex1().min_w(0.0).child(clock_running.then(|| ticker(1.0))).child(pane)
}

/// `focusWorkflowRunPanel`: brings one run's panel into view. The panel may not be on screen yet when the tasks pane was
/// closed, in which case this asks again on the next frame the caller draws.
pub fn focus_workflow_run_panel(run_id: &str) -> bool {
    let (Some(panel), Some(pane), Some((offset, _, _))) = (rect_of(("workflow-run", run_id)), rect_of("tasks-pane"), scroll_state("tasks-pane")) else { return false };
    scroll_request("tasks-pane", ScrollRequest::Reveal { y: panel.y - pane.y + offset, h: panel.h });
    true
}

pub use mewrk_client::side_panes::TasksPaneTab;

/// The label each tab goes by, which also names the pane while it is shown.
pub fn tasks_pane_tab_label(tab: TasksPaneTab) -> &'static str {
    match tab {
        TasksPaneTab::Tasks => t("任务", "Tasks"),
        TasksPaneTab::History => t("历史记录", "History"),
    }
}

/// The pane's title bar: its two pages as fixed tabs. Neither closes nor moves, so the strip offers selection only.
pub fn tasks_pane_tab_bar(active: TasksPaneTab, on_select: impl Fn(&mut App, TasksPaneTab) + 'static) -> E {
    let c = theme();
    let id = |t: TasksPaneTab| if t == TasksPaneTab::Tasks { "tasks" } else { "history" };
    let tabs = [(TasksPaneTab::Tasks, "ListChecks"), (TasksPaneTab::History, "History")]
        .into_iter()
        .map(|(tab, glyph)| PageTab { id: id(tab).to_owned(), label: tasks_pane_tab_label(tab).to_owned(), title: None, icon: Some(icon(glyph, 12.0).color(c.text_secondary)) })
        .collect();
    page_tabs("tasks-pane", tabs, Some(id(active)), Rc::new(move |app, selected| on_select(app, if selected == "tasks" { TasksPaneTab::Tasks } else { TasksPaneTab::History })), PageTabsOptions { aria_label: t("任务面板标签", "Tasks pane tabs"), more_label: t("更多页面", "More pages"), on_close: None, close_on_delete_key: false, on_reorder: None })
}

/// What a task row's stop control does (`stopTaskItem` in `App.tsx`): the row stays and shows a pending spinner until
/// the host's answer. Answers land in [`StoppingTasks`].
#[derive(Default)]
pub struct StoppingTasks {
    keys: Vec<(String, String)>,
}

impl StoppingTasks {
    pub fn for_conversation(&self, conversation_id: &str) -> Vec<String> {
        self.keys.iter().filter(|k| k.0 == conversation_id).map(|k| k.1.clone()).collect()
    }
}

/// The parts of a stop that belong to other areas. Each is optional because each is that area's state as well as a host
/// call: closing a page goes through the browser controller's intents, stopping automation also cancels the model run,
/// and closing a terminal also drops its tab. The shell's `actions::stop_item` fills them.
///
/// `close_browser_session` is `requestBrowserSessionClose`, `close_pages_served_at` is `closePagesServedAt`,
/// `stop_browser_automation` is `stopBrowserAutomation`, and `destroy_terminal` is `destroyTerminalSession`; without one this
/// calls the host's `close_terminal`.
#[derive(Clone, Default)]
pub struct StopHooks {
    pub close_browser_session: Option<Rc<dyn Fn(&mut App, &str)>>,
    pub stop_browser_automation: Option<Rc<dyn Fn(&mut App, &str)>>,
    pub close_pages_served_at: Option<Rc<dyn Fn(&mut App, &str, &PreviewServerSnapshot)>>,
    pub destroy_terminal: Option<Rc<dyn Fn(&mut App, &TerminalSessionState)>>,
}

/// `stopTaskItem`. A task-level stop never escalates to cancelling the whole run: a row the host cannot address just
/// fails to stop. Returns without acting when the row has no conversation or a stop is already pending.
pub fn stop_task_item(app: &mut App, item: &TaskItem, hooks: &StopHooks) {
    let Some(conversation_id) = item.conversation_id.clone() else { return };
    let key = (conversation_id.clone(), item.id.clone());
    if app.local::<StoppingTasks>().keys.contains(&key) {
        return;
    }
    let (command, args): (&str, Value) = match &item.kind {
        TaskKind::Browser { session_id, automation_tool: None, .. } => {
            if let Some(close) = &hooks.close_browser_session {
                close(app, session_id);
            }
            return;
        }
        TaskKind::Browser { .. } => {
            if let Some(stop) = &hooks.stop_browser_automation {
                stop(app, &conversation_id);
            }
            return;
        }
        TaskKind::Preview { server, .. } => {
            // The host's own change event closes the page too, but not before this row has gone: closing here is what
            // makes one click read as one action.
            if let Some(close_pages) = &hooks.close_pages_served_at {
                close_pages(app, &conversation_id, server);
            }
            ("preview_stop_server", json!({ "handle": server.handle }))
        }
        TaskKind::Terminal { terminal } => {
            if let Some(destroy) = &hooks.destroy_terminal {
                destroy(app, terminal);
                return;
            }
            ("close_terminal", json!({ "conversationId": terminal.conversation_id, "terminalId": terminal.terminal_id }))
        }
        TaskKind::Shell { shell } => ("stop_shell_task", json!({ "conversationId": shell.conversation_id, "shellTaskId": shell.shell_task_id })),
        TaskKind::Subagent { agent } | TaskKind::Workflow { agent, .. } => {
            // A miss means the host has no live entry for this row: report nothing and leave the run alone.
            let address = agent.name.as_deref().filter(|n| !n.is_empty()).unwrap_or(&agent.label);
            if address.is_empty() {
                return;
            }
            ("stop_conversation_task", json!({ "conversationId": conversation_id, "task": address }))
        }
        _ => return,
    };
    app.local::<StoppingTasks>().keys.push(key.clone());
    app.invoke::<Value>(command, args, move |app, result| {
        if let Err(e) = result {
            eprintln!("Failed to stop task: {e}");
        }
        app.local::<StoppingTasks>().keys.retain(|k| *k != key);
        repaint();
    });
}

/// The sources this crate can fill on its own for one conversation; the rest are the shell's to add.
pub fn shell_task_sources(app: &mut App, conversation_id: &str) -> Vec<ShellTaskSnapshot> {
    shell_tasks::install(app);
    app.local::<shell_tasks::ShellTaskStore>().for_conversation(conversation_id)
}

/// `workflowProgressByRun` in `App.tsx`: the live ledgers keyed by the run's view id, for `TasksPaneProps::workflow_progress`.
pub fn workflow_progress_by_run(agents: &[Rc<SubagentView>], entries_by_call: &std::collections::BTreeMap<String, Vec<mewrk_client::runs::WorkflowProgressEntry>>) -> HashMap<String, WorkflowProgressView> {
    let messages = workflow_progress_messages();
    let mut by_run = HashMap::new();
    for agent in agents {
        for call_id in &agent.call_ids {
            if let Some(entries) = entries_by_call.get(call_id) {
                let rows: Vec<WorkflowProgressEntry> = entries.iter().map(WorkflowProgressEntry::from).collect();
                by_run.insert(agent.id.clone(), derive_workflow_progress(&rows, &messages));
            }
        }
    }
    by_run
}

/// `workflowRunIdsByRun` in `App.tsx`, for `TasksPaneProps::workflow_run_ids`.
pub fn workflow_run_ids_by_run(agents: &[Rc<SubagentView>], run_ids_by_call: &std::collections::BTreeMap<String, String>) -> HashMap<String, String> {
    let mut by_run = HashMap::new();
    for agent in agents {
        for call_id in &agent.call_ids {
            if let Some(run_id) = run_ids_by_call.get(call_id) {
                by_run.insert(agent.id.clone(), run_id.clone());
            }
        }
    }
    by_run
}
