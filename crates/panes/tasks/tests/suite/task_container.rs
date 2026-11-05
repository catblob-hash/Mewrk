//! Upstream's `lib/taskContainer.test.ts`.

use std::collections::HashMap;
use std::rc::Rc;

use crate::common::iso;
use mewrk_client::model::{ConversationPlan, ModelUsage, PlanStatus, SubagentRunKind, UserAbortedTaskMetrics, UserAbortedTaskRecord};
use mewrk_pane_tasks::context::SubagentViewStatus as S;
use mewrk_pane_tasks::shell_tasks::{ShellTaskOutcome, ShellTaskSnapshot};
use mewrk_pane_tasks::sources::{BrowserStatus, PreviewRemoteControl, PreviewServerSnapshot, PreviewServerStatus, RemoteControlPlatform, RemoteControlProtocol, RemoteControlState, TerminalPhase, TerminalSessionState};
use mewrk_pane_tasks::subagents::{AgentRole, SubagentView};
use mewrk_pane_tasks::task_container::*;
use mewrk_pane_tasks::util::parse_time;

fn now() -> i64 {
    parse_time("2026-07-20T02:00:00Z").unwrap()
}

fn messages() -> TaskContainerMessages {
    TaskContainerMessages {
        workflow_label: "工作流".into(),
        running_step_count: Box::new(|r, t| format!("{r}/{t} 个步骤进行中")),
        step_count: Box::new(|t| format!("{t} 个步骤")),
        terminal_idle: "空闲".into(),
        terminal_busy: "正在执行命令".into(),
        shell_running: "正在运行".into(),
        shell_exited: Box::new(|c| format!("已失败（退出码 {c}）")),
        shell_failed: "已失败".into(),
        preview_label: "开发服务器".into(),
        preview_starting: "启动中".into(),
        preview_running: "运行中".into(),
        preview_failed: "连接失败".into(),
        browser_label: "浏览器页面".into(),
        browser_loading: "正在加载".into(),
        browser_suspended: "已挂起".into(),
        browser_idle: "已就绪".into(),
        browser_automation: Box::new(|t| format!("模型正在操作：{t}")),
        user_aborted: "用户中止操作".into(),
        plan_label: "实施计划".into(),
        plan_drafting: "撰写中".into(),
        plan_awaiting_approval: "待批准".into(),
        plan_approved: "已批准".into(),
        plan_rejected: "已退回".into(),
        plan_updated_ago: Box::new(|m| if m == 0 { "刚刚更新".to_owned() } else { format!("{m} 分钟前更新") }),
        fork_approved: "已创建子对话 · 点击打开".into(),
        fork_declined: "用户拒绝了分叉".into(),
    }
}

pub fn agent(id: &str, f: impl FnOnce(&mut SubagentView)) -> Rc<SubagentView> {
    let mut v = SubagentView {
        id: id.into(),
        name: None,
        ledger_owner: None,
        kind: SubagentRunKind::General,
        workflow_run: false,
        label: id.into(),
        task: format!("执行 {id}"),
        status: S::Completed,
        summary: format!("{id} 的结论"),
        contexts: vec![],
        updates: vec![],
        live: None,
        call_ids: vec![id.into()],
        parent_id: None,
        depth: 0,
        child_ids: vec![],
        phase: None,
        phase_index: None,
        step_index: None,
        role: None,
        model_id: None,
        script_name: None,
        usage: ModelUsage::default(),
        tool_count: Some(0),
        created_at: "2026-07-20T01:00:00Z".into(),
        completed_at: Some("2026-07-20T01:05:00Z".into()),
    };
    f(&mut v);
    Rc::new(v)
}

fn running(v: &mut SubagentView) {
    v.status = S::Running;
    v.completed_at = None;
}

fn terminal(f: impl FnOnce(&mut TerminalSessionState)) -> TerminalSessionState {
    let mut t = TerminalSessionState { terminal_id: "terminal-1".into(), conversation_id: "conversation-1".into(), label: "终端 1".into(), phase: TerminalPhase::Running, busy: false, has_history: false, cwd: "C:/repo".into(), shell: "bash".into(), session_id: Some("session-1".into()) };
    f(&mut t);
    t
}

fn shell(f: impl FnOnce(&mut ShellTaskSnapshot)) -> ShellTaskSnapshot {
    let mut s = ShellTaskSnapshot { shell_task_id: "shell-1".into(), conversation_id: "conversation-1".into(), tool_name: "bash".into(), command: "npm install".into(), started_at: iso(now() - 90_000), ..Default::default() };
    f(&mut s);
    s
}

fn browser(f: impl FnOnce(&mut BrowserStatus)) -> BrowserStatus {
    let mut b = BrowserStatus { has_page: true, open: true, url: "https://example.test/docs".into(), title: Some("文档".into()), ..Default::default() };
    f(&mut b);
    b
}

fn server(f: impl FnOnce(&mut PreviewServerSnapshot)) -> PreviewServerSnapshot {
    let mut s = PreviewServerSnapshot { handle: "1".into(), server_id: "dev".into(), name: "dev".into(), port: 5173, status: PreviewServerStatus::Running, started_at: "2026-07-20T01:00:00Z".into(), cwd: "C:/repo".into(), session_id: Some("conversation-1".into()), machine: None, url: None, workspace: None, remote: None };
    f(&mut s);
    s
}

