//! Upstream's `lib/subagents.test.ts`.

use std::rc::Rc;

use crate::common::ctx;
use mewrk_client::model::{ModelUsage, SubagentRunKind, SubagentRunRecord};
use mewrk_pane_tasks::context::{Ctx, SubagentViewStatus as S};
use mewrk_pane_tasks::subagents::*;
use serde_json::{json, Value};

fn msgs() -> SubagentViewMessages {
    SubagentViewMessages::default()
}

fn views(contexts: &[Ctx]) -> Vec<Rc<SubagentView>> {
    derive_subagent_views(contexts, &msgs())
}

fn result(output: &str, success: bool, at: &str) -> Value {
    json!({ "success": success, "output": output, "executedAt": at, "durationMs": 12 })
}

fn ok(output: &str) -> Value {
    result(output, true, "2026-07-14T02:00:00Z")
}

fn merge(base: Value, over: Value) -> Value {
    let mut base = base;
    if let (Some(b), Some(o)) = (base.as_object_mut(), over.as_object()) {
        for (k, v) in o {
            if v.is_null() {
                b.remove(k);
            } else {
                b.insert(k.clone(), v.clone());
            }
        }
    }
    base
}

fn subagent(over: Value) -> Ctx {
    ctx(merge(json!({ "id": "tool-subagent", "kind": "tool", "toolName": "subagent", "input": { "task": "审查路由", "label": "路由审查" }, "result": ok("最终结论"), "createdAt": "2026-07-14T01:00:00Z" }), over))
}

fn spawn(over: Value) -> Ctx {
    ctx(merge(json!({ "id": "tool-spawn", "kind": "tool", "toolName": "agent_spawn", "input": { "task": "审查 API", "name": "helper", "label": "审查" }, "result": ok("ok"), "createdAt": "2026-07-14T01:00:00Z" }), over))
}

fn tool(id: &str, name: &str, input: Value, over: Value) -> Ctx {
    ctx(merge(json!({ "id": id, "kind": "tool", "toolName": name, "input": input, "result": ok(""), "createdAt": "2026-07-14T02:00:00Z" }), over))
}

fn live() -> Value {
    json!({
        "contexts": [
            { "id": "live-read", "kind": "tool", "toolName": "read", "input": { "path": "README.md" }, "result": ok("README content"), "streaming": true, "streamStatus": "completed", "createdAt": "2026-07-14T01:20:00Z" },
            { "id": "live-answer", "kind": "assistant", "content": "正在整理结果", "streaming": true, "createdAt": "2026-07-14T01:31:00Z" }
        ],
        "updates": [{ "content": "已经完成目录扫描", "createdAt": "2026-07-14T01:30:00Z" }]
    })
}

fn kinds(view: &SubagentView) -> Vec<&'static str> {
    view.contexts.iter().map(|c| c.item.kind_str()).collect()
}

fn users(view: &SubagentView) -> Vec<String> {
    view.contexts.iter().filter(|c| c.is_user()).map(|c| c.content().unwrap().to_owned()).collect()
}

fn ids(view: &SubagentView) -> Vec<String> {
    view.contexts.iter().map(|c| c.id().to_owned()).collect()
}

#[test]
fn preserves_a_failed_steps_published_role_without_a_worker_record() {
    let streamed = json!({ "id": "workflow-step-failed", "kind": "tool", "toolName": "workflow_step", "input": { "label": "review", "task": "review the host", "role": "reviewer", "roleModelId": "sonnet-5" }, "result": ok("最终结论"), "createdAt": "2026-07-14T01:00:00Z", "streaming": true });
    let settled = merge(streamed.clone(), json!({ "streaming": false, "result": result("registration capacity exhausted", false, "2026-07-14T02:00:00Z") }));
    for card in [streamed, settled.clone(), serde_json::from_str(&settled.to_string()).unwrap()] {
        let all = views(&[ctx(card)]);
        let view = all.iter().find(|v| v.id == "workflow-step-failed").unwrap();
        assert_eq!(view.label, "review");
        assert_eq!(view.role, Some(AgentRole { name: "reviewer".into(), model_id: Some("sonnet-5".into()) }));
        assert_eq!(view.model_id.as_deref(), Some("sonnet-5"));
    }
    let roleless = merge(settled, json!({ "input": { "label": "review", "task": "review the host" } }));
    assert!(views(&[ctx(roleless)]).is_empty());
}

#[test]
fn prefers_the_persisted_transcript_pins_its_task_first_and_projects_updates_as_tools() {
    let all = views(&[subagent(json!({ "subagent": { "task": "审查路由", "status": "completed", "contexts": [{ "id": "child-answer", "kind": "assistant", "content": "路由由 router.ts 统一注册", "createdAt": "2026-07-14T01:50:00Z" }], "updates": [{ "content": "定位到入口", "createdAt": "2026-07-14T01:20:00Z" }] } }))]);
    assert_eq!(all.len(), 1);
    let v = &all[0];
    assert_eq!((v.id.as_str(), v.label.as_str(), v.task.as_str(), v.status), ("tool-subagent", "路由审查", "审查路由", S::Completed));
    assert_eq!(v.summary, "路由由 router.ts 统一注册");
    assert_eq!(v.completed_at.as_deref(), Some("2026-07-14T02:00:00Z"));
    assert_eq!(kinds(v), ["user", "tool", "assistant"]);
    assert_eq!(v.contexts[0].content(), Some("审查路由"));
    let t = v.contexts[1].tool().unwrap();
    assert_eq!(t.tool_name, "subagent_update");
    assert_eq!(t.input.get("message"), Some(&json!("定位到入口")));
    assert_eq!((t.result.success, t.result.output.as_str(), t.result.executed_at.as_str()), (true, "状态已返回给主智能体", "2026-07-14T01:20:00Z"));
    assert_eq!(v.contexts[2].id(), "child-answer");
    assert_eq!(v.updates.len(), 1);
}

#[test]
fn does_not_duplicate_a_task_already_at_the_start_of_a_persisted_transcript() {
    let v = &views(&[subagent(json!({ "subagent": { "task": "审查路由", "status": "completed", "contexts": [{ "id": "child-task", "kind": "user", "content": "审查路由", "createdAt": "2026-07-14T01:00:00Z" }], "updates": [] } }))])[0];
    assert_eq!(ids(v), ["child-task"]);
}

#[test]
fn moves_a_matching_legacy_task_to_the_front_instead_of_synthesizing_a_duplicate() {
    let v = &views(&[subagent(json!({ "subagent": { "task": "审查路由", "status": "completed", "contexts": [
        { "id": "legacy-answer", "kind": "assistant", "content": "先于任务落盘的旧记录", "createdAt": "2026-07-14T01:01:00Z" },
        { "id": "legacy-task", "kind": "user", "content": "审查路由", "createdAt": "2026-07-14T01:02:00Z" }
    ], "updates": [] } }))])[0];
    assert_eq!(ids(v), ["legacy-task", "legacy-answer"]);
    assert_eq!(users(v), ["审查路由"]);
}

#[test]
fn builds_a_task_first_live_transcript_with_structured_nested_tools() {
    let v = &views(&[subagent(json!({ "id": "stream-tool-run-call-live", "input": { "task": "核对测试" }, "result": ok(""), "streaming": true, "streamStatus": "running", "live": live() }))])[0];
    assert_eq!((v.id.as_str(), v.label.as_str(), v.status, v.summary.as_str()), ("stream-tool-run-call-live", "子代理", S::Running, "已经完成目录扫描"));
    assert_eq!(v.completed_at, None);
    assert!(v.live.is_some());
    assert_eq!(kinds(v), ["user", "tool", "tool", "assistant"]);
    assert_eq!(v.contexts[0].content(), Some("核对测试"));
    assert_eq!(v.contexts[1].tool().unwrap().tool_name, "read");
    assert_eq!(v.contexts[2].tool().unwrap().tool_name, "subagent_update");
    assert_eq!(v.contexts[3].content(), Some("正在整理结果"));
    assert!(v.contexts[3].streaming);
}

