//! Port of `lib/modelStream.test.ts`.

use std::sync::Arc;

use serde_json::json;

use super::fixtures::*;
use crate::model::{ContextItem, ModelUsage};
use crate::runs::conversation_turns::sum_model_usage;
use crate::runs::model_stream::*;
use crate::runs::run_contexts::{contexts_from_model_run, live_reasoning_from_model_run};

fn usage(input: Option<u64>, output: Option<u64>) -> ModelUsage {
    ModelUsage { input_tokens: input, output_tokens: output, ..Default::default() }
}

fn reduce_at(run: &mut ModelRunState, value: serde_json::Value, at: &str) {
    reduce_model_stream_event(run, &event(value), at);
}

#[test]
fn puts_an_answered_questions_answers_on_its_live_card() {
    let asked = json!({ "questions": [{ "question": "选哪个数据库？", "header": "数据库", "options": [{ "label": "Postgres" }, { "label": "SQLite" }] }] });
    let answered = json!({ "questions": asked["questions"], "answers": { "选哪个数据库？": "Postgres" } });
    let mut run = run_fixture();
    reduce_all(
        &mut run,
        vec![
            announced(0, "call-ask", "ask_user", "ctx-ask"),
            json!({ "type": "tool_call_arguments_ready", "round": 0, "callId": "call-ask", "input": asked }),
            json!({ "type": "tool_execution_started", "round": 0, "callId": "call-ask" }),
            json!({ "type": "tool_call_arguments_ready", "round": 0, "callId": "call-ask", "input": answered }),
            json!({ "type": "tool_execution_completed", "round": 0, "callId": "call-ask", "result": tool_result(true, "answered") }),
        ],
    );
    let contexts = contexts_from_model_run(&run, true);
    let card = contexts.iter().find(|context| context.id() == "ctx-ask").unwrap();
    assert!(card.is_streaming());
    assert_eq!(card.stream_status, Some(StreamingToolPhase::Completed));
    let ContextItem::Tool { input, requested_input, .. } = &card.item else { panic!("a tool card") };
    assert_eq!(serde_json::Value::Object(input.clone()), answered);
    assert_eq!(requested_input.clone().map(serde_json::Value::Object), Some(asked));
}

#[test]
fn keeps_interleaved_reasoning_live_until_every_known_item_closes() {
    let mut run = run_fixture();
    reduce_all(
        &mut run,
        vec![
            json!({ "type": "reasoning_start", "round": 0 }),
            json!({ "type": "reasoning_delta", "round": 0, "delta": "A" }),
            json!({ "type": "reasoning_delta", "round": 0, "item": 1, "delta": "B" }),
            json!({ "type": "reasoning_done", "round": 0, "item": 9 }),
            json!({ "type": "reasoning_done", "round": 0 }),
        ],
    );
    assert_eq!(run.completed_reasoning_by_round.get(&0), Some(&false));
    assert!(live_reasoning_from_model_run(Some(&run)).is_some());
    let streaming: Vec<Option<bool>> = contexts_from_model_run(&run, true).iter().filter(|row| matches!(row.item, ContextItem::Reasoning { .. })).map(|row| row.streaming).collect();
    assert_eq!(streaming, [Some(false), Some(true)]);
    reduce_all(&mut run, vec![json!({ "type": "reasoning_done", "round": 0, "item": 1, "durationMs": 42 })]);
    assert_eq!(run.completed_reasoning_by_round.get(&0), Some(&true));
    assert_eq!(live_reasoning_from_model_run(Some(&run)), None);
    assert_eq!(run.reasoning_duration_by_round.get(&0), Some(&42));
    reduce_all(&mut run, vec![json!({ "type": "reasoning_start", "round": 0, "item": 1 })]);
    assert_eq!(run.completed_reasoning_by_round.get(&0), Some(&false));
    reset_round_for_retry(&mut run, 0);
    assert!(run.active_reasoning_items_by_round.get(&0).is_none());
}

#[test]
fn appends_text_deltas_per_round_and_marks_earlier_reasoning_complete() {
    let mut run = run_fixture();
    reduce_all(&mut run, vec![json!({ "type": "reasoning_delta", "round": 0, "delta": "思考" }), json!({ "type": "text_delta", "round": 0, "delta": "你" }), json!({ "type": "text_delta", "round": 0, "delta": "好" })]);
    assert_eq!(run.streamed_text_by_round[&0], "你好");
    assert_eq!(run.streamed_reasoning_by_round[&0], ["思考"]);
    // The first text delta closes the round's reasoning stream.
    assert_eq!(run.completed_reasoning_by_round.get(&0), Some(&true));
}

#[test]
fn keeps_interleaved_reasoning_items_in_their_own_slots() {
    let mut run = run_fixture();
    reduce_all(
        &mut run,
        vec![
            json!({ "type": "reasoning_start", "round": 1, "item": 0 }),
            json!({ "type": "reasoning_delta", "round": 1, "item": 0, "delta": "甲" }),
            json!({ "type": "reasoning_start", "round": 1, "item": 1 }),
            json!({ "type": "reasoning_delta", "round": 1, "item": 1, "delta": "乙" }),
            json!({ "type": "reasoning_delta", "round": 1, "item": 0, "delta": "续" }),
        ],
    );
    assert_eq!(run.streamed_reasoning_by_round[&1], ["甲续", "乙"]);
    assert_eq!(run.reasoning_started_at_by_round[&1], AT);
}