fn fork(f: impl FnOnce(&mut ForkDecisionRecord)) -> ForkDecisionRecord {
    let mut d = ForkDecisionRecord { fork_id: "fork-1".into(), workspace_id: "workspace-1".into(), source_conversation_id: "conversation-1".into(), title: "迁移升级脚本".into(), prompt: "迁移升级脚本\n把 v4 的迁移拆成两步。".into(), requested_at: "2026-07-20T01:40:00Z".into(), decided_at: "2026-07-20T01:41:00Z".into(), approved: true, child_conversation_id: Some("conversation-2".into()) };
    f(&mut d);
    d
}

fn aborted(f: impl FnOnce(&mut UserAbortedTaskRecord)) -> UserAbortedTaskRecord {
    let mut r = UserAbortedTaskRecord { id: "abort-1".into(), source_kind: "terminal".into(), source_identity: "terminal:terminal-1".into(), label: "构建终端".into(), detail: "正在执行命令".into(), metrics: UserAbortedTaskMetrics { child_count: None, tokens: None, tool_count: None, elapsed_ms: None }, started_at: iso(1_000), ended_at: iso(6_000), reason: "userAborted".into() };
    f(&mut r);
    r
}

fn src() -> TaskSources {
    TaskSources { now: Some(now()), ..TaskSources::default() }
}

fn derive(s: TaskSources) -> Vec<TaskItem> {
    derive_task_items(&s, &messages())
}

fn with_browser(b: BrowserStatus) -> TaskSources {
    TaskSources { browser: Some(b), browser_session_id: Some("conversation-1".into()), ..src() }
}

#[test]
fn binds_same_name_task_rows_to_their_source_conversation() {
    for workflow in [false, true] {
        let rows: Vec<_> = ["A", "B"].iter().map(|c| derive(TaskSources { conversation_id: Some((*c).into()), agents: vec![agent("same", |v| v.workflow_run = workflow)], ..TaskSources::default() }).remove(0)).collect();
        assert_eq!(rows.iter().map(|r| r.conversation_id.clone().unwrap()).collect::<Vec<_>>(), ["A", "B"]);
    }
}

#[test]
fn treats_a_ceiling_as_finished_and_only_real_terminations_as_failures() {
    assert_eq!(task_state_for_status(S::Running), TaskItemState::Running);
    assert_eq!(task_state_for_status(S::Completed), TaskItemState::Finished);
    assert_eq!(task_state_for_status(S::RoundLimit), TaskItemState::Finished);
    for s in [S::Interrupted, S::Failed, S::Stopped] {
        assert_eq!(task_state_for_status(s), TaskItemState::Failed);
    }
}

#[test]
fn renders_a_subagent_as_a_leaf_row() {
    let items = derive(TaskSources { agents: vec![agent("parent", |v| { v.child_ids = vec!["child".into()]; running(v) }), agent("child", |v| { v.parent_id = Some("parent".into()); v.depth = 1 })], ..src() });
    assert_eq!(items.len(), 1);
    assert_eq!((items[0].kind_name(), items[0].id.as_str(), items[0].state), ("subagent", "parent", TaskItemState::Running));
    assert!(items[0].children.is_empty());
    assert_eq!(flatten_task_items(&items).iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), ["parent"]);
}

#[test]
fn reports_each_of_the_four_metric_columns_from_the_agents_own_record() {
    let items = derive(TaskSources { agents: vec![agent("parent", |v| {
        v.child_ids = vec!["child".into()];
        v.usage = ModelUsage { input_tokens: Some(900), cached_input_tokens: Some(100), output_tokens: Some(340), total_tokens: Some(1240), reasoning_tokens: None };
        v.tool_count = Some(7);
        v.completed_at = Some("2026-07-20T01:02:30Z".into());
    })], ..src() });
    assert_eq!(items[0].metrics, TaskMetrics { child_count: Some(0), tokens: Some(1240), tool_count: Some(7), elapsed_ms: Some(150_000) });
}

#[test]
fn measures_a_running_agent_against_now_and_falls_back_to_the_token_breakdown() {
    let items = derive(TaskSources { agents: vec![agent("live", |v| { running(v); v.usage = ModelUsage { input_tokens: Some(500), cached_input_tokens: Some(400), output_tokens: Some(60), ..Default::default() } })], ..src() });
    assert_eq!(items[0].metrics.tokens, Some(560));
    assert_eq!(items[0].metrics.elapsed_ms, Some(3_600_000));
}

#[test]
fn leaves_the_token_and_elapsed_columns_empty_when_the_record_reported_neither() {
    let items = derive(TaskSources { agents: vec![agent("bare", |v| { v.created_at = String::new(); v.completed_at = None })], ..src() });
    assert_eq!((items[0].metrics.tokens, items[0].metrics.elapsed_ms), (None, None));
}

#[test]
fn shows_a_failed_runs_summary_as_its_hover_text_and_nothing_on_one_that_finished() {
    let items = derive(TaskSources { agents: vec![agent("broken", |v| { v.status = S::Failed; v.summary = "工具调用超时".into() }), agent("ok", |v| v.summary = "一切正常".into())], ..src() });
    assert_eq!(items[0].error.as_deref(), Some("工具调用超时"));
    assert_eq!(items[1].error, None);
}

fn step(id: &str, f: impl FnOnce(&mut SubagentView)) -> Rc<SubagentView> {
    agent(id, |v| { v.kind = SubagentRunKind::WorkflowStep; v.parent_id = Some("run".into()); v.depth = 1; f(v) })
}

fn run(f: impl FnOnce(&mut SubagentView)) -> Rc<SubagentView> {
    agent("run", |v| { v.kind = SubagentRunKind::WorkflowStep; v.workflow_run = true; f(v) })
}

