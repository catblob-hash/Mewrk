//! Port of `lib/runContexts.test.ts`.

use serde_json::json;

use super::fixtures::*;
use crate::model::{ContextItem, Conversation, ModelUsage, ReasoningForm};
use crate::runs::model_stream::*;
use crate::runs::run_contexts::*;

const AT2: &str = "2026-08-29T00:00:00.000Z";

fn tool_state(id: &str, tool_name: &str, input: serde_json::Value, requested: Option<serde_json::Value>) -> StreamingToolState {
    StreamingToolState {
        id: id.into(),
        call_id: format!("call-{id}"),
        tool_name: tool_name.into(),
        requested_input: requested.map(|value| value.as_object().unwrap().clone()),
        input: input.as_object().unwrap().clone(),
        result: serde_json::from_value(tool_result(true, "ok")).unwrap(),
        stream_status: StreamingToolPhase::Completed,
        live: Default::default(),
        created_at: AT.into(),
    }
}

fn cancelled_run() -> ModelRunState {
    let mut run = ModelRunState::new("ws", "run_cancelled", request("encrypted"), "2026-08-14T00:00:00.000Z".into());
    run.streamed_text_by_round.insert(2, "取消前可见的正文".into());
    let mut workflow = tool_state("ctx-workflow", "workflow", json!({ "plan": { "name": "audit" } }), None);
    workflow.live = std::sync::Arc::new(SubagentLiveState { status: Some(SubagentLiveStatus::Running), ..Default::default() });
    run.streamed_tools_by_round.insert(1, vec![workflow]);
    run
}

fn reasoning_fields(context: &LiveContext) -> (Option<&str>, Option<ReasoningForm>, Option<u64>, Option<u64>) {
    match &context.item {
        ContextItem::Reasoning { content, form, duration_ms, tokens, .. } => (content.as_deref(), *form, *duration_ms, *tokens),
        _ => panic!("reasoning"),
    }
}

#[test]
fn keeps_encrypted_and_readable_items_distinct_through_events_and_projection() {
    let mut run = cancelled_run();
    run.streamed_text_by_round.clear();
    run.streamed_tools_by_round.clear();
    reduce_all(
        &mut run,
        vec![
            json!({ "type": "reasoning_start", "round": 1, "item": 0, "form": "encrypted" }),
            json!({ "type": "reasoning_start", "round": 1, "item": 1, "form": "plaintext" }),
            json!({ "type": "reasoning_delta", "round": 1, "item": 1, "delta": "readable" }),
            json!({ "type": "reasoning_done", "round": 1, "item": 1, "durationMs": 1 }),
        ],
    );
    for streaming in [true, false] {
        let cards = contexts_from_model_run(&run, streaming);
        assert_eq!(cards[0].id(), reasoning_segment_context_id(&run.request_id, 1, 0));
        assert_eq!(reasoning_fields(&cards[0]).0, Some(""));
        assert_eq!(reasoning_fields(&cards[0]).1, Some(ReasoningForm::Encrypted));
        assert_eq!(cards[1].id(), reasoning_segment_context_id(&run.request_id, 1, 1));
        assert_eq!(reasoning_fields(&cards[1]).0, Some("readable"));
        assert_eq!(reasoning_fields(&cards[1]).1, Some(ReasoningForm::Plaintext));
    }
    reset_round_for_retry(&mut run, 1);
    assert!(run.reasoning_forms_by_round.get(&1).is_none());
}

#[test]
fn replaces_a_completed_hook_before_any_refresh_while_retaining_its_distinct_injection() {
    let mut run = cancelled_run();
    run.streamed_tools_by_round.clear();
    run.streamed_text_by_round.clear();
    run.streamed_hooks_by_round.insert(
        3,
        vec![StreamingHookState {
            execution_id: "exec_1".into(),
            hook_id: "hook_1".into(),
            hook_name: "Hook".into(),
            event: "PreToolUse".into(),
            status_message: None,
            result: serde_json::from_value(tool_result(true, "streamed display")).unwrap(),
            status: StreamingHookStatus::Succeeded,
            context_injected: true,
            reason: None,
            created_at: AT.into(),
        }],
    );
    let display = context(json!({ "id": "ctx_hook_run_cancelled_exec_1_display_0", "kind": "system", "content": "host terminal display", "localOnly": true, "createdAt": AT }));
    let injection = context(json!({ "id": "ctx_hook_run_cancelled_exec_1_injection_0", "kind": "system", "content": "injected context", "localOnly": false, "createdAt": AT }));
    assert_eq!(merge_finalized_interrupted_run_contexts(&run, &[display.clone(), injection.clone()]), [display, injection]);
    run.streamed_hooks_by_round.get_mut(&3).unwrap()[0].status = StreamingHookStatus::Running;
    assert_eq!(merge_finalized_interrupted_run_contexts(&run, &[]).len(), 1);
}

