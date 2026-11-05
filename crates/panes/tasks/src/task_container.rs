//! `lib/taskContainer.ts`: everything a conversation has running, projected into one list of task rows.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use mewrk_client::model::{ConversationPlan, ModelUsage, PlanStatus, UserAbortedTaskRecord};
pub use mewrk_client::runs::ForkDecisionRecord;

use crate::context::SubagentViewStatus;
use crate::shell_tasks::{ShellTaskOutcome, ShellTaskSnapshot};
use crate::sources::{preview_page_is_served_by, preview_server_address, remote_control_detail, BrowserStatus, PreviewServerSnapshot, PreviewServerStatus, TerminalPhase, TerminalSessionState};
use crate::subagents::SubagentView;
use crate::util::{collapse_spaces, js_trim, now_ms, parse_time, utf16_len, utf16_prefix};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskItemState {
    Running,
    Finished,
    Failed,
}

pub fn task_state_for_status(status: SubagentViewStatus) -> TaskItemState {
    match status {
        SubagentViewStatus::Running => TaskItemState::Running,
        // A run that reached a ceiling still returned usable output.
        SubagentViewStatus::Completed | SubagentViewStatus::RoundLimit => TaskItemState::Finished,
        _ => TaskItemState::Failed,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorkflowPhaseGroup {
    pub phase: Option<String>,
    pub phase_index: i64,
    pub steps: Vec<Rc<SubagentView>>,
}

/// A kind that cannot report a column leaves it `None`: "—" is a different statement than "0".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TaskMetrics {
    pub child_count: Option<i64>,
    pub tokens: Option<i64>,
    pub tool_count: Option<i64>,
    pub elapsed_ms: Option<i64>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TaskKind {
    Subagent { agent: Rc<SubagentView> },
    Workflow { agent: Rc<SubagentView>, steps: Vec<Rc<SubagentView>>, phases: Vec<WorkflowPhaseGroup> },
    Terminal { terminal: TerminalSessionState },
    Shell { shell: ShellTaskSnapshot },
    Preview { server: PreviewServerSnapshot, address: String },
    Browser { browser: BrowserStatus, session_id: String, automation_tool: Option<String> },
    Plan { plan: ConversationPlan },
    Fork { decision: ForkDecisionRecord },
    Aborted { source_kind: String, source_identity: String },
}

#[derive(Clone, Debug, PartialEq)]
pub struct TaskItem {
    /// Source identity captured when the row is derived, never the active selection.
    pub conversation_id: Option<String>,
    pub id: String,
    pub label: String,
    pub detail: String,
    pub state: TaskItemState,
    pub metrics: TaskMetrics,
    pub children: Vec<TaskItem>,
    /// Only ever set on a failed row: a finished row with a stale error would claim a failure that did not happen.
    pub error: Option<String>,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub kind: TaskKind,
}

impl TaskItem {
    pub fn kind_name(&self) -> &'static str {
        match self.kind {
            TaskKind::Subagent { .. } => "subagent",
            TaskKind::Workflow { .. } => "workflow",
            TaskKind::Terminal { .. } => "terminal",
            TaskKind::Shell { .. } => "shell",
            TaskKind::Preview { .. } => "preview",
            TaskKind::Browser { .. } => "browser",
            TaskKind::Plan { .. } => "plan",
            TaskKind::Fork { .. } => "fork",
            TaskKind::Aborted { .. } => "aborted",
        }
    }
}

pub struct TaskContainerMessages {
    pub workflow_label: String,
    pub running_step_count: Box<dyn Fn(usize, usize) -> String>,
    pub step_count: Box<dyn Fn(usize) -> String>,
    pub terminal_idle: String,
    pub terminal_busy: String,
    pub shell_running: String,
    pub shell_exited: Box<dyn Fn(i32) -> String>,
    pub shell_failed: String,
    pub preview_label: String,
    pub preview_starting: String,
    pub preview_running: String,
    pub preview_failed: String,
    pub browser_label: String,
    pub browser_loading: String,
    pub browser_suspended: String,
    pub browser_idle: String,
    pub browser_automation: Box<dyn Fn(&str) -> String>,
    pub plan_label: String,
    pub plan_drafting: String,
    pub plan_awaiting_approval: String,
    pub plan_approved: String,
    pub plan_rejected: String,
    pub plan_updated_ago: Box<dyn Fn(i64) -> String>,
    pub fork_approved: String,
    pub fork_declined: String,
    pub user_aborted: String,
}

/// Localized task-row copy (`taskContainerMessages(t)`), shared with the message stream so a run reads the same on both.
pub fn task_container_messages() -> TaskContainerMessages {
    use mewrk_client::{t, tf};
    TaskContainerMessages {
        workflow_label: t("工作流", "Workflow").into(),
        running_step_count: Box::new(|running, total| tf("{running}/{total} 个步骤进行中", "{running}/{total} steps running", &[("running", &running), ("total", &total)])),
        step_count: Box::new(|total| tf("{total} 个步骤", "{total} steps", &[("total", &total)])),
        terminal_idle: t("空闲", "Idle").into(),
        terminal_busy: t("正在执行命令", "Running a command").into(),
        shell_running: t("正在运行", "Running").into(),
        shell_exited: Box::new(|code| tf("已失败（退出码 {code}）", "Failed (exit {code})", &[("code", &code)])),
        shell_failed: t("已失败", "Failed").into(),
        preview_label: t("开发服务器", "Dev server").into(),
        preview_starting: t("启动中", "Starting").into(),
        preview_running: t("运行中", "Running").into(),
        preview_failed: t("连接失败", "Failed").into(),
        browser_label: t("预览页面", "Preview page").into(),
        browser_loading: t("正在加载", "Loading").into(),
        browser_suspended: t("已挂起", "Suspended").into(),
        browser_idle: t("已就绪", "Ready").into(),
        browser_automation: Box::new(|tool| tf("模型正在操作：{tool}", "Model is driving: {tool}", &[("tool", &tool)])),
        plan_label: t("实施计划", "Implementation plan").into(),
        plan_drafting: t("撰写中", "Drafting").into(),
        plan_awaiting_approval: t("待批准", "Awaiting approval").into(),
        plan_approved: t("已批准", "Approved").into(),
        plan_rejected: t("已退回", "Sent back").into(),
        plan_updated_ago: Box::new(|minutes| {
            if minutes < 1 {
                return t("刚刚更新", "Updated just now").to_owned();
            }
            if minutes < 60 {
                return tf("{count} 分钟前更新", if minutes == 1 { "Updated {count} minute ago" } else { "Updated {count} minutes ago" }, &[("count", &minutes)]);
            }
            if minutes < 1440 {
                let hours = (minutes as f64 / 60.0).round() as i64;
                return tf("{count} 小时前更新", if hours == 1 { "Updated {count} hour ago" } else { "Updated {count} hours ago" }, &[("count", &hours)]);
            }
            let days = (minutes as f64 / 1440.0).round() as i64;
            tf("{count} 天前更新", if days == 1 { "Updated {count} day ago" } else { "Updated {count} days ago" }, &[("count", &days)])
        }),
        fork_approved: t("已创建子对话 · 点击打开", "Child conversation created · open").into(),
        fork_declined: t("用户拒绝了分叉", "The user declined the fork").into(),
        user_aborted: t("用户中止操作", "Operation aborted by user").into(),
    }
}

const NO_METRICS: TaskMetrics = TaskMetrics { child_count: None, tokens: None, tool_count: None, elapsed_ms: None };
/// Unnamed phases sort after every named one (`Number.MAX_SAFE_INTEGER`).
const UNPHASED: i64 = 9_007_199_254_740_991;

fn group_steps_by_phase(steps: &[Rc<SubagentView>]) -> Vec<WorkflowPhaseGroup> {
    let mut groups: Vec<WorkflowPhaseGroup> = Vec::new();
    for step in steps {
        if let Some(existing) = groups.iter_mut().find(|g| g.phase == step.phase) {
            existing.steps.push(step.clone());
            if let Some(index) = step.phase_index {
                existing.phase_index = existing.phase_index.min(index);
            }
        } else {
            groups.push(WorkflowPhaseGroup { phase: step.phase.clone(), phase_index: step.phase_index.unwrap_or(UNPHASED), steps: vec![step.clone()] });
        }
    }
    groups.sort_by_key(|g| g.phase_index);
    groups
}

fn workflow_state(agent: &SubagentView, steps: &[Rc<SubagentView>]) -> TaskItemState {
    let own = task_state_for_status(agent.status);
    if own == TaskItemState::Running {
        return own;
    }
    if steps.iter().any(|s| task_state_for_status(s.status) == TaskItemState::Failed) {
        return TaskItemState::Failed;
    }
    own
}

fn elapsed_ms(started_at: &str, ended_at: Option<&str>, now: i64) -> Option<i64> {
    let start = parse_time(started_at)?;
    let end = match ended_at.filter(|e| !e.is_empty()) {
        Some(e) => parse_time(e)?,
        None => now,
    };
    // An end before the start is a clock artifact, not a negative duration.
    Some((end - start).max(0))
}

/// Cached input is excluded from the fallback because it is already inside `input_tokens`.
pub fn token_total(usage: &ModelUsage) -> Option<i64> {
    if let Some(total) = usage.total_tokens {
        return Some(total as i64);
    }
    match (usage.input_tokens, usage.output_tokens) {
        (None, None) => None,
        (a, b) => Some((a.unwrap_or(0) + b.unwrap_or(0)) as i64),
    }
}

fn agent_error(agent: &SubagentView, state: TaskItemState) -> Option<String> {
    (state == TaskItemState::Failed && !agent.summary.is_empty()).then(|| agent.summary.clone())
}

fn agent_metrics(agent: &SubagentView, child_count: i64, now: i64) -> TaskMetrics {
    TaskMetrics { child_count: Some(child_count), tokens: token_total(&agent.usage), tool_count: agent.tool_count.map(|c| c as i64), elapsed_ms: elapsed_ms(&agent.created_at, agent.completed_at.as_deref(), now) }
}

pub type ToolExplanationLookup = Rc<dyn Fn(&str) -> Option<String>>;

struct AgentCtx<'a> {
    by_id: &'a HashMap<&'a str, Rc<SubagentView>>,
    messages: &'a TaskContainerMessages,
    now: i64,
    inherited_model_id: Option<&'a str>,
    tool_explanation: Option<&'a ToolExplanationLookup>,
}