#[test]
fn keeps_running_agents_first_and_sorts_terminal_records_newest_first() {
    let all = views(&[
        subagent(json!({ "id": "old", "input": { "task": "old" }, "result": result("old", true, "2026-07-12T00:00:00Z") })),
        subagent(json!({ "id": "running", "input": { "task": "running" }, "streaming": true, "streamStatus": "running", "live": merge(live(), json!({ "updates": [] })) })),
        subagent(json!({ "id": "interrupted", "input": { "task": "interrupted" }, "result": result("stopped", false, "2026-07-14T00:00:00Z"), "subagent": { "task": "interrupted", "status": "interrupted", "contexts": [], "updates": [] } })),
    ]);
    let got: Vec<_> = all.iter().map(|v| (v.id.as_str(), v.status)).collect();
    assert_eq!(got, [("running", S::Running), ("interrupted", S::Interrupted), ("old", S::Completed)]);
}

#[test]
fn uses_the_stable_local_context_id_across_the_streaming_to_persisted_replacement() {
    let streaming = views(&[subagent(json!({ "id": "stream-tool-run-call-stable", "streaming": true, "streamStatus": "running", "live": live() }))]);
    let persisted = views(&[subagent(json!({ "id": "stream-tool-run-call-stable", "subagent": { "task": "审查路由", "status": "completed", "contexts": [], "updates": [] } }))]);
    assert_eq!(streaming[0].id, "stream-tool-run-call-stable");
    assert_eq!(persisted[0].id, streaming[0].id);
}

#[test]
fn assigns_deterministic_avatar_tones() {
    assert_eq!(hash_subagent_id("agent-a"), hash_subagent_id("agent-a"));
    assert_eq!(subagent_avatar_tone("agent-a"), subagent_avatar_tone("agent-a"));
    // FNV-1a over code points, as upstream's `Math.imul` loop computes it.
    assert_eq!(hash_subagent_id(""), 2_166_136_261);
    assert_eq!(hash_subagent_id("a"), 0xE40C_292C);
}

#[test]
fn localizes_generated_labels_placeholders_summaries_and_supplemental_updates() {
    let m = SubagentViewMessages { fallback_label: "Subagent".into(), workflow_step_label: "Workflow step".into(), missing_task: "No task description provided".into(), update_returned: "Status returned to the primary agent".into(), running: "Working".into(), interrupted: "Subagent interrupted".into(), failed: "Subagent failed".into(), stopped: "Subagent stopped".into(), round_limit: "Subagent hit its round limit".into(), completed: "Subagent completed".into() };
    let running = &derive_subagent_views(&[subagent(json!({ "input": {}, "result": ok(""), "streaming": true, "streamStatus": "running", "live": { "contexts": [], "updates": [], "status": "running" } }))], &m)[0];
    assert_eq!((running.label.as_str(), running.task.as_str(), running.summary.as_str()), ("Subagent", "", "Working"));
    assert_eq!(running.contexts[0].content(), Some("No task description provided"));
    let completed = &derive_subagent_views(&[subagent(json!({ "input": {}, "result": ok(""), "subagent": { "task": "", "status": "completed", "contexts": [], "updates": [{ "content": "Progress", "createdAt": "2026-07-14T01:30:00Z" }] } }))], &m)[0];
    assert_eq!(completed.contexts.iter().find_map(Ctx::tool).unwrap().result.output, "Status returned to the primary agent");
    let interrupted = &derive_subagent_views(&[subagent(json!({ "input": {}, "result": result("", false, "2026-07-14T02:00:00Z"), "subagent": { "task": "", "status": "interrupted", "contexts": [], "updates": [] } }))], &m)[0];
    assert_eq!(interrupted.summary, "Subagent interrupted");
    let done = &derive_subagent_views(&[subagent(json!({ "input": {}, "result": ok(""), "subagent": { "task": "", "status": "completed", "contexts": [], "updates": [] } }))], &m)[0];
    assert_eq!(done.summary, "Subagent completed");
}

#[test]
fn reports_the_records_own_token_total_and_counts_only_this_agents_tool_calls() {
    let v = &views(&[subagent(json!({ "subagent": { "task": "审查路由", "status": "completed", "contexts": [
        { "id": "child-read", "kind": "tool", "toolName": "read", "input": { "path": "src/router.ts" }, "result": ok("router source"), "createdAt": "2026-07-14T01:10:00Z" },
        { "id": "child-spawn", "kind": "tool", "toolName": "agent_spawn", "input": { "task": "深挖", "name": "deep" }, "result": ok("已派生"), "createdAt": "2026-07-14T01:20:00Z", "subagent": { "name": "deep", "task": "深挖", "status": "completed", "contexts": [
            { "id": "deep-read", "kind": "tool", "toolName": "read", "input": { "path": "src/api.ts" }, "result": ok("api source"), "createdAt": "2026-07-14T01:21:00Z" }
        ], "updates": [], "usage": { "inputTokens": 10, "outputTokens": 5, "totalTokens": 15 } } }
    ], "updates": [], "usage": { "inputTokens": 900, "cachedInputTokens": 100, "outputTokens": 340, "totalTokens": 1240 } } }))])[0];
    assert_eq!(v.usage, ModelUsage { input_tokens: Some(900), cached_input_tokens: Some(100), output_tokens: Some(340), total_tokens: Some(1240), reasoning_tokens: None });
    assert_eq!(v.tool_count, Some(2));
    assert_eq!(v.child_ids, ["deep"]);
}

#[test]
fn leaves_the_metrics_empty_for_a_record_that_never_reported_usage() {
    let v = &views(&[subagent(json!({ "subagent": { "task": "审查路由", "status": "completed", "contexts": [], "updates": [] } }))])[0];
    assert_eq!(v.usage, ModelUsage::default());
    assert_eq!(v.tool_count, Some(0));
}

#[test]
fn does_not_read_a_record_less_spawns_accepted_call_as_a_completed_child() {
    assert_eq!(views(&[spawn(json!({}))])[0].status, S::Interrupted);
    assert_eq!(views(&[spawn(json!({ "live": { "status": "running", "contexts": [], "updates": [] } }))])[0].status, S::Running);
    assert_eq!(views(&[spawn(json!({ "result": result("name taken", false, "2026-07-14T02:00:00Z") }))])[0].status, S::Interrupted);
}

#[test]
fn merges_agent_spawn_and_agent_send_continuations_into_one_view_keyed_by_name() {
    let first = spawn(json!({ "subagent": { "name": "helper", "task": "审查 API", "status": "completed", "contexts": [
        { "id": "t1-task", "kind": "user", "content": "审查 API", "createdAt": "2026-07-14T01:00:01Z" },
        { "id": "t1-answer", "kind": "assistant", "content": "初版结论", "createdAt": "2026-07-14T01:01:00Z" }
    ], "updates": [] } }));
    let send = tool("tool-send", "agent_send", json!({ "agent": "helper", "message": "补充风险" }), json!({ "subagent": { "name": "helper", "task": "审查 API", "status": "completed", "contexts": [
        { "id": "t1-task", "kind": "user", "content": "审查 API", "createdAt": "2026-07-14T01:00:01Z" },
        { "id": "t1-answer", "kind": "assistant", "content": "初版结论", "createdAt": "2026-07-14T01:01:00Z" },
        { "id": "t2-message", "kind": "user", "content": "补充风险", "createdAt": "2026-07-14T02:00:00Z" },
        { "id": "t2-answer", "kind": "assistant", "content": "修订后的结论", "createdAt": "2026-07-14T02:01:00Z" }
    ], "updates": [{ "content": "正在修订", "createdAt": "2026-07-14T02:00:30Z" }] } }));
    let all = views(&[first, send]);
    assert_eq!(all.len(), 1);
    let v = &all[0];
    assert_eq!((v.id.as_str(), v.name.as_deref(), v.label.as_str(), v.task.as_str(), v.status), ("helper", Some("helper"), "审查", "审查 API", S::Completed));
    assert_eq!(v.call_ids, ["tool-spawn", "tool-send"]);
    assert!(v.contexts.iter().any(|c| c.content() == Some("修订后的结论")));
    assert_eq!(v.updates[0].content, "正在修订");
}

