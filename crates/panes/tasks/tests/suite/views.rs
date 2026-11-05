//! Upstream's component tests (`TasksPane`, `WorkflowRunPanel`, `SubagentPanel`, `ForkRequestTray`, `HistoryPane`,
//! `UsageStatsCard`), scripted against real views on a real `App`: what is on screen is asked by key, what a click does
//! by what its handler reports. The strings a row shows are checked one level down, in the logic tests.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::common::*;
use fluent::el::*;
use fluent::ui::Key;
use mewrk_client::model::{ModelUsage, SubagentRunKind, UserAbortedTaskMetrics, UserAbortedTaskRecord};
use mewrk_client::App;
use mewrk_pane_tasks::context::SubagentViewStatus as S;
use mewrk_pane_tasks::fork_tray::*;
use mewrk_pane_tasks::history_pane::*;
use mewrk_pane_tasks::history_record::*;
use mewrk_pane_tasks::shell_tasks::{ShellTaskOutcome, ShellTaskSnapshot};
use mewrk_pane_tasks::sources::{BrowserStatus, PreviewRemoteControl, PreviewServerSnapshot, PreviewServerStatus, RemoteControlPlatform, RemoteControlProtocol, RemoteControlState, TerminalPhase, TerminalSessionState};
use mewrk_pane_tasks::subagent_panel::*;
use mewrk_pane_tasks::subagents::{AgentRole, SubagentView};
use mewrk_pane_tasks::task_container::*;
use mewrk_pane_tasks::tasks_pane::*;
use mewrk_pane_tasks::usage_card::*;
use mewrk_pane_tasks::usage_statistics::*;
use mewrk_pane_tasks::util::now_ms;
use mewrk_pane_tasks::workflow_panel::*;
use mewrk_pane_tasks::workflow_progress::*;
use mewrk_pane_tasks::workflow_runs::derive_workflow_run;
use serde_json::{json, Value};

fn agent(id: &str, f: impl FnOnce(&mut SubagentView)) -> Rc<SubagentView> {
    let mut v = SubagentView { id: id.into(), name: None, ledger_owner: None, kind: SubagentRunKind::General, workflow_run: false, label: id.into(), task: format!("执行 {id}"), status: S::Completed, summary: format!("{id} 的结论"), contexts: vec![], updates: vec![], live: None, call_ids: vec![id.into()], parent_id: None, depth: 0, child_ids: vec![], phase: None, phase_index: None, step_index: None, role: None, model_id: None, script_name: None, usage: ModelUsage::default(), tool_count: Some(0), created_at: "2026-07-20T01:00:00Z".into(), completed_at: Some("2026-07-20T01:05:00Z".into()) };
    f(&mut v);
    Rc::new(v)
}

fn running(v: &mut SubagentView) {
    v.status = S::Running;
    v.completed_at = None;
}

fn terminal() -> TerminalSessionState {
    TerminalSessionState { terminal_id: "terminal-1".into(), conversation_id: "conversation-1".into(), label: "终端 1".into(), phase: TerminalPhase::Running, busy: false, has_history: false, cwd: String::new(), shell: String::new(), session_id: None }
}

fn shell(f: impl FnOnce(&mut ShellTaskSnapshot)) -> ShellTaskSnapshot {
    let mut s = ShellTaskSnapshot { shell_task_id: "shell-1".into(), conversation_id: "conversation-1".into(), tool_name: "bash".into(), command: "npm install".into(), started_at: iso(now_ms() - 90_000), ..Default::default() };
    f(&mut s);
    s
}

fn page() -> BrowserStatus {
    BrowserStatus { has_page: true, open: true, url: "https://example.test/docs".into(), title: Some("文档".into()), ..Default::default() }
}

fn dev_server() -> PreviewServerSnapshot {
    PreviewServerSnapshot { handle: "7".into(), server_id: "dev".into(), name: "dev".into(), port: 5173, status: PreviewServerStatus::Running, started_at: "2026-07-20T01:00:00Z".into(), cwd: "C:/repo".into(), session_id: Some("conversation-1".into()), machine: None, url: None, workspace: None, remote: None }
}

fn fork_decision(approved: bool) -> ForkDecisionRecord {
    ForkDecisionRecord { fork_id: "fork-1".into(), workspace_id: "workspace-1".into(), source_conversation_id: "conversation-1".into(), title: "迁移升级脚本".into(), prompt: "p".into(), requested_at: "2026-07-20T01:40:00Z".into(), decided_at: "2026-07-20T01:41:00Z".into(), approved, child_conversation_id: approved.then(|| "conversation-2".to_owned()) }
}

/// A pane whose handlers write down what they were asked to do.
struct Pane {
    log: Calls,
    sources: TaskSources,
    stopping: Vec<String>,
    open_handler: bool,
    selected_row: Option<String>,
    selected_agent: Option<String>,
}

impl Pane {
    fn new(sources: TaskSources) -> Self {
        Self { log: calls(), sources, stopping: vec![], open_handler: true, selected_row: None, selected_agent: None }
    }

    fn view(&self) -> impl FnMut(&mut App) -> E + use<> {
        let (log, sources, stopping, open_handler) = (self.log.clone(), self.sources.clone(), self.stopping.clone(), self.open_handler);
        let (selected_row, selected_agent) = (self.selected_row.clone(), self.selected_agent.clone());
        move |app: &mut App| {
            let (l1, l2, l3) = (log.clone(), log.clone(), log.clone());
            let mut props = TasksPaneProps::new(sources.clone(), move |_, id| l1.borrow_mut().push(format!("select:{id}")), move |_, item| l2.borrow_mut().push(format!("stop:{}:{}", item.kind_name(), item.id)));
            if open_handler {
                props.on_open_item = Some(Rc::new(move |_: &mut App, item: &TaskItem| l3.borrow_mut().push(format!("open:{}:{}", item.kind_name(), item.id))));
            }
            props.stopping_ids = stopping.clone();
            props.selected_row_id = selected_row.clone();
            props.selected_agent_id = selected_agent.clone();
            col().child(tasks_pane(app, props))
        }
    }
}

fn row_key<'a>(kind: &'a str, id: &'a str) -> (&'static str, &'a str, &'a str) {
    ("task-row", kind, id)
}

fn stop_key<'a>(kind: &'a str, id: &'a str) -> (&'static str, &'a str, &'a str) {
    ("task-stop", kind, id)
}

#[test]
fn renders_the_list_without_any_chrome_of_its_own() {
    let mut rig = Rig::new("empty");
    let mut view = Pane::new(TaskSources::default()).view();
    rig.settle(&mut view);
    let r = rect_of("tasks-pane").unwrap();
    near(r, 0.0, 0.0, 900.0, 800.0);
    assert!(!on_screen("tasks-finished-toggle"));
}

#[test]
fn gives_a_running_agent_a_terminal_and_a_shell_command_one_row_each() {
    let mut rig = Rig::new("rows");
    let mut view = Pane::new(TaskSources { agents: vec![agent("live", running)], terminals: vec![terminal()], shell_tasks: vec![shell(|_| {})], ..Default::default() }).view();
    rig.settle(&mut view);
    let a = rect_of(row_key("subagent", "live")).unwrap();
    let b = rect_of(row_key("terminal", "terminal-1")).unwrap();
    let c = rect_of(row_key("shell", "shell-1")).unwrap();
    // The pane's padding, then rows two pixels apart, each a title and a detail in 6px of padding.
    near(a, 8.0, 8.0, 884.0, 41.0);
    assert_eq!(b.y, a.bottom() + 2.0);
    assert_eq!(c.y, b.bottom() + 2.0);
    assert_eq!(rect_of(stop_key("subagent", "live")).unwrap().w, 22.0);
}

#[test]
fn routes_an_agent_row_to_its_transcript_and_the_other_rows_to_their_pages() {
    let mut rig = Rig::new("routes");
    let pane = Pane::new(TaskSources { agents: vec![agent("live", running)], terminals: vec![terminal()], shell_tasks: vec![shell(|_| {})], browser: Some(page()), browser_session_id: Some("conversation-1".into()), ..Default::default() });
    let log = pane.log.clone();
    let mut view = pane.view();
    rig.settle(&mut view);
    rig.click(&mut view, row_key("subagent", "live"));
    rig.click(&mut view, row_key("terminal", "terminal-1"));
    rig.click(&mut view, row_key("browser", "preview:conversation-1"));
    rig.click(&mut view, row_key("shell", "shell-1"));
    assert_eq!(*log.borrow(), ["select:live", "open:terminal:terminal-1", "open:browser:preview:conversation-1", "open:shell:shell-1"]);
}

#[test]
fn leaves_page_rows_inert_without_a_handler_for_them() {
    let mut rig = Rig::new("inert");
    let mut pane = Pane::new(TaskSources { terminals: vec![terminal()], ..Default::default() });
    pane.open_handler = false;
    let log = pane.log.clone();
    let mut view = pane.view();
    rig.settle(&mut view);
    rig.click(&mut view, row_key("terminal", "terminal-1"));
    assert!(log.borrow().is_empty());
}