#[test]
fn keeps_partial_text_and_replaces_a_live_workflow_with_its_trusted_terminal_record() {
    let finalized = context(json!({
        "id": "ctx-workflow", "kind": "tool", "toolName": "workflow", "round": 1, "modelTurnId": "model-turn-run_cancelled-1",
        "input": { "plan": { "name": "audit" } },
        "result": tool_result(true, "workflow started"),
        "subagent": { "name": "workflow-a1", "kind": "workflowStep", "task": "audit", "status": "interrupted", "contexts": [], "updates": [{ "content": "步骤正在执行", "createdAt": AT }] },
        "attestation": "host-attestation", "createdAt": AT
    }));
    let merged = merge_finalized_interrupted_run_contexts(&cancelled_run(), &[finalized]);
    assert_eq!(merged.iter().filter(|context| context.id() == "ctx-workflow").count(), 1);
    let workflow = merged.iter().find(|context| context.id() == "ctx-workflow").unwrap();
    let ContextItem::Tool { attestation, subagent, .. } = workflow else { panic!() };
    assert_eq!(attestation, "host-attestation");
    assert!(subagent.is_some());
    let partial = merged.iter().find(|context| matches!(context, ContextItem::Assistant { content, .. } if !content.is_empty())).unwrap();
    let ContextItem::Assistant { content, interrupted, .. } = partial else { panic!() };
    assert_eq!(content, "取消前可见的正文");
    assert!(*interrupted);
}

fn prose_run(request_id: &str) -> ModelRunState {
    let mut run = cancelled_run();
    run.request_id = request_id.into();
    run.streamed_tools_by_round.clear();
    run.streamed_reasoning_by_round.insert(1, vec!["先读代码".into()]);
    run.completed_reasoning_by_round.insert(1, true);
    run.streamed_text_by_round.clear();
    run.streamed_text_by_round.insert(1, "读完了".into());
    run
}

/// These literals are a cross-language contract: the host's `round_prose_context_id` and `round_model_turn_id` must
/// derive exactly the same strings.
#[test]
fn derives_identities_by_run_and_round_exactly_as_the_host_does() {
    assert_eq!(round_prose_context_id("run_1", 1, "assistant"), "ctx_assistant_run_1_1");
    assert_eq!(round_prose_context_id("run_1", 2, "reasoning"), "ctx_reasoning_run_1_2");
    assert_eq!(round_model_turn_id("run_1", 3), "model-turn-run_1-3");
    assert_eq!(run_stream_context_prefixes("run_1"), ["ctx_assistant_run_1_", "ctx_reasoning_run_1_", "ctx_hook_run_1_"]);
}