fn binding() -> Value {
    json!({ "source": "user", "sourceKey": "k", "name": "code-reviewer", "revision": 1, "memoryEpoch": 1, "providerId": "p", "modelId": "sonnet-5", "memory": "none", "scopeKey": "s", "systemPromptSnapshot": "" })
}

#[test]
fn names_the_role_a_spawn_asked_for_until_its_record_supersedes_it() {
    let streaming = spawn(json!({ "input": { "task": "审查 API", "name": "helper", "agent_type": "reviewer" }, "streaming": true, "streamStatus": "completed", "live": { "contexts": [], "updates": [], "status": "running" } }));
    let v = &views(&[streaming])[0];
    assert_eq!(v.role, Some(AgentRole { name: "reviewer".into(), model_id: None }));
    assert_eq!(v.model_id, None);
    let record: Result<SubagentRunRecord, _> = serde_json::from_value(json!({ "name": "helper", "task": "审查 API", "status": "completed", "contexts": [], "updates": [], "agentDefinition": binding() }));
    // The binding's own wire form is the host's; if its required fields move this says so, rather than passing silently.
    let record = record.expect("the host's AgentDefinitionBinding fixture");
    let settled = spawn(json!({ "input": { "task": "审查 API", "name": "helper", "agent_type": "reviewer" }, "subagent": serde_json::to_value(record).unwrap() }));
    let v = &views(&[settled])[0];
    assert_eq!(v.role, Some(AgentRole { name: "code-reviewer".into(), model_id: Some("sonnet-5".into()) }));
    assert_eq!(v.model_id.as_deref(), Some("sonnet-5"));
}

#[test]
fn does_not_mint_a_view_for_a_saved_child_message_to_the_main_agent() {
    let first = spawn(json!({ "subagent": { "name": "helper", "task": "审查 API", "status": "completed", "contexts": [{ "id": "up-task", "kind": "user", "content": "审查 API", "createdAt": "2026-07-14T01:00:01Z" }], "updates": [] } }));
    let upward = tool("tool-message-up", "send_message", json!({ "message": "已定位到根因" }), json!({}));
    let all = views(&[first.clone(), upward]);
    assert_eq!(all.iter().map(|v| v.name.clone()).collect::<Vec<_>>(), [Some("helper".to_owned())]);
    assert_eq!(all[0].call_ids, ["tool-spawn"]);
    let downward = tool("tool-message-down", "send_message", json!({ "target": "helper", "message": "补充" }), json!({}));
    assert_eq!(views(&[first, downward])[0].call_ids, ["tool-spawn", "tool-message-down"]);
}

#[test]
fn keeps_a_saved_queue_only_send_message_out_of_the_transcript_until_drained() {
    let record = json!({ "name": "helper", "task": "审查 API", "status": "completed", "contexts": [
        { "id": "queue-task", "kind": "user", "content": "审查 API", "createdAt": "2026-07-14T01:00:01Z" },
        { "id": "queue-answer", "kind": "assistant", "content": "初版结论", "createdAt": "2026-07-14T01:01:00Z" }
    ], "updates": [] });
    let queued = tool("tool-queue-only", "send_message", json!({ "target": "helper", "message": "暂存这条要求" }), json!({ "subagent": record }));
    let v = &views(&[spawn(json!({ "subagent": record })), queued])[0];
    assert_eq!(v.name.as_deref(), Some("helper"));
    assert_eq!(v.call_ids, ["tool-spawn", "tool-queue-only"]);
    assert!(!users(v).iter().any(|u| u == "暂存这条要求"));
}

mod send_message_to {
    use super::*;

    fn record(status: &str, extra: Value) -> Value {
        let mut contexts = json!([
            { "id": "m-task", "kind": "user", "content": "审查 API", "createdAt": "2026-07-14T01:00:01Z" },
            { "id": "m-answer", "kind": "assistant", "content": "初版结论", "createdAt": "2026-07-14T01:01:00Z" }
        ]);
        contexts.as_array_mut().unwrap().extend(extra.as_array().cloned().unwrap_or_default());
        json!({ "name": "helper", "task": "审查 API", "status": status, "contexts": contexts, "updates": [] })
    }

    fn send(over: Value) -> Ctx {
        ctx(merge(json!({ "id": "tool-message", "kind": "tool", "toolName": "send_message", "input": { "to": "helper", "message": "补充并发风险", "summary": "并发风险" }, "result": result("Message queued for helper; it reads it before its next model request.", true, "2026-07-14T02:00:01Z"), "createdAt": "2026-07-14T02:00:00Z" }), over))
    }

    #[test]
    fn tells_the_main_agents_call_from_the_retired_child_note_by_its_recipient() {
        let c = send(json!({ "input": { "message": "已定位到根因" } }));
        assert!(is_child_main_message_context(&c.tool().unwrap()));
        let c = send(json!({}));
        assert!(!is_child_main_message_context(&c.tool().unwrap()));
        assert_eq!(send_message_recipient(&c.tool().unwrap()), "helper");
        let c = send(json!({ "input": { "target": "helper", "message": "补充" } }));
        assert!(!is_child_main_message_context(&c.tool().unwrap()));
        assert_eq!(send_message_recipient(&c.tool().unwrap()), "");
        let c = send(json!({ "input": { "message": "已定位" } }));
        assert_eq!(send_message_recipient(&c.tool().unwrap()), "");
        let c = send(json!({ "toolName": "agent_send", "input": { "to": "helper" } }));
        assert_eq!(send_message_recipient(&c.tool().unwrap()), "");
    }

    #[test]
    fn merges_a_queued_message_into_the_childs_one_view_without_writing_its_user_turn() {
        let queued = send(json!({}));
        assert!(is_queued_message_context(&queued.tool().unwrap()));
        let all = views(&[spawn(json!({ "subagent": record("completed", json!([])) })), queued]);
        assert_eq!(all.len(), 1);
        assert_eq!((all[0].id.as_str(), all[0].status), ("helper", S::Completed));
        assert_eq!(all[0].call_ids, ["tool-spawn", "tool-message"]);
        assert_eq!(users(&all[0]), ["审查 API"]);
        assert_eq!((all[0].task.as_str(), all[0].label.as_str()), ("审查 API", "审查"));
    }

    #[test]
    fn keeps_a_queued_message_from_speaking_for_the_childs_status() {
        let running = spawn(json!({ "streaming": true, "streamStatus": "completed", "live": { "contexts": [], "updates": [], "status": "running" } }));
        let streaming_queue = send(json!({ "streaming": true, "streamStatus": "completed", "live": { "contexts": [], "updates": [] } }));
        assert_eq!(views(&[running.clone(), streaming_queue])[0].status, S::Running);
        assert_eq!(views(&[running, send(json!({}))])[0].status, S::Running);
        assert_eq!(views(&[spawn(json!({})), send(json!({}))])[0].status, S::Interrupted);
    }

    #[test]
    fn does_not_open_a_phantom_view_for_a_queued_or_refused_message_with_no_child() {
        assert!(views(&[send(json!({}))]).is_empty());
        assert!(views(&[send(json!({ "result": result("There is no child agent named helper.", false, "2026-07-14T02:00:00Z") }))]).is_empty());
        let all = views(&[spawn(json!({ "subagent": record("completed", json!([])) })), send(json!({ "input": { "to": "other", "message": "你好" } }))]);
        assert_eq!(all.iter().map(|v| v.id.as_str()).collect::<Vec<_>>(), ["helper"]);
        assert_eq!(all[0].call_ids, ["tool-spawn"]);
    }