#[test]
fn buffers_a_completed_that_arrives_before_its_announcement_and_applies_it_on_announce() {
    // Reconnection can deliver a terminal event before its announcement.
    let mut run = run_fixture();
    let effects = reduce_all(
        &mut run,
        vec![json!({ "type": "tool_execution_completed", "round": 1, "callId": "c1", "result": tool_result(false, "炸了") }), announced(1, "c1", "bash", "ctx_tool_1")],
    );
    let row = &run.streamed_tools_by_round[&1][0];
    assert_eq!(row.stream_status, StreamingToolPhase::Completed);
    assert_eq!(row.result.output, "炸了");
    assert!(!row.result.success);
    assert!(run.pending_tool_events_by_round[&1].get("c1").is_none());
    // Emit the completion effect when the buffered announcement supplies the tool name.
    let completed: Vec<&ModelStreamEffect> = effects.iter().filter(|effect| matches!(effect, ModelStreamEffect::ToolCompleted { .. })).collect();
    assert_eq!(completed, [&ModelStreamEffect::ToolCompleted { tool_name: Some("bash".into()), success: false, output: "炸了".into() }]);

    // An unannounced call has no identity to project into the timeline.
    let mut orphan = run_fixture();
    let effects = reduce_all(&mut orphan, vec![json!({ "type": "tool_execution_completed", "round": 1, "callId": "lost", "result": tool_result(false, "炸了") })]);
    assert!(orphan.streamed_tools_by_round.get(&1).is_none());
    assert!(!effects.iter().any(|effect| matches!(effect, ModelStreamEffect::ToolCompleted { .. })));
}

#[test]
fn replays_buffered_arguments_and_completion_in_arrival_order() {
    let mut run = run_fixture();
    reduce_all(
        &mut run,
        vec![
            json!({ "type": "tool_call_arguments_ready", "round": 1, "callId": "c1", "input": { "command": "ls" } }),
            json!({ "type": "tool_execution_completed", "round": 1, "callId": "c1", "result": tool_result(true, "ok") }),
            announced(1, "c1", "bash", "ctx_tool_1"),
        ],
    );
    let row = &run.streamed_tools_by_round[&1][0];
    assert_eq!(row.input.get("command"), Some(&json!("ls")));
    assert_eq!(row.stream_status, StreamingToolPhase::Completed);
}

#[test]
fn bounds_what_an_orphaned_call_can_buffer() {
    let mut run = run_fixture();
    for index in 0..40 {
        reduce_all(&mut run, vec![json!({ "type": "tool_execution_started", "round": 1, "callId": "c1", "_": index })]);
    }
    assert_eq!(run.pending_tool_events_by_round[&1]["c1"].len(), 32);
    for index in 0..40 {
        reduce_all(&mut run, vec![json!({ "type": "tool_execution_started", "round": 1, "callId": format!("c{index}x") })]);
    }
    assert_eq!(run.pending_tool_events_by_round[&1].len(), 16);
}

#[test]
fn ignores_empty_deltas() {
    let base = run_fixture();
    let mut run = base.clone();
    reduce_all(&mut run, vec![json!({ "type": "text_delta", "round": 0, "delta": "" })]);
    assert_eq!(run, base);
}

fn settled_card(id: &str, output: &str) -> serde_json::Value {
    json!({ "id": id, "kind": "tool", "toolName": "bash", "round": 1, "input": { "command": "ls" }, "result": tool_result(false, output), "createdAt": AT })
}

#[test]
fn turns_a_settled_tool_card_into_a_replacement_effect_without_touching_run_state() {
    let base = run_fixture();
    let mut run = base.clone();
    let settled = settled_card("ctx_tool_settled", "已派生");
    let effects = reduce_all(&mut run, vec![json!({ "type": "tool_context_settled", "round": 2, "context": settled })]);
    // The live projection remains unchanged; replacement only affects the document copy.
    assert_eq!(run, base);
    assert_eq!(effects, [ModelStreamEffect::ToolContextSettled { context: context(settled) }]);
}

#[test]
fn promotes_a_stranded_announced_row_to_completed_when_its_settled_card_arrives() {
    let mut run = run_fixture();
    reduce_all(&mut run, vec![announced(1, "c1", "bash", "ctx_tool_1")]);
    reduce_all(&mut run, vec![json!({ "type": "tool_context_settled", "round": 1, "context": settled_card("ctx_tool_1", "command not found") })]);
    let row = &run.streamed_tools_by_round[&1][0];
    assert_eq!(row.stream_status, StreamingToolPhase::Completed);
    assert!(!row.result.success);
    assert_eq!(row.result.output, "command not found");
    assert_eq!(row.input.get("command"), Some(&json!("ls")));
}