fn subagent_item(agent: &Rc<SubagentView>, c: &AgentCtx<'_>) -> TaskItem {
    let state = task_state_for_status(agent.status);
    let display_name = agent.name.as_deref().filter(|n| !n.is_empty()).unwrap_or(&agent.label);
    let title = generated_task_title(agent, c.tool_explanation);
    let preview = prompt_preview(&agent.task);
    let detail = [Some(title.as_str()), Some(preview.as_str()), agent.model_id.as_deref(), c.inherited_model_id].into_iter().flatten().find(|s| !s.is_empty()).unwrap_or("").to_owned();
    TaskItem {
        conversation_id: None,
        id: agent.id.clone(),
        // `role:name`: who is doing it, then what it is called.
        label: role_title(agent.role.as_ref().map(|r| r.name.as_str()), display_name),
        detail,
        state,
        metrics: agent_metrics(agent, 0, c.now),
        children: Vec::new(),
        error: agent_error(agent, state),
        started_at: agent.created_at.clone(),
        ended_at: agent.completed_at.clone(),
        kind: TaskKind::Subagent { agent: agent.clone() },
    }
}

fn workflow_item(agent: &Rc<SubagentView>, c: &AgentCtx<'_>) -> TaskItem {
    let steps: Vec<Rc<SubagentView>> = agent
        .child_ids
        .iter()
        .filter_map(|id| c.by_id.get(id.as_str()))
        .filter(|child| child.kind == mewrk_client::model::SubagentRunKind::WorkflowStep)
        .cloned()
        .collect();
    let running = steps.iter().filter(|s| task_state_for_status(s.status) == TaskItemState::Running).count();
    let state = workflow_state(agent, &steps);
    let name = agent.name.as_deref().filter(|n| !n.is_empty()).or(Some(agent.label.as_str()).filter(|l| !l.is_empty()));
    TaskItem {
        conversation_id: None,
        id: agent.id.clone(),
        label: name.unwrap_or(&c.messages.workflow_label).to_owned(),
        detail: agent.script_name.clone().unwrap_or_else(|| if running > 0 { (c.messages.running_step_count)(running, steps.len()) } else { (c.messages.step_count)(steps.len()) }),
        state,
        metrics: agent_metrics(agent, steps.len() as i64, c.now),
        children: steps.iter().map(|s| task_item_for_agent(s, c)).collect(),
        error: agent_error(agent, state),
        started_at: agent.created_at.clone(),
        ended_at: agent.completed_at.clone(),
        kind: TaskKind::Workflow { agent: agent.clone(), phases: group_steps_by_phase(&steps), steps },
    }
}