    #[test]
    fn lets_a_resumed_runs_cumulative_record_supersede_the_spawn_cards() {
        let resumed = send(json!({ "result": ok("helper had finished; it was resumed in the background with your message and its full context."), "subagent": record("stopped", json!([
            { "id": "m-user-2", "kind": "user", "content": "补充并发风险", "createdAt": "2026-07-14T02:00:02Z" },
            { "id": "m-answer-2", "kind": "assistant", "content": "补充后的结论", "createdAt": "2026-07-14T02:01:00Z" }
        ])) }));
        assert!(!is_queued_message_context(&resumed.tool().unwrap()));
        let all = views(&[spawn(json!({ "subagent": record("completed", json!([])) })), resumed]);
        assert_eq!(all.len(), 1);
        let v = &all[0];
        assert_eq!((v.id.as_str(), v.task.as_str(), v.status, v.summary.as_str()), ("helper", "审查 API", S::Stopped, "补充后的结论"));
        assert_eq!(v.call_ids, ["tool-spawn", "tool-message"]);
        assert_eq!(users(v), ["审查 API", "补充并发风险"]);
        assert_eq!(v.contexts.iter().filter(|c| c.id() == "m-task").count(), 1);
    }

    #[test]
    fn keys_a_resumed_run_that_has_only_streamed_so_far_by_to() {
        let resuming = send(json!({ "streaming": true, "streamStatus": "completed", "live": { "status": "running", "contexts": [
            { "id": "m-user-2", "kind": "user", "content": "补充并发风险", "createdAt": "2026-07-14T02:00:02Z" },
            { "id": "m-live", "kind": "assistant", "content": "正在补充", "streaming": true, "createdAt": "2026-07-14T02:00:10Z" }
        ], "updates": [] } }));
        assert!(!is_queued_message_context(&resuming.tool().unwrap()));
        let all = views(&[spawn(json!({ "subagent": record("completed", json!([])) })), resuming]);
        assert_eq!(all.len(), 1);
        let v = &all[0];
        assert_eq!((v.id.as_str(), v.status), ("helper", S::Running));
        assert_eq!(v.live.as_ref().unwrap().contexts.iter().map(Ctx::id).collect::<Vec<_>>(), ["m-user-2", "m-live"]);
        assert_eq!(users(v), ["审查 API", "补充并发风险"]);
        assert!(ids(v).iter().any(|i| i == "m-answer") && ids(v).iter().any(|i| i == "m-live"));
        assert_eq!(v.completed_at, None);
    }

    #[test]
    fn lets_a_resumed_run_stand_without_its_spawn_card() {
        let all = views(&[send(json!({ "subagent": record("completed", json!([])) }))]);
        assert_eq!(all.iter().map(|v| v.id.as_str()).collect::<Vec<_>>(), ["helper"]);
        assert_eq!(all[0].call_ids, ["tool-message"]);
    }

    #[test]
    fn keeps_every_legacy_target_card_reading_as_before_beside_the_new_form() {
        let legacy = send(json!({ "id": "tool-legacy", "input": { "target": "helper", "message": "旧的排队" } }));
        let v = &views(&[spawn(json!({ "subagent": record("completed", json!([])) })), legacy.clone(), send(json!({}))])[0];
        assert_eq!(v.call_ids, ["tool-spawn", "tool-legacy", "tool-message"]);
        assert_eq!(views(&[legacy]).iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["helper"]);
    }
}

#[test]
fn keeps_the_persisted_conversation_visible_while_a_continuation_streams() {
    let first = spawn(json!({ "subagent": { "name": "helper", "task": "审查 API", "status": "completed", "contexts": [
        { "id": "old-task", "kind": "user", "content": "审查 API", "createdAt": "2026-07-14T01:00:01Z" },
        { "id": "old-answer", "kind": "assistant", "content": "上一轮结论", "createdAt": "2026-07-14T01:01:00Z" }
    ], "updates": [{ "content": "上一轮已完成", "createdAt": "2026-07-14T01:00:30Z" }] } }));
    let send = tool("tool-send-live", "agent_send", json!({ "agent": "helper", "message": "补充检查并发边界" }), json!({ "result": result("消息已发送给子代理 helper。", true, "2026-07-14T02:00:01Z"), "streaming": true, "streamStatus": "completed", "live": { "contexts": [
        { "id": "new-reasoning", "kind": "reasoning", "content": "检查并发交接", "createdAt": "2026-07-14T02:00:10Z" },
        { "id": "new-answer", "kind": "assistant", "content": "正在补充并发结论", "streaming": true, "createdAt": "2026-07-14T02:00:20Z" }
    ], "updates": [{ "content": "正在检查并发", "createdAt": "2026-07-14T02:00:15Z" }], "status": "running" } }));
    let v = &views(&[first, send])[0];
    assert_eq!(v.status, S::Running);
    assert_eq!(users(v), ["审查 API", "补充检查并发边界"]);
    assert!(ids(v).iter().any(|i| i == "old-answer") && ids(v).iter().any(|i| i == "new-answer"));
    assert_eq!(v.updates.iter().map(|u| u.content.as_str()).collect::<Vec<_>>(), ["上一轮已完成", "正在检查并发"]);
}

#[test]
fn orders_a_queued_message_inside_the_live_child_conversation() {
    let first = spawn(json!({ "streaming": true, "streamStatus": "completed", "live": { "contexts": [
        { "id": "before-send", "kind": "assistant", "content": "第一轮结论", "createdAt": "2026-07-14T01:04:00Z" },
        { "id": "after-send", "kind": "assistant", "content": "已处理追加指令", "streaming": true, "createdAt": "2026-07-14T01:06:00Z" }
    ], "updates": [], "status": "running" } }));
    let send = tool("tool-send-queued", "agent_send", json!({ "agent": "helper", "message": "继续检查取消竞态" }), json!({ "result": result("消息已排队。", true, "2026-07-14T01:05:01Z"), "streaming": true, "streamStatus": "completed", "live": { "contexts": [], "updates": [] }, "createdAt": "2026-07-14T01:05:00Z" }));
    let v = &views(&[first, send])[0];
    assert_eq!(v.status, S::Running);
    let text: Vec<_> = v.contexts.iter().filter_map(Ctx::content).collect();
    assert_eq!(text, ["审查 API", "第一轮结论", "继续检查取消竞态", "已处理追加指令"]);
    assert_eq!(v.live.as_ref().unwrap().contexts.iter().map(Ctx::id).collect::<Vec<_>>(), ["before-send", "after-send"]);
}

#[test]
fn does_not_duplicate_an_agent_send_message_when_a_record_is_backfilled_onto_an_older_call() {
    let first = spawn(json!({ "subagent": { "name": "helper", "task": "审查 API", "status": "completed", "contexts": [
        { "id": "backfill-task", "kind": "user", "content": "审查 API", "createdAt": "2026-07-14T01:00:01Z" },
        { "id": "backfill-first", "kind": "assistant", "content": "第一轮完成", "createdAt": "2026-07-14T01:04:00Z" },
        { "id": "backfill-send", "kind": "user", "content": "追加验证", "createdAt": "2026-07-14T01:05:01Z" },
        { "id": "backfill-second", "kind": "assistant", "content": "追加验证完成", "createdAt": "2026-07-14T01:06:00Z" }
    ], "updates": [] } }));
    let send = tool("tool-send-backfilled", "agent_send", json!({ "agent": "helper", "message": "追加验证" }), json!({ "result": result("消息已排队。", true, "2026-07-14T01:05:00Z"), "createdAt": "2026-07-14T01:05:00Z" }));
    let v = &views(&[first, send])[0];
    assert_eq!(users(v).iter().filter(|u| *u == "追加验证").count(), 1);
    assert_eq!(v.status, S::Completed);
}

