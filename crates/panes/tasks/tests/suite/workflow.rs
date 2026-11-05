//! Upstream's `lib/workflowProgress.test.ts` and `lib/workflowRuns.test.ts`.

use std::rc::Rc;

use mewrk_client::model::{ModelUsage, SubagentRunKind};
use mewrk_pane_tasks::context::SubagentViewStatus as S;
use mewrk_pane_tasks::subagents::{AgentRole, SubagentView};
use mewrk_pane_tasks::task_container::*;
use mewrk_pane_tasks::util::parse_time;
use mewrk_pane_tasks::workflow_progress::{WorkflowStepState as W, *};
use mewrk_pane_tasks::workflow_runs::*;
use serde_json::json;

fn entry(v: serde_json::Value) -> WorkflowProgressEntry {
    serde_json::from_value(v).unwrap()
}

fn step(index: i64, over: serde_json::Value) -> WorkflowProgressEntry {
    let mut base = json!({ "kind": "agent", "index": index, "state": "start" });
    for (k, v) in over.as_object().unwrap() {
        base[k] = v.clone();
    }
    entry(base)
}

fn progress(entries: &[WorkflowProgressEntry]) -> WorkflowProgressView {
    derive_workflow_progress(entries, &WorkflowProgressMessages::default())
}

#[test]
fn merges_a_steps_whole_lifetime_into_one_row_that_never_moves() {
    let view = progress(&[step(0, json!({ "state": "start", "label": "读取" })), step(1, json!({ "state": "start", "label": "改写" })), step(0, json!({ "state": "progress", "label": "读取" })), step(1, json!({ "state": "done", "label": "改写" })), step(0, json!({ "state": "done", "label": "读取" }))]);
    let rows: Vec<_> = view.phases.iter().flat_map(|p| &p.steps).collect();
    assert_eq!(rows.iter().map(|r| r.index).collect::<Vec<_>>(), [0, 1]);
    assert_eq!(rows.iter().map(|r| r.state).collect::<Vec<_>>(), [W::Done, W::Done]);
    assert_eq!((view.done, view.running), (2, false));
}

#[test]
fn is_idempotent_under_a_replayed_stream() {
    let entries = [step(0, json!({ "state": "done" })), step(1, json!({ "state": "progress" }))];
    let doubled: Vec<_> = entries.iter().chain(&entries).cloned().collect();
    assert_eq!(progress(&doubled), progress(&entries));
}

#[test]
fn keeps_same_named_phases_apart_and_sorts_unphased_steps_last() {
    let view = progress(&[step(0, json!({ "phase": "检查", "phaseIndex": 0 })), step(1, json!({ "phase": "检查", "phaseIndex": 1 })), step(2, json!({}))]);
    assert_eq!(view.phases.iter().map(|p| p.phase_index).collect::<Vec<_>>(), [Some(0), Some(1), None]);
    assert_eq!(view.phases.iter().map(|p| p.heading.as_str()).collect::<Vec<_>>(), ["检查", "检查", "未分组"]);
    assert_eq!(view.phases[2].steps[0].index, 2);
}

#[test]
fn sorts_steps_by_index_regardless_of_arrival_order() {
    let view = progress(&[step(2, json!({})), step(0, json!({})), step(1, json!({}))]);
    assert_eq!(view.phases[0].steps.iter().map(|r| r.index).collect::<Vec<_>>(), [0, 1, 2]);
}

#[test]
fn carries_the_cached_blocked_and_skipped_badges_through() {
    let view = progress(&[step(0, json!({ "state": "done", "cached": true })), step(1, json!({ "state": "start", "blocked": true })), step(2, json!({ "state": "error", "skipped": true }))]);
    let rows = &view.phases[0].steps;
    assert!(rows[0].cached && rows[1].blocked && rows[2].skipped);
    assert_eq!(rows[2].state, W::Error);
    assert_eq!((view.failed, view.done), (1, 1));
}

#[test]
fn interpolates_the_live_counts_into_the_progress_label() {
    let view = progress(&[step(0, json!({ "state": "done" })), step(1, json!({ "state": "done" })), step(2, json!({ "state": "progress" }))]);
    assert_eq!(view.progress_label, "工作流进度：3 步中已完成 2 步");
    assert!(view.running);
}