#[test]
fn each_reasoning_segment_gets_a_row_and_an_identity_by_segment_number() {
    // Each item is one card. Round metadata belongs only to the first card, and
    // the live pointer targets only the final growing segment.
    let mut run = prose_run("run_segments");
    run.streamed_text_by_round.clear();
    run.streamed_reasoning_by_round.insert(1, vec!["第一段".into(), "第二段".into()]);
    run.completed_reasoning_by_round.clear();
    run.reasoning_started_at_by_round.insert(1, AT.into());
    run.reasoning_duration_by_round.insert(1, 5_000);
    let contexts = contexts_from_model_run(&run, true);
    let reasoning: Vec<&LiveContext> = contexts.iter().filter(|context| matches!(context.item, ContextItem::Reasoning { .. })).collect();
    assert_eq!(reasoning.len(), 2);
    assert_eq!(reasoning[0].id(), reasoning_segment_context_id(&run.request_id, 1, 0));
    assert_eq!(reasoning_fields(reasoning[0]).0, Some("第一段"));
    assert_eq!(reasoning[0].started_at.as_deref(), Some(AT));
    assert_eq!(reasoning_fields(reasoning[0]).2, Some(5_000));
    assert_eq!(reasoning[0].streaming, Some(false));
    assert_eq!(reasoning[1].id(), reasoning_segment_context_id(&run.request_id, 1, 1));
    assert_eq!(reasoning_fields(reasoning[1]).0, Some("第二段"));
    assert_eq!(reasoning[1].streaming, Some(true));
    assert_eq!(reasoning[1].started_at, None);
    assert_eq!(reasoning_fields(reasoning[1]).2, None);
    assert_eq!(reasoning_segment_context_id("req", 2, 0), round_prose_context_id("req", 2, "reasoning"));
    assert_eq!(reasoning_segment_context_id("req", 2, 3), "ctx_reasoning_req_2_3");
}

#[test]
fn the_streaming_projection_uses_the_derived_identities() {
    let projected = contexts_from_model_run(&prose_run("run_a"), true);
    let reasoning = projected.iter().find(|context| matches!(context.item, ContextItem::Reasoning { .. })).unwrap();
    let assistant = projected.iter().find(|context| matches!(context.item, ContextItem::Assistant { .. })).unwrap();
    assert_eq!(reasoning.id(), round_prose_context_id("run_a", 1, "reasoning"));
    assert_eq!(assistant.id(), round_prose_context_id("run_a", 1, "assistant"));
    let turn = |context: &LiveContext| match &context.item {
        ContextItem::Reasoning { model_turn_id, .. } | ContextItem::Assistant { model_turn_id, .. } => model_turn_id.clone(),
        _ => None,
    };
    assert_eq!(turn(reasoning).as_deref(), Some(round_model_turn_id("run_a", 1).as_str()));
    assert_eq!(turn(assistant).as_deref(), Some(round_model_turn_id("run_a", 1).as_str()));
}

/// Encrypted reasoning requests may provide no summary, leaving the streamed text empty. The round must still appear
/// in the timeline so its duration and token count remain visible.
#[test]
fn projects_a_reasoning_round_with_no_summary_carrying_duration_and_tokens() {
    let mut run = prose_run("run_hidden");
    run.streamed_text_by_round.clear();
    run.streamed_reasoning_by_round.clear();
    run.reasoning_started_at_by_round.insert(1, AT2.into());
    run.reasoning_duration_by_round.insert(1, 18_000);
    run.usage_by_round.insert(1, ModelUsage { output_tokens: Some(2000), reasoning_tokens: Some(1240), ..Default::default() });
    let projected = contexts_from_model_run(&run, true);
    assert_eq!(projected.len(), 1);
    assert_eq!(projected[0].id(), round_prose_context_id("run_hidden", 1, "reasoning"));
    assert_eq!(reasoning_fields(&projected[0]), (Some(""), Some(ReasoningForm::Encrypted), Some(18_000), Some(1240)));
    assert_eq!(projected[0].started_at.as_deref(), Some(AT2));
}

/// With identical state and empty prose, changing only the model setting must change the live card form. Form derives
/// from model attributes, not inferred from whether prose is empty.
#[test]
fn the_live_cards_form_comes_from_the_model_not_from_whether_it_has_text() {
    let mut run = prose_run("run_plaintext");
    run.request = request("plaintext");
    run.streamed_text_by_round.clear();
    run.streamed_reasoning_by_round.clear();
    run.reasoning_started_at_by_round.insert(1, AT2.into());
    run.reasoning_duration_by_round.insert(1, 18_000);
    let projected = contexts_from_model_run(&run, true);
    assert_eq!(reasoning_fields(&projected[0]).0, Some(""));
    assert_eq!(reasoning_fields(&projected[0]).1, Some(ReasoningForm::Plaintext));
}