#[test]
fn leaves_a_completed_live_row_alone_when_its_settled_card_arrives() {
    let mut run = run_fixture();
    reduce_all(
        &mut run,
        vec![
            announced(1, "c1", "bash", "ctx_tool_1"),
            json!({ "type": "tool_call_arguments_ready", "round": 1, "callId": "c1", "input": { "command": "ls" } }),
            json!({ "type": "tool_execution_completed", "round": 1, "callId": "c1", "result": tool_result(true, "ok") }),
        ],
    );
    let before = run.clone();
    reduce_all(&mut run, vec![json!({ "type": "tool_context_settled", "round": 1, "context": settled_card("ctx_tool_1", "ok（持久化改写）") })]);
    assert_eq!(run, before);
    assert_eq!(run.streamed_tools_by_round[&1][0].result.output, "ok");
}

#[test]
fn drops_the_failed_rounds_partials_when_the_first_post_retry_event_arrives() {
    let mut run = run_fixture();
    run.streamed_text_by_round.insert(1, "旧内容".into());
    run.usage_by_round.insert(1, usage(Some(5), Some(5)));
    run.retry = Some(ModelRunRetryState { round: 1, attempt: 2, max_attempts: 3, message: "retrying".into(), dirty: true });
    let revision = run.usage_revision;
    let effects = reduce_all(&mut run, vec![json!({ "type": "text_delta", "round": 1, "delta": "新" })]);
    assert_eq!(run.streamed_text_by_round[&1], "新");
    assert_eq!(run.retry, None);
    assert!(run.usage_by_round.get(&1).is_none());
    // Publish the reduced cumulative usage immediately after removing failed-round usage.
    assert!(effects.contains(&ModelStreamEffect::TurnUsage { usage: sum_model_usage(&[ModelUsage::default()]), revision }));
}

#[test]
fn keeps_subagent_and_hook_events_out_of_retry_resets() {
    let mut run = run_fixture();
    run.streamed_text_by_round.insert(1, "部分".into());
    run.retry = Some(ModelRunRetryState { round: 1, attempt: 1, max_attempts: 3, message: "m".into(), dirty: true });
    reduce_all(&mut run, vec![json!({ "type": "subagent_delta", "round": 1, "callId": "call", "channel": "text", "delta": "x" })]);
    assert_eq!(run.streamed_text_by_round[&1], "部分");
    assert!(run.retry.is_some());
}

#[test]
fn records_every_terminal_lifecycle_status_a_child_reports() {
    // The status channel's wire vocabulary is `AgentLiveStatus::wire`. Dropping
    // the terminal values would pin the child to its last-known state, so a run
    // that failed or was stopped would keep rendering as if it were still going.
    for (wire, status) in [("failed", SubagentLiveStatus::Failed), ("stopped", SubagentLiveStatus::Stopped), ("roundLimit", SubagentLiveStatus::RoundLimit)] {
        let mut run = run_fixture();
        reduce_all(
            &mut run,
            vec![
                announced(1, "call", "agent_spawn", "ctx-call"),
                json!({ "type": "subagent_delta", "round": 1, "callId": "call", "channel": "status", "delta": "running" }),
                json!({ "type": "subagent_delta", "round": 1, "callId": "call", "channel": "status", "delta": wire }),
            ],
        );
        assert_eq!(run.streamed_tools_by_round[&1][0].live.status, Some(status));
    }
}

#[test]
fn treats_an_empty_status_delta_as_a_heartbeat_and_keeps_the_current_status() {
    let mut run = run_fixture();
    reduce_all(
        &mut run,
        vec![
            announced(1, "call", "agent_spawn", "ctx-call"),
            json!({ "type": "subagent_delta", "round": 1, "callId": "call", "channel": "status", "delta": "running" }),
            json!({ "type": "subagent_delta", "round": 1, "callId": "call", "channel": "status", "delta": "" }),
        ],
    );
    assert_eq!(run.streamed_tools_by_round[&1][0].live.status, Some(SubagentLiveStatus::Running));
}

#[test]
fn publishes_cumulative_usage_with_each_usage_update() {
    let mut run = run_fixture();
    let effects = reduce_all(&mut run, vec![json!({ "type": "usage_updated", "round": 0, "usage": { "inputTokens": 10, "outputTokens": 2 } })]);
    assert_eq!(run.usage_revision, 1);
    assert_eq!(effects.iter().filter(|effect| matches!(effect, ModelStreamEffect::TurnUsage { .. })).count(), 1);
}

#[test]
fn keeps_a_direct_childs_usage_under_its_bare_call_id() {
    let mut run = run_fixture();
    reduce_all(&mut run, vec![announced(1, "child", "agent_spawn", "ctx-child"), nested("child", json!({ "type": "usage_updated", "round": 0, "usage": { "inputTokens": 9, "outputTokens": 3 } }))]);
    assert_eq!(run.subagent_usage_by_call.keys().collect::<Vec<_>>(), ["child"]);
    assert_eq!(run.subagent_usage_by_call["child"][&0], usage(Some(9), Some(3)));
}

fn contents(live: &SubagentLiveState) -> Vec<String> {
    live.contexts
        .iter()
        .map(|row| match &row.item {
            ContextItem::Assistant { content, .. } => content.clone(),
            ContextItem::Reasoning { content, .. } => content.clone().unwrap_or_default(),
            _ => String::new(),
        })
        .collect()
}