fn task_item_for_agent(agent: &Rc<SubagentView>, c: &AgentCtx<'_>) -> TaskItem {
    if agent.workflow_run { workflow_item(agent, c) } else { subagent_item(agent, c) }
}

/// Just the workflow runs, for a caller that renders them outside the task tree. The message stream goes through this
/// so it cannot disagree with the panel about a run's steps.
pub fn derive_workflow_items(agents: &[Rc<SubagentView>], messages: &TaskContainerMessages, now: i64, inherited_model_id: Option<&str>) -> Vec<TaskItem> {
    let by_id: HashMap<&str, Rc<SubagentView>> = agents.iter().map(|a| (a.id.as_str(), a.clone())).collect();
    let c = AgentCtx { by_id: &by_id, messages, now, inherited_model_id, tool_explanation: None };
    agents.iter().filter(|a| a.depth == 0 && a.workflow_run).map(|a| workflow_item(a, &c)).collect()
}

fn preview_server_item(server: &PreviewServerSnapshot, m: &TaskContainerMessages, now: i64) -> TaskItem {
    let remote = server.remote.as_ref();
    let failed = remote.is_some() && server.status == PreviewServerStatus::Failed;
    let address = match remote {
        Some(remote) => remote_control_detail(None, Some(remote)).unwrap_or_else(|| server.name.clone()),
        None => preview_server_address(server.port, server.url.as_deref()),
    };
    let status = if failed {
        &m.preview_failed
    } else if server.status == PreviewServerStatus::Starting {
        &m.preview_starting
    } else {
        &m.preview_running
    };
    let machine = server.machine.as_deref().filter(|s| !s.is_empty()).map(|s| format!(" · {s}")).unwrap_or_default();
    TaskItem {
        conversation_id: None,
        id: format!("preview-server:{}", server.handle),
        label: server.name.clone(),
        detail: format!("{status} · {address}{machine}"),
        state: if failed { TaskItemState::Failed } else { TaskItemState::Running },
        metrics: TaskMetrics { elapsed_ms: elapsed_ms(&server.started_at, None, now), ..NO_METRICS },
        children: Vec::new(),
        error: if failed { remote.and_then(|r| r.message.as_deref()).map(js_trim).filter(|s| !s.is_empty()).map(str::to_owned) } else { None },
        started_at: server.started_at.clone(),
        ended_at: None,
        kind: TaskKind::Preview { server: server.clone(), address },
    }
}