/// A zero reasoning-token count means no reasoning. Responses reports `0` rather than omitting the field.
#[test]
fn does_not_write_a_providers_zero_reasoning_tokens_into_the_context() {
    let mut run = prose_run("run_zero");
    run.usage_by_round.insert(1, ModelUsage { output_tokens: Some(12), reasoning_tokens: Some(0), ..Default::default() });
    let projected = contexts_from_model_run(&run, true);
    let reasoning = projected.iter().find(|context| matches!(context.item, ContextItem::Reasoning { .. })).unwrap();
    assert_eq!(reasoning_fields(reasoning).3, None);
}

/// Some reasoning models report reasoning tokens only in usage. The live projection must create the metadata-only card
/// so it does not appear suddenly before the answer at settlement.
#[test]
fn a_token_only_reasoning_round_has_a_card_while_streaming_and_is_not_marked_streaming() {
    let mut run = prose_run("run_tokens");
    run.streamed_reasoning_by_round.clear();
    run.completed_reasoning_by_round.clear();
    run.streamed_text_by_round.insert(1, "答案".into());
    run.usage_by_round.insert(1, ModelUsage { output_tokens: Some(40), reasoning_tokens: Some(28), ..Default::default() });
    let projected = contexts_from_model_run(&run, true);
    let at = projected.iter().position(|context| matches!(context.item, ContextItem::Reasoning { .. })).unwrap();
    assert_eq!(projected[at].id(), round_prose_context_id("run_tokens", 1, "reasoning"));
    assert_eq!(reasoning_fields(&projected[at]).0, Some(""));
    assert_eq!(reasoning_fields(&projected[at]).3, Some(28));
    // With no start or delta, usage-only accounting is not active reasoning and must not start the timer.
    assert!(!projected[at].is_streaming());
    // The reasoning card precedes prose from the same round.
    assert!(at < projected.iter().position(|context| matches!(context.item, ContextItem::Assistant { .. })).unwrap());
}

/// Preserve host-issued ids verbatim. Rewriting them would desynchronize the read model from the conversation store.
#[test]
fn settlement_identities_are_kept_verbatim() {
    let run = prose_run("run_b");
    let settled = vec![
        context(json!({ "id": round_prose_context_id("run_b", 1, "reasoning"), "kind": "reasoning", "content": "先读代码", "round": 1, "modelTurnId": round_model_turn_id("run_b", 1), "createdAt": AT })),
        context(json!({ "id": "ctx_assistant_deadbeef", "kind": "assistant", "content": "读完了", "round": 1, "modelTurnId": "ctx_model-turn_deadbeef", "createdAt": AT })),
    ];
    assert_eq!(backfill_requested_tool_input(&settled, &run), settled);
}

fn conversation_with(contexts: Vec<ContextItem>) -> Conversation {
    let settings = serde_json::json!({ "enabledTools": [] });
    serde_json::from_value(json!({ "id": "conv", "title": "t", "createdAt": AT, "updatedAt": AT, "settings": settings, "contexts": contexts })).unwrap()
}

#[test]
fn a_runs_prose_rows_are_replaced_in_place_leaving_no_second_copy() {
    let plain = |run: &ModelRunState| -> Vec<ContextItem> { contexts_from_model_run(run, false).into_iter().map(|context| context.item).collect() };
    let mut conversation = conversation_with(vec![user("ctx_user", "去读一下")]);
    assert!(apply_streamed_run_contexts(&mut conversation, &plain(&prose_run("run_c")), "run_c"));
    let mut grown = prose_run("run_c");
    grown.streamed_text_by_round.insert(1, "读完了，接着写".into());
    assert!(apply_streamed_run_contexts(&mut conversation, &plain(&grown), "run_c"));
    let assistants: Vec<&ContextItem> = conversation.contexts.iter().filter(|context| matches!(context, ContextItem::Assistant { .. })).collect();
    assert_eq!(assistants.len(), 1);
    assert_eq!(assistants[0].id(), round_prose_context_id("run_c", 1, "assistant"));
    let ContextItem::Assistant { content, .. } = assistants[0] else { panic!() };
    assert_eq!(content, "读完了，接着写");
    // Nothing changed: the same conversation answers.
    assert!(!apply_streamed_run_contexts(&mut conversation, &plain(&grown), "run_c"));
}

mod backfill {
    use super::*;