#[test]
fn retains_nested_retry_content_until_replacement_events_arrive() {
    let mut run = run_fixture();
    reduce_all(
        &mut run,
        vec![
            announced(1, "child", "agent_spawn", "ctx-child"),
            nested("child", json!({ "type": "text_delta", "round": 0, "delta": "old" })),
            nested("child", json!({ "type": "reasoning_delta", "round": 0, "item": 1, "delta": "thought" })),
            nested("child", json!({ "type": "usage_updated", "round": 0, "usage": { "outputTokens": 8 } })),
            nested("child", json!({ "type": "stream_retry_scheduled", "round": 0, "attempt": 1, "maxAttempts": 2, "delayMs": 100, "message": "retry" })),
        ],
    );
    let live = |run: &ModelRunState| run.streamed_tools_by_round[&1][0].live.clone();
    assert_eq!(contents(&live(&run)), ["old", "thought"]);
    assert_eq!(live(&run).usage_by_round.as_ref().unwrap()[&0], usage(None, Some(8)));
    reduce_all(&mut run, vec![nested("child", json!({ "type": "text_delta", "round": 1, "delta": "other" }))]);
    assert_eq!(live(&run).contexts.len(), 3);
    reduce_all(&mut run, vec![nested("child", json!({ "type": "text_delta", "round": 0, "delta": "new" })), nested("child", json!({ "type": "text_delta", "round": 0, "delta": " tail" }))]);
    assert_eq!(contents(&live(&run)), ["other", "new tail"]);
    assert!(live(&run).usage_by_round.as_ref().unwrap().get(&0).is_none());
}

#[test]
fn preserves_nested_reasoning_item_identities_and_counts_duration_only_once() {
    let mut run = run_fixture();
    reduce_all(
        &mut run,
        vec![
            announced(1, "child", "agent_spawn", "ctx-child"),
            nested("child", json!({ "type": "reasoning_delta", "round": 0, "delta": "甲" })),
            nested("child", json!({ "type": "reasoning_delta", "round": 0, "item": 1, "delta": "乙" })),
            nested("child", json!({ "type": "reasoning_delta", "round": 0, "delta": "续" })),
            nested("child", json!({ "type": "reasoning_done", "round": 0, "item": 1, "durationMs": 50 })),
            nested("child", json!({ "type": "usage_updated", "round": 0, "usage": { "reasoningTokens": 9 } })),
        ],
    );
    let live = &run.streamed_tools_by_round[&1][0].live;
    assert_eq!(contents(live), ["甲续", "乙"]);
    assert_ne!(live.contexts[0].id(), live.contexts[1].id());
    assert_eq!(live.contexts.iter().map(|row| row.streaming).collect::<Vec<_>>(), [Some(true), Some(false)]);
    let figures: Vec<(Option<u64>, Option<u64>)> = live
        .contexts
        .iter()
        .map(|row| match &row.item {
            ContextItem::Reasoning { duration_ms, tokens, .. } => (*duration_ms, *tokens),
            _ => panic!("reasoning"),
        })
        .collect();
    assert_eq!(figures, [(Some(50), Some(9)), (None, None)]);
}

#[test]
fn gives_a_childs_summary_less_reasoning_its_own_card_duration_and_tokens() {
    let mut run = run_fixture();
    reduce_all(
        &mut run,
        vec![
            announced(1, "child", "agent_spawn", "ctx-child"),
            nested("child", json!({ "type": "reasoning_start", "round": 0 })),
            nested("child", json!({ "type": "reasoning_done", "round": 0, "durationMs": 7500 })),
            nested("child", json!({ "type": "usage_updated", "round": 0, "usage": { "outputTokens": 900, "reasoningTokens": 640 } })),
        ],
    );
    let live = &run.streamed_tools_by_round[&1][0].live;
    let card = live.contexts.iter().find(|row| matches!(row.item, ContextItem::Reasoning { .. })).unwrap();
    let ContextItem::Reasoning { content, duration_ms, tokens, .. } = &card.item else { unreachable!() };
    assert_eq!(content.as_deref(), Some(""));
    assert_eq!(card.streaming, Some(false));
    assert_eq!(*duration_ms, Some(7500));
    assert_eq!(*tokens, Some(640));
}