/// `hasPage` says only that a native surface exists: opening the pane mints one at `about:blank`. Blank is the browser
/// at rest; anything that gives it a history of its own brings the row back.
fn page_holds_document(page: &BrowserStatus) -> bool {
    if page.loading || page.error.as_deref().is_some_and(|e| !e.is_empty()) {
        return true;
    }
    if page.can_go_back || page.can_go_forward {
        return true;
    }
    !page.url.is_empty() && !page.url.eq_ignore_ascii_case("about:blank")
}

fn browser_item(browser: &BrowserStatus, session_id: &str, automation_tool: Option<&str>, m: &TaskContainerMessages) -> Option<TaskItem> {
    // The host reports a sleeping page as having no live surface, and every tab not in front sleeps, so `suspended` has
    // to keep its row on its own.
    if !browser.has_page && !browser.suspended {
        return None;
    }
    if automation_tool.is_none_or(str::is_empty) && !page_holds_document(browser) {
        return None;
    }
    let automation_tool = automation_tool.filter(|t| !t.is_empty());
    let detail = match automation_tool {
        Some(tool) => (m.browser_automation)(tool),
        None if browser.suspended => m.browser_suspended.clone(),
        None if browser.loading => m.browser_loading.clone(),
        None => m.browser_idle.clone(),
    };
    let error = browser.error.clone().filter(|e| !e.is_empty());
    let title = browser.title.as_deref().map(js_trim).filter(|t| !t.is_empty());
    Some(TaskItem {
        conversation_id: None,
        // One row per native session.
        id: format!("preview:{session_id}"),
        label: title.or(Some(browser.url.as_str()).filter(|u| !u.is_empty())).unwrap_or(&m.browser_label).to_owned(),
        detail,
        // Running even when idle: it holds a live process until someone closes it, and this row is the only way to.
        state: if error.is_some() { TaskItemState::Failed } else { TaskItemState::Running },
        metrics: NO_METRICS,
        children: Vec::new(),
        error,
        started_at: String::new(),
        ended_at: None,
        kind: TaskKind::Browser { browser: browser.clone(), session_id: session_id.to_owned(), automation_tool: automation_tool.map(str::to_owned) },
    })
}

