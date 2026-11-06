//! `taskStreamEvent`: a background task that outlived the run that started it, streaming into the overlay the timeline
//! draws over its persisted card.

use std::sync::Arc;

use serde_json::{json, Value};

use super::fixtures::{assistant, context, tool_result, user, AT};
use super::rig::*;
use crate::model::ContextItem;
use crate::runs::{LiveContext, RenderedContext, StreamingToolPhase, SubagentLiveStatus, TaskOverlayView};

const SPAWN: &str = "ctx_tool_spawn";
const CALL: &str = "call_spawn";

fn spawn_card(attestation: &str, record: Option<Value>) -> ContextItem {
    let mut card = json!({
        "id": SPAWN, "kind": "tool", "toolName": "agent_spawn", "round": 1, "providerCallId": CALL,
        "input": { "name": "scout", "task": "梳理仓库" }, "result": tool_result(true, "已在后台启动 scout"), "attestation": attestation, "createdAt": AT
    });
    if let Some(record) = record {
        card["subagent"] = record;
    }
    context(card)
}

fn record(status: &str, contexts: Value) -> Value {
    json!({ "name": "scout", "task": "梳理仓库", "status": status, "contexts": contexts, "updates": [{ "content": "正在读 src", "createdAt": AT }] })
}

fn checkpoint() -> Value {
    record("interrupted", json!([{ "id": "ctx_child_first", "kind": "assistant", "content": "第一轮读完了", "createdAt": AT }]))
}

fn seeded(rig: &mut Rig, card: ContextItem) {
    rig.set_contexts(vec![user("ctx_user", "派个后台代理去梳理仓库"), card, assistant("ctx_assistant", "已派生，回合结束。")]);
}

fn push(rig: &mut Rig, event: Value) {
    crate::runs::events::handle_push_event(&mut rig.app, &json!({ "type": "taskStreamEvent", "conversationId": CONV, "event": event }));
}

/// A child's own model text, as the host wraps it for the parent's call.
fn child_text(delta: &str) -> Value {
    json!({ "type": "subagent_event", "round": 1, "callId": CALL, "event": { "type": "text_delta", "round": 2, "delta": delta } })
}

fn child_status(status: &str) -> Value {
    json!({ "type": "subagent_delta", "round": 1, "callId": CALL, "channel": "status", "delta": status })
}

fn overlay(rig: &mut Rig) -> Option<Arc<TaskOverlayView>> {
    rig.app.task_overlay(CONV)
}

fn spawn_live(rig: &mut Rig) -> Option<LiveContext> {
    overlay(rig).and_then(|view| view.get(SPAWN).cloned())
}

/// The child's streamed reply for round 2, as the overlay's card holds it.
fn streamed_reply(rig: &mut Rig) -> Option<String> {
    let card = spawn_live(rig)?;
    card.live?.contexts.iter().find(|context| context.id() == format!("subagent-stream-{SPAWN}-assistant-2")).map(|context| match &context.item {
        ContextItem::Assistant { content, .. } => content.clone(),
        _ => String::new(),
    })
}

#[test]
fn task_overlay_a_pushed_child_event_updates_the_spawn_card_and_leaves_the_document_alone() {
    let mut rig = Rig::new();
    rig.adopted();
    seeded(&mut rig, spawn_card("attested-1", Some(checkpoint())));
    let before = rig.conversation().contexts.clone();

    push(&mut rig, child_text("正在"));
    push(&mut rig, child_text("读取 crates"));
    rig.until("the commit", |rig| streamed_reply(rig).as_deref() == Some("正在读取 crates"));

    let view = overlay(&mut rig).unwrap();
    assert_eq!(view.contexts.len(), 1, "only the card the call id names");
    let card = view.get(SPAWN).unwrap();
    // The persisted card itself, drawn as a running call's row is.
    assert_eq!(card.item, before[1]);
    assert_eq!(card.streaming, Some(true));
    assert_eq!(card.stream_status, Some(StreamingToolPhase::Completed));
    let live = card.live.as_ref().unwrap();
    // A streaming child is running, whatever its checkpoint says; what the record already showed stays.
    assert_eq!(live.status, Some(SubagentLiveStatus::Running));
    assert_eq!(live.contexts[0].id(), "ctx_child_first");
    assert_eq!(live.updates.len(), 1);

    // Display only: the attested card is not edited, and nothing reads as a model turn.
    assert_eq!(rig.conversation().contexts, before);
    assert!(rig.app.run_view(CONV).is_none());
    assert!(!rig.app.model_run_is_active(CONV));
}