#[test]
fn accounts_for_a_workflow_steps_usage_through_both_of_its_envelopes() {
    // A step's own model events reach the renderer wrapped twice — the run's
    // envelope around the step's. Reading only the immediate payload dropped
    // every workflow's tokens, so the turn's total silently omitted them.
    let step = |call: &str, context: &str| nested("wf", json!({ "type": "tool_call_announced", "round": 0, "callId": call, "toolName": "workflow_step", "contextId": context }));
    let spent = |call: &str, tokens: u64| {
        nested("wf", json!({ "type": "subagent_event", "round": 0, "callId": call, "event": { "type": "usage_updated", "round": 0, "usage": { "inputTokens": tokens, "outputTokens": 1 } } }))
    };
    let mut run = run_fixture();
    let effects = reduce_all(&mut run, vec![announced(1, "wf", "workflow", "ctx-wf"), step("ws1", "ctx-ws1"), step("ws2", "ctx-ws2"), spent("ws1", 100), spent("ws2", 40)]);
    // Two steps of one run are distinct spenders: keyed by the whole call path,
    // they accumulate instead of overwriting each other under "wf".
    assert_eq!(run.subagent_usage_by_call["wf/ws1"][&0], usage(Some(100), Some(1)));
    assert_eq!(run.subagent_usage_by_call["wf/ws2"][&0], usage(Some(40), Some(1)));
    assert_eq!(cumulative_model_run_usage(&run), usage(Some(140), Some(2)));
    assert_eq!(effects.iter().filter(|effect| matches!(effect, ModelStreamEffect::TurnUsage { .. })).count(), 2);
    // The leaf snapshot also lands on the step's own live state, which is what
    // the task row reads before the run settles and writes a record.
    let workflow = &run.streamed_tools_by_round[&1][0].live;
    let steps: Vec<_> = workflow.contexts.iter().filter(|context| matches!(context.item, ContextItem::Tool { .. })).map(|context| context.live.as_ref().unwrap().usage_by_round.clone().unwrap()).collect();
    assert_eq!(steps[0][&0], usage(Some(100), Some(1)));
    assert_eq!(steps[1][&0], usage(Some(40), Some(1)));
}

#[test]
fn replaces_a_childs_per_round_snapshot_instead_of_accumulating_it() {
    // Provider snapshots are already cumulative for their round. Adding them
    // would multiply a child's cost by however many chunks its provider chose
    // to report usage in.
    let snapshot = |output: u64| nested("child", json!({ "type": "usage_updated", "round": 0, "usage": { "inputTokens": 50, "outputTokens": output } }));
    let mut run = run_fixture();
    reduce_all(&mut run, vec![announced(1, "child", "agent_spawn", "ctx-child"), snapshot(2), snapshot(9)]);
    assert_eq!(run.subagent_usage_by_call["child"][&0], usage(Some(50), Some(9)));
    assert_eq!(run.streamed_tools_by_round[&1][0].live.usage_by_round.as_ref().unwrap()[&0], usage(Some(50), Some(9)));
}

#[test]
fn splits_the_turn_exactly_once_for_a_steered_input_deduplicating_replays() {
    let input = json!({ "type": "user_input_received", "round": 0, "id": "queued_1", "content": "插话", "createdAt": AT });
    let mut run = run_fixture();
    let first = reduce_all(&mut run, vec![input.clone()]);
    assert_eq!(first.iter().filter(|effect| matches!(effect, ModelStreamEffect::TurnSplit { .. })).count(), 1);
    assert_eq!(first.iter().filter(|effect| matches!(effect, ModelStreamEffect::SteeringDelivered { .. })).count(), 1);
    assert_eq!(run.steered_inputs_by_round[&0].len(), 1);
    let replay = reduce_all(&mut run, vec![input]);
    assert_eq!(replay.iter().filter(|effect| matches!(effect, ModelStreamEffect::TurnSplit { .. })).count(), 0);
    assert_eq!(run.steered_inputs_by_round[&0].len(), 1);
}

#[test]
fn shows_what_the_host_adds_between_rounds_in_the_live_turn_in_the_hosts_order() {
    let notice = json!({
        "id": "ctx_agent-result_handoff", "kind": "tool", "toolName": "box", "round": 2, "input": { "none": [] }, "notice": "handoff",
        "result": { "success": true, "output": "<task-notification>", "executedAt": AT, "durationMs": 0 }, "createdAt": AT
    });
    let continuation = json!({ "id": "ctx_hook-continuation_1", "kind": "user", "content": "Also write ENCORE.", "createdAt": AT });
    let mut run = run_fixture();
    let effects = reduce_all(
        &mut run,
        vec![
            json!({ "type": "text_delta", "round": 1, "delta": "第一轮" }),
            // A Stop hook's continuation joins before the next round's steering…
            json!({ "type": "host_context_added", "round": 2, "context": continuation }),
            json!({ "type": "user_input_received", "round": 2, "id": "queued_1", "content": "插话", "createdAt": AT }),
            // …and the boundary's notices after it.
            json!({ "type": "host_context_added", "round": 2, "context": notice }),
            json!({ "type": "host_context_added", "round": 2, "context": notice }),
            json!({ "type": "text_delta", "round": 2, "delta": "第二轮" }),
        ],
    );
    // Host additions split no turn; only the steered input does.
    assert_eq!(effects.iter().filter(|effect| matches!(effect, ModelStreamEffect::TurnSplit { .. })).count(), 1);
    let expected = ["ctx_assistant_run_test_1", "ctx_hook-continuation_1", "queued_1", "ctx_agent-result_handoff", "ctx_assistant_run_test_2"];
    assert_eq!(ids(&contexts_from_model_run(&run, true)), expected);
    // Settling an interrupted run keeps them: the host persisted them already.
    assert_eq!(ids(&contexts_from_model_run(&run, false)), expected);
    // A retry of round two redoes the request, not what the host added ahead of it.
    reduce_all(
        &mut run,
        vec![json!({ "type": "stream_retry_scheduled", "round": 2, "attempt": 1, "maxAttempts": 3, "delayMs": 0, "message": "overloaded" }), json!({ "type": "text_delta", "round": 2, "delta": "重来" })],
    );
    assert_eq!(ids(&contexts_from_model_run(&run, true)), expected);
}