/// `role:title`. A run that named no role is titled by its own name alone.
pub fn role_title(role: Option<&str>, title: &str) -> String {
    match role.map(js_trim).filter(|r| !r.is_empty()) {
        Some(name) => format!("{name}:{title}"),
        None => title.to_owned(),
    }
}

/// The title the local helper model made of a child's task, stored beside the `agent_spawn` card that started it.
pub fn generated_task_title(agent: &SubagentView, lookup: Option<&ToolExplanationLookup>) -> String {
    let Some(lookup) = lookup else { return String::new() };
    for call_id in &agent.call_ids {
        if let Some(title) = lookup(call_id) {
            let title = js_trim(&title);
            if !title.is_empty() {
                return title.to_owned();
            }
        }
    }
    String::new()
}

const PROMPT_PREVIEW_LENGTH: usize = 240;

/// The prompt an agent was given, as one line for a subtitle.
pub fn prompt_preview(text: &str) -> String {
    let line = collapse_spaces(text);
    if utf16_len(&line) > PROMPT_PREVIEW_LENGTH { format!("{}…", utf16_prefix(&line, PROMPT_PREVIEW_LENGTH)) } else { line }
}

/// Where a shell command ran, or its workspace's root on a row recorded before the host kept that.
pub fn shell_task_directory(shell: &ShellTaskSnapshot) -> Option<String> {
    [shell.cwd.as_deref(), shell.workspace_root.as_deref()].into_iter().flatten().map(js_trim).find(|s| !s.is_empty()).map(str::to_owned)
}

/// What a shell command's row and page are called: `zsh:/Users/me/project`.
pub fn shell_task_title(shell: &ShellTaskSnapshot) -> String {
    match shell_task_directory(shell) {
        Some(directory) => format!("{}:{directory}", shell.tool_name),
        None => shell.tool_name.clone(),
    }
}

fn shell_elapsed_ms(shell: &ShellTaskSnapshot, now: i64) -> Option<i64> {
    if shell.outcome.is_some() {
        if let Some(duration) = shell.duration_ms {
            return Some(duration as i64);
        }
        // No end on record means no duration to freeze at.
        if shell.ended_at.as_deref().is_none_or(str::is_empty) {
            return None;
        }
    }
    elapsed_ms(&shell.started_at, shell.ended_at.as_deref(), now)
}