#[test]
fn task_overlay_the_card_is_drawn_live_in_its_own_place() {
    let mut rig = Rig::new();
    rig.adopted();
    seeded(&mut rig, spawn_card("attested-1", None));
    push(&mut rig, child_text("还在跑"));
    rig.until("the commit", |rig| streamed_reply(rig).is_some());

    let view = overlay(&mut rig).unwrap();
    let persisted = rig.conversation().contexts.clone();
    let rendered: Vec<RenderedContext<'_>> = view.rendered_contexts(&persisted).collect();
    assert_eq!(rendered.iter().map(|context| context.id()).collect::<Vec<_>>(), ["ctx_user", SPAWN, "ctx_assistant"], "nothing moves and nothing is added");
    assert!(matches!(rendered[0], RenderedContext::Persisted(_)));
    assert!(matches!(rendered[2], RenderedContext::Persisted(_)));
    let card = rendered[1];
    assert!(matches!(card, RenderedContext::Live(_)));
    assert!(card.streaming());
    assert_eq!(card.stream_status(), Some(StreamingToolPhase::Completed));
    assert_eq!(card.live().unwrap().status, Some(SubagentLiveStatus::Running));
}

#[test]
fn task_overlay_events_for_a_conversation_with_a_run_are_left_to_its_stream() {
    let mut rig = Rig::new();
    rig.adopted();
    seeded(&mut rig, spawn_card("attested-1", None));
    rig.send("继续");
    let run = rig.wait_for_run(1);
    rig.until("the run", |rig| rig.app.run_view(CONV).is_some());

    push(&mut rig, child_text("不该出现"));
    rig.idle(300);
    assert!(overlay(&mut rig).is_none());
    assert!(rig.state().task_overlays.is_empty(), "nothing buffered for later either");

    run.finish(response(json!([])));
    rig.run_exits();
    assert!(overlay(&mut rig).is_none(), "what the run's time dropped stays dropped");
    push(&mut rig, child_text("回合结束后"));
    rig.until("the commit", |rig| streamed_reply(rig).as_deref() == Some("回合结束后"));
}

#[test]
fn task_overlay_an_event_for_a_card_the_conversation_does_not_have_shows_nothing() {
    let mut rig = Rig::new();
    rig.adopted();
    seeded(&mut rig, spawn_card("attested-1", None));
    push(&mut rig, json!({ "type": "subagent_delta", "round": 1, "callId": "call_elsewhere", "channel": "text", "delta": "迷路的" }));
    rig.idle(300);
    assert!(overlay(&mut rig).is_none());
    assert!(rig.state().task_overlays.is_empty());
}

#[test]
fn task_overlay_a_run_starting_drops_it() {
    let mut rig = Rig::new();
    rig.adopted();
    seeded(&mut rig, spawn_card("attested-1", None));
    push(&mut rig, child_text("后台进行中"));
    rig.until("the commit", |rig| streamed_reply(rig).is_some());

    rig.send("看看进度");
    let run = rig.wait_for_run(1);
    rig.until("the run", |rig| rig.app.run_view(CONV).is_some());
    assert!(rig.state().task_overlays.is_empty(), "the run's stream owns the card now");
    assert!(overlay(&mut rig).is_none());

    run.finish(response(json!([])));
    rig.run_exits();
    assert!(overlay(&mut rig).is_none(), "the run's end brings nothing back");
}