#[test]
fn puts_what_the_host_adds_to_a_childs_run_in_the_childs_live_transcript() {
    let notice = json!({ "id": "ctx_agent-result_child", "kind": "tool", "toolName": "box", "round": 2, "input": {}, "result": tool_result(true, "child notice"), "createdAt": AT });
    let added = nested("call_agent", json!({ "type": "host_context_added", "round": 2, "context": notice }));
    let mut run = run_fixture();
    reduce_all(&mut run, vec![announced(1, "call_agent", "agent_spawn", "ctx-agent"), added.clone(), added]);
    assert_eq!(ids(&run.streamed_tools_by_round[&1][0].live.contexts), ["ctx_agent-result_child"]);
}

#[test]
fn reports_tool_completion_with_the_announced_tool_name() {
    let mut run = run_fixture();
    let effects = reduce_all(&mut run, vec![announced(0, "call_1", "preview_snapshot", "ctx-call_1"), json!({ "type": "tool_execution_completed", "round": 0, "callId": "call_1", "result": tool_result(true, "[]") })]);
    assert!(effects.contains(&ModelStreamEffect::ToolCompleted { tool_name: Some("preview_snapshot".into()), success: true, output: "[]".into() }));
}

#[test]
fn marks_reasoning_done_only_when_the_round_actually_streamed_reasoning() {
    let mut untouched = run_fixture();
    reduce_all(&mut untouched, vec![json!({ "type": "reasoning_done", "round": 0 })]);
    assert_eq!(untouched.completed_reasoning_by_round.get(&0), None);

    let mut streamed = run_fixture();
    streamed.streamed_reasoning_by_round.insert(0, vec!["想法".into()]);
    reduce_all(&mut streamed, vec![json!({ "type": "reasoning_done", "round": 0 })]);
    assert_eq!(streamed.completed_reasoning_by_round.get(&0), Some(&true));
}

#[test]
fn opens_a_reasoning_round_from_reasoning_start_alone_and_closes_it_with_the_providers_duration() {
    let mut run = run_fixture();
    reduce_all(&mut run, vec![json!({ "type": "reasoning_start", "round": 1 }), json!({ "type": "reasoning_done", "round": 1, "durationMs": 18000 })]);
    assert_eq!(run.reasoning_started_at_by_round[&1], AT);
    // Item 0 must retain an empty slot for encrypted-only reasoning.
    assert_eq!(run.streamed_reasoning_by_round[&1], [""]);
    assert_eq!(run.completed_reasoning_by_round.get(&1), Some(&true));
    assert_eq!(run.reasoning_duration_by_round.get(&1), Some(&18000));
}

#[test]
fn keeps_the_first_reasoning_start_of_a_round_and_lets_a_delta_open_one_on_providers_that_never_send_it() {
    let mut repeated = run_fixture();
    reduce_at(&mut repeated, json!({ "type": "reasoning_start", "round": 1 }), AT);
    reduce_at(&mut repeated, json!({ "type": "reasoning_start", "round": 1 }), "2026-08-04T00:00:09.000Z");
    assert_eq!(repeated.reasoning_started_at_by_round[&1], AT);

    let mut delta_only = run_fixture();
    reduce_all(&mut delta_only, vec![json!({ "type": "reasoning_delta", "round": 1, "delta": "想法" })]);
    assert_eq!(delta_only.reasoning_started_at_by_round[&1], AT);
}

#[test]
fn drops_the_failed_attempts_reasoning_clock_when_the_round_is_reset_for_a_retry() {
    let mut run = run_fixture();
    run.reasoning_started_at_by_round.extend([(1, "2026-08-04T00:00:00.000Z".to_owned()), (2, "keep".to_owned())]);
    run.reasoning_duration_by_round.extend([(1, 9000), (2, 1)]);
    reset_round_for_retry(&mut run, 1);
    assert_eq!(run.reasoning_started_at_by_round.iter().map(|(k, v)| (*k, v.as_str())).collect::<Vec<_>>(), [(2, "keep")]);
    assert_eq!(run.reasoning_duration_by_round.iter().map(|(k, v)| (*k, *v)).collect::<Vec<_>>(), [(2, 1)]);
}

#[test]
fn measures_the_rounds_own_reasoning_duration_when_the_provider_reports_none() {
    let mut run = run_fixture();
    reduce_at(&mut run, json!({ "type": "reasoning_start", "round": 1 }), "2026-08-04T00:00:01.000Z");
    reduce_at(&mut run, json!({ "type": "reasoning_done", "round": 1 }), "2026-08-04T00:00:14.000Z");
    assert_eq!(run.reasoning_duration_by_round.get(&1), Some(&13_000));
}