#[test]
fn gives_a_dev_server_its_own_row_opened_and_stopped() {
    let mut rig = Rig::new("server");
    let pane = Pane::new(TaskSources { preview_servers: vec![dev_server()], ..Default::default() });
    let log = pane.log.clone();
    let mut view = pane.view();
    rig.settle(&mut view);
    rig.click(&mut view, row_key("preview", "preview-server:7"));
    rig.click(&mut view, stop_key("preview", "preview-server:7"));
    assert_eq!(*log.borrow(), ["open:preview:preview-server:7", "stop:preview:preview-server:7"]);
}

#[test]
fn the_shell_lifts_the_tray_and_escape_goes_past_it() {
    let mut rig = Rig::new("tray");
    let request = PendingForkRequest { fork_id: "fork-1".into(), workspace_id: "workspace-1".into(), source_conversation_id: "conversation-1".into(), source_title: "来源".into(), prompt: "把它拆开".into(), requested_at: "2026-07-20T01:00:00Z".into() };
    let dismissed: Calls = calls();
    let mut view = {
        let dismissed = dismissed.clone();
        move |_: &mut App| -> E {
            let tray = fork_request_tray(std::slice::from_ref(&request), 0.0, Rc::new(|_, _, _| {}), None).unwrap().overlay_at(400.0, 12.0);
            let dismissed = dismissed.clone();
            col().child(tray).on_key_global(move |_, press| {
                if press.key == Key::Escape {
                    dismissed.borrow_mut().push("heard".to_owned());
                }
                false
            })
        }
    };
    rig.settle(&mut view);
    assert!(on_screen(("fork-card", "fork-1")));
    assert!(overlay_rects().iter().any(|r| r.x >= 399.0), "the page under it makes room, as it does for a menu");
    rig.press_key(&mut view, Key::Escape, false);
    rig.settle(&mut view);
    assert_eq!(*dismissed.borrow(), ["heard"], "the tray keeps nobody from the key");
    assert!(on_screen(("fork-card", "fork-1")), "and stays until it is answered");
}

#[test]
fn keeps_a_remote_control_session_that_gave_up_as_a_failed_row_that_can_still_be_stopped() {
    let mut rig = Rig::new("remote");
    let server = PreviewServerSnapshot { handle: "9".into(), name: "pixel".into(), port: 0, status: PreviewServerStatus::Failed, url: Some("http://127.0.0.1:41000/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/".into()), remote: Some(PreviewRemoteControl { protocol: RemoteControlProtocol::Adb, platform: RemoteControlPlatform::Android, target: "emulator-5554 · this computer".into(), target_display: None, label: None, state: RemoteControlState::Failed, read_only: false, launch: None, message: None }), ..dev_server() };
    let pane = Pane::new(TaskSources { preview_servers: vec![server], ..Default::default() });
    let log = pane.log.clone();
    let mut view = pane.view();
    rig.settle(&mut view);
    // It is in the finish list, and yet it has the control a finished row would not.
    assert!(on_screen("tasks-finished-toggle"));
    rig.click(&mut view, stop_key("preview", "preview-server:9"));
    assert_eq!(*log.borrow(), ["stop:preview:preview-server:9"]);
}

#[test]
fn stops_a_running_row_without_opening_it() {
    let mut rig = Rig::new("stop");
    let pane = Pane::new(TaskSources { shell_tasks: vec![shell(|_| {})], ..Default::default() });
    let log = pane.log.clone();
    let mut view = pane.view();
    rig.settle(&mut view);
    rig.click(&mut view, stop_key("shell", "shell-1"));
    assert_eq!(*log.borrow(), ["stop:shell:shell-1"]);
}

#[test]
fn disables_the_stop_control_once_an_abort_is_already_in_flight() {
    let mut rig = Rig::new("stopping");
    let mut pane = Pane::new(TaskSources { agents: vec![agent("live", running)], ..Default::default() });
    pane.stopping = vec!["live".into()];
    let log = pane.log.clone();
    let mut view = pane.view();
    rig.settle(&mut view);
    rig.click(&mut view, stop_key("subagent", "live"));
    assert!(log.borrow().is_empty());
}

#[test]
fn keeps_the_clock_moving_for_running_work_and_stops_it_for_history() {
    let mut rig = Rig::new("clock");
    let mut live = Pane::new(TaskSources { shell_tasks: vec![shell(|_| {})], ..Default::default() }).view();
    rig.settle(&mut live);
    assert!(rig.last_wake.is_some(), "a row measured against the clock has to ask for frames");
    let mut done = Pane::new(TaskSources { shell_tasks: vec![shell(|s| { s.outcome = Some(ShellTaskOutcome::Succeeded); s.exit_code = Some(0); s.ended_at = Some(iso(now_ms())) })], ..Default::default() }).view();
    rig.settle(&mut done);
    // The metric that was rolling settles, and then nothing asks for another frame.
    std::thread::sleep(std::time::Duration::from_millis(550));
    rig.settle(&mut done);
    assert_eq!(rig.last_wake, None);
}

#[test]
fn moves_a_finished_shell_command_into_the_finish_list_with_no_stop_control() {
    let mut rig = Rig::new("finished");
    let mut view = Pane::new(TaskSources { shell_tasks: vec![shell(|s| { s.command = "npm test".into(); s.outcome = Some(ShellTaskOutcome::Failed); s.exit_code = Some(1); s.ended_at = Some(iso(now_ms())); s.cwd = Some("/Users/me/web".into()) })], ..Default::default() }).view();
    rig.settle(&mut view);
    assert!(on_screen(row_key("shell", "shell-1")));
    assert!(!on_screen(stop_key("shell", "shell-1")));
}

#[test]
fn shows_finished_rows_under_an_open_disclosure_until_it_is_folded() {
    let mut rig = Rig::new("disclosure");
    let mut view = Pane::new(TaskSources { agents: vec![agent("live", running), agent("done", |_| {})], ..Default::default() }).view();
    rig.settle(&mut view);
    assert!(on_screen(row_key("subagent", "done")));
    rig.click(&mut view, "tasks-finished-toggle");
    assert!(!on_screen(row_key("subagent", "done")));
    assert!(on_screen(row_key("subagent", "live")));
    rig.click(&mut view, "tasks-finished-toggle");
    assert!(on_screen(row_key("subagent", "done")));
}

fn audit_roster() -> Vec<Rc<SubagentView>> {
    vec![
        agent("run", |v| { v.kind = SubagentRunKind::WorkflowStep; v.workflow_run = true; v.label = "review-proxy".into(); v.task = "审查代理路径".into(); running(v); v.child_ids = vec!["review-1".into(), "verify-1".into()] }),
        agent("review-1", |v| { v.kind = SubagentRunKind::WorkflowStep; v.parent_id = Some("run".into()); v.depth = 1; v.label = "审查一".into(); v.call_ids = vec!["wf-ws1".into()]; v.role = Some(AgentRole { name: "reviewer".into(), model_id: Some("sonnet-5".into()) }); v.phase = Some("Review".into()); v.phase_index = Some(0); running(v) }),
        agent("verify-1", |v| { v.kind = SubagentRunKind::WorkflowStep; v.parent_id = Some("run".into()); v.depth = 1; v.label = "验证一".into(); v.call_ids = vec!["wf-ws2".into()]; v.phase = Some("Verify".into()); v.phase_index = Some(1); running(v) }),
    ]
}

#[test]
fn draws_a_workflow_as_its_own_panel_whose_steps_open_but_whose_run_does_not() {
    let mut rig = Rig::new("workflow");
    let pane = Pane::new(TaskSources { agents: audit_roster(), ..Default::default() });
    let log = pane.log.clone();
    let mut view = pane.view();
    rig.settle(&mut view);
    assert!(on_screen(("workflow-bar", "run")));
    assert!(!on_screen(row_key("workflow", "run")));
    rig.click(&mut view, ("workflow-bar", "run"));
    assert!(log.borrow().is_empty(), "the bar is not a control");
    rig.click(&mut view, ("workflow-step", "agent:verify-1"));
    assert_eq!(*log.borrow(), ["select:verify-1"]);
}

#[test]
fn renders_persisted_user_aborts_as_inert_failed_rows() {
    let mut rig = Rig::new("aborted");
    let record = UserAbortedTaskRecord { id: "abort-1".into(), source_kind: "subagent".into(), source_identity: "agent:call-1:2026-01-01T00:00:00Z".into(), label: "中止审查".into(), detail: "检查任务栏".into(), metrics: UserAbortedTaskMetrics { child_count: Some(0), tokens: Some(10), tool_count: Some(1), elapsed_ms: Some(1000) }, started_at: "2026-01-01T00:00:00Z".into(), ended_at: "2026-01-01T00:00:01Z".into(), reason: "userAborted".into() };
    let pane = Pane::new(TaskSources { user_aborted_tasks: vec![record], ..Default::default() });
    let log = pane.log.clone();
    let mut view = pane.view();
    rig.settle(&mut view);
    rig.click(&mut view, row_key("aborted", "aborted:abort-1"));
    assert!(log.borrow().is_empty());
    assert!(!on_screen(stop_key("aborted", "aborted:abort-1")));
}