#[test]
fn folds_a_workflow_runs_steps_into_one_row_grouped_by_declared_phase_order() {
    let items = derive(TaskSources { agents: vec![
        run(|v| { v.label = "review-proxy".into(); v.child_ids = vec!["verify-1".into(), "review-1".into(), "review-2".into()]; running(v) }),
        step("review-1", |v| { v.phase = Some("Review".into()); v.phase_index = Some(0) }),
        step("review-2", |v| { v.phase = Some("Review".into()); v.phase_index = Some(0); running(v) }),
        step("verify-1", |v| { v.phase = Some("Verify".into()); v.phase_index = Some(1) }),
    ], ..src() });
    assert_eq!(items.len(), 1);
    let TaskKind::Workflow { steps, phases, .. } = &items[0].kind else { panic!() };
    assert_eq!(items[0].label, "review-proxy");
    assert_eq!(items[0].detail, "1/3 个步骤进行中");
    assert_eq!(steps.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["verify-1", "review-1", "review-2"]);
    assert_eq!(phases.iter().map(|p| p.phase.as_deref().unwrap()).collect::<Vec<_>>(), ["Review", "Verify"]);
    assert_eq!(phases[0].steps.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["review-1", "review-2"]);
    assert_eq!(items[0].children.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["verify-1", "review-1", "review-2"]);
    assert_eq!(items[0].metrics.child_count, Some(3));
}

#[test]
fn collects_unphased_steps_into_a_single_anonymous_group() {
    let items = derive(TaskSources { agents: vec![run(|v| v.child_ids = vec!["a".into(), "b".into()]), step("a", |_| {}), step("b", |_| {})], ..src() });
    let TaskKind::Workflow { phases, .. } = &items[0].kind else { panic!() };
    assert_eq!(items[0].detail, "2 个步骤");
    assert_eq!(phases.len(), 1);
    assert_eq!(phases[0].phase, None);
    assert_eq!(phases[0].steps.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["a", "b"]);
}

#[test]
fn reports_a_settled_workflow_as_interrupted_when_any_step_did_not_finish() {
    let items = derive(TaskSources { agents: vec![run(|v| v.child_ids = vec!["ok".into(), "broken".into()]), step("ok", |_| {}), step("broken", |v| v.status = S::Failed)], ..src() });
    assert_eq!(items[0].state, TaskItemState::Failed);
}

#[test]
fn treats_a_lone_workflow_step_as_its_own_subagent_row() {
    let items = derive(TaskSources { agents: vec![agent("step", |v| v.kind = SubagentRunKind::WorkflowStep)], ..src() });
    assert_eq!(items[0].kind_name(), "subagent");
}

#[test]
fn keeps_a_run_that_has_spawned_no_step_a_workflow_rather_than_an_openable_row() {
    let items = derive(TaskSources { agents: vec![run(|v| { v.label = "review-proxy".into(); running(v) })], ..src() });
    assert_eq!(items.len(), 1);
    assert_eq!((items[0].kind_name(), items[0].detail.as_str()), ("workflow", "0 个步骤"));
}

#[test]
fn counts_a_live_shell_as_running_whether_or_not_a_command_is_in_flight() {
    let items = derive(TaskSources { terminals: vec![terminal(|_| {}), terminal(|t| { t.terminal_id = "terminal-2".into(); t.label = "终端 2".into(); t.busy = true }), terminal(|t| { t.terminal_id = "terminal-3".into(); t.label = "终端 3".into(); t.phase = TerminalPhase::Exited })], ..src() });
    let got: Vec<_> = items.iter().map(|i| (i.label.as_str(), i.state, i.detail.as_str())).collect();
    assert_eq!(got, [("终端 1", TaskItemState::Running, "空闲"), ("终端 2", TaskItemState::Running, "正在执行命令"), ("终端 3", TaskItemState::Finished, "空闲")]);
    assert_eq!(items[0].metrics, TaskMetrics::default());
}

#[test]
fn makes_each_dev_server_a_running_row_carrying_where_it_answers() {
    let items = derive(TaskSources { preview_servers: vec![server(|_| {}), server(|s| { s.handle = "2".into(); s.server_id = "api".into(); s.name = "api".into(); s.port = 8080; s.status = PreviewServerStatus::Starting })], ..src() });
    assert!(items.iter().all(|i| i.kind_name() == "preview" && i.state == TaskItemState::Running));
    assert_eq!(items[0].label, "dev");
    assert_eq!(items[0].detail, "运行中 · http://localhost:5173");
    assert_eq!(items[1].detail, "启动中 · http://localhost:8080");
    assert_eq!(items[0].metrics.elapsed_ms, Some(now() - parse_time("2026-07-20T01:00:00Z").unwrap()));
}

const VIEWER: &str = "http://127.0.0.1:41000/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/";

#[test]
fn a_dev_server_counts_its_uptime_from_what_the_host_reported() {
    // Read the way the shell reads it, off the host's own JSON: the browser pane's list is what the row is made from.
    let host = serde_json::json!({ "handle": "1", "serverId": "dev", "name": "dev", "port": 5173, "status": "running", "startedAt": iso(now() - 90_000), "cwd": "C:/repo" });
    let server: PreviewServerSnapshot = serde_json::from_value(host).unwrap();
    assert_eq!(server.cwd, "C:/repo");
    let items = derive(TaskSources { preview_servers: vec![server], ..src() });
    assert_eq!(items[0].metrics.elapsed_ms, Some(90_000));
}