#[test]
fn measures_it_from_the_first_text_delta_on_providers_that_send_no_done_frame() {
    let mut run = run_fixture();
    reduce_at(&mut run, json!({ "type": "reasoning_start", "round": 1 }), "2026-08-04T00:00:01.000Z");
    reduce_at(&mut run, json!({ "type": "text_delta", "round": 1, "delta": "答" }), "2026-08-04T00:00:04.500Z");
    assert_eq!(run.completed_reasoning_by_round.get(&1), Some(&true));
    assert_eq!(run.reasoning_duration_by_round.get(&1), Some(&3_500));
}

#[test]
fn keeps_the_sidecars_duration_when_reasoning_reopens_and_closes_again() {
    let mut run = run_fixture();
    reduce_at(&mut run, json!({ "type": "reasoning_start", "round": 1 }), "2026-08-04T00:00:01.000Z");
    reduce_at(&mut run, json!({ "type": "reasoning_done", "round": 1, "durationMs": 18000 }), "2026-08-04T00:00:02.000Z");
    reduce_at(&mut run, json!({ "type": "reasoning_delta", "round": 1, "delta": "再想想" }), "2026-08-04T00:00:03.000Z");
    reduce_at(&mut run, json!({ "type": "reasoning_done", "round": 1 }), "2026-08-04T00:01:00.000Z");
    assert_eq!(run.reasoning_duration_by_round.get(&1), Some(&18_000));
}

#[test]
fn a_retried_workflow_step_starts_its_tile_over() {
    let mut run = run_fixture();
    let step = || nested("wf", json!({ "type": "tool_call_announced", "round": 0, "callId": "ws1", "toolName": "workflow_step", "contextId": "ctx-ws1" }));
    reduce_all(
        &mut run,
        vec![
            announced(1, "wf", "workflow", "ctx-wf"),
            step(),
            nested("wf", json!({ "type": "subagent_event", "round": 0, "callId": "ws1", "event": { "type": "text_delta", "round": 0, "delta": "abandoned" } })),
        ],
    );
    let tile = |run: &ModelRunState| run.streamed_tools_by_round[&1][0].live.contexts.iter().find(|context| context.id() == "ctx-ws1").unwrap().clone();
    assert!(tile(&run).live.is_some());
    reduce_all(&mut run, vec![step()]);
    assert!(tile(&run).live.is_none());
    assert_eq!(tile(&run).stream_status, Some(StreamingToolPhase::Announced));
}

#[test]
fn drops_a_childs_unfinished_encrypted_reasoning_and_keeps_plaintext_fragments() {
    let streaming = |id: &str, content: &str| {
        let mut card = LiveContext::settled(context(json!({ "id": id, "kind": "reasoning", "content": content, "createdAt": AT })));
        card.streaming = Some(true);
        card
    };
    let done = LiveContext::settled(context(json!({ "id": "reasoning-done", "kind": "reasoning", "content": "", "durationMs": 9000, "createdAt": AT })));
    let mut contexts = vec![streaming("reasoning-empty", ""), streaming("reasoning-text", "想到一半"), done];
    settle_subagent_contexts(&mut contexts);
    assert_eq!(ids(&contexts), ["reasoning-text", "reasoning-done"]);
    assert_eq!(contexts[0].streaming, None);
}

#[test]
fn a_settled_workflow_step_keeps_its_own_ending_rather_than_a_fabricated_interruption() {
    let card = |id: &str, status: Option<SubagentLiveStatus>| {
        let mut card = LiveContext::settled(context(json!({ "id": id, "kind": "tool", "toolName": "workflow_step", "input": {}, "result": { "success": true, "output": "", "executedAt": AT, "durationMs": 0 }, "createdAt": AT })));
        card.streaming = Some(true);
        card.stream_status = Some(StreamingToolPhase::Running);
        card.live = status.map(|status| Arc::new(SubagentLiveState { status: Some(status), ..Default::default() }));
        card
    };
    let mut contexts = vec![card("idle", Some(SubagentLiveStatus::Idle)), card("failed", Some(SubagentLiveStatus::Failed)), card("unknown", None)];
    settle_subagent_contexts(&mut contexts);
    let results: Vec<(bool, String)> = contexts.iter().map(|context| match &context.item { ContextItem::Tool { result, .. } => (result.success, result.output.clone()), _ => panic!() }).collect();
    assert_eq!(results[0], (true, String::new()));
    assert_eq!(results[1], (false, String::new()));
    assert!(!results[2].0);
    assert!(!results[2].1.is_empty());
}

mod budget {
    use super::*;

    fn activity(id: String) -> LiveContext {
        LiveContext::settled(context(json!({ "id": id, "kind": "tool", "toolName": "subagent_activity", "input": {}, "result": { "success": true, "output": "", "executedAt": AT, "durationMs": 0 }, "createdAt": AT })))
    }

    /// A live child holding `count` rows, optionally with one nested child.
    fn child(prefix: &str, count: usize, nested: Option<SubagentLiveState>) -> SubagentLiveState {
        let mut contexts: Vec<LiveContext> = (0..count).map(|index| activity(format!("{prefix}-{index}"))).collect();
        if let Some(nested) = nested {
            let mut call = activity(format!("{prefix}-nested-call"));
            call.live = Some(Arc::new(nested));
            contexts.push(call);
        }
        SubagentLiveState { contexts, ..Default::default() }
    }