#[test]
fn opens_an_approved_fork_and_leaves_a_declined_one_a_record() {
    let mut rig = Rig::new("forks");
    let pane = Pane::new(TaskSources { fork_decisions: vec![fork_decision(true)], ..Default::default() });
    let log = pane.log.clone();
    let mut view = pane.view();
    rig.settle(&mut view);
    rig.click(&mut view, row_key("fork", "fork:fork-1"));
    assert_eq!(*log.borrow(), ["open:fork:fork:fork-1"]);
    let declined = Pane::new(TaskSources { fork_decisions: vec![fork_decision(false)], ..Default::default() });
    let log = declined.log.clone();
    let mut view = declined.view();
    rig.settle(&mut view);
    rig.click(&mut view, row_key("fork", "fork:fork-1"));
    assert!(log.borrow().is_empty());
}

#[test]
fn opens_a_focused_row_from_the_keyboard() {
    let mut rig = Rig::new("keys");
    let pane = Pane::new(TaskSources { terminals: vec![terminal()], ..Default::default() });
    let log = pane.log.clone();
    let mut view = pane.view();
    rig.settle(&mut view);
    rig.click(&mut view, row_key("terminal", "terminal-1"));
    log.borrow_mut().clear();
    rig.press_key(&mut view, Key::Enter, false);
    rig.press_key(&mut view, Key::Char(' '), false);
    rig.press_key(&mut view, Key::Char('x'), false);
    assert_eq!(log.borrow().len(), 2);
}

#[test]
fn reports_the_four_metric_columns_and_drops_a_terminals_outright() {
    let items = derive_task_items(
        &TaskSources { agents: vec![agent("live", |v| { running(v); v.child_ids = vec!["child".into()]; v.usage.total_tokens = Some(8_400); v.tool_count = Some(7); v.created_at = iso(now_ms() - 90_000) })], terminals: vec![terminal()], shell_tasks: vec![shell(|_| {})], ..Default::default() },
        &task_container_messages(),
    );
    let columns = task_metric_columns(&items[0]);
    assert_eq!(columns.iter().map(|c| c.0).collect::<Vec<_>>(), ["children", "tokens", "tools", "elapsed"]);
    assert_eq!(columns[1].1, "8.4k");
    assert!(columns[3].1.starts_with("1m3") || columns[3].1.starts_with("1m2"), "{}", columns[3].1);
    assert!(task_metric_columns(&items[1]).is_empty());
    assert_eq!(task_metric_columns(&items[2]).iter().map(|c| c.0).collect::<Vec<_>>(), ["elapsed"]);
    // An agent that reported no tokens says so with a dash rather than dropping the column.
    let bare = derive_task_items(&TaskSources { agents: vec![agent("bare", |_| {})], ..Default::default() }, &task_container_messages());
    assert_eq!(task_metric_columns(&bare[0])[1].1, "—");
}

#[test]
fn formats_tokens_the_way_upstream_rounds_them() {
    assert_eq!((format_tokens(999), format_tokens(1_240), format_tokens(12_400)), ("999".to_owned(), "1.2k".to_owned(), "12k".to_owned()));
}

// ---------------------------------------------------------------- WorkflowRunPanel

fn audit_item(ledger_owners: bool) -> TaskItem {
    let agents = vec![
        agent("run", |v| { v.kind = SubagentRunKind::WorkflowStep; v.workflow_run = true; v.label = "fetch-models-gate-audit".into(); v.script_name = Some("gate-audit-plan".into()); v.task = "Audit every pre-flight gate".into(); running(v); v.child_ids = vec!["s0".into(), "s1".into()]; v.usage.total_tokens = Some(87_200) }),
        agent("s0", |v| { v.kind = SubagentRunKind::WorkflowStep; v.parent_id = Some("run".into()); v.depth = 1; v.label = "audit:rust-path".into(); v.phase = Some("Audit".into()); v.phase_index = Some(0); v.role = Some(AgentRole { name: "auditor".into(), model_id: Some("sonnet-5".into()) }); if ledger_owners { v.ledger_owner = Some("audit-run/ws1".into()) } }),
        agent("s1", |v| { v.kind = SubagentRunKind::WorkflowStep; v.parent_id = Some("run".into()); v.depth = 1; v.label = "audit:renderer-path".into(); v.phase = Some("Audit".into()); v.phase_index = Some(0); running(v); if ledger_owners { v.ledger_owner = Some("audit-run/ws2".into()) } }),
    ];
    derive_workflow_items(&agents, &task_container_messages(), now_ms(), None).remove(0)
}

fn ledger() -> WorkflowProgressView {
    let entries: Vec<WorkflowProgressEntry> = serde_json::from_value(json!([
        { "kind": "agent", "index": 0, "state": "done", "label": "audit:rust-path", "phase": "Audit", "phaseIndex": 0 },
        { "kind": "agent", "index": 1, "state": "progress", "label": "audit:renderer-path", "phase": "Audit", "phaseIndex": 0 },
        { "kind": "log", "index": 0, "state": "progress", "message": "2/3 done" }
    ]))
    .unwrap();
    derive_workflow_progress(&entries, &WorkflowProgressMessages::default())
}

struct Panel {
    item: TaskItem,
    progress: Option<WorkflowProgressView>,
    run_id: Option<String>,
    stopping: bool,
    log: Calls,
}

impl Panel {
    fn view(self) -> impl FnMut(&mut App) -> E {
        move |_| {
            let (open, control, stop) = (self.log.clone(), self.log.clone(), self.log.clone());
            col().child(workflow_run_panel(WorkflowRunPanelProps {
                view: derive_workflow_run(&self.item, self.progress.as_ref()),
                selected_agent_id: None,
                stopping: self.stopping,
                run_id: self.run_id.clone(),
                on_open_agent: Some(Rc::new(move |_, id| open.borrow_mut().push(format!("open:{id}")))),
                on_stop: Some(Rc::new(move |_| stop.borrow_mut().push("stop".to_owned()))),
                on_step_control: Some(Rc::new(move |_, run, index, action| control.borrow_mut().push(format!("{run}:{index}:{}", action_name(action))))),
            }))
        }
    }
}

fn control_key<'a>(step: &'a str, action: &'a str) -> (&'static str, &'a str, &'a str) {
    ("workflow-step-control", step, action)
}

#[test]
fn addresses_the_run_and_the_step_index_when_skip_or_retry_is_clicked() {
    let mut rig = Rig::new("skip");
    let log = calls();
    let mut view = Panel { item: audit_item(false), progress: Some(ledger()), run_id: Some("run-abc".into()), stopping: false, log: log.clone() }.view();
    rig.settle(&mut view);
    // Only a step still moving can be skipped: the scheduler never re-opens a settled slot.
    assert!(!on_screen(control_key("agent:s0", "skip")));
    rig.hover(&mut view, ("workflow-step", "agent:s1"));
    rig.click(&mut view, control_key("agent:s1", "skip"));
    rig.click(&mut view, control_key("agent:s1", "retry"));
    assert_eq!(*log.borrow(), ["run-abc:1:skip", "run-abc:1:retry"]);
}

#[test]
fn keeps_skip_and_retry_once_the_round_that_started_the_run_is_gone() {
    let mut rig = Rig::new("addressed");
    let log = calls();
    let mut view = Panel { item: audit_item(true), progress: None, run_id: None, stopping: false, log: log.clone() }.view();
    rig.settle(&mut view);
    rig.hover(&mut view, ("workflow-step", "agent:s1"));
    rig.click(&mut view, control_key("agent:s1", "skip"));
    assert_eq!(*log.borrow(), ["audit-run:1:skip"]);
    assert!(!on_screen(control_key("agent:s0", "skip")));
}

#[test]
fn offers_no_skip_or_retry_on_a_step_it_could_not_match_or_a_run_it_cannot_address() {
    let mut rig = Rig::new("noslot");
    for run_id in [Some("run-abc".to_owned()), None] {
        let mut view = Panel { item: audit_item(false), progress: None, run_id, stopping: false, log: calls() }.view();
        rig.settle(&mut view);
        assert!(!on_screen(control_key("agent:s1", "skip")) && !on_screen(control_key("agent:s1", "retry")));
    }
}

#[test]
fn opens_a_steps_transcript_and_offers_a_slot_with_no_agent_no_way_to_open_one() {
    let mut rig = Rig::new("slots");
    let entries: Vec<WorkflowProgressEntry> = serde_json::from_value(json!([{ "kind": "agent", "index": 5, "state": "start", "label": "queued", "phase": "Audit", "phaseIndex": 0, "blocked": true }])).unwrap();
    let log = calls();
    let mut view = Panel { item: audit_item(false), progress: Some(derive_workflow_progress(&entries, &WorkflowProgressMessages::default())), run_id: None, stopping: false, log: log.clone() }.view();
    rig.settle(&mut view);
    rig.click(&mut view, ("workflow-step", "slot:5"));
    assert!(log.borrow().is_empty());
    rig.click(&mut view, ("workflow-step", "agent:s0"));
    assert_eq!(*log.borrow(), ["open:s0"]);
}