#[test]
fn keeps_a_running_child_running_when_a_call_addressed_to_it_fails() {
    let rejected = tool("tool-send-rejected", "agent_send", json!({ "agent": "helper", "message": "补充要求" }), json!({ "result": result("子代理正忙，消息未送达。", false, "2026-07-14T01:05:00Z"), "streaming": true, "streamStatus": "completed", "createdAt": "2026-07-14T01:05:00Z" }));
    let running = spawn(json!({ "streaming": true, "streamStatus": "completed", "live": { "contexts": [], "updates": [], "status": "running" } }));
    assert_eq!(views(&[running, rejected.clone()])[0].status, S::Running);
    let finished = spawn(json!({ "streaming": true, "streamStatus": "completed", "live": { "contexts": [], "updates": [], "status": "idle" } }));
    assert_eq!(views(&[finished, rejected])[0].status, S::Completed);
}

#[test]
fn does_not_let_a_failed_tool_call_inside_the_child_end_the_childs_run() {
    let running = spawn(json!({ "streaming": true, "streamStatus": "completed", "live": { "contexts": [
        { "id": "live-failing-read", "kind": "tool", "toolName": "read", "input": { "path": "missing.md" }, "result": result("文件不存在", false, "2026-07-14T01:21:00Z"), "streaming": true, "streamStatus": "completed", "createdAt": "2026-07-14T01:21:00Z" }
    ], "updates": [], "status": "running" } }));
    assert_eq!(views(&[running])[0].status, S::Running);
}

#[test]
fn reports_the_terminal_lifecycle_status_a_child_actually_reached() {
    for (wire, status) in [("failed", S::Failed), ("stopped", S::Stopped), ("roundLimit", S::RoundLimit)] {
        let c = spawn(json!({ "streaming": true, "streamStatus": "completed", "live": { "contexts": [], "updates": [], "status": wire } }));
        assert_eq!(views(&[c])[0].status, status);
    }
}

#[test]
fn keeps_the_agent_running_while_its_call_completed_but_the_child_still_streams() {
    let c = |status: &str| spawn(json!({ "streaming": true, "streamStatus": "completed", "live": { "contexts": [], "updates": [], "status": status } }));
    assert_eq!(views(&[c("running")])[0].status, S::Running);
    let idle = spawn(json!({ "streaming": true, "streamStatus": "completed", "live": { "contexts": [{ "id": "idle-answer", "kind": "assistant", "content": "结论", "createdAt": "2026-07-14T01:01:00Z" }], "updates": [], "status": "idle" } }));
    assert_eq!(views(&[idle])[0].status, S::Completed);
    assert_eq!(views(&[c("interrupted")])[0].status, S::Interrupted);
}

#[test]
fn does_not_mark_an_already_running_agent_complete_during_a_hand_off() {
    let first = spawn(json!({ "streaming": true, "streamStatus": "completed", "live": { "contexts": [], "updates": [], "status": "running" } }));
    let send = tool("tool-send-running", "agent_send", json!({ "agent": "helper", "message": "再检查一遍边界条件" }), json!({ "streaming": true, "streamStatus": "completed", "live": { "contexts": [], "updates": [] }, "createdAt": "2026-07-14T01:05:00Z" }));
    let v = &views(&[first, send])[0];
    assert_eq!(v.status, S::Running);
    assert_eq!(v.call_ids, ["tool-spawn", "tool-send-running"]);
}

#[test]
fn derives_a_view_from_a_lone_continuation_and_finds_it_by_any_call_id() {
    let send = tool("tool-send-only", "agent_send", json!({ "agent": "a9", "message": "继续上次的分析" }), json!({ "subagent": { "name": "a9", "task": "历史任务", "status": "interrupted", "contexts": [
        { "id": "old-task", "kind": "user", "content": "历史任务", "createdAt": "2026-07-13T01:00:00Z" },
        { "id": "new-message", "kind": "user", "content": "继续上次的分析", "createdAt": "2026-07-14T01:00:00Z" }
    ], "updates": [] }, "createdAt": "2026-07-14T01:00:00Z" }));
    let all = views(&[send]);
    assert_eq!((all.len(), all[0].id.as_str(), all[0].task.as_str(), all[0].status), (1, "a9", "历史任务", S::Interrupted));
    assert!(Rc::ptr_eq(find_subagent_view(&all, Some("tool-send-only")).unwrap(), &all[0]));
    assert!(Rc::ptr_eq(find_subagent_view(&all, Some("a9")).unwrap(), &all[0]));
    assert!(find_subagent_view(&all, Some("ghost")).is_none());
}

#[test]
fn projects_workflow_steps_as_specialized_read_only_child_views() {
    let step = tool("tool-workflow-step", "workflow_step", json!({ "task": "Inspect the scheduler." }), json!({ "subagent": { "kind": "workflowStep", "task": "Inspect the scheduler.", "status": "completed", "contexts": [], "updates": [] } }));
    let v = &views(&[step])[0];
    assert_eq!((v.id.as_str(), v.name.as_deref(), v.kind, v.label.as_str(), v.task.as_str(), v.status), ("tool-workflow-step", None, SubagentRunKind::WorkflowStep, "工作流步骤", "Inspect the scheduler.", S::Completed));
}

#[test]
fn keeps_an_externalized_workflow_step_visible_and_trusts_its_input_status() {
    let input = json!({ "task": "Inspect the scheduler.", "runId": "wf-run-1", "stepIndex": 0, "status": "interrupted", "outputBytes": 620000, "outputSha256": "ab12" });
    let all = views(&[tool("tool-external-step", "workflow_step", input, json!({ "result": ok("部分结论") }))]);
    assert_eq!(all.len(), 1);
    assert_eq!((all[0].id.as_str(), all[0].kind, all[0].status), ("tool-external-step", SubagentRunKind::WorkflowStep, S::Interrupted));
    // Without coordinates it is a childless helper call and stays hidden.
    assert!(views(&[tool("tool-external-step", "workflow_step", json!({ "task": "Inspect the scheduler.", "status": "interrupted" }), json!({}))]).is_empty());
}

#[test]
fn identifies_a_workflow_step_by_its_run_scoped_shell_address_not_its_pool_name() {
    let streaming = tool("tool-step-live", "workflow_step", json!({ "label": "Review", "ledger": "audit/ws1-as-reviewer", "task": "Inspect the scheduler." }), json!({ "streaming": true, "live": { "status": "running", "contexts": [], "updates": [], "usageByRound": {} } }));
    let v = &views(&[streaming])[0];
    assert_eq!((v.id.as_str(), v.name.as_deref(), v.ledger_owner.as_deref(), v.label.as_str()), ("audit/ws1-as-reviewer", None, Some("audit/ws1-as-reviewer"), "Review"));
    let input = json!({ "label": "Review", "ledger": "audit/ws1-as-reviewer", "task": "Inspect the scheduler.", "runId": "audit", "stepIndex": 0, "status": "completed", "outputBytes": 620000, "outputSha256": "ab12" });
    let v = &views(&[tool("tool-step-live", "workflow_step", input.clone(), json!({ "result": ok("预览") }))])[0];
    assert_eq!((v.id.as_str(), v.ledger_owner.as_deref()), ("audit/ws1-as-reviewer", Some("audit/ws1-as-reviewer")));
    // Once the body is grafted back the record names the step by its pool name, and nothing follows it.
    let grafted = tool("tool-step-live", "workflow_step", input.clone(), json!({ "subagent": { "kind": "workflowStep", "name": "ws1-as-reviewer", "task": "Inspect the scheduler.", "status": "completed", "contexts": [], "updates": [] } }));
    let v = &views(&[grafted])[0];
    assert_eq!((v.id.as_str(), v.name.as_deref(), v.ledger_owner.as_deref()), ("audit/ws1-as-reviewer", None, Some("audit/ws1-as-reviewer")));
    // A shell from before the address names no ledger: the pane asks for nothing rather than the conversation's rows.
    let older = json!({ "label": "Review", "task": "Inspect the scheduler.", "runId": "audit", "stepIndex": 0, "status": "completed" });
    let v = &views(&[tool("tool-step-live", "workflow_step", older, json!({}))])[0];
    assert_eq!((v.id.as_str(), v.name.as_deref(), v.ledger_owner.as_deref()), ("tool-step-live", None, None));
    // A `ledger` argument the model wrote into its own call is not read.
    let spawned = tool("tool-spawn-live", "agent_spawn", json!({ "name": "helper", "prompt": "帮忙", "ledger": "audit/ws1" }), json!({ "streaming": true, "live": { "status": "running", "contexts": [], "updates": [], "usageByRound": {} } }));
    let v = &views(&[spawned])[0];
    assert_eq!((v.id.as_str(), v.name.as_deref(), v.ledger_owner.as_deref()), ("helper", Some("helper"), Some("helper")));
}