fn remote(state: RemoteControlState, message: Option<&str>) -> PreviewRemoteControl {
    PreviewRemoteControl { protocol: RemoteControlProtocol::Adb, platform: RemoteControlPlatform::Android, target: "emulator-5554 · this computer".into(), target_display: None, label: None, state, read_only: false, launch: None, message: message.map(str::to_owned) }
}

#[test]
fn words_a_remote_control_rows_device_in_the_persons_language_when_the_host_does() {
    let items = derive(TaskSources { preview_servers: vec![server(|s| { s.handle = "r1".into(); s.name = "pixel".into(); s.port = 0; s.url = Some(VIEWER.into()); s.remote = Some(PreviewRemoteControl { target_display: Some("emulator-5554 · 本机".into()), ..remote(RemoteControlState::Ready, None) }) })], ..src() });
    assert_eq!(items[0].detail, "运行中 · Android · emulator-5554 · 本机");
}

#[test]
fn describes_a_remote_control_row_by_its_device_never_the_viewers_address_or_a_port() {
    let items = derive(TaskSources { preview_servers: vec![
        server(|s| { s.handle = "r1".into(); s.name = "pixel".into(); s.port = 0; s.url = Some(VIEWER.into()); s.remote = Some(remote(RemoteControlState::Ready, None)) }),
        server(|s| { s.handle = "r2".into(); s.name = "pixel-2".into(); s.port = 0; s.status = PreviewServerStatus::Starting; s.url = Some(format!("{VIEWER}2/")); s.remote = Some(remote(RemoteControlState::Connecting, None)) }),
        server(|s| { s.handle = "r3".into(); s.name = "pixel-3".into(); s.port = 0; s.status = PreviewServerStatus::Failed; s.url = Some(format!("{VIEWER}3/")); s.remote = Some(remote(RemoteControlState::Failed, None)) }),
    ], ..src() });
    assert_eq!(items.iter().map(|i| i.detail.as_str()).collect::<Vec<_>>(), ["运行中 · Android · emulator-5554 · this computer", "启动中 · Android · emulator-5554 · this computer", "连接失败 · Android · emulator-5554 · this computer"]);
    assert_eq!(items.iter().map(|i| i.state).collect::<Vec<_>>(), [TaskItemState::Running, TaskItemState::Running, TaskItemState::Failed]);
    for item in &items {
        assert!(!item.detail.contains("127.0.0.1") && !item.detail.contains(":0"));
        let TaskKind::Preview { address, .. } = &item.kind else { panic!() };
        assert!(!address.contains("127.0.0.1"));
    }
}

#[test]
fn gives_a_session_that_gave_up_its_reason_as_the_rows_hover_text_and_no_other_row_one() {
    let items = derive(TaskSources { preview_servers: vec![
        server(|s| { s.handle = "r1".into(); s.port = 0; s.status = PreviewServerStatus::Failed; s.url = Some(VIEWER.into()); s.remote = Some(remote(RemoteControlState::Failed, Some("adb server is not running  "))) }),
        server(|s| { s.handle = "r2".into(); s.port = 0; s.status = PreviewServerStatus::Failed; s.url = Some(format!("{VIEWER}2/")); s.remote = Some(remote(RemoteControlState::Failed, None)) }),
        server(|s| { s.handle = "r3".into(); s.port = 0; s.url = Some(format!("{VIEWER}3/")); s.remote = Some(remote(RemoteControlState::Ready, Some("reconnected after a drop"))) }),
    ], ..src() });
    assert_eq!(items.iter().map(|i| i.error.as_deref()).collect::<Vec<_>>(), [Some("adb server is not running"), None, None]);
}

fn session(id: &str, b: BrowserStatus) -> BrowserSession {
    BrowserSession { session_id: id.into(), status: b }
}

#[test]
fn pairs_a_page_with_a_remote_control_row_by_token_path() {
    let items = derive(TaskSources {
        preview_servers: vec![server(|s| { s.handle = "r1".into(); s.port = 0; s.url = Some(VIEWER.into()); s.remote = Some(remote(RemoteControlState::Ready, None)) })],
        browser_sessions: Some(vec![session("conversation-1", browser(|b| b.url = format!("{VIEWER}stream"))), session("conversation-1#agent-1", browser(|b| b.url = "http://127.0.0.1:41000/cccccccccccccccccccccccccccccccc/".into()))]),
        browser_session_id: Some("conversation-1".into()),
        ..src()
    });
    assert_eq!(items.iter().map(TaskItem::kind_name).collect::<Vec<_>>(), ["preview", "browser"]);
    let TaskKind::Browser { session_id, .. } = &items[1].kind else { panic!() };
    assert_eq!(session_id, "conversation-1#agent-1");
}

#[test]
fn drops_the_page_row_of_a_page_one_of_the_dev_servers_is_serving() {
    let items = derive(TaskSources {
        preview_servers: vec![server(|_| {})],
        browser_sessions: Some(vec![session("conversation-1", browser(|b| b.url = "http://localhost:5173/settings".into())), session("conversation-1#agent-1", browser(|_| {}))]),
        browser_session_id: Some("conversation-1".into()),
        ..src()
    });
    assert_eq!(items.iter().map(TaskItem::kind_name).collect::<Vec<_>>(), ["preview", "browser"]);
    let TaskKind::Browser { session_id, .. } = &items[1].kind else { panic!() };
    assert_eq!(session_id, "conversation-1#agent-1");
}

#[test]
fn keeps_a_suspended_browser_page_in_the_list_rather_than_dropping_it() {
    let items = derive(with_browser(browser(|b| { b.has_page = false; b.suspended = true })));
    assert_eq!(items.len(), 1);
    assert_eq!((items[0].kind_name(), items[0].label.as_str(), items[0].detail.as_str(), items[0].state), ("browser", "文档", "已挂起", TaskItemState::Running));
}