#[test]
fn stops_the_run_and_shows_the_log_only_when_the_live_ledger_carried_one() {
    let mut rig = Rig::new("runlog");
    let log = calls();
    let mut view = Panel { item: audit_item(false), progress: Some(ledger()), run_id: None, stopping: false, log: log.clone() }.view();
    rig.settle(&mut view);
    rig.click(&mut view, ("workflow-stop", "run"));
    assert_eq!(*log.borrow(), ["stop"]);
    assert!(!on_screen(("workflow-logs", "run")));
    rig.click(&mut view, ("workflow-logs-toggle", "run"));
    assert!(on_screen(("workflow-logs", "run")));
    // A settled run has no ledger, so no log to offer; and a stop in flight offers no second one.
    let mut settled = Panel { item: audit_item(false), progress: None, run_id: None, stopping: true, log: log.clone() }.view();
    rig.settle(&mut settled);
    assert!(!on_screen(("workflow-logs-toggle", "run")));
    log.borrow_mut().clear();
    rig.click(&mut settled, ("workflow-stop", "run"));
    assert!(log.borrow().is_empty());
}

// ---------------------------------------------------------------- SubagentPanel

fn panel_view(agent_id: &str, agents: Vec<Rc<SubagentView>>, chromeless: bool, log: Calls) -> impl FnMut(&mut App) -> E + use<> {
    let id = agent_id.to_owned();
    move |app| {
        let agent = agents.iter().find(|a| a.id == id).unwrap().clone();
        let (select, close, drawn) = (log.clone(), log.clone(), log.clone());
        let transcript = move |_: &mut App, args: TranscriptArgs<'_>| -> E {
            drawn.borrow_mut().push(format!("transcript:{}:{}", args.contexts.len(), args.streaming));
            div::<App>().key("transcript").flex1()
        };
        col().child(subagent_panel(app, SubagentPanelProps { agent, agents: agents.clone(), on_select_agent: Some(Rc::new(move |_, id| select.borrow_mut().push(format!("select:{id}")))), on_close: Some(Rc::new(move |_| close.borrow_mut().push("close".to_owned()))), chromeless, path_base_dir: None, transcript: &transcript }))
    }
}

#[test]
fn links_back_to_a_real_parent_agent_but_never_to_the_workflow_run_above_a_step() {
    let mut rig = Rig::new("nav");
    let log = calls();
    let agents = vec![agent("parent", |v| v.child_ids = vec!["child".into()]), agent("child", |v| { v.parent_id = Some("parent".into()); v.depth = 1 })];
    let mut view = panel_view("child", agents, false, log.clone());
    rig.settle(&mut view);
    rig.click(&mut view, ("subagent-nav", "parent:parent".to_owned()));
    assert!(log.borrow().iter().any(|l| l == "select:parent"));
    let run = vec![agent("run", |v| { v.workflow_run = true; v.child_ids = vec!["step".into()] }), agent("step", |v| { v.parent_id = Some("run".into()); v.depth = 1 })];
    let mut view = panel_view("step", run, false, log.clone());
    rig.settle(&mut view);
    assert!(!on_screen(("subagent-nav", "parent:run".to_owned())));
}

#[test]
fn walks_to_its_children_and_closes_back_to_the_conversation() {
    let mut rig = Rig::new("children");
    let log = calls();
    let agents = vec![agent("parent", |v| { running(v); v.child_ids = vec!["child".into()] }), agent("child", |v| { v.parent_id = Some("parent".into()); v.depth = 1 })];
    let mut view = panel_view("parent", agents, false, log.clone());
    rig.settle(&mut view);
    assert!(log.borrow().iter().any(|l| l == "transcript:0:true"), "{:?}", log.borrow());
    rig.click(&mut view, ("subagent-nav", "child:child".to_owned()));
    rig.click(&mut view, "subagent-back");
    assert!(log.borrow().iter().any(|l| l == "select:child") && log.borrow().iter().any(|l| l == "close"));
}

#[test]
fn drops_its_own_header_when_the_host_draws_one_keeping_the_nav_and_the_transcript() {
    let mut rig = Rig::new("chromeless");
    let agents = vec![agent("parent", |v| v.child_ids = vec!["child".into()]), agent("child", |v| { v.parent_id = Some("parent".into()); v.depth = 1 })];
    let mut view = panel_view("parent", agents, true, calls());
    rig.settle(&mut view);
    assert!(!on_screen("subagent-back"));
    assert!(on_screen(("subagent-nav", "child:child".to_owned())) && on_screen("transcript"));
}

#[test]
fn the_streamed_panel_refuses_a_workflow_run_and_finds_a_step_by_any_call_id() {
    let mut rig = Rig::new("streamed");
    let contexts = vec![ctx(json!({ "id": "tool-workflow", "kind": "tool", "toolName": "workflow", "input": { "scriptName": "plan" }, "result": { "success": true, "output": "", "executedAt": "2026-08-26T02:00:00Z", "durationMs": 0 }, "subagent": { "kind": "workflowStep", "task": "Run.", "status": "completed", "contexts": [
        { "id": "tool-step", "kind": "tool", "toolName": "workflow_step", "input": { "label": "审查一", "task": "审查" }, "result": { "success": true, "output": "Done", "executedAt": "2026-08-26T02:00:00Z", "durationMs": 0 }, "subagent": { "kind": "workflowStep", "task": "审查", "status": "completed", "contexts": [], "updates": [] }, "createdAt": "2026-08-26T01:00:00Z" }
    ], "updates": [] }, "createdAt": "2026-08-26T02:00:00Z" }))];
    let results = Rc::new(RefCell::new(Vec::<bool>::new()));
    for (selected, expected) in [("tool-workflow", false), ("tool-step", true)] {
        let (contexts, pushed) = (contexts.clone(), results.clone());
        let mut view = move |app: &mut App| {
            let transcript = |_: &mut App, _: TranscriptArgs<'_>| -> E { div() };
            let panel = streamed_subagent_panel(app, StreamedSubagentPanelProps { conversation_id: "c".into(), rendered_contexts: &contexts, revision: 1, selected_subagent_id: Some(selected.into()), on_select_agent: Rc::new(|_, _| {}), on_close: Rc::new(|_| {}), chromeless: true, path_base_dir: None, dock: None, transcript: &transcript });
            pushed.borrow_mut().push(panel.is_some());
            col().child(panel)
        };
        rig.frame(&mut view);
        assert_eq!(*results.borrow().last().unwrap(), expected, "{selected}");
    }
}

// ---------------------------------------------------------------- ForkRequestTray

fn fork_request(id: &str, prompt: &str) -> PendingForkRequest {
    PendingForkRequest { fork_id: id.into(), workspace_id: "workspace-9".into(), source_conversation_id: "conversation-9".into(), source_title: "父会话".into(), prompt: prompt.into(), requested_at: "2026-09-05T10:00:00Z".into() }
}

fn tray_view(requests: Vec<PendingForkRequest>, log: Calls, open_source: bool) -> impl FnMut(&mut App) -> E {
    move |_| {
        let (decide, open) = (log.clone(), log.clone());
        let source: Option<ForkOpenSource> = open_source.then(|| Rc::new(move |_: &mut App, ws: &str, conv: &str| open.borrow_mut().push(format!("source:{ws}:{conv}"))) as ForkOpenSource);
        let tray = fork_request_tray(&requests, 0.0, Rc::new(move |_, id, approved| decide.borrow_mut().push(format!("{id}:{approved}"))), source);
        col().key("root").child(div().key("below").h(400.0).shrink0()).child(tray)
    }
}

#[test]
fn renders_nothing_when_there_is_no_pending_request() {
    let mut rig = Rig::new("trayempty");
    let mut view = tray_view(vec![], calls(), false);
    rig.settle(&mut view);
    assert!(!on_screen("fork-request-tray"));
}

#[test]
fn renders_one_card_per_request_with_the_newest_on_top() {
    let mut rig = Rig::new("trayorder");
    let mut view = tray_view(vec![fork_request("fork-old", "较早"), fork_request("fork-new", "较新")], calls(), false);
    rig.settle(&mut view);
    assert!(rect_of(("fork-card", "fork-new")).unwrap().y < rect_of(("fork-card", "fork-old")).unwrap().y);
    // Pinned to the top-right corner, 12px in.
    let tray = rect_of("fork-request-tray").unwrap();
    near(tray, 900.0 - 12.0 - 360.0, 12.0, 360.0, tray.h);
}