    fn run_with_tools(tools: Vec<StreamingToolState>) -> ModelRunState {
        let mut run = cancelled_run();
        run.request_id = "run_backfill".into();
        run.streamed_text_by_round.clear();
        run.streamed_tools_by_round.insert(1, tools);
        run
    }

    fn settled_tool(id: &str, tool_name: &str, input: serde_json::Value, requested: Option<serde_json::Value>) -> ContextItem {
        let mut card = json!({ "id": id, "kind": "tool", "toolName": tool_name, "round": 1, "modelTurnId": "model-turn-run_backfill-1", "input": input, "result": tool_result(true, "ok"), "attestation": format!("attestation-{id}"), "createdAt": AT });
        if let Some(requested) = requested {
            card["requestedInput"] = requested;
        }
        context(card)
    }

    fn requested(context: &ContextItem) -> Option<serde_json::Value> {
        match context {
            ContextItem::Tool { requested_input, .. } => requested_input.clone().map(serde_json::Value::Object),
            _ => None,
        }
    }

    /// Host-synthesized `task_wait` cards were never announced and have no streamed counterpart. Matching by tool name
    /// would assign them the model call's original input, invalidating their receipt signature.
    #[test]
    fn gives_no_host_synthesized_card_of_the_same_name_the_model_calls_arguments() {
        let run = run_with_tools(vec![tool_state("ctx_tool_model_wait", "task_wait", json!({ "label": "改写后" }), Some(json!({ "label": "模型原始" })))]);
        let settled = [settled_tool("ctx_agent-result_deadbeef", "task_wait", json!({ "label": "宿主代收" }), None), settled_tool("ctx_tool_model_wait", "task_wait", json!({ "label": "改写后" }), None)];
        let filled = backfill_requested_tool_input(&settled, &run);
        assert_eq!(requested(&filled[0]), None);
        assert_eq!(requested(&filled[1]), Some(json!({ "label": "模型原始" })));
    }

    /// Concurrent asynchronous tools settle together at round end, so authoritative order differs from announcement order.
    #[test]
    fn pairs_each_card_with_its_own_call_when_the_orders_differ() {
        let run = run_with_tools(vec![
            tool_state("ctx_tool_search_a", "web_search", json!({ "query": "改写 A" }), Some(json!({ "query": "原始 A" }))),
            tool_state("ctx_tool_read", "read", json!({ "path": "a.md" }), None),
            tool_state("ctx_tool_search_c", "web_search", json!({ "query": "改写 C" }), Some(json!({ "query": "原始 C" }))),
        ]);
        let settled = [
            settled_tool("ctx_tool_read", "read", json!({ "path": "a.md" }), None),
            settled_tool("ctx_tool_search_a", "web_search", json!({ "query": "改写 A" }), None),
            settled_tool("ctx_tool_search_c", "web_search", json!({ "query": "改写 C" }), None),
        ];
        let filled = backfill_requested_tool_input(&settled, &run);
        assert_eq!(requested(&filled[1]), Some(json!({ "query": "原始 A" })));
        assert_eq!(requested(&filled[2]), Some(json!({ "query": "原始 C" })));
    }

    /// A card that already carries requested input must not shift matching of later same-named cards.
    #[test]
    fn a_card_with_its_arguments_moves_no_later_pairing() {
        let run = run_with_tools(vec![
            tool_state("ctx_tool_write_1", "write", json!({ "path": "改写 1" }), Some(json!({ "path": "原始 1" }))),
            tool_state("ctx_tool_write_2", "write", json!({ "path": "改写 2" }), Some(json!({ "path": "原始 2" }))),
        ]);
        let settled = [settled_tool("ctx_tool_write_1", "write", json!({ "path": "改写 1" }), Some(json!({ "path": "原始 1" }))), settled_tool("ctx_tool_write_2", "write", json!({ "path": "改写 2" }), None)];
        let filled = backfill_requested_tool_input(&settled, &run);
        assert_eq!(requested(&filled[0]), Some(json!({ "path": "原始 1" })));
        assert_eq!(requested(&filled[1]), Some(json!({ "path": "原始 2" })));
    }