#[test]
fn task_overlay_a_terminal_status_settles_the_card_until_the_backfill_replaces_it() {
    let mut rig = Rig::new();
    rig.adopted();
    seeded(&mut rig, spawn_card("attested-1", Some(checkpoint())));
    push(&mut rig, child_text("最后一段"));
    push(&mut rig, child_status("idle"));
    rig.until("the commit", |rig| spawn_live(rig).is_some_and(|card| card.live.unwrap().status == Some(SubagentLiveStatus::Idle)));
    // The child is done, but the persisted card has not caught up: the settled transcript stays on screen.
    let card = spawn_live(&mut rig).unwrap();
    assert!(card.live.unwrap().contexts.iter().all(|context| !context.is_streaming()));

    // The host backfilled the record and the renderer reloaded it: the persisted card is the authority again.
    let backfilled = spawn_card("attested-2", Some(record("completed", json!([
        { "id": "ctx_child_first", "kind": "assistant", "content": "第一轮读完了", "createdAt": AT },
        { "id": "ctx_child_last", "kind": "assistant", "content": "最后一段", "createdAt": AT },
    ]))));
    rig.app.doc.workspaces[0].conversations[0].contexts[1] = backfilled;
    assert!(overlay(&mut rig).is_none());
    assert!(rig.state().task_overlays.is_empty());
}

#[test]
fn task_overlay_a_card_whose_persisted_copy_changed_is_seeded_again_from_it() {
    let mut rig = Rig::new();
    rig.adopted();
    seeded(&mut rig, spawn_card("attested-1", None));
    push(&mut rig, child_text("旧的"));
    rig.until("the commit", |rig| streamed_reply(rig).is_some());

    rig.app.doc.workspaces[0].conversations[0].contexts[1] = spawn_card("attested-2", Some(checkpoint()));
    push(&mut rig, child_text("新的"));
    rig.until("the second commit", |rig| streamed_reply(rig).as_deref() == Some("新的"));
    let card = spawn_live(&mut rig).unwrap();
    assert_eq!(card.item, rig.conversation().contexts[1], "drawn over the card as it is now");
    assert_eq!(card.live.unwrap().contexts[0].id(), "ctx_child_first");
}

#[test]
fn task_overlay_workflow_progress_is_kept_per_call() {
    let mut rig = Rig::new();
    rig.adopted();
    let workflow = context(json!({
        "id": "ctx_tool_workflow", "kind": "tool", "toolName": "workflow", "round": 1, "providerCallId": "call_workflow",
        "input": { "scriptName": "audit" }, "result": tool_result(true, "workflow started"), "createdAt": AT
    }));
    rig.set_contexts(vec![user("ctx_user", "跑个工作流"), workflow]);
    push(&mut rig, json!({ "type": "workflow_progress", "round": 1, "callId": "call_workflow", "runId": "wf_run_1", "entry": { "kind": "agent", "index": 0, "state": "start", "label": "scan" } }));
    rig.until("the commit", |rig| overlay(rig).is_some_and(|view| view.workflow_progress_by_call.get("call_workflow").is_some_and(|entries| entries.len() == 1)));
    let view = overlay(&mut rig).unwrap();
    assert_eq!(view.workflow_run_id_by_call.get("call_workflow").map(String::as_str), Some("wf_run_1"));
    assert_eq!(view.get("ctx_tool_workflow").and_then(|card| card.live.as_ref()).and_then(|live| live.status), Some(SubagentLiveStatus::Running));
}

#[test]
fn task_overlay_deleting_the_conversation_forgets_it() {
    let mut rig = Rig::new();
    rig.adopted();
    seeded(&mut rig, spawn_card("attested-1", None));
    push(&mut rig, child_text("还在跑"));
    rig.until("the commit", |rig| streamed_reply(rig).is_some());

    rig.app.delete_conversation(WS, CONV);
    rig.until("the deletion", |rig| rig.app.conversation(CONV).is_none());
    assert!(rig.state().task_overlays.is_empty());
}