#[test]
fn reports_the_decision_for_the_card_that_was_acted_on() {
    let mut rig = Rig::new("traydecide");
    let log = calls();
    let mut view = tray_view(vec![fork_request("fork-7", "短")], log.clone(), false);
    rig.settle(&mut view);
    rig.click(&mut view, ("fork-card-approve", "fork-7"));
    rig.click(&mut view, ("fork-card-deny", "fork-7"));
    assert_eq!(*log.borrow(), ["fork-7:true", "fork-7:false"]);
}

#[test]
fn offers_an_expand_toggle_only_for_a_long_prompt() {
    let mut rig = Rig::new("traylong");
    let long = "很长的提示".repeat(60);
    let mut view = tray_view(vec![fork_request("fork-short", "短"), fork_request("fork-long", &long)], calls(), false);
    rig.settle(&mut view);
    assert!(!on_screen(("fork-card-toggle", "fork-short")));
    assert!(on_screen(("fork-card-prompt", "fork-long", false)));
    let clamped = rect_of(("fork-card", "fork-long")).unwrap().h;
    rig.click(&mut view, ("fork-card-toggle", "fork-long"));
    rig.settle(&mut view);
    assert!(on_screen(("fork-card-prompt", "fork-long", true)));
    assert!(rect_of(("fork-card", "fork-long")).unwrap().h > clamped);
    rig.click(&mut view, ("fork-card-toggle", "fork-long"));
    assert!(on_screen(("fork-card-prompt", "fork-long", false)));
}

#[test]
fn jumps_to_the_source_conversation_only_when_a_handler_is_given() {
    let mut rig = Rig::new("traysource");
    let log = calls();
    let mut view = tray_view(vec![fork_request("fork-1", "短")], log.clone(), true);
    rig.settle(&mut view);
    rig.click(&mut view, ("fork-card-source", "fork-1"));
    assert_eq!(*log.borrow(), ["source:workspace-9:conversation-9"]);
    let mut inert = tray_view(vec![fork_request("fork-1", "短")], log, false);
    rig.settle(&mut inert);
    assert!(!on_screen(("fork-card-source", "fork-1")));
}

#[test]
fn the_tray_lets_a_press_through_everywhere_but_its_cards() {
    let mut rig = Rig::new("traypass");
    let log = calls();
    let press = log.clone();
    let mut view = move |_: &mut App| {
        let (decide, under) = (press.clone(), press.clone());
        col().child(div().key("under").h(800.0).on_click(move |_| under.borrow_mut().push("under".to_owned()))).child(fork_request_tray(&[fork_request("fork-1", "短")], 0.0, Rc::new(move |_, id, _| decide.borrow_mut().push(id.to_owned())), None))
    };
    rig.settle(&mut view);
    let card = rect_of(("fork-card", "fork-1")).unwrap();
    rig.click_at(&mut view, card.x + 20.0, card.y + 10.0);
    assert!(log.borrow().is_empty(), "a press on a card stays on the card");
    let tray = rect_of("fork-request-tray").unwrap();
    rig.click_at(&mut view, tray.x + 2.0, tray.bottom() + 40.0);
    assert_eq!(*log.borrow(), ["under"]);
}

#[test]
fn the_fork_store_dedupes_and_folds_the_hosts_events() {
    let mut store = ForkStore::default();
    let event = json!({ "type": "forkRequested", "forkId": "f1", "workspaceId": "w", "sourceConversationId": "c", "sourceTitle": "t", "prompt": "p", "requestedAt": "x" });
    store.apply_event(&event);
    store.apply_event(&event);
    assert_eq!(store.requests.len(), 1);
    let decision = json!({ "forkId": "f1", "workspaceId": "w", "sourceConversationId": "c", "title": "t", "prompt": "p", "requestedAt": "x", "decidedAt": "y", "approved": true, "childConversationId": "c2" });
    for _ in 0..2 {
        store.apply_event(&json!({ "type": "forkResolved", "forkId": "f1", "decision": decision }));
    }
    assert!(store.requests.is_empty());
    assert_eq!(store.decisions["c"].len(), 1);
    // A retraction carries no decision.
    store.apply_event(&event);
    store.apply_event(&json!({ "type": "forkResolved", "forkId": "f1" }));
    assert!(store.requests.is_empty());
    assert_eq!(store.decisions["c"].len(), 1);
}

// ---------------------------------------------------------------- HistoryPane

type ListFn = dyn Fn() -> Result<Vec<HistoryEntry>, String>;
type LoadFn = dyn Fn(i64) -> Result<Option<HistoryEntryDetail>, String>;

/// Answers from fixtures, on the next tick, as upstream's tests mock the runtime.
struct Fake {
    list: Rc<ListFn>,
    load: Rc<LoadFn>,
    lists: Rc<RefCell<Vec<Option<Vec<String>>>>>,
}

impl HistoryReader for Fake {
    fn list(&self, app: &App, _: &str, owners: Option<&[String]>, done: Box<dyn FnOnce(&mut App, Result<Vec<HistoryEntry>, String>) + Send>) {
        self.lists.borrow_mut().push(owners.map(<[String]>::to_vec));
        let result = (self.list)();
        app.post(move |app| done(app, result));
    }
    fn load(&self, app: &App, _: &str, seq: i64, done: Box<dyn FnOnce(&mut App, Result<Option<HistoryEntryDetail>, String>) + Send>) {
        let result = (self.load)(seq);
        app.post(move |app| done(app, result));
    }
}

fn entry(seq: i64, kind: &str, detail: Value, over: Value) -> HistoryEntry {
    let mut base = json!({ "seq": seq, "createdAt": "2026-09-15T10:00:00Z", "kind": kind, "detail": detail });
    if kind != "edit" {
        base["requestId"] = json!("run_a");
    }
    for (k, v) in over.as_object().unwrap() {
        base[k] = v.clone();
    }
    serde_json::from_value(base).unwrap()
}

type Pair = (HistoryEntry, Option<HistoryEntryDetail>);

fn sent(seq: i64, content: &str) -> Pair {
    let e = entry(seq, "edit", json!({ "source": "message", "inserted": 1 }), json!({}));
    let d = HistoryEntryDetail { entry: e.clone(), ops: Some(vec![HistoryOp { ordinal: 0, op: "insert".into(), context_id: format!("ctx_{seq}"), position: Some(0), body: Some(json!({ "kind": "user", "content": content }).to_string()), before: None }]), ..Default::default() };
    (e, Some(d))
}

fn sent_request(seq: i64, run: &str, added: i64, system: &str) -> Pair {
    let e = entry(seq, "request", json!({ "type": "model", "attempt": 1, "modelId": "claude-opus-5", "messagesAdded": added }), json!({ "requestId": run }));
    let parts = vec![HistoryPart { ordinal: 0, kind: "system".into(), hash: format!("h-{system}"), body: system.into(), bytes: 1, truncated: false }];
    let d = HistoryEntryDetail { entry: e.clone(), body: Some("{}".into()), parts: Some(parts), ..Default::default() };
    (e, Some(d))
}

fn settled(seq: i64, run: &str, rows: Vec<Value>) -> Pair {
    let e = entry(seq, "run", json!({ "source": "run", "inserted": rows.len() }), json!({ "requestId": run }));
    let ops = rows.iter().enumerate().map(|(i, r)| HistoryOp { ordinal: i as i64, op: "insert".into(), context_id: format!("ctx_{seq}_{i}"), position: Some(i as i64), body: Some(r.to_string()), before: None }).collect();
    let d = HistoryEntryDetail { entry: e.clone(), ops: Some(ops), ..Default::default() };
    (e, Some(d))
}

fn served(pairs: Vec<Pair>) -> Fake {
    let list: Vec<HistoryEntry> = pairs.iter().map(|p| p.0.clone()).collect();
    let bodies: HashMap<i64, HistoryEntryDetail> = pairs.into_iter().filter_map(|(e, d)| d.map(|d| (e.seq, d))).collect();
    Fake { list: Rc::new(move || Ok(list.clone())), load: Rc::new(move |seq| Ok(bodies.get(&seq).cloned())), lists: Rc::default() }
}

fn history_view(reader: Rc<Fake>, owners: Option<Vec<String>>, streaming: bool) -> impl FnMut(&mut App) -> E {
    move |app| {
        let mut props = HistoryPaneProps::new("conv_1", 1, streaming);
        props.owners = owners.clone();
        props.reader = Some(reader.clone());
        col().child(history_pane(app, props))
    }
}

/// Past the 150 ms debounce, with the answers fed back through the app's mailbox.
fn run_history(rig: &mut Rig, view: &mut dyn FnMut(&mut App) -> E) {
    rig.frame(view);
    std::thread::sleep(std::time::Duration::from_millis(170));
    for _ in 0..8 {
        rig.frame(view);
    }
}

fn message_key(bar: &str, row: &str) -> (&'static str, String) {
    ("history-row", format!("{bar}:{row}"))
}

fn state(rig: &mut Rig) -> &mut PaneState {
    pane_state(&mut rig.app, "conv_1", None)
}