#[test]
fn keeps_two_runs_first_steps_apart_though_both_pools_call_them_ws1() {
    let step = |id: &str, ledger: &str, task: &str| json!({ "id": id, "kind": "tool", "toolName": "workflow_step", "input": { "label": "Review", "ledger": ledger, "task": task }, "result": ok("完成"), "subagent": { "kind": "workflowStep", "name": "ws1", "task": task, "status": "completed", "contexts": [{ "id": format!("{id}-answer"), "kind": "assistant", "content": task, "createdAt": "2026-08-04T01:00:02Z" }], "updates": [] }, "createdAt": "2026-08-04T01:00:01Z" });
    let run = |id: &str, name: &str, child: Value| ctx(json!({ "id": id, "kind": "tool", "toolName": "workflow", "input": { "name": name, "scriptName": "review", "scriptBytes": 2048, "scriptSha256": "0f1e" }, "result": ok("{\"status\":\"complete\"}"), "subagent": { "kind": "workflowStep", "task": "Review.", "status": "completed", "contexts": [child], "updates": [] }, "createdAt": "2026-08-04T01:00:00Z" }));
    let all = views(&[run("tool-workflow-1", "audit", step("tool-wf1-ws1", "audit/ws1", "第一次运行")), run("tool-workflow-2", "audit-2", step("tool-wf2-ws1", "audit-2/ws1", "第二次运行"))]);
    let step_ids: Vec<_> = all.iter().filter(|v| !v.workflow_run).map(|v| v.id.as_str()).collect();
    assert!(step_ids.contains(&"audit/ws1") && step_ids.contains(&"audit-2/ws1"));
    let unique: std::collections::HashSet<_> = all.iter().map(|v| v.id.as_str()).collect();
    assert_eq!(unique.len(), all.len());
    assert_eq!(find_subagent_view(&all, Some("audit/ws1")).unwrap().task, "第一次运行");
    assert_eq!(find_subagent_view(&all, Some("audit-2/ws1")).unwrap().task, "第二次运行");
    assert_eq!(find_subagent_view(&all, Some("audit")).unwrap().child_ids, ["audit/ws1"]);
    assert_eq!(find_subagent_view(&all, Some("audit-2")).unwrap().child_ids, ["audit-2/ws1"]);
    let c = ctx(step("tool-wf2-ws1", "audit-2/ws1", "第二次运行"));
    assert_eq!(agent_timeline_route_id(&c.tool().unwrap()), "audit-2/ws1");
}

#[test]
fn reads_an_externalized_steps_tokens_off_its_shell_without_loading_the_body() {
    let input = json!({ "task": "Inspect the scheduler.", "runId": "wf-run-1", "stepIndex": 0, "status": "completed", "usage": { "inputTokens": 8100, "cachedInputTokens": 6000, "outputTokens": 420, "totalTokens": 8520 }, "toolUseCount": 7, "outputBytes": 620000, "outputSha256": "ab12" });
    let v = &views(&[tool("tool-external-step", "workflow_step", input, json!({ "result": ok("预览") }))])[0];
    assert_eq!(v.usage, ModelUsage { input_tokens: Some(8100), cached_input_tokens: Some(6000), output_tokens: Some(420), total_tokens: Some(8520), reasoning_tokens: None });
    assert_eq!(v.tool_count, Some(7));
    let older = json!({ "task": "Inspect the scheduler.", "runId": "wf-run-1", "stepIndex": 0, "status": "completed", "outputBytes": 620000, "outputSha256": "ab12" });
    let v = &views(&[tool("tool-external-step", "workflow_step", older, json!({}))])[0];
    assert_eq!(v.usage, ModelUsage::default());
    assert_eq!(v.tool_count, None);
}

#[test]
fn reports_a_running_agents_streamed_usage_before_any_record_exists() {
    let v = &views(&[subagent(json!({ "id": "stream-tool-run-call-live", "input": { "task": "核对测试" }, "result": ok(""), "streaming": true, "streamStatus": "running", "live": merge(live(), json!({ "usageByRound": { "0": { "inputTokens": 1200, "outputTokens": 90 }, "1": { "inputTokens": 1500, "outputTokens": 40 } } })) }))])[0];
    assert_eq!(v.status, S::Running);
    assert_eq!((v.usage.input_tokens, v.usage.output_tokens, v.usage.total_tokens), (Some(2700), Some(130), None));
}

#[test]
fn lets_a_settled_record_supersede_the_live_snapshots_it_replaces() {
    let v = &views(&[subagent(json!({ "streaming": true, "streamStatus": "completed", "live": merge(live(), json!({ "usageByRound": { "0": { "inputTokens": 1200, "outputTokens": 90 } } })), "subagent": { "task": "审查路由", "status": "completed", "contexts": [], "updates": [], "usage": { "inputTokens": 1200, "outputTokens": 90, "totalTokens": 1290 } } }))])[0];
    assert_eq!((v.usage.input_tokens, v.usage.output_tokens, v.usage.total_tokens), (Some(1200), Some(90), Some(1290)));
}

#[test]
fn grafts_a_loaded_body_onto_its_externalized_step_and_stops_advertising_coordinates() {
    let contexts = [ctx(json!({ "id": "tool-workflow", "kind": "tool", "toolName": "workflow", "input": { "scriptName": "review-proxy", "scriptBytes": 4096, "scriptSha256": "0f1e" }, "result": ok("{\"status\":\"complete\"}"), "subagent": { "kind": "workflowStep", "task": "Review the proxy.", "status": "completed", "contexts": [
        { "id": "tool-external-step", "kind": "tool", "toolName": "workflow_step", "input": { "task": "Inspect the scheduler.", "runId": "wf-run-1", "stepIndex": 1, "status": "completed" }, "result": ok("预览"), "createdAt": "2026-08-04T01:00:00Z" }
    ], "updates": [] }, "createdAt": "2026-08-04T01:00:00Z" }))];
    let call = |id: &str| vec![id.to_owned()];
    assert_eq!(find_external_step_body_ref(&contexts, &call("tool-external-step")), Some(ExternalStepBodyRef { run_id: "wf-run-1".into(), step_index: 1 }));
    assert_eq!(find_external_step_body_ref(&contexts, &call("tool-workflow")), None);
    let record: SubagentRunRecord = serde_json::from_value(json!({ "kind": "workflowStep", "task": "Inspect the scheduler.", "status": "completed", "contexts": [{ "id": "step-answer", "kind": "assistant", "content": "完整正文", "createdAt": "2026-08-04T01:00:01Z" }], "updates": [] })).unwrap();
    let grafted = graft_external_step_bodies(&contexts, &|r| (r.run_id == "wf-run-1" && r.step_index == 1).then(|| record.clone()));
    let run = grafted[0].tool().unwrap();
    let step = mewrk_pane_tasks::context::tool_of(&run.subagent.unwrap().contexts[0]).unwrap();
    assert_eq!(step.subagent, Some(&record));
    assert_eq!(find_external_step_body_ref(&grafted, &call("tool-external-step")), None);
    // The original tree is untouched, and a lookup that answers nothing hands the input back by reference.
    assert_eq!(find_external_step_body_ref(&contexts, &call("tool-external-step")).is_some(), true);
    assert!(matches!(graft_external_step_bodies(&contexts, &|_| None), std::borrow::Cow::Borrowed(_)));
}