fn shell_item(shell: &ShellTaskSnapshot, m: &TaskContainerMessages, now: i64) -> TaskItem {
    let failed = shell.outcome == Some(ShellTaskOutcome::Failed);
    let explanation = shell.explanation.as_deref().map(js_trim).filter(|e| !e.is_empty());
    let command = Some(shell.command.as_str()).filter(|c| !c.is_empty());
    TaskItem {
        conversation_id: None,
        id: shell.shell_task_id.clone(),
        label: shell_task_title(shell),
        detail: explanation.or(command).unwrap_or(&m.shell_running).to_owned(),
        // A stopped process stays neutral unless the taskbar persisted an abort record for it.
        state: match shell.outcome {
            None => TaskItemState::Running,
            Some(ShellTaskOutcome::Failed) => TaskItemState::Failed,
            Some(_) => TaskItemState::Finished,
        },
        metrics: TaskMetrics { elapsed_ms: shell_elapsed_ms(shell, now), ..NO_METRICS },
        children: Vec::new(),
        error: failed.then(|| shell.exit_code.map_or_else(|| m.shell_failed.clone(), |code| (m.shell_exited)(code))),
        started_at: shell.started_at.clone(),
        ended_at: shell.ended_at.clone(),
        kind: TaskKind::Shell { shell: shell.clone() },
    }
}

fn task_item_source_identity(item: &TaskItem, model_request_id: Option<&str>) -> String {
    match &item.kind {
        TaskKind::Aborted { source_identity, .. } => source_identity.clone(),
        TaskKind::Subagent { agent } | TaskKind::Workflow { agent, .. } => format!("agent:{}:{}", agent.call_ids.last().unwrap_or(&agent.id), agent.created_at),
        TaskKind::Terminal { terminal } => format!("terminal:{}", terminal.terminal_id),
        TaskKind::Shell { shell } => format!("shell:{}", shell.shell_task_id),
        TaskKind::Preview { server, .. } => format!("preview-server:{}", server.handle),
        TaskKind::Browser { session_id, automation_tool, .. } => match automation_tool {
            Some(_) => format!("browser-automation:{}:{session_id}", model_request_id.unwrap_or("unknown")),
            None => format!("browser-page:{session_id}"),
        },
        TaskKind::Plan { .. } => "plan".to_owned(),
        TaskKind::Fork { decision } => format!("fork:{}", decision.fork_id),
    }
}

fn aborted_task_item(record: &UserAbortedTaskRecord, m: &TaskContainerMessages) -> TaskItem {
    let span = parse_time(&record.ended_at).and_then(|end| elapsed_ms(&record.started_at, Some(&record.ended_at), end));
    TaskItem {
        conversation_id: None,
        id: format!("aborted:{}", record.id),
        label: record.label.clone(),
        detail: if record.detail.is_empty() { m.user_aborted.clone() } else { format!("{} · {}", m.user_aborted, record.detail) },
        state: TaskItemState::Failed,
        metrics: TaskMetrics {
            child_count: record.metrics.child_count.map(|v| v as i64),
            tokens: record.metrics.tokens.map(|v| v as i64),
            tool_count: record.metrics.tool_count.map(|v| v as i64),
            elapsed_ms: span.or(record.metrics.elapsed_ms.map(|v| v as i64)),
        },
        children: Vec::new(),
        error: Some(m.user_aborted.clone()),
        started_at: record.started_at.clone(),
        ended_at: Some(record.ended_at.clone()),
        kind: TaskKind::Aborted { source_kind: record.source_kind.clone(), source_identity: record.source_identity.clone() },
    }
}

fn plan_item(plan: &ConversationPlan, awaiting: bool, drafting: bool, m: &TaskContainerMessages, now: i64) -> TaskItem {
    let status = if awaiting {
        &m.plan_awaiting_approval
    } else {
        match plan.status {
            PlanStatus::Approved => &m.plan_approved,
            PlanStatus::Rejected => &m.plan_rejected,
            _ => &m.plan_drafting,
        }
    };
    let minutes = parse_time(&plan.updated_at).map_or(0, |updated| (((now - updated) as f64 / 60_000.0).round() as i64).max(0));
    TaskItem {
        conversation_id: None,
        id: "plan".to_owned(),
        label: m.plan_label.clone(),
        detail: format!("{status} · {}", (m.plan_updated_ago)(minutes)),
        state: if awaiting || drafting { TaskItemState::Running } else { TaskItemState::Finished },
        metrics: NO_METRICS,
        children: Vec::new(),
        error: None,
        started_at: plan.created_at.clone(),
        ended_at: None,
        kind: TaskKind::Plan { plan: plan.clone() },
    }
}