#[test]
fn says_recording_has_not_started_rather_than_showing_an_empty_list() {
    let mut rig = Rig::new("h-empty");
    let reader = Rc::new(served(vec![]));
    let mut view = history_view(reader.clone(), None, false);
    rig.frame(&mut view);
    assert!(on_screen("history-loading") || rect_of("history-loading").is_none());
    run_history(&mut rig, &mut view);
    assert!(on_screen("history-empty"));
    assert_eq!(*reader.lists.borrow(), [None], "the conversation's own history when no agent is named");
}

#[test]
fn lays_a_turn_out_as_the_messages_it_added_with_no_request_among_them() {
    let mut rig = Rig::new("h-turn");
    let reader = Rc::new(served(vec![
        sent(1, "你看到追加的工具了吗"),
        sent_request(2, "run_a", 1, "You are Mewrk."),
        (entry(3, "response", json!({ "finishReason": "stop" }), json!({ "answers": 2 })), None),
        settled(4, "run_a", vec![json!({ "kind": "system", "content": "Tools added: shell", "localOnly": true, "toolsAdded": ["shell"] }), json!({ "kind": "reasoning", "content": "想一想" }), json!({ "kind": "tool", "toolName": "ls", "input": { "path": "." }, "result": { "success": true, "output": "a.txt" } }), json!({ "kind": "assistant", "content": "只有一个文件" })]),
    ]));
    let mut view = history_view(reader, None, false);
    run_history(&mut rig, &mut view);
    // The prompt it opened with, what the user sent, and what the run put on the timeline.
    assert!(on_screen(message_key("b1", "p2-system")));
    assert!(on_screen(message_key("b1:e1", "1-0")));
    for i in 0..4 {
        assert!(on_screen(message_key("b1:e4", &format!("4-{i}"))), "{i}");
    }
    // A settled run's response is not drawn again, and no request is a row.
    assert!(!on_screen(("history-row", "b1:e3".to_owned())));
    // Opening a row shows its text; closing it again takes it away.
    rig.click(&mut view, message_key("b1:e4", "4-3"));
    assert!(on_screen(("history-text", "b1:e4:4-3")));
    rig.click(&mut view, message_key("b1:e4", "4-3"));
    assert!(!on_screen(("history-text", "b1:e4:4-3")));
}

#[test]
fn stands_the_users_edits_as_a_bar_with_the_message_a_deletion_took_away() {
    let mut rig = Rig::new("h-edits");
    let removal = entry(5, "edit", json!({ "source": "edit", "removed": 1 }), json!({}));
    let rewrite = entry(6, "edit", json!({ "source": "edit", "replaced": 1 }), json!({}));
    let removal_detail = HistoryEntryDetail { entry: removal.clone(), ops: Some(vec![HistoryOp { ordinal: 0, op: "remove".into(), context_id: "ctx_4_0".into(), position: None, body: None, before: Some(json!({ "kind": "assistant", "content": "我先逛逛" }).to_string()) }]), ..Default::default() };
    let rewrite_detail = HistoryEntryDetail { entry: rewrite.clone(), ops: Some(vec![HistoryOp { ordinal: 0, op: "replace".into(), context_id: "ctx_1".into(), position: None, body: Some(json!({ "kind": "user", "content": "在两个工作区里看看" }).to_string()), before: Some(json!({ "kind": "user", "content": "随便玩一下" }).to_string()) }]), ..Default::default() };
    let reader = Rc::new(served(vec![
        sent(1, "随便玩一下"),
        sent_request(2, "run_a", 1, "You are Mewrk."),
        (entry(3, "response", json!({}), json!({ "answers": 2 })), None),
        settled(4, "run_a", vec![json!({ "kind": "assistant", "content": "我先逛逛" })]),
        (removal, Some(removal_detail)),
        (rewrite, Some(rewrite_detail)),
        sent_request(7, "run_b", 0, "You are Mewrk."),
        (entry(8, "response", json!({}), json!({ "requestId": "run_b", "answers": 7 })), None),
        settled(9, "run_b", vec![json!({ "kind": "assistant", "content": "两个工作区都是 Mewrk" })]),
    ]));
    let mut view = history_view(reader, None, false);
    run_history(&mut rig, &mut view);
    assert!(on_screen(("history-bar", "b1")) && on_screen(("history-bar", "b5")) && on_screen(("history-bar", "b7")));
    // The newest bar opened for the reader, and only that one.
    assert_eq!(state(&mut rig).open_rows.iter().cloned().collect::<Vec<_>>(), ["b7"]);
    rig.click(&mut view, ("history-bar", "b5"));
    run_history(&mut rig, &mut view);
    assert!(on_screen(message_key("b5:e5", "5-0")) && on_screen(message_key("b5:e6", "6-0")));
    // A rewritten message opens into its diff.
    rig.click(&mut view, message_key("b5:e6", "6-0"));
    assert!(on_screen(("history-diff", "b5:e6:6-0")));
    // Closing the newest bar stays closed.
    rig.click(&mut view, ("history-bar", "b7"));
    run_history(&mut rig, &mut view);
    assert!(!state(&mut rig).open_rows.contains("b7"));
}

#[test]
fn folds_a_turn_whose_last_request_never_got_its_answer_and_opens_a_rewrite_into_its_diff() {
    let mut rig = Rig::new("h-cut");
    let response = entry(5, "response", json!({ "finishReason": "tool-calls" }), json!({ "requestId": "run_b", "answers": 4 }));
    let tool = entry(6, "tool", json!({ "name": "shell", "rewritten": true }), json!({ "requestId": "run_b", "callId": "c1" }));
    let reply = HistoryEntryDetail { entry: response.clone(), body: Some(json!({ "role": "assistant", "content": [{ "type": "text", "text": "我先看看" }, { "type": "tool-call", "toolCallId": "c1", "toolName": "shell", "input": { "command": "ls" } }] }).to_string()), ..Default::default() };
    let call = HistoryEntryDetail { entry: tool.clone(), body: Some(json!({ "input": { "command": "ls -a" }, "requestedInput": { "command": "ls" } }).to_string()), ..Default::default() };
    let reader = Rc::new(served(vec![sent(1, "第一句"), sent_request(2, "run_a", 1, "S"), (entry(3, "response", json!({}), json!({ "requestId": "run_a", "answers": 2 })), None), sent_request(4, "run_b", 1, "S"), (response, Some(reply)), (tool, Some(call)), sent_request(7, "run_b", 0, "S")]));
    let mut view = history_view(reader, None, false);
    run_history(&mut rig, &mut view);
    assert!(on_screen(("history-row", "b4:e5".to_owned())) && on_screen(("history-row", "b4:e6".to_owned())));
    rig.click(&mut view, ("history-row", "b4:e6".to_owned()));
    run_history(&mut rig, &mut view);
    assert!(on_screen(("history-diff", "b4:e6")));
}

#[test]
fn draws_the_system_prompt_only_in_the_turn_that_changed_it() {
    let mut rig = Rig::new("h-prompt");
    let reader = Rc::new(served(vec![sent(1, "第一句"), sent_request(2, "run_a", 1, "You are Mewrk."), sent(3, "第二句"), sent_request(4, "run_b", 1, "You are Mewrk."), sent(5, "第三句"), sent_request(6, "run_c", 1, "You are Mewrk, careful.")]));
    let mut view = history_view(reader, None, false);
    run_history(&mut rig, &mut view);
    // The newest turn changed it; it reads against the request before it.
    assert!(on_screen(message_key("b5", "p6-system")));
    rig.click(&mut view, ("history-bar", "b3"));
    run_history(&mut rig, &mut view);
    assert!(!on_screen(message_key("b3", "p4-system")), "the turn that changed nothing draws no prompt");
    assert!(on_screen(message_key("b3:e3", "3-0")));
}

#[test]
fn gives_what_nothing_has_sent_yet_a_bar_of_its_own() {
    let mut rig = Rig::new("h-pending");
    let reader = Rc::new(served(vec![sent(1, "第一句"), sent_request(2, "run_a", 1, "S"), sent(3, "还没发出去")]));
    let mut view = history_view(reader, None, false);
    run_history(&mut rig, &mut view);
    assert!(on_screen(("history-bar", "b1")) && on_screen(("history-bar", "b3")));
    assert!(on_screen(message_key("b3:e3", "3-0")));
}

#[test]
fn keeps_the_history_visible_when_one_entry_cannot_be_read_and_retries_it() {
    let mut rig = Rig::new("h-retry");
    let (message, message_body) = sent(1, "看看这个文件");
    let (request, request_body) = sent_request(2, "run_a", 1, "S");
    let (run, run_body) = settled(3, "run_a", vec![json!({ "kind": "assistant", "content": "好的" })]);
    let refused = Rc::new(std::cell::Cell::new(false));
    let reader = Rc::new(Fake {
        list: { let l = vec![message, request, run]; Rc::new(move || Ok(l.clone())) },
        load: Rc::new(move |seq| {
            if seq == 3 && !refused.replace(true) {
                return Err("正文读取失败".to_owned());
            }
            Ok(match seq { 1 => message_body.clone(), 2 => request_body.clone(), 3 => run_body.clone(), _ => None })
        }),
        lists: Rc::default(),
    });
    let mut view = history_view(reader, None, false);
    run_history(&mut rig, &mut view);
    // What the user sent still reads; only the settlement says it failed.
    assert!(on_screen(message_key("b1:e1", "1-0")));
    assert_eq!(state(&mut rig).detail_errors.get(&3).map(String::as_str), Some("正文读取失败"));
    rig.click(&mut view, ("history-retry", "b1:e3".to_owned()));
    run_history(&mut rig, &mut view);
    assert!(state(&mut rig).detail_errors.is_empty());
    assert!(on_screen(message_key("b1:e3", "3-0")));
}