#[test]
fn takes_localized_labels_from_the_options_object() {
    let m = WorkflowProgressMessages { fallback_step_label: "step".into(), unphased_heading: "ungrouped".into(), cached_badge: "cached".into(), blocked_badge: "blocked".into(), skipped_badge: "skipped".into(), progress_label: Box::new(|d, t| format!("{d}/{t}")) };
    let view = derive_workflow_progress(&[step(0, json!({})), step(1, json!({ "phaseIndex": 0, "phase": "A" }))], &m);
    assert_eq!(view.progress_label, "0/2");
    assert_eq!(view.phases.iter().map(|p| p.heading.as_str()).collect::<Vec<_>>(), ["A", "ungrouped"]);
    assert_eq!(view.phases[1].steps[0].label, "step");
}

#[test]
fn accumulates_logs_and_trims_the_oldest_past_the_window() {
    let logs: Vec<_> = (0..1200).map(|i| entry(json!({ "kind": "log", "index": i, "state": "progress", "message": format!("line {i}") }))).collect();
    let view = progress(&logs);
    assert_eq!(view.logs.len(), 500);
    assert_eq!(view.logs[0].message, "line 700");
    assert_eq!(view.logs.last().unwrap().message, "line 1199");
}

#[test]
fn drops_unknown_row_kinds_instead_of_throwing() {
    let view = progress(&[step(0, json!({ "state": "done" })), entry(json!({ "kind": "future", "index": 9, "state": "start" }))]);
    assert_eq!((view.total, view.done), (1, 1));
}

#[test]
fn projects_an_empty_stream_to_an_empty_card_rather_than_failing() {
    let view = progress(&[]);
    assert!(view.phases.is_empty());
    assert_eq!((view.total, view.running), (0, false));
}

fn now() -> i64 {
    parse_time("2026-08-01T00:20:12Z").unwrap()
}

fn agent(id: &str, f: impl FnOnce(&mut SubagentView)) -> Rc<SubagentView> {
    let mut v = SubagentView { id: id.into(), name: None, ledger_owner: None, kind: SubagentRunKind::WorkflowStep, workflow_run: false, label: id.into(), task: format!("执行 {id}"), status: S::Completed, summary: String::new(), contexts: vec![], updates: vec![], live: None, call_ids: vec![id.into()], parent_id: None, depth: 0, child_ids: vec![], phase: None, phase_index: None, step_index: None, role: None, model_id: None, script_name: None, usage: ModelUsage::default(), tool_count: Some(0), created_at: "2026-08-01T00:00:00Z".into(), completed_at: Some("2026-08-01T00:05:00Z".into()) };
    f(&mut v);
    Rc::new(v)
}

fn messages() -> TaskContainerMessages {
    task_container_messages()
}

type StepEdit = Box<dyn FnOnce(&mut SubagentView)>;

fn run_from(steps: Vec<StepEdit>) -> TaskItem {
    let child_ids: Vec<String> = (0..steps.len()).map(|i| format!("s{i}")).collect();
    let mut agents = vec![agent("run", |v| {
        v.workflow_run = true;
        v.label = "fetch-models-gate-audit".into();
        v.task = "Audit every pre-flight gate on the 拉取模型 path".into();
        v.status = S::Running;
        v.completed_at = None;
        v.child_ids = child_ids;
        v.usage.total_tokens = Some(87_200);
    })];
    for (i, edit) in steps.into_iter().enumerate() {
        agents.push(agent(&format!("s{i}"), |v| {
            v.parent_id = Some("run".into());
            v.depth = 1;
            v.call_ids = vec![format!("wf-ws{}", i + 1)];
            edit(v)
        }));
    }
    let mut items = derive_workflow_items(&agents, &messages(), now(), None);
    assert_eq!(items.len(), 1);
    items.remove(0)
}

fn phased(label: &str, phase: &str, index: i64) -> StepEdit {
    let (label, phase) = (label.to_owned(), phase.to_owned());
    Box::new(move |v| {
        v.label = label;
        v.phase = Some(phase);
        v.phase_index = Some(index);
    })
}