#[test]
fn names_a_workflow_run_after_its_plan_whose_body_the_public_input_strips_away() {
    let run = tool("tool-workflow", "workflow", json!({ "scriptName": "review-proxy", "scriptBytes": 4096, "scriptSha256": "0f1e" }), json!({ "subagent": { "kind": "workflowStep", "task": "Review the proxy.", "status": "completed", "contexts": [], "updates": [] }, "createdAt": "2026-07-29T01:00:00Z" }));
    let v = &views(&[run])[0];
    assert_eq!((v.kind, v.label.as_str()), (SubagentRunKind::WorkflowStep, "review-proxy"));
}

#[test]
fn keeps_a_renamed_run_out_of_the_row_belonging_to_the_name_it_was_renamed_off() {
    let input = json!({ "name": "sweep", "scriptName": "sweep-plan", "scriptBytes": 512, "scriptSha256": "ab" });
    let settled = tool("tool-workflow-sweep", "workflow", input.clone(), json!({ "subagent": { "name": "sweep", "kind": "workflowStep", "task": "扫一遍", "status": "completed", "contexts": [], "updates": [] }, "createdAt": "2026-09-01T01:00:00Z" }));
    let renamed = tool("tool-workflow-sweep-2", "workflow", input, json!({ "result": ok("workflow:sweep-2"), "streaming": true, "streamStatus": "completed", "live": { "contexts": [], "updates": [], "status": "running" }, "createdAt": "2026-09-01T02:00:00Z" }));
    let all = views(&[settled, renamed]);
    assert_eq!(all.iter().map(|v| v.id.as_str()).collect::<Vec<_>>(), ["tool-workflow-sweep-2", "sweep"]);
    assert_eq!(all[0].call_ids, ["tool-workflow-sweep-2"]);
    assert_eq!(all[1].call_ids, ["tool-workflow-sweep"]);
    assert_eq!((all[0].status, all[1].status), (S::Running, S::Completed));
}

#[test]
fn does_not_restate_the_task_on_a_card_stamped_after_the_childs_first_turn() {
    let c = ctx(json!({ "id": "tool-spawn-late-card", "kind": "tool", "toolName": "agent_spawn", "input": { "name": "helper", "prompt": "审查 API" }, "result": result("子代理 helper 已启动", true, "2026-08-26T01:00:05Z"), "subagent": { "name": "helper", "task": "审查 API", "status": "completed", "contexts": [
        { "id": "child-task", "kind": "user", "content": "审查 API", "createdAt": "2026-08-26T01:00:01Z" },
        { "id": "child-answer", "kind": "assistant", "content": "结论", "createdAt": "2026-08-26T01:03:00Z" }
    ], "updates": [] }, "createdAt": "2026-08-26T01:00:05Z" }));
    assert_eq!(ids(&views(&[c])[0]), ["child-task", "child-answer"]);
}

#[test]
fn does_not_restate_an_externalized_steps_task_once_its_body_is_grafted_back() {
    let c = ctx(json!({ "id": "tool-external-step", "kind": "tool", "toolName": "workflow_step", "input": { "label": "审查一", "task": "审查代理内核", "runId": "wf-run-1", "stepIndex": 0, "status": "completed" }, "result": result("预览", true, "2026-08-26T02:00:00Z"), "subagent": { "kind": "workflowStep", "task": "审查代理内核", "status": "completed", "contexts": [
        { "id": "step-task", "kind": "user", "content": "审查代理内核", "createdAt": "2026-08-26T01:00:01Z" },
        { "id": "step-answer", "kind": "assistant", "content": "完整正文", "createdAt": "2026-08-26T01:03:00Z" }
    ], "updates": [] }, "createdAt": "2026-08-26T02:00:00Z" }));
    assert_eq!(ids(&views(&[c])[0]), ["step-task", "step-answer"]);
}

#[test]
fn still_shows_a_saved_follow_up_the_child_never_drained() {
    let first = ctx(json!({ "id": "tool-spawn", "kind": "tool", "toolName": "agent_spawn", "input": { "name": "helper", "prompt": "审查 API" }, "result": result("已启动", true, "2026-08-26T01:00:05Z"), "subagent": { "name": "helper", "task": "审查 API", "status": "completed", "contexts": [{ "id": "child-task", "kind": "user", "content": "审查 API", "createdAt": "2026-08-26T01:00:01Z" }], "updates": [] }, "createdAt": "2026-08-26T01:00:05Z" }));
    let follow = ctx(json!({ "id": "tool-followup", "kind": "tool", "toolName": "followup_task", "input": { "target": "helper", "message": "补充并发风险" }, "result": result("已送达", true, "2026-08-26T01:10:00Z"), "createdAt": "2026-08-26T01:10:00Z" }));
    assert_eq!(users(&views(&[first, follow])[0]), ["审查 API", "补充并发风险"]);
}

#[test]
fn still_shows_a_saved_follow_up_whose_text_a_forked_parent_turn_repeats() {
    let first = ctx(json!({ "id": "tool-spawn", "kind": "tool", "toolName": "agent_spawn", "input": { "name": "alpha", "prompt": "审查 API", "context": "conversation" }, "result": result("已启动", true, "2026-08-26T10:00:05Z"), "createdAt": "2026-08-26T10:00:05Z" }));
    let follow = ctx(json!({ "id": "tool-followup", "kind": "tool", "toolName": "followup_task", "input": { "target": "alpha", "message": "补充并发风险" }, "result": result("已送达", true, "2026-08-26T10:01:00Z"), "subagent": { "name": "alpha", "task": "审查 API", "status": "completed", "contexts": [
        { "id": "parent-u1", "kind": "user", "content": "补充并发风险", "createdAt": "2026-08-26T09:00:00Z" },
        { "id": "child-task", "kind": "user", "content": "审查 API", "createdAt": "2026-08-26T10:00:00Z" },
        { "id": "child-answer", "kind": "assistant", "content": "结论", "createdAt": "2026-08-26T10:00:03Z" }
    ], "updates": [] }, "createdAt": "2026-08-26T10:01:00Z" }));
    let v = &views(&[first, follow])[0];
    assert_eq!(users(v), ["审查 API", "补充并发风险", "补充并发风险"]);
    let last = v.contexts.last().unwrap();
    assert_eq!((last.content(), last.created_at()), (Some("补充并发风险"), "2026-08-26T10:01:00Z"));
}

fn run_with_step() -> Ctx {
    ctx(json!({ "id": "tool-workflow", "kind": "tool", "toolName": "workflow", "input": { "scriptName": "review-proxy", "scriptBytes": 4096, "scriptSha256": "0f1e" }, "result": ok("{\"status\":\"complete\"}"), "subagent": { "kind": "workflowStep", "task": "Review the proxy.", "status": "completed", "contexts": [
        { "id": "tool-workflow-step", "kind": "tool", "toolName": "workflow_step", "input": { "label": "审查一", "task": "审查代理内核" }, "result": ok("Done"), "subagent": { "kind": "workflowStep", "task": "审查代理内核", "status": "completed", "contexts": [], "updates": [] }, "createdAt": "2026-08-26T01:00:00Z" }
    ], "updates": [] }, "createdAt": "2026-08-26T02:00:00Z" }))
}