#[test]
fn says_so_when_an_entry_is_no_longer_in_the_history() {
    let mut rig = Rig::new("h-gone");
    let (message, _) = sent(1, "x");
    let reader = Rc::new(served(vec![(message, None), sent_request(2, "run_a", 1, "S")]));
    let mut view = history_view(reader, None, false);
    run_history(&mut rig, &mut view);
    assert!(state(&mut rig).detail_errors[&1].contains("已不在历史记录里"));
}

#[test]
fn surfaces_a_read_failure_instead_of_an_empty_pane() {
    let mut rig = Rig::new("h-fail");
    let reader = Rc::new(Fake { list: Rc::new(|| Err("对话库不可用".to_owned())), load: Rc::new(|_| Ok(None)), lists: Rc::default() });
    let mut view = history_view(reader, None, false);
    run_history(&mut rig, &mut view);
    assert!(on_screen("history-failure"));
    assert_eq!(state(&mut rig).failure.as_deref(), Some("对话库不可用"));
}

#[test]
fn reads_one_agents_history_and_does_not_read_again_once_a_frame() {
    let mut rig = Rig::new("h-owner");
    let reader = Rc::new(served(vec![]));
    let mut view = history_view(reader.clone(), Some(vec!["reviewer".into()]), false);
    run_history(&mut rig, &mut view);
    assert!(on_screen("history-empty"));
    assert_eq!(*reader.lists.borrow(), [Some(vec!["reviewer".to_owned()])]);
    for _ in 0..20 {
        rig.frame(&mut view);
    }
    assert_eq!(reader.lists.borrow().len(), 1, "a pane that is not streaming reads once");
}

#[test]
fn polls_a_running_turn_and_does_not_call_it_interrupted() {
    let mut rig = Rig::new("h-poll");
    let reader = Rc::new(served(vec![sent(1, "第一句"), sent_request(2, "run_a", 1, "S"), (entry(3, "response", json!({}), json!({ "requestId": "run_a", "answers": 2 })), None), sent(4, "第二句"), sent_request(5, "run_b", 1, "S")]));
    let mut view = history_view(reader.clone(), None, true);
    run_history(&mut rig, &mut view);
    let first = reader.lists.borrow().len();
    assert!(first >= 1 && rig.last_wake.is_some(), "a running turn keeps asking for frames");
    std::thread::sleep(std::time::Duration::from_millis(1050));
    rig.frame(&mut view);
    assert!(reader.lists.borrow().len() > first);
    let bars = history_bars(&state(&mut rig).entries.clone().unwrap(), true);
    assert_eq!(bars.iter().map(|b| b.kind).collect::<Vec<_>>(), [BarKind::Turn, BarKind::Turn]);
}

#[test]
fn formats_a_turns_cost_as_it_is_shortened_upstream() {
    assert_eq!(format_history_tokens(12_400), "12k");
    assert_eq!(format_history_tokens(8_100), "8.1k");
    assert_eq!(format_history_tokens(210), "210");
    assert_eq!(format_history_tokens(2_300_000), "2.3M");
}

// ---------------------------------------------------------------- UsageStatsCard

fn hour(y: i32, m: u32, d: u32, h: u32) -> i64 {
    resolve_local(chrono::NaiveDate::from_ymd_opt(y, m, d).unwrap().and_hms_opt(h, 0, 0).unwrap()).timestamp_millis()
}

fn usage_bucket(at: i64, model: &str, total: i64) -> UsageBucket {
    UsageBucket { hour_start_ms: at, provider_id: "p1".into(), provider_name: "Provider".into(), model_id: model.into(), origin: "conversation".into(), requests: 1, total_tokens: total, input_tokens: total, ..Default::default() }
}

fn stats(models: usize) -> UsageStatistics {
    UsageStatistics {
        generated_at_ms: hour(2026, 8, 20, 12),
        ledger_started_at_ms: 0,
        activity: vec![ActivityBucket { hour_start_ms: hour(2026, 8, 19, 9), user_messages: 3, assistant_messages: 4, sessions: 1 }],
        usage: (0..models).map(|i| usage_bucket(hour(2026, 8, 19, 9), &format!("model-{i}"), 1000 - i as i64)).collect(),
    }
}

fn card_view(statistics: UsageStatistics) -> impl FnMut(&mut App) -> E {
    move |_| col().child(usage_stats_card(&statistics, false))
}

fn usage_key(key: &'static str) -> (&'static str, &'static str) {
    ("usage-card", key)
}

#[test]
fn fills_the_eight_tiles_two_columns_until_there_is_room_for_four() {
    let mut rig = Rig::new("u-tiles");
    let mut view = card_view(stats(1));
    rig.settle(&mut view);
    for key in ["sessions", "messages", "tokens", "activeDays", "currentStreak", "longestStreak", "peakHour", "favoriteModel"] {
        assert!(on_screen(("usage-tile", key)), "{key}");
    }
    let (a, b) = (rect_of(("usage-tile", "sessions")).unwrap(), rect_of(("usage-tile", "tokens")).unwrap());
    assert_eq!(a.y, b.y, "four to a row at this width");
    rig.size = (500.0, 800.0);
    rig.settle(&mut view);
    let (a, b) = (rect_of(("usage-tile", "sessions")).unwrap(), rect_of(("usage-tile", "tokens")).unwrap());
    assert!(b.y > a.y, "two to a row when squeezed");
}

#[test]
fn switches_between_its_tabs_and_ranges() {
    let mut rig = Rig::new("u-tabs");
    let mut view = card_view(stats(1));
    rig.settle(&mut view);
    assert!(on_screen("usage-heatmap") && on_screen("usage-note"));
    rig.click(&mut view, usage_key("usage-tab-models"));
    rig.settle(&mut view);
    assert!(!on_screen("usage-heatmap"));
    assert!(on_screen("usage-chart-plot"));
    assert!(on_screen(("usage-legend-row", "p1\u{0}model-0")));
    rig.click(&mut view, usage_key("usage-range-7d"));
    rig.settle(&mut view);
    // Seven bars, the last of them today.
    assert!(on_screen(("usage-chart-column", "2026-08-20")) && on_screen(("usage-chart-column", "2026-08-14")));
    rig.click(&mut view, usage_key("usage-tab-overview"));
    assert!(on_screen("usage-heatmap"));
}

#[test]
fn says_the_range_holds_no_model_usage_rather_than_drawing_an_empty_chart() {
    let mut rig = Rig::new("u-none");
    let mut view = card_view(stats(0));
    rig.settle(&mut view);
    rig.click(&mut view, usage_key("usage-tab-models"));
    rig.settle(&mut view);
    assert!(on_screen("usage-models-empty") && !on_screen("usage-chart-plot"));
}

#[test]
fn keeps_the_legend_short_until_the_rest_are_asked_for() {
    let mut rig = Rig::new("u-legend");
    let mut view = card_view(stats(9));
    rig.settle(&mut view);
    rig.click(&mut view, usage_key("usage-tab-models"));
    rig.settle(&mut view);
    assert!(on_screen(("usage-legend-row", "p1\u{0}model-5")));
    assert!(!on_screen(("usage-legend-row", "p1\u{0}model-6")));
    rig.click(&mut view, "usage-legend-more");
    assert!(on_screen(("usage-legend-row", "p1\u{0}model-8")));
    rig.click(&mut view, "usage-legend-more");
    assert!(!on_screen(("usage-legend-row", "p1\u{0}model-6")));
}

#[test]
fn hovers_and_steps_through_the_bars_and_names_the_day_only_while_one_is_chosen() {
    let mut rig = Rig::new("u-hover");
    let mut view = card_view(stats(2));
    rig.settle(&mut view);
    rig.click(&mut view, usage_key("usage-range-7d"));
    rig.click(&mut view, usage_key("usage-tab-models"));
    rig.settle(&mut view);
    rig.input.pointer = None;
    rig.settle(&mut view);
    assert!(!on_screen("usage-chart-tip"));
    // The day the tokens fell on names itself under the pointer, and a day with none has nothing to say.
    rig.hover(&mut view, ("usage-chart-column", "2026-08-19"));
    rig.settle(&mut view);
    assert!(on_screen("usage-chart-tip"));
    rig.hover(&mut view, ("usage-chart-column", "2026-08-14"));
    rig.settle(&mut view);
    assert!(!on_screen("usage-chart-tip"));
    rig.input.pointer = None;
    rig.settle(&mut view);
    // One tab stop plus arrow keys, rather than 240 focusable bars.
    rig.click(&mut view, ("usage-chart-column", "2026-08-19"));
    rig.press_key(&mut view, Key::Left, false);
    rig.settle(&mut view);
    rig.press_key(&mut view, Key::End, false);
    rig.settle(&mut view);
    rig.press_key(&mut view, Key::Left, false);
    rig.settle(&mut view);
    assert!(on_screen("usage-chart-tip"), "2026-08-19 holds tokens, so stepping onto it names them");
    rig.press_key(&mut view, Key::Escape, false);
    rig.settle(&mut view);
    assert!(!on_screen("usage-chart-tip"));
}