#[test]
fn paints_a_browser_page_red_only_when_it_actually_reported_an_error() {
    let loading = derive(with_browser(browser(|b| { b.loading = true; b.title = None })));
    assert_eq!((loading[0].state, loading[0].detail.as_str(), loading[0].label.as_str()), (TaskItemState::Running, "正在加载", "https://example.test/docs"));
    let failed = derive(with_browser(browser(|b| b.error = Some("ERR_NAME_NOT_RESOLVED".into()))));
    assert_eq!((failed[0].state, failed[0].error.as_deref()), (TaskItemState::Failed, Some("ERR_NAME_NOT_RESOLVED")));
}

#[test]
fn omits_the_browser_row_when_the_conversation_owns_no_page() {
    assert!(derive(with_browser(browser(|b| b.has_page = false))).is_empty());
}

#[test]
fn gives_a_blank_page_no_row() {
    assert!(derive(with_browser(browser(|b| { b.url = "about:blank".into(); b.title = None }))).is_empty());
    assert!(derive(with_browser(browser(|b| { b.url = String::new(); b.title = None }))).is_empty());
}

#[test]
fn brings_a_blank_pages_row_back_the_moment_it_has_a_history_of_its_own() {
    let cases: Vec<Box<dyn Fn(&mut BrowserStatus)>> = vec![Box::new(|b| b.loading = true), Box::new(|b| b.error = Some("ERR_NAME_NOT_RESOLVED".into())), Box::new(|b| b.can_go_back = true), Box::new(|b| b.can_go_forward = true)];
    for case in cases {
        let items = derive(with_browser(browser(|b| { b.url = "about:blank".into(); b.title = None; case(b) })));
        assert_eq!(items.iter().map(TaskItem::kind_name).collect::<Vec<_>>(), ["browser"]);
    }
}

#[test]
fn keeps_the_row_of_a_blank_page_the_model_is_holding() {
    let items = derive(TaskSources { browser_automation_tool: Some("preview_navigate".into()), ..with_browser(browser(|b| { b.url = "about:blank".into(); b.title = None })) });
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].detail, "模型正在操作：preview_navigate");
}

#[test]
fn omits_the_browser_row_when_no_session_id_names_what_stopping_it_would_close() {
    assert!(derive(TaskSources { browser: Some(browser(|_| {})), ..src() }).is_empty());
}

#[test]
fn gives_every_preview_session_its_own_row() {
    let items = derive(TaskSources { browser_sessions: Some(vec![session("conversation-1", browser(|b| b.title = Some("主页".into()))), session("conversation-1#agent-1", browser(|b| b.title = Some("Documentation".into())))]), ..src() });
    let got: Vec<_> = items.iter().map(|i| (i.id.as_str(), i.label.as_str())).collect();
    assert_eq!(got, [("preview:conversation-1", "主页"), ("preview:conversation-1#agent-1", "Documentation")]);
}

#[test]
fn attributes_automation_to_the_primary_session_not_to_every_open_tab() {
    let items = derive(TaskSources {
        browser_session_id: Some("conversation-1".into()),
        browser_sessions: Some(vec![session("conversation-1", browser(|b| b.title = Some("主页".into()))), session("conversation-1#agent-1", browser(|b| b.title = Some("Documentation".into())))]),
        browser_automation_tool: Some("preview_click".into()),
        ..src()
    });
    assert_eq!(items[0].detail, "模型正在操作：preview_click");
    assert_eq!(items[1].detail, "已就绪");
    let tool = |i: &TaskItem| match &i.kind {
        TaskKind::Browser { automation_tool, .. } => automation_tool.clone(),
        _ => None,
    };
    assert_eq!((tool(&items[0]).as_deref(), tool(&items[1])), (Some("preview_click"), None));
}

#[test]
fn runs_the_browser_row_while_the_model_drives_it_however_idle_the_page_looks() {
    let idle = derive(with_browser(browser(|_| {})));
    assert_eq!(idle[0].state, TaskItemState::Running);
    assert_eq!(running_task_items(&idle).len(), 1);
    let driven = derive(TaskSources { browser_automation_tool: Some("preview_click".into()), ..with_browser(browser(|_| {})) });
    assert_eq!((driven[0].state, driven[0].detail.as_str()), (TaskItemState::Running, "模型正在操作：preview_click"));
    assert_eq!(running_task_items(&driven).len(), 1);
    assert!(finished_task_items(&driven).is_empty());
}

#[test]
fn keeps_the_row_running_while_a_requested_automation_stop_is_still_in_flight() {
    let items = derive(TaskSources { browser_automation_stopping: true, ..with_browser(browser(|_| {})) });
    assert_eq!(items[0].state, TaskItemState::Running);
    assert!(matches!(&items[0].kind, TaskKind::Browser { automation_tool: Some(_), .. }));
}

#[test]
fn still_paints_a_driven_page_red_when_it_reported_an_error() {
    let items = derive(TaskSources { browser_automation_tool: Some("navigate".into()), ..with_browser(browser(|b| b.error = Some("ERR_CONNECTION_RESET".into()))) });
    assert_eq!(items[0].state, TaskItemState::Failed);
}

#[test]
fn tells_the_browser_rows_stop_control_what_it_is_stopping() {
    let items = derive(TaskSources { browser_automation_tool: Some("type".into()), ..with_browser(browser(|_| {})) });
    assert!(matches!(&items[0].kind, TaskKind::Browser { automation_tool: Some(t), .. } if t == "type"));
}