    /// Requested input identical to the card's `input` was not hook-rewritten and must not be stored.
    #[test]
    fn writes_no_requested_input_when_the_arguments_were_not_rewritten() {
        let run = run_with_tools(vec![tool_state("ctx_tool_read", "read", json!({ "path": "a.md" }), Some(json!({ "path": "a.md" })))]);
        let settled = [settled_tool("ctx_tool_read", "read", json!({ "path": "a.md" }), None)];
        assert_eq!(backfill_requested_tool_input(&settled, &run), settled);
    }
}

/// Encrypted reasoning has two properties plaintext reasoning does not: while it runs there is nothing to read, and if
/// the round ends before it finishes there is no ciphertext behind it that a later turn could replay.
mod encrypted {
    use super::*;

    fn encrypted_run() -> ModelRunState {
        let mut run = cancelled_run();
        run.request_id = "run_thinking".into();
        run.streamed_text_by_round.clear();
        run.streamed_tools_by_round.clear();
        run
    }

    fn open(items: &[&str]) -> ModelRunState {
        let mut run = encrypted_run();
        run.streamed_reasoning_by_round.insert(1, items.iter().map(|item| (*item).to_owned()).collect());
        run.completed_reasoning_by_round.insert(1, false);
        run.reasoning_started_at_by_round.insert(1, AT.into());
        run
    }

    /// The stream indicator narrates this round beside the cat; a card would be an empty disclosure with a clock.
    #[test]
    fn shows_no_card_while_thinking_with_no_summary() {
        assert!(contexts_from_model_run(&open(&[""]), true).is_empty());
    }

    /// A Responses summary is readable text as it arrives, so it streams into its own card like plaintext reasoning.
    #[test]
    fn streams_a_card_when_it_has_a_summary() {
        let projected = contexts_from_model_run(&open(&["先读文件"]), true);
        assert_eq!(projected.len(), 1);
        assert_eq!(reasoning_fields(&projected[0]).1, Some(ReasoningForm::Encrypted));
        assert_eq!(reasoning_fields(&projected[0]).0, Some("先读文件"));
        assert_eq!(projected[0].streaming, Some(true));
    }

    #[test]
    fn the_card_appears_with_duration_and_tokens_once_thinking_ends() {
        let mut run = open(&[""]);
        run.completed_reasoning_by_round.insert(1, true);
        run.reasoning_duration_by_round.insert(1, 83_000);
        run.usage_by_round.insert(1, ModelUsage { output_tokens: Some(900), reasoning_tokens: Some(1240), ..Default::default() });
        let projected = contexts_from_model_run(&run, true);
        assert_eq!(reasoning_fields(&projected[0]).2, Some(83_000));
        assert_eq!(reasoning_fields(&projected[0]).3, Some(1240));
        assert_eq!(projected[0].streaming, Some(false));
    }

    /// No summary and no `done` frame means nothing to show. A summary that did stream stays as an interrupted card.
    #[test]
    fn an_interrupt_only_drops_encrypted_reasoning_with_no_summary() {
        assert!(contexts_from_interrupted_run(&open(&[""])).is_empty());
        let kept = contexts_from_interrupted_run(&open(&["先读文件"]));
        assert_eq!(kept.len(), 1);
        let ContextItem::Reasoning { form, content, interrupted, duration_ms, .. } = &kept[0] else { panic!() };
        assert_eq!(*form, Some(ReasoningForm::Encrypted));
        assert_eq!(content.as_deref(), Some("先读文件"));
        assert!(*interrupted);
        assert_eq!(*duration_ms, None);
    }

    #[test]
    fn an_interrupt_keeps_encrypted_reasoning_that_finished() {
        let mut run = open(&[""]);
        run.completed_reasoning_by_round.insert(1, true);
        run.reasoning_duration_by_round.insert(1, 9_000);
        let kept = contexts_from_interrupted_run(&run);
        let ContextItem::Reasoning { duration_ms, .. } = &kept[0] else { panic!() };
        assert_eq!(*duration_ms, Some(9_000));
    }