fn live_step(label: &str, phase: &str, index: i64) -> StepEdit {
    let first = phased(label, phase, index);
    Box::new(move |v| {
        first(v);
        v.status = S::Running;
        v.completed_at = None;
    })
}

fn ledger(entries: serde_json::Value) -> WorkflowProgressView {
    let rows: Vec<_> = entries.as_array().unwrap().iter().map(|e| entry(e.clone())).collect();
    progress(&rows)
}

#[test]
fn reads_the_plans_name_and_description_off_the_run_itself() {
    let view = derive_workflow_run(&run_from(vec![phased("a", "Audit", 0)]), None);
    assert_eq!(view.name, "fetch-models-gate-audit");
    assert_eq!(view.description, "Audit every pre-flight gate on the 拉取模型 path");
    assert_eq!(view.tokens, Some(87_200));
    assert!(view.running);
}

#[test]
fn carries_each_steps_role_its_model_its_tokens_and_its_wall_time() {
    let view = derive_workflow_run(
        &run_from(vec![Box::new(|v: &mut SubagentView| {
            v.label = "audit:rust-path".into();
            v.phase = Some("Audit".into());
            v.phase_index = Some(0);
            v.role = Some(AgentRole { name: "auditor".into(), model_id: Some("sonnet-5".into()) });
            v.usage.total_tokens = Some(87_200);
            v.created_at = "2026-08-01T00:00:00Z".into();
            v.completed_at = Some("2026-08-01T00:05:46Z".into());
        })]),
        None,
    );
    let s = &view.steps[0];
    assert_eq!((s.label.as_str(), s.role.as_deref(), s.model_id.as_deref(), s.tokens, s.state), ("audit:rust-path", Some("auditor"), Some("sonnet-5"), Some(87_200), WorkflowRunStepState::Finished));
    assert_eq!(format_run_elapsed(s.elapsed_ms.unwrap()), "5m 46s");
}

#[test]
fn leaves_the_model_absent_when_a_step_named_no_role() {
    let view = derive_workflow_run(&run_from(vec![phased("a", "P", 0)]), None);
    assert_eq!((view.steps[0].role.clone(), view.steps[0].model_id.clone()), (None, None));
}

#[test]
fn groups_by_declared_phase_index_and_counts_each_phases_finished_steps() {
    let view = derive_workflow_run(&run_from(vec![phased("a", "Audit", 0), live_step("b", "Audit", 0), live_step("c", "Verify", 1)]), None);
    let got: Vec<_> = view.phases.iter().map(|p| (p.heading.as_deref().unwrap(), p.done, p.total)).collect();
    assert_eq!(got, [("Audit", 1, 2), ("Verify", 0, 1)]);
    assert_eq!(phase_tone(&view.phases[0]), WorkflowStepTone::Running);
}

#[test]
fn adds_the_plan_slots_the_roster_cannot_have_matched_to_steps_by_label() {
    let p = ledger(json!([
        { "kind": "agent", "index": 0, "state": "done", "label": "a", "phase": "Audit", "phaseIndex": 0, "cached": true },
        { "kind": "agent", "index": 1, "state": "progress", "label": "b", "phase": "Audit", "phaseIndex": 0 },
        { "kind": "agent", "index": 2, "state": "start", "label": "c", "phase": "Audit", "phaseIndex": 0, "blocked": true },
        { "kind": "log", "index": 0, "state": "progress", "message": "2/3 done" }
    ]));
    let view = derive_workflow_run(&run_from(vec![live_step("b", "Audit", 0)]), Some(&p));
    let got: Vec<_> = view.steps.iter().map(|s| (s.label.as_str(), step_tone(s), s.agent_id.as_deref(), s.plan_index)).collect();
    assert_eq!(got, [("a", WorkflowStepTone::Cached, None, Some(0)), ("b", WorkflowStepTone::Running, Some("s0"), Some(1)), ("c", WorkflowStepTone::Pending, None, Some(2))]);
    assert_eq!(view.step_count, 3);
    assert_eq!(view.logs, ["2/3 done"]);
}