#[test]
fn does_not_invent_an_elapsed_duration_for_recovered_shell_history_with_an_unknown_end() {
    let items = derive(TaskSources { shell_tasks: vec![shell(|s| s.outcome = Some(ShellTaskOutcome::Failed))], ..src() });
    assert_eq!((items[0].state, items[0].metrics.elapsed_ms, items[0].ended_at.clone()), (TaskItemState::Failed, None, None));
    assert!(running_task_items(&items).is_empty());
}

#[test]
fn makes_every_running_shell_command_its_own_stoppable_row() {
    let items = derive(TaskSources { shell_tasks: vec![shell(|_| {}), shell(|s| { s.shell_task_id = "shell-2".into(); s.tool_name = "powershell".into(); s.command = "Get-Process".into() }), shell(|s| { s.shell_task_id = "shell-3".into(); s.tool_name = "pwsh".into(); s.command = "Get-Process".into() })], ..src() });
    assert_eq!(items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), ["shell-1", "shell-2", "shell-3"]);
    assert_eq!((items[0].label.as_str(), items[0].detail.as_str()), ("bash", "npm install"));
    assert_eq!((items[1].label.as_str(), items[2].label.as_str()), ("powershell", "pwsh"));
    assert_eq!(running_task_items(&items).len(), 3);
    assert!(finished_task_items(&items).is_empty());
}

#[test]
fn collapses_a_finished_shell_command_into_the_finish_list_a_failure_told_by_its_colour() {
    let items = derive(TaskSources { shell_tasks: vec![shell(|s| { s.outcome = Some(ShellTaskOutcome::Succeeded); s.exit_code = Some(0); s.ended_at = Some(iso(now() - 30_000)) }), shell(|s| { s.shell_task_id = "shell-2".into(); s.command = "npm test".into(); s.outcome = Some(ShellTaskOutcome::Failed); s.exit_code = Some(1); s.ended_at = Some(iso(now() - 10_000)) })], ..src() });
    assert!(running_task_items(&items).is_empty());
    assert_eq!(finished_task_items(&items).len(), 2);
    assert_eq!((items[0].state, items[0].detail.as_str(), items[0].error.clone()), (TaskItemState::Finished, "npm install", None));
    assert_eq!((items[1].state, items[1].detail.as_str(), items[1].error.as_deref()), (TaskItemState::Failed, "npm test", Some("已失败（退出码 1）")));
}

#[test]
fn keeps_a_stopped_command_neutral_without_a_taskbar_abort_record() {
    let items = derive(TaskSources { shell_tasks: vec![shell(|s| { s.outcome = Some(ShellTaskOutcome::Stopped); s.stopping = true; s.ended_at = Some(iso(now() - 5_000)) })], ..src() });
    assert_eq!((items[0].state, items[0].detail.as_str(), items[0].error.clone()), (TaskItemState::Finished, "npm install", None));
}

#[test]
fn retains_a_user_aborted_task_as_one_failed_historical_row() {
    let items = derive(TaskSources { terminals: vec![terminal(|_| {})], user_aborted_tasks: vec![aborted(|_| {})], ..src() });
    assert_eq!(items.len(), 1);
    assert_eq!((items[0].kind_name(), items[0].state, items[0].detail.as_str(), items[0].error.as_deref(), items[0].ended_at.clone(), items[0].metrics.elapsed_ms), ("aborted", TaskItemState::Failed, "用户中止操作 · 正在执行命令", Some("用户中止操作"), Some(iso(6_000)), Some(5_000)));
    assert!(running_task_items(&items).is_empty());
    assert_eq!(finished_task_items(&items).len(), 1);
}

#[test]
fn still_renders_a_persisted_abort_record_whose_task_kind_has_been_retired() {
    let items = derive(TaskSources { user_aborted_tasks: vec![aborted(|r| { r.source_kind = "webSearch".into(); r.source_identity = "web-search:search-1".into(); r.label = "2026 年 Web 搜索趋势".into(); r.detail = "正在搜索".into() })], ..src() });
    assert_eq!(items.len(), 1);
    assert!(matches!(&items[0].kind, TaskKind::Aborted { source_kind, .. } if source_kind == "webSearch"));
    assert_eq!((items[0].state, items[0].detail.as_str()), (TaskItemState::Failed, "用户中止操作 · 正在搜索"));
}

#[test]
fn keeps_browser_automation_history_separate_from_the_surviving_page() {
    let items = derive(TaskSources { model_request_id: Some("request-1".into()), user_aborted_tasks: vec![aborted(|r| { r.source_kind = "browser".into(); r.source_identity = "browser-automation:request-1:conversation-1".into(); r.label = "自动化页面".into() })], ..with_browser(browser(|_| {})) });
    assert_eq!(items.iter().map(TaskItem::kind_name).collect::<Vec<_>>(), ["browser", "aborted"]);
}

#[test]
fn does_not_let_closed_page_history_replace_a_newly_opened_page() {
    let items = derive(TaskSources { user_aborted_tasks: vec![aborted(|r| { r.source_kind = "browser".into(); r.source_identity = "browser-page:conversation-1".into(); r.label = "旧页面".into() })], ..with_browser(browser(|_| {})) });
    assert_eq!(items.iter().map(TaskItem::kind_name).collect::<Vec<_>>(), ["browser"]);
}