    /// Plaintext reasoning is its own content, so an unfinished fragment of it is still a truthful record and stays.
    #[test]
    fn unfinished_plaintext_reasoning_stays() {
        let mut run = open(&["想到一半"]);
        run.request = request("plaintext");
        let kept = contexts_from_interrupted_run(&run);
        let ContextItem::Reasoning { form, content, interrupted, .. } = &kept[0] else { panic!() };
        assert_eq!(*form, Some(ReasoningForm::Plaintext));
        assert_eq!(content.as_deref(), Some("想到一半"));
        assert!(*interrupted);
    }

    /// The backend mints its own interrupted reasoning during teardown, and those contexts reach the merge without
    /// passing the projection's gate: the same rule applies.
    #[test]
    fn the_hosts_unfinished_encrypted_reasoning_stays_with_a_summary_and_goes_without_one() {
        let run = open(&["先读文件"]);
        let replaced = context(json!({ "id": reasoning_segment_context_id("run_thinking", 1, 0), "kind": "reasoning", "content": "先读文件，再改", "form": "encrypted", "interrupted": true, "createdAt": AT }));
        let host_only = context(json!({ "id": "ctx_reasoning_host_only", "kind": "reasoning", "content": "宿主自己捡回来的片段", "form": "encrypted", "interrupted": true, "createdAt": AT }));
        let bodiless = context(json!({ "id": "ctx_reasoning_host_bodiless", "kind": "reasoning", "content": "", "form": "encrypted", "interrupted": true, "createdAt": AT }));
        assert_eq!(merge_finalized_interrupted_run_contexts(&run, &[replaced.clone(), host_only.clone(), bodiless]), [replaced, host_only]);
    }
}

mod live_reasoning {
    use super::*;

    #[test]
    fn reports_the_round_still_thinking_with_its_known_tokens() {
        let mut run = cancelled_run();
        run.reasoning_started_at_by_round.extend([(1, AT.to_owned()), (2, "2026-08-04T00:00:09.000Z".to_owned())]);
        run.completed_reasoning_by_round.insert(1, true);
        run.usage_by_round.insert(2, ModelUsage { output_tokens: Some(10), reasoning_tokens: Some(640), ..Default::default() });
        // The newest open round wins: round 1 closed when its tools began.
        assert_eq!(live_reasoning_from_model_run(Some(&run)), Some(LiveReasoningView { started_at: "2026-08-04T00:00:09.000Z".into(), tokens: 640 }));
    }

    #[test]
    fn reports_nothing_once_thinking_ended_or_with_no_run() {
        let mut done = cancelled_run();
        done.reasoning_started_at_by_round.insert(1, AT.into());
        done.completed_reasoning_by_round.insert(1, true);
        assert_eq!(live_reasoning_from_model_run(Some(&done)), None);
        assert_eq!(live_reasoning_from_model_run(None), None);
    }

    /// Every provider gets a count: most report reasoning usage only once the round ends, so until then the line counts
    /// what it can see itself.
    #[test]
    fn counts_streamed_text_and_the_sidecars_estimate_when_the_provider_reports_nothing() {
        let mut silent = cancelled_run();
        silent.reasoning_started_at_by_round.insert(1, AT.into());
        silent.usage_by_round.insert(1, ModelUsage { output_tokens: Some(4), reasoning_tokens: Some(0), ..Default::default() });
        // Nothing to count yet is still a count.
        assert_eq!(live_reasoning_from_model_run(Some(&silent)), Some(LiveReasoningView { started_at: AT.into(), tokens: 0 }));
        // Omitted thinking (the sidecar's estimate) plus the summary text streamed after it.
        let mut narrated = silent.clone();
        narrated.streamed_reasoning_by_round.insert(1, vec!["abcdefgh".into(), "ijkl".into()]);
        narrated.hidden_reasoning_tokens_by_round.insert(1, [(0, 300), (1, 20)].into_iter().collect());
        assert_eq!(live_reasoning_from_model_run(Some(&narrated)), Some(LiveReasoningView { started_at: AT.into(), tokens: 323 }));
        // The provider's own figure wins once it is the larger one.
        let mut reported = narrated.clone();
        reported.usage_by_round.insert(1, ModelUsage { output_tokens: Some(900), reasoning_tokens: Some(800), ..Default::default() });
        assert_eq!(live_reasoning_from_model_run(Some(&reported)), Some(LiveReasoningView { started_at: AT.into(), tokens: 800 }));
    }
}