#[test]
fn marks_the_workflow_run_as_a_run_and_refuses_to_open_it_as_an_agent() {
    let all = views(&[run_with_step()]);
    let run = all.iter().find(|v| v.id == "tool-workflow").unwrap();
    let step = all.iter().find(|v| v.id == "tool-workflow-step").unwrap();
    assert!(run.workflow_run && !step.workflow_run);
    assert!(Rc::ptr_eq(find_openable_subagent_view(&all, Some("tool-workflow-step")).unwrap(), step));
    assert!(find_openable_subagent_view(&all, Some("tool-workflow")).is_none());
    assert!(Rc::ptr_eq(find_subagent_view(&all, Some("tool-workflow")).unwrap(), run));
}

fn starting_run() -> Ctx {
    ctx(json!({ "id": "tool-workflow", "kind": "tool", "toolName": "workflow", "input": { "scriptName": "review-proxy", "scriptBytes": 4096, "scriptSha256": "0f1e" }, "result": ok(""), "streaming": true, "streamStatus": "running", "live": { "contexts": [], "updates": [], "status": "running" }, "createdAt": "2026-08-26T02:00:00Z" }))
}

#[test]
fn marks_a_run_that_has_not_spawned_a_step_yet_as_a_run() {
    let all = views(&[starting_run()]);
    assert!(all[0].child_ids.is_empty() && all[0].workflow_run);
    assert!(find_openable_subagent_view(&all, Some("tool-workflow")).is_none());
}

#[test]
fn resolves_an_approval_cards_source_to_the_requesters_openable_page() {
    let spawned = ctx(json!({ "id": "tool-spawn", "kind": "tool", "toolName": "agent_spawn", "input": { "name": "researcher", "prompt": "调研路由" }, "result": ok("已派生"), "subagent": { "name": "researcher", "task": "调研路由", "status": "completed", "contexts": [], "updates": [] }, "createdAt": "2026-08-26T01:00:00Z" }));
    let step = ctx(json!({ "id": "call-step-1", "kind": "tool", "toolName": "workflow_step", "input": { "label": "审查一", "task": "审查代理内核" }, "result": ok(""), "streaming": true, "streamStatus": "running", "live": { "contexts": [], "updates": [], "status": "running" }, "createdAt": "2026-08-26T02:00:00Z" }));
    let all = views(&[spawned, step]);
    assert_eq!(approval_prompt_subagent_view(&all, Some("researcher"), Some("call-x")).unwrap().id, "researcher");
    assert_eq!(approval_prompt_subagent_view(&all, Some("ws1"), Some("call-step-1")).unwrap().id, "call-step-1");
    assert!(approval_prompt_subagent_view(&all, None, None).is_none());
    assert!(approval_prompt_subagent_view(&views(&[starting_run()]), None, Some("tool-workflow")).is_none());
}

#[test]
fn opens_no_agent_thread_for_a_web_search_call() {
    assert!(views(&[tool("tool-web-search", "web_search", json!({ "objective": "查清发布日期" }), json!({}))]).is_empty());
}

fn agent_tool(id: &str, name: &str, input: Value, success: bool, streaming: bool, live: Option<Value>, subagent: Option<Value>) -> Ctx {
    let mut base = json!({ "id": id, "kind": "tool", "toolName": name, "input": input, "result": { "success": success, "output": "", "executedAt": "2026-07-14T00:00:00Z", "durationMs": 5 }, "createdAt": "2026-07-14T00:00:00Z" });
    if streaming || live.is_some() {
        base["streaming"] = json!(true);
        base["streamStatus"] = json!("completed");
    }
    if let Some(l) = live {
        base["live"] = l;
    }
    if let Some(s) = subagent {
        base["subagent"] = s;
    }
    ctx(base)
}

fn run_status(c: &Ctx) -> S {
    agent_timeline_run_status(&c.tool().unwrap())
}

fn route(c: &Ctx) -> String {
    agent_timeline_route_id(&c.tool().unwrap())
}

fn rec(name: &str, status: &str) -> Value {
    json!({ "name": name, "task": "t", "status": status, "contexts": [], "updates": [] })
}

#[test]
fn keeps_accepted_spawn_and_send_calls_running_through_the_streamed_handoff_gap() {
    let empty = Some(json!({ "contexts": [], "updates": [] }));
    let running = Some(json!({ "contexts": [], "updates": [], "status": "running" }));
    assert_eq!(run_status(&agent_tool("spawn", "agent_spawn", json!({ "name": "spawned" }), true, true, None, None)), S::Running);
    assert_eq!(run_status(&agent_tool("send", "agent_send", json!({ "agent": "continued" }), true, true, None, None)), S::Running);
    assert_eq!(run_status(&agent_tool("queue", "send_message", json!({ "target": "beta" }), true, true, None, None)), S::Completed);
    assert_eq!(run_status(&agent_tool("queue-to", "send_message", json!({ "to": "beta", "message": "m" }), true, true, empty, None)), S::Completed);
    assert_eq!(run_status(&agent_tool("resume", "send_message", json!({ "to": "beta", "message": "m" }), true, false, running, None)), S::Running);
    let stream = Some(json!({ "contexts": [{ "id": "c", "kind": "assistant", "content": "…", "createdAt": "2026-07-14T00:00:01Z" }], "updates": [] }));
    assert_eq!(run_status(&agent_tool("resume-stream", "send_message", json!({ "to": "beta", "message": "m" }), true, true, stream, None)), S::Running);
    assert_eq!(run_status(&agent_tool("resume-done", "send_message", json!({ "to": "beta", "message": "m" }), true, false, None, Some(rec("beta", "roundLimit")))), S::RoundLimit);
    assert_eq!(run_status(&agent_tool("persisted", "agent_spawn", json!({ "name": "finished" }), true, false, None, Some(rec("finished", "completed")))), S::Completed);
}

#[test]
fn gives_a_live_child_the_terminal_status_it_actually_reached() {
    for (wire, status) in [("failed", S::Failed), ("stopped", S::Stopped), ("roundLimit", S::RoundLimit), ("interrupted", S::Interrupted), ("idle", S::Completed)] {
        let c = agent_tool("live", "agent_spawn", json!({}), true, false, Some(json!({ "contexts": [], "updates": [], "status": wire })), None);
        assert_eq!(run_status(&c), status);
    }
    // A settled context whose live state still claims running never reported an ending.
    let mut c = agent_tool("settled-running", "workflow_step", json!({}), false, false, Some(json!({ "contexts": [], "updates": [], "status": "running" })), None);
    c.streaming = false;
    assert_eq!(run_status(&c), S::Interrupted);
}

#[test]
fn routes_a_row_to_its_agent_by_name_before_falling_back_to_the_call_id() {
    assert_eq!(route(&agent_tool("call", "agent_spawn", json!({ "name": "spawned" }), true, false, None, Some(rec("recorded", "completed")))), "recorded");
    assert_eq!(route(&agent_tool("call", "agent_send", json!({ "agent": "addressed" }), true, false, None, None)), "addressed");
    assert_eq!(route(&agent_tool("call", "send_message", json!({ "target": "queued" }), true, false, None, None)), "queued");
    assert_eq!(route(&agent_tool("call", "send_message", json!({ "to": "addressed-by-to", "message": "m" }), true, false, None, None)), "addressed-by-to");
    assert_eq!(route(&agent_tool("call", "send_message", json!({ "to": "alias" }), true, false, None, Some(rec("recorded", "completed")))), "recorded");
    assert_eq!(route(&agent_tool("call", "agent_spawn", json!({ "name": "declared" }), true, false, None, None)), "declared");
    assert_eq!(route(&agent_tool("anonymous", "workflow_step", json!({}), true, false, None, None)), "anonymous");
}