#[test]
fn freezes_a_finished_commands_elapsed_column_at_its_real_duration() {
    let tasks = vec![shell(|s| { s.started_at = iso(now() - 125_000); s.ended_at = Some(iso(now() - 25_000)); s.outcome = Some(ShellTaskOutcome::Succeeded); s.exit_code = Some(0) })];
    assert_eq!(derive(TaskSources { shell_tasks: tasks.clone(), ..src() })[0].metrics.elapsed_ms, Some(100_000));
    assert_eq!(derive(TaskSources { shell_tasks: tasks, now: Some(now() + 600_000), ..TaskSources::default() })[0].metrics.elapsed_ms, Some(100_000));
}

#[test]
fn keeps_a_stopping_shell_command_running_and_its_row_unchanged() {
    let items = derive(TaskSources { shell_tasks: vec![shell(|s| s.stopping = true)], ..src() });
    assert_eq!((items.len(), items[0].state, items[0].detail.as_str()), (1, TaskItemState::Running, "npm install"));
}

#[test]
fn reports_how_long_a_shell_command_has_been_running_from_the_hosts_start() {
    let tasks = vec![shell(|s| s.started_at = iso(now() - 125_000))];
    assert_eq!(derive(TaskSources { shell_tasks: tasks.clone(), ..src() })[0].metrics.elapsed_ms, Some(125_000));
    assert_eq!(derive(TaskSources { shell_tasks: tasks, now: Some(now() + 30_000), ..TaskSources::default() })[0].metrics.elapsed_ms, Some(155_000));
}

#[test]
fn reports_the_run_time_the_commands_machine_measured_once_it_has_ended() {
    let remote_task = shell(|s| { s.ended_at = Some(iso(now() - 10_000)); s.outcome = Some(ShellTaskOutcome::Succeeded); s.exit_code = Some(0); s.duration_ms = Some(42_500) });
    assert_eq!(derive(TaskSources { shell_tasks: vec![remote_task.clone()], ..src() })[0].metrics.elapsed_ms, Some(42_500));
    let local = ShellTaskSnapshot { duration_ms: None, ..remote_task };
    assert_eq!(derive(TaskSources { shell_tasks: vec![local], ..src() })[0].metrics.elapsed_ms, Some(80_000));
    assert_eq!(derive(TaskSources { shell_tasks: vec![shell(|_| {})], ..src() })[0].metrics.elapsed_ms, Some(90_000));
}

#[test]
fn titles_a_shell_command_by_its_shell_and_the_directory_it_ran_in() {
    let items = derive(TaskSources { shell_tasks: vec![
        shell(|s| { s.workspace_root = Some("/Users/me/web".into()); s.cwd = Some("/Users/me/web/src".into()) }),
        shell(|s| { s.shell_task_id = "shell-2".into(); s.tool_name = "powershell".into(); s.workspace_root = Some("C:\\src\\api".into()) }),
        shell(|s| s.shell_task_id = "shell-3".into()),
        shell(|s| { s.shell_task_id = "shell-4".into(); s.tool_name = "pwsh".into(); s.workspace_root = Some("C:\\src\\web".into()) }),
    ], ..src() });
    let expected = ["bash:/Users/me/web/src", "powershell:C:\\src\\api", "bash", "pwsh:C:\\src\\web"];
    assert_eq!(items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(), expected);
    let titles: Vec<String> = items.iter().map(|i| match &i.kind { TaskKind::Shell { shell } => shell_task_title(shell), _ => panic!() }).collect();
    assert_eq!(titles, expected);
    assert_eq!(items[0].detail, "npm install");
}

#[test]
fn subtitles_a_shell_command_with_the_helper_models_summary_once_it_has_one() {
    let items = derive(TaskSources { shell_tasks: vec![shell(|s| s.explanation = Some("  安装项目依赖 ".into())), shell(|s| s.shell_task_id = "shell-2".into())], ..src() });
    assert_eq!((items[0].detail.as_str(), items[1].detail.as_str()), ("安装项目依赖", "npm install"));
}

#[test]
fn records_an_approved_fork_as_a_finished_row_that_says_the_child_is_there() {
    let items = derive(TaskSources { fork_decisions: vec![fork(|_| {})], ..src() });
    assert_eq!(items.len(), 1);
    assert_eq!((items[0].kind_name(), items[0].id.as_str(), items[0].label.as_str(), items[0].detail.as_str(), items[0].state, items[0].error.clone()), ("fork", "fork:fork-1", "迁移升级脚本", "已创建子对话 · 点击打开", TaskItemState::Finished, None));
    assert_eq!((items[0].started_at.as_str(), items[0].ended_at.as_deref()), ("2026-07-20T01:40:00Z", Some("2026-07-20T01:41:00Z")));
    assert_eq!(items[0].metrics, TaskMetrics::default());
    assert!(running_task_items(&items).is_empty());
    assert_eq!(finished_task_items(&items).iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), ["fork:fork-1"]);
}

#[test]
fn records_a_declined_fork_as_its_own_row_rather_than_dropping_the_request() {
    let items = derive(TaskSources { fork_decisions: vec![fork(|d| { d.fork_id = "fork-2".into(); d.approved = false; d.child_conversation_id = None })], ..src() });
    assert_eq!((items[0].kind_name(), items[0].id.as_str(), items[0].detail.as_str(), items[0].state, items[0].error.clone()), ("fork", "fork:fork-2", "用户拒绝了分叉", TaskItemState::Finished, None));
    assert_eq!(finished_task_items(&items).len(), 1);
}

#[test]
fn titles_a_fork_by_its_prompts_first_line_when_the_record_carries_no_title() {
    assert_eq!(derive(TaskSources { fork_decisions: vec![fork(|d| d.title = String::new())], ..src() })[0].label, "迁移升级脚本");
}