#[test]
fn gives_two_same_named_steps_one_row_each_rather_than_collapsing_them() {
    let p = ledger(json!([
        { "kind": "agent", "index": 0, "state": "done", "label": "P", "phase": "P", "phaseIndex": 0 },
        { "kind": "agent", "index": 1, "state": "progress", "label": "P", "phase": "P", "phaseIndex": 0 }
    ]));
    let view = derive_workflow_run(&run_from(vec![phased("P", "P", 0), live_step("P", "P", 0)]), Some(&p));
    let got: Vec<_> = view.steps.iter().map(|s| (s.agent_id.as_deref(), s.plan_index)).collect();
    assert_eq!(got, [(Some("s0"), Some(0)), (Some("s1"), Some(1))]);
}

#[test]
fn leaves_a_step_it_cannot_match_without_a_plan_index_rather_than_guessing_one() {
    let p = ledger(json!([{ "kind": "agent", "index": 0, "state": "start", "label": "queued", "phase": "P", "phaseIndex": 0, "blocked": true }]));
    let view = derive_workflow_run(&run_from(vec![live_step("renamed", "P", 0)]), Some(&p));
    let got: Vec<_> = view.steps.iter().map(|s| (s.label.as_str(), s.plan_index)).collect();
    assert_eq!(got, [("queued", Some(0)), ("renamed", None)]);
}

#[test]
fn keeps_the_rosters_answer_for_a_slot_both_sources_describe() {
    let p = ledger(json!([{ "kind": "agent", "index": 0, "state": "progress", "label": "a", "phase": "Audit", "phaseIndex": 0 }]));
    let view = derive_workflow_run(
        &run_from(vec![Box::new(|v: &mut SubagentView| {
            v.label = "a".into();
            v.phase = Some("Audit".into());
            v.phase_index = Some(0);
            v.role = Some(AgentRole { name: "auditor".into(), model_id: Some("sonnet-5".into()) });
            v.usage.total_tokens = Some(1_500);
        })]),
        Some(&p),
    );
    assert_eq!(view.steps.len(), 1);
    assert_eq!((view.steps[0].model_id.as_deref(), view.steps[0].tokens, view.steps[0].state), (Some("sonnet-5"), Some(1_500), WorkflowRunStepState::Finished));
}

#[test]
fn keeps_a_skipped_step_marked_as_skipped_once_its_agent_reports_back() {
    let p = ledger(json!([{ "kind": "agent", "index": 0, "state": "error", "label": "a", "phase": "P", "phaseIndex": 0, "skipped": true }]));
    let first = phased("a", "P", 0);
    let view = derive_workflow_run(&run_from(vec![Box::new(move |v: &mut SubagentView| { first(v); v.status = S::Stopped })]), Some(&p));
    assert_eq!(step_tone(&view.steps[0]), WorkflowStepTone::Skipped);
}

#[test]
fn sorts_unphased_steps_after_every_declared_phase() {
    let loose: StepEdit = Box::new(|v| v.label = "loose".into());
    let view = derive_workflow_run(&run_from(vec![loose, phased("declared", "Audit", 0)]), None);
    assert_eq!(view.phases.iter().map(|p| p.heading.as_deref()).collect::<Vec<_>>(), [Some("Audit"), None]);
}

#[test]
fn reads_a_steps_slot_and_its_run_off_its_address() {
    assert_eq!(step_address(Some("audit/ws3")), Some(("audit".to_owned(), 2)));
    assert_eq!(step_address(Some("a/b/ws1")), Some(("a/b".to_owned(), 0)));
    for not in ["helper", "audit/ws0", "audit/ws", "/ws1", "audit/ws1-as-reviewer"] {
        assert_eq!(step_address(Some(not)), None, "{not}");
    }
    assert_eq!(step_address(None), None);
}

#[test]
fn rounds_tokens_the_way_the_task_rows_do() {
    assert_eq!(format_run_tokens(999), "999");
    assert_eq!(format_run_tokens(1_240), "1.2k");
    assert_eq!(format_run_tokens(12_400), "12k");
    assert_eq!(format_run_tokens(87_200), "87k");
}

#[test]
fn spells_wall_time_with_a_space_unlike_the_compact_metric_column() {
    assert_eq!(format_run_elapsed(9_000), "9s");
    assert_eq!(format_run_elapsed(84_000), "1m 24s");
    assert_eq!(format_run_elapsed(1_212_000), "20m 12s");
    assert_eq!(format_run_elapsed(3_840_000), "1h 04m");
}