#[test]
fn thins_the_date_labels_keeping_the_most_recent_bar_dated() {
    let mut rig = Rig::new("u-labels");
    let mut statistics = stats(1);
    statistics.usage.push(usage_bucket(hour(2026, 7, 1, 9), "model-0", 10));
    let mut view = card_view(statistics);
    rig.settle(&mut view);
    rig.click(&mut view, usage_key("usage-tab-models"));
    rig.settle(&mut view);
    assert!(on_screen(("usage-chart-day", "2026-08-20")));
    let drawn = (0..60).filter(|i| {
        let day = chrono::NaiveDate::from_ymd_opt(2026, 7, 1).unwrap() + chrono::Duration::days(*i);
        on_screen(("usage-chart-day", day.format("%Y-%m-%d").to_string()))
    });
    assert!(drawn.count() <= 8);
}

#[test]
fn reads_buckets_in_the_apps_language() {
    assert_eq!(format_share(0.1234), "12.3%");
    let at = hour(2026, 8, 3, 0);
    assert_eq!(bucket_label_in(at, UsageBucketSize::Day, true), "Aug 3");
    assert_eq!(bucket_label_in(at, UsageBucketSize::Month, true), "Aug 2026");
    assert_eq!(bucket_title_in(at, UsageBucketSize::Week, true), "Aug 3 – Aug 9");
    assert_eq!(bucket_label_in(at, UsageBucketSize::Month, false), "2026年8月");
    assert_eq!(bucket_label_in(at, UsageBucketSize::Day, false), "8月3日");
}

// ---------------------------------------------------------------- what a reader gets

#[test]
fn a_row_that_opens_says_where_it_goes_and_its_figures_stay_out_of_it() {
    let mut rig = Rig::new("a11y-rows");
    let mut pane = Pane::new(TaskSources { agents: vec![agent("scout", running)], shell_tasks: vec![shell(|_| {})], ..Default::default() });
    pane.selected_agent = Some("scout".into());
    let mut view = pane.view();
    let tree = rig.a11y_tree(&mut view);
    let row = said(&tree, "打开子代理 scout");
    assert_eq!((row.role(), row.aria_current()), (Role::Button, Some(AriaCurrent::True)));
    assert_eq!(said(&tree, "打开“bash”").role(), Role::Button);
    assert_eq!(said(&tree, "中止“scout”").role(), Role::Button);
    assert!(!tree.iter().any(|(_, n)| n.value().is_some_and(|v| v.ends_with("工具"))), "the metrics are `aria-hidden`");
    assert_eq!(nameless(&tree), Vec::<String>::new());
    set_a11y_active(false);
}

#[test]
fn the_finished_list_says_when_it_is_open() {
    let mut rig = Rig::new("a11y-finished");
    let mut view = Pane::new(TaskSources { shell_tasks: vec![shell(|s| s.outcome = Some(ShellTaskOutcome::Succeeded))], ..Default::default() }).view();
    let tree = rig.a11y_tree(&mut view);
    let toggle = tree.iter().map(|(_, n)| n).find(|n| n.label().is_some_and(|l| l.starts_with("已完成"))).expect("the disclosure");
    assert_eq!((toggle.role(), toggle.is_expanded()), (Role::Button, Some(true)));
    set_a11y_active(false);
}

#[test]
fn a_workflow_is_a_busy_section_whose_steps_open_and_whose_log_says_it_folds() {
    let mut rig = Rig::new("a11y-workflow");
    let log = calls();
    let mut view = Panel { item: audit_item(true), progress: Some(ledger()), run_id: Some("run-abc".into()), stopping: false, log }.view();
    let tree = rig.a11y_tree(&mut view);
    let section = said(&tree, "工作流 fetch-models-gate-audit");
    assert_eq!(section.role(), Role::Region);
    assert!(section.is_busy());
    assert_eq!(said(&tree, "打开步骤 auditor:audit:rust-path").role(), Role::Button);
    assert_eq!(said(&tree, "中止工作流").role(), Role::Button);
    let toggle = tree.iter().map(|(_, n)| n).find(|n| n.label().is_some_and(|l| l.starts_with("运行日志"))).expect("the log's disclosure");
    assert_eq!(toggle.is_expanded(), Some(false));
    assert_eq!(nameless(&tree), Vec::<String>::new());
    set_a11y_active(false);
}

#[test]
fn a_subagent_page_is_named_by_its_title_and_its_links_by_where_they_go() {
    let mut rig = Rig::new("a11y-subagent");
    let agents = vec![agent("parent", |v| v.child_ids = vec!["child".into()]), agent("child", |v| { v.parent_id = Some("parent".into()); v.depth = 1 })];
    let mut view = panel_view("child", agents.clone(), false, calls());
    let tree = rig.a11y_tree(&mut view);
    assert_eq!(said(&tree, "子代理组结构").role(), Role::Navigation);
    assert_eq!(said(&tree, "返回上级子代理 parent").role(), Role::Button);
    assert_eq!(said(&tree, "返回对话").role(), Role::Button);
    assert!(tree.iter().any(|(_, n)| n.role() == Role::Region && !n.labelled_by().is_empty()), "the page goes by its title");
    assert_eq!(nameless(&tree), Vec::<String>::new());
    set_a11y_active(false);
    let mut view = panel_view("parent", agents, false, calls());
    let tree = rig.a11y_tree(&mut view);
    assert_eq!(said(&tree, "打开子代理 child").role(), Role::Button);
    set_a11y_active(false);
}

#[test]
fn the_fork_tray_is_a_polite_named_region_and_its_cards_are_not_controls() {
    let mut rig = Rig::new("a11y-fork");
    let mut view = tray_view(vec![fork_request("fork-1", "把它拆开")], calls(), true);
    let tree = rig.a11y_tree(&mut view);
    let tray = said(&tree, "分叉请求");
    assert_eq!((tray.role(), tray.live()), (Role::Region, Some(accesskit::Live::Polite)));
    assert_eq!(said(&tree, "拒绝").role(), Role::Button);
    assert_eq!(said(&tree, "批准").role(), Role::Button);
    said(&tree, "把它拆开");
    assert_eq!(nameless(&tree), Vec::<String>::new());
    set_a11y_active(false);
}

#[test]
fn a_read_that_failed_is_an_alert_and_a_fold_says_it_folds() {
    let mut rig = Rig::new("a11y-history");
    let reader = Rc::new(Fake { list: Rc::new(|| Err("对话库不可用".to_owned())), load: Rc::new(|_| Ok(None)), lists: Rc::default() });
    let mut view = history_view(reader, None, false);
    run_history(&mut rig, &mut view);
    let tree = rig.a11y_tree(&mut view);
    assert_eq!(said(&tree, "对话库不可用").role(), Role::Alert);
    set_a11y_active(false);

    let (message, message_body) = sent(1, "看看这个文件");
    let (request, request_body) = sent_request(2, "run_a", 1, "S");
    let (run, _) = settled(3, "run_a", vec![json!({ "kind": "assistant", "content": "好的" })]);
    let reader = Rc::new(Fake {
        list: { let l = vec![message, request, run]; Rc::new(move || Ok(l.clone())) },
        load: Rc::new(move |seq| match seq {
            1 => Ok(message_body.clone()),
            2 => Ok(request_body.clone()),
            _ => Err("正文读取失败".to_owned()),
        }),
        lists: Rc::default(),
    });
    let mut rig = Rig::new("a11y-history-rows");
    let mut view = history_view(reader, None, false);
    run_history(&mut rig, &mut view);
    let tree = rig.a11y_tree(&mut view);
    assert!(tree.iter().any(|(_, n)| n.role() == Role::Button && n.is_expanded() == Some(true)), "the bar that opened says so");
    assert!(tree.iter().any(|(_, n)| n.role() == Role::Button && n.is_expanded() == Some(false)), "and so does a row that has not");
    let alert = tree.iter().map(|(_, n)| n).find(|n| n.role() == Role::Alert).expect("the entry that could not be read");
    assert_eq!(alert.label(), Some("正文读取失败"));
    assert_eq!(said(&tree, "重试").role(), Role::Button);
    assert_eq!(nameless(&tree), Vec::<String>::new());
    set_a11y_active(false);
}