#[test]
fn orders_fork_rows_by_when_each_decision_was_made() {
    let items = derive(TaskSources { fork_decisions: vec![fork(|d| { d.fork_id = "late".into(); d.decided_at = "2026-07-20T01:50:00Z".into() }), fork(|d| { d.fork_id = "early".into(); d.decided_at = "2026-07-20T01:41:00Z".into() })], ..src() });
    assert_eq!(items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), ["fork:early", "fork:late"]);
}

#[test]
fn binds_fork_rows_to_the_conversation_that_raised_them() {
    let items = derive(TaskSources { conversation_id: Some("conversation-1".into()), fork_decisions: vec![fork(|_| {})], ..src() });
    assert_eq!(items[0].conversation_id.as_deref(), Some("conversation-1"));
}

#[test]
fn leaves_the_shell_rows_other_three_columns_empty_rather_than_claiming_zero() {
    let m = derive(TaskSources { shell_tasks: vec![shell(|_| {})], ..src() })[0].metrics;
    assert_eq!((m.child_count, m.tokens, m.tool_count), (None, None, None));
}

#[test]
fn splits_running_rows_from_everything_the_finish_disclosure_hides() {
    let items = derive(TaskSources { agents: vec![agent("live", running), agent("done", |_| {}), agent("broken", |v| v.status = S::Interrupted)], terminals: vec![terminal(|t| t.phase = TerminalPhase::Exited)], ..src() });
    assert_eq!(running_task_items(&items).iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), ["live"]);
    assert_eq!(finished_task_items(&items).iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), ["done", "broken", "terminal-1"]);
}

#[test]
fn lists_finished_rows_of_every_kind_as_one_list_latest_to_finish_first() {
    let at = |ago: i64| iso(now() - ago * 1000);
    let items = derive(TaskSources {
        agents: vec![agent("early", |v| { v.created_at = at(600); v.completed_at = Some(at(500)) }), agent("late", |v| { v.created_at = at(300); v.completed_at = Some(at(20)) })],
        terminals: vec![terminal(|t| t.phase = TerminalPhase::Exited)],
        shell_tasks: vec![shell(|s| { s.outcome = Some(ShellTaskOutcome::Succeeded); s.exit_code = Some(0); s.ended_at = Some(at(100)) }), shell(|s| { s.shell_task_id = "shell-2".into(); s.outcome = Some(ShellTaskOutcome::Failed); s.exit_code = Some(2); s.ended_at = Some(at(5)) })],
        ..src()
    });
    assert_eq!(finished_task_items(&items).iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), ["shell-2", "late", "shell-1", "early", "terminal-1"]);
}

#[test]
fn titles_a_subagent_row_role_name_and_subtitles_it_with_what_it_was_asked() {
    let items = derive(TaskSources {
        agents: vec![
            agent("call-1", |v| { v.name = Some("reviewer-1".into()); v.role = Some(AgentRole { name: "reviewer".into(), model_id: Some("claude-opus".into()) }); v.task = "Review the auth\n  module for   races".into() }),
            agent("call-2", |v| { v.name = Some("helper".into()); v.task = String::new() }),
        ],
        inherited_model_id: Some("conversation-model".into()),
        ..src()
    });
    assert_eq!((items[0].label.as_str(), items[0].detail.as_str()), ("reviewer:reviewer-1", "Review the auth module for races"));
    assert_eq!((items[1].label.as_str(), items[1].detail.as_str()), ("helper", "conversation-model"));
}

#[test]
fn swaps_a_subagents_task_excerpt_for_the_local_models_title_once_it_lands() {
    let titles = Rc::new(std::cell::RefCell::new(HashMap::<String, String>::new()));
    let lookup = titles.clone();
    let lookup: ToolExplanationLookup = Rc::new(move |id| lookup.borrow().get(id).cloned());
    let d = || -> Vec<String> {
        derive(TaskSources {
            agents: vec![
                agent("reviewer-1", |v| { v.name = Some("reviewer-1".into()); v.call_ids = vec!["ctx-spawn".into()]; v.task = "Review the auth\n  module for   races".into() }),
                agent("helper", |v| { v.name = Some("helper".into()); v.call_ids = vec!["ctx-other".into()]; v.task = "Run the tests".into() }),
            ],
            tool_explanation: Some(lookup.clone()),
            ..src()
        })
        .into_iter()
        .map(|i| i.detail)
        .collect()
    };
    assert_eq!(d(), ["Review the auth module for races", "Run the tests"]);
    titles.borrow_mut().insert("ctx-spawn".into(), "  审查认证模块的竞态  ".into());
    titles.borrow_mut().insert("ctx-other".into(), "   ".into());
    assert_eq!(d(), ["审查认证模块的竞态", "Run the tests"]);
}

#[test]
fn counts_running_work_without_the_plan_which_sits_in_that_list_at_every_status() {
    let items = derive(TaskSources {
        agents: vec![agent("live", running), agent("done", |_| {})],
        terminals: vec![terminal(|t| t.phase = TerminalPhase::Exited)],
        shell_tasks: vec![shell(|_| {})],
        plan: Some(ConversationPlan { conversation_id: "conversation-1".into(), markdown: "# 计划".into(), status: PlanStatus::Draft, created_at: iso(now()), updated_at: iso(now()) }),
        plan_drafting: true,
        ..src()
    });
    assert!(running_task_items(&items).iter().any(|i| i.kind_name() == "plan"));
    assert_eq!(count_running_tasks(&items), 2);
}