fn fork_item(decision: &ForkDecisionRecord, m: &TaskContainerMessages) -> TaskItem {
    let label = if decision.title.is_empty() { decision.prompt.split('\n').next().unwrap_or("").to_owned() } else { decision.title.clone() };
    TaskItem {
        conversation_id: None,
        id: format!("fork:{}", decision.fork_id),
        label,
        detail: if decision.approved { m.fork_approved.clone() } else { m.fork_declined.clone() },
        state: TaskItemState::Finished,
        metrics: NO_METRICS,
        children: Vec::new(),
        error: None,
        started_at: decision.requested_at.clone(),
        ended_at: Some(decision.decided_at.clone()),
        kind: TaskKind::Fork { decision: decision.clone() },
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct BrowserSession {
    pub session_id: String,
    pub status: BrowserStatus,
}

#[derive(Clone, Default)]
pub struct TaskSources {
    pub conversation_id: Option<String>,
    pub agents: Vec<Rc<SubagentView>>,
    pub terminals: Vec<TerminalSessionState>,
    pub shell_tasks: Vec<ShellTaskSnapshot>,
    pub browser: Option<BrowserStatus>,
    pub preview_servers: Vec<PreviewServerSnapshot>,
    pub browser_session_id: Option<String>,
    /// One row per live session with a document. When supplied it replaces the `browser` pair.
    pub browser_sessions: Option<Vec<BrowserSession>>,
    pub browser_automation_tool: Option<String>,
    pub browser_automation_stopping: bool,
    pub model_request_id: Option<String>,
    pub user_aborted_tasks: Vec<UserAbortedTaskRecord>,
    pub plan: Option<ConversationPlan>,
    pub plan_awaiting_approval: bool,
    pub plan_drafting: bool,
    pub fork_decisions: Vec<ForkDecisionRecord>,
    pub inherited_model_id: Option<String>,
    pub tool_explanation: Option<ToolExplanationLookup>,
    /// The clock for the elapsed column; `None` reads the real one.
    pub now: Option<i64>,
}

/// Stand-in for a page whose automation stop is in flight after its last tool left the stream. Message text only.
const STOPPING_AUTOMATION_TOOL: &str = "browser";

/// Only depth-0 agents become top-level rows; their descendants are `children`, so the sidebar renders one tree.
pub fn derive_task_items(sources: &TaskSources, messages: &TaskContainerMessages) -> Vec<TaskItem> {
    let now = sources.now.unwrap_or_else(now_ms);
    let by_id: HashMap<&str, Rc<SubagentView>> = sources.agents.iter().map(|a| (a.id.as_str(), a.clone())).collect();
    let c = AgentCtx { by_id: &by_id, messages, now, inherited_model_id: sources.inherited_model_id.as_deref(), tool_explanation: sources.tool_explanation.as_ref() };
    let mut items: Vec<TaskItem> = Vec::new();
    // A stop that has been asked for but not yet observed is still automation: the row has to stay running to show
    // its pending spinner.
    let automation = if sources.browser_automation_stopping { Some(sources.browser_automation_tool.as_deref().unwrap_or(STOPPING_AUTOMATION_TOOL)) } else { sources.browser_automation_tool.as_deref() };

    items.extend(sources.agents.iter().filter(|a| a.depth == 0).map(|a| task_item_for_agent(a, &c)));
    items.extend(sources.terminals.iter().map(|terminal| TaskItem {
        conversation_id: None,
        id: terminal.terminal_id.clone(),
        label: terminal.label.clone(),
        detail: if terminal.busy { messages.terminal_busy.clone() } else { messages.terminal_idle.clone() },
        state: if terminal.phase == TerminalPhase::Running { TaskItemState::Running } else { TaskItemState::Finished },
        metrics: NO_METRICS,
        children: Vec::new(),
        error: None,
        started_at: String::new(),
        ended_at: None,
        kind: TaskKind::Terminal { terminal: terminal.clone() },
    }));
    items.extend(sources.shell_tasks.iter().map(|s| shell_item(s, messages, now)));
    items.extend(sources.preview_servers.iter().map(|s| preview_server_item(s, messages, now)));

    // Only the primary session is the surface Agent tools drive, so automation is reported against it alone.
    let single;
    let sessions: &[BrowserSession] = match (&sources.browser_sessions, &sources.browser, &sources.browser_session_id) {
        (Some(all), _, _) => all,
        (None, Some(status), Some(id)) => {
            single = [BrowserSession { session_id: id.clone(), status: status.clone() }];
            &single
        }
        _ => &[],
    };
    for session in sessions {
        // A page a server row accounts for gets no row of its own: two stop controls over one resource is how a page
        // outlives the process it was serving.
        if sources.preview_servers.iter().any(|s| preview_page_is_served_by(Some(&session.status.url), s)) {
            continue;
        }
        let primary = sources.browser_session_id.as_deref().is_none_or(|id| id == session.session_id);
        if let Some(page) = browser_item(&session.status, &session.session_id, if primary { automation } else { None }, messages) {
            items.push(page);
        }
    }

    if let Some(plan) = &sources.plan {
        items.push(plan_item(plan, sources.plan_awaiting_approval, sources.plan_drafting, messages, now));
    }
    let mut forks: Vec<&ForkDecisionRecord> = sources.fork_decisions.iter().collect();
    forks.sort_by(|l, r| l.decided_at.cmp(&r.decided_at));
    items.extend(forks.into_iter().map(|d| fork_item(d, messages)));

    let aborted_by_source: HashMap<&str, &UserAbortedTaskRecord> = sources.user_aborted_tasks.iter().map(|r| (r.source_identity.as_str(), r)).collect();
    let model_request_id = sources.model_request_id.as_deref();
    let live_sources: HashSet<String> = items.iter().map(|i| task_item_source_identity(i, model_request_id)).collect();
    let mut merged: Vec<TaskItem> = items
        .into_iter()
        .map(|item| {
            let identity = task_item_source_identity(&item, model_request_id);
            // A preview is a state, not a task with a history: an old record must not resurrect a dead page.
            if identity.starts_with("browser-page:") {
                return item;
            }
            match aborted_by_source.get(identity.as_str()) {
                Some(record) => aborted_task_item(record, messages),
                None => item,
            }
        })
        .collect();
    for record in &sources.user_aborted_tasks {
        if !record.source_identity.starts_with("browser-page:") && !live_sources.contains(&record.source_identity) {
            merged.push(aborted_task_item(record, messages));
        }
    }
    fn bind(item: &mut TaskItem, conversation: &Option<String>) {
        item.conversation_id = conversation.clone();
        for child in &mut item.children {
            bind(child, conversation);
        }
    }
    for item in &mut merged {
        bind(item, &sources.conversation_id);
    }
    merged
}

fn finished_at(item: &TaskItem) -> i64 {
    item.ended_at.as_deref().and_then(parse_time).or_else(|| parse_time(&item.started_at)).unwrap_or(i64::MIN)
}

/// Rows under the "finish" disclosure: every kind in one list, latest to finish first. The plan is never history.
pub fn finished_task_items(items: &[TaskItem]) -> Vec<&TaskItem> {
    let mut rows: Vec<(usize, i64, &TaskItem)> = items.iter().filter(|i| i.state != TaskItemState::Running && !matches!(i.kind, TaskKind::Plan { .. })).enumerate().map(|(index, item)| (index, finished_at(item), item)).collect();
    rows.sort_by(|l, r| r.1.cmp(&l.1).then(l.0.cmp(&r.0)));
    rows.into_iter().map(|r| r.2).collect()
}

pub fn running_task_items(items: &[TaskItem]) -> Vec<&TaskItem> {
    items.iter().filter(|i| i.state == TaskItemState::Running || matches!(i.kind, TaskKind::Plan { .. })).collect()
}

/// How many rows of the running list are work still going on. The plan is a document, not a process.
pub fn count_running_tasks(items: &[TaskItem]) -> usize {
    items.iter().filter(|i| i.state == TaskItemState::Running && !matches!(i.kind, TaskKind::Plan { .. })).count()
}

/// Every row of the tree, parents before their own children.
pub fn flatten_task_items(items: &[TaskItem]) -> Vec<&TaskItem> {
    items.iter().flat_map(|i| std::iter::once(i).chain(flatten_task_items(&i.children))).collect()
}