    fn run_with(children: Vec<SubagentLiveState>) -> ModelRunState {
        let mut run = run_fixture();
        run.streamed_tools_by_round.insert(
            1,
            children
                .into_iter()
                .enumerate()
                .map(|(index, live)| StreamingToolState {
                    id: format!("tool-{index}"),
                    call_id: format!("call-{index}"),
                    tool_name: "agent_spawn".into(),
                    requested_input: None,
                    input: Default::default(),
                    result: serde_json::from_value(tool_result(true, "")).unwrap(),
                    stream_status: StreamingToolPhase::Running,
                    live: Arc::new(live),
                    created_at: AT.into(),
                })
                .collect(),
        );
        run
    }

    fn row_count(live: &SubagentLiveState) -> usize {
        live.contexts.iter().map(|context| 1 + context.live.as_ref().map_or(0, |child| row_count(child))).sum()
    }

    #[test]
    fn leaves_a_run_inside_the_budget_untouched() {
        let mut run = run_with(vec![child("a", 10, None), child("b", 10, None)]);
        let before = run.clone();
        assert!(!trim_live_context_budget(&mut run));
        assert_eq!(run, before);
    }

    #[test]
    fn evicts_the_deepest_child_first_then_the_oldest_at_that_depth() {
        // Over budget, the nested (deepest) transcript must give up its rows before
        // any depth-0 sibling, and among the depth-0 siblings the earliest must go
        // before the latest.
        let mut run = run_with(vec![child("first", 600, Some(child("deep", 600, None))), child("second", 600, None)]);
        assert!(trim_live_context_budget(&mut run));
        let tools = &run.streamed_tools_by_round[&1];
        let deep = tools[0].live.contexts.iter().find_map(|context| context.live.as_ref()).unwrap();
        // The deepest transcript is stripped to its single surviving row.
        assert_eq!(deep.contexts.len(), 1);
        // The oldest depth-0 child gave up rows; the newest kept all of its own.
        assert!(tools[0].live.contexts.len() < 601);
        assert_eq!(tools[1].live.contexts.len(), 600);
    }

    #[test]
    fn never_drops_a_child_entirely_so_an_evicted_agent_still_shows_its_last_row() {
        let mut run = run_with((0..40).map(|index| child(&format!("c{index}"), 100, None)).collect());
        trim_live_context_budget(&mut run);
        assert!(run.streamed_tools_by_round[&1].iter().all(|tool| !tool.live.contexts.is_empty()));
    }

    #[test]
    fn brings_the_whole_tree_back_under_the_global_budget() {
        let mut run = run_with(vec![child("a", 500, Some(child("a-deep", 500, None))), child("b", 500, None), child("c", 500, None)]);
        trim_live_context_budget(&mut run);
        let total: usize = run.streamed_tools_by_round[&1].iter().map(|tool| row_count(&tool.live)).sum();
        assert!(total <= SUBAGENT_LIVE_TOTAL_CONTEXT_BUDGET, "{total}");
    }
}

mod coalescer {
    use super::*;

    fn delta(text: &str) -> crate::model::ModelStreamEvent {
        event(json!({ "type": "text_delta", "round": 0, "delta": text }))
    }

    #[test]
    fn coalesces_deltas_into_one_commit_per_scheduled_flush() {
        let mut coalescer = ModelStreamCoalescer::default();
        assert!(coalescer.push(delta("a")), "the first event arms the timer");
        assert!(!coalescer.push(delta("b")));
        assert!(!coalescer.push(delta("c")));
        let batch: Vec<_> = coalescer.take().into_iter().map(|entry| entry.event).collect();
        assert_eq!(batch, [delta("a"), delta("b"), delta("c")]);
        assert!(coalescer.push(delta("d")), "a commit disarms it");
    }

    #[test]
    fn keeps_lifecycle_events_in_arrival_order_inside_the_scheduled_batch() {
        let mut coalescer = ModelStreamCoalescer::default();
        let done = event(json!({ "type": "reasoning_done", "round": 0 }));
        coalescer.push(delta("a"));
        coalescer.push(done.clone());
        coalescer.push(delta("b"));
        assert_eq!(coalescer.take().into_iter().map(|entry| entry.event).collect::<Vec<_>>(), [delta("a"), done, delta("b")]);
    }

    #[test]
    fn dispose_drops_what_was_not_committed() {
        let mut coalescer = ModelStreamCoalescer::default();
        coalescer.push(delta("b"));
        coalescer.dispose();
        assert!(coalescer.take().is_empty());
        assert!(coalescer.push(delta("c")), "and nothing stays armed");
    }

    #[test]
    fn stamps_each_event_with_its_arrival_time_not_the_flush_time() {
        let mut coalescer = ModelStreamCoalescer::default();
        coalescer.push(delta("a"));
        let batch = coalescer.take();
        assert!(crate::runs::util::parse_iso(&batch[0].at).is_some());
    }
}
