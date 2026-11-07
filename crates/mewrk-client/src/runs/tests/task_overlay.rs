//! `taskStreamEvent`: a background task that outlived the run that started it, streaming into the overlay the timeline
//! draws over its persisted card.

use std::sync::Arc;

use serde_json::{json, Value};

use super::fixtures::{announced, assistant, context, tool_result, user, AT};
use super::rig::*;
use crate::model::ContextItem;
use crate::runs::{LiveContext, RenderedContext, StreamingToolPhase, SubagentLiveState, SubagentLiveStatus, TaskOverlayView};

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

/// The child's streamed reply for round 2, in a live state of the spawn card.
fn reply_in(live: &SubagentLiveState) -> Option<String> {
    live.contexts.iter().find(|context| context.id() == format!("subagent-stream-{SPAWN}-assistant-2")).map(|context| match &context.item {
        ContextItem::Assistant { content, .. } => content.clone(),
        _ => String::new(),
    })
}

/// The child's streamed reply, as the overlay's card holds it.
fn streamed_reply(rig: &mut Rig) -> Option<String> {
    reply_in(spawn_live(rig)?.live.as_ref()?)
}

/// The same, as the run's own stream shows it while the run is live.
fn run_reply(rig: &mut Rig) -> Option<String> {
    let view = rig.app.run_view(CONV)?;
    let card = view.contexts.iter().find(|context| context.id() == SPAWN)?;
    reply_in(card.live.as_ref()?)
}

fn json_of(context: &ContextItem) -> Value {
    serde_json::to_value(context).unwrap()
}

/// What the host's store holds for the conversation: what the reload that settles a run brings back.
fn stored(rig: &mut Rig, contexts: Vec<ContextItem>) {
    rig.host.store.lock().unwrap().get_mut(CONV).unwrap().contexts = contexts;
}

/// A run that spawns the background child, which is part-way through its round-2 reply when the test settles the run.
fn run_spawning_the_child(rig: &mut Rig) -> Arc<RunCall> {
    rig.send("派个后台代理去梳理仓库");
    let run = rig.wait_for_run(1);
    run.emit(announced(1, CALL, "agent_spawn", SPAWN));
    run.emit(json!({ "type": "tool_execution_completed", "round": 1, "callId": CALL, "result": tool_result(true, "已在后台启动 scout") }));
    run.emit(child_status("running"));
    run.emit(child_text("正在读"));
    rig.until("the child's text in the run", |rig| run_reply(rig).as_deref() == Some("正在读"));
    run
}

/// The host settles the run: its stream delivers `run_concluded`, ahead of the answer `run_model` returns.
fn host_settles(rig: &mut Rig, run: &RunCall) {
    let request_id = run.args["requestId"].as_str().unwrap().to_owned();
    run.emit(json!({ "type": "run_concluded", "requestId": request_id }));
    rig.until("run_concluded", |rig| rig.state().settled_on_host.get(CONV) == Some(&request_id));
}

fn spawn_attestation(rig: &Rig) -> Option<String> {
    rig.conversation().contexts.iter().find(|context| context.id() == SPAWN).and_then(|context| match context {
        ContextItem::Tool { attestation, .. } => Some(attestation.clone()),
        _ => None,
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

/// A task an earlier run started goes on streaming while a later run is live, and its card goes on moving: on that run's
/// stream, whose reducer has no row for the call and hands the event on, and pushed, from before the run began on the host.
/// The run's view draws the card in its place, ahead of the run's own projection.
#[test]
fn task_overlay_a_task_from_an_earlier_run_keeps_moving_while_a_later_run_is_live() {
    let mut rig = Rig::new();
    rig.adopted();
    seeded(&mut rig, spawn_card("attested-1", None));
    rig.send("继续");
    let run = rig.wait_for_run(1);
    rig.until("the run", |rig| rig.app.run_view(CONV).is_some());

    push(&mut rig, child_text("推来的"));
    run.emit(child_text("，随运行流来的"));
    rig.until("both in the run's view", |rig| run_reply(rig).as_deref() == Some("推来的，随运行流来的"));
    assert!(overlay(&mut rig).is_none(), "with a run, its view is what to read");
    let view = rig.app.run_view(CONV).unwrap();
    assert_eq!(view.in_place, 1);
    let persisted = rig.conversation().contexts.clone();
    let rendered: Vec<RenderedContext<'_>> = view.rendered_contexts(&persisted).collect();
    assert_eq!(rendered[1].id(), SPAWN, "drawn where its persisted card is, not after the run's projection");
    assert!(matches!(rendered[1], RenderedContext::Live(_)));
    assert_eq!(rendered[1].live().unwrap().status, Some(SubagentLiveStatus::Running));

    run.finish(response(json!([])));
    rig.run_exits();
    assert_eq!(streamed_reply(&mut rig).as_deref(), Some("推来的，随运行流来的"), "the run's end leaves it where it was");
    push(&mut rig, child_text("，回合结束后"));
    rig.until("the commit", |rig| streamed_reply(rig).as_deref() == Some("推来的，随运行流来的，回合结束后"));
}

/// A workflow an earlier run started reports on a later run's stream: its rows are kept under its card's context id and
/// reach the run's view beside the run's own.
#[test]
fn task_overlay_a_workflow_from_an_earlier_run_reports_through_a_later_runs_view() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.set_contexts(vec![user("ctx_user", "跑个工作流"), workflow_card()]);
    rig.send("顺便看看别的");
    let run = rig.wait_for_run(1);
    rig.until("the run", |rig| rig.app.run_view(CONV).is_some());

    run.emit(workflow_step(0, "scan"));
    rig.until("the row in the run's view", |rig| rig.app.run_view(CONV).is_some_and(|view| view.workflow_progress_by_call.get("ctx_tool_workflow").is_some_and(|entries| entries.len() == 1)));
    let view = rig.app.run_view(CONV).unwrap();
    assert_eq!(view.workflow_run_id_by_call.get("ctx_tool_workflow").map(String::as_str), Some("wf_run_1"));
    assert!(view.contexts[..view.in_place].iter().any(|context| context.id() == "ctx_tool_workflow"));
}

#[test]
fn task_overlay_an_event_for_a_card_the_conversation_does_not_have_shows_nothing() {
    let mut rig = Rig::new();
    rig.adopted();
    seeded(&mut rig, spawn_card("attested-1", None));
    push(&mut rig, json!({ "type": "subagent_delta", "round": 1, "callId": "call_elsewhere", "channel": "text", "delta": "迷路的" }));
    // Past the commit that folds it.
    rig.idle(crate::runs::model_stream::MODEL_STREAM_COMMIT_INTERVAL_MS + 300);
    assert!(overlay(&mut rig).is_none());
    assert!(rig.state().task_overlays.is_empty());
}

/// Starting a run used to drop the overlay, and with it whatever the card had streamed that its record lacks.
#[test]
fn task_overlay_a_run_starting_keeps_what_the_card_streamed() {
    let mut rig = Rig::new();
    rig.adopted();
    seeded(&mut rig, spawn_card("attested-1", None));
    push(&mut rig, child_text("后台进行中"));
    rig.until("the commit", |rig| streamed_reply(rig).is_some());

    rig.send("看看进度");
    let run = rig.wait_for_run(1);
    rig.until("the run", |rig| rig.app.run_view(CONV).is_some());
    assert_eq!(run_reply(&mut rig).as_deref(), Some("后台进行中"));

    run.finish(response(json!([])));
    rig.run_exits();
    assert_eq!(streamed_reply(&mut rig).as_deref(), Some("后台进行中"));
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

/// A running child's checkpoint rewrites its card: the host saved the round the child settled and re-attested the card.
/// What streamed since is not in it, so the card carries on over the new copy instead of starting over from it.
#[test]
fn task_overlay_a_running_childs_checkpoint_keeps_what_streamed_since() {
    let mut rig = Rig::new();
    rig.adopted();
    seeded(&mut rig, spawn_card("attested-1", None));
    push(&mut rig, child_text("第二轮"));
    rig.until("the commit", |rig| streamed_reply(rig).is_some());

    rig.app.doc.workspaces[0].conversations[0].contexts[1] = spawn_card("attested-2", Some(checkpoint()));
    push(&mut rig, child_text("还在写"));
    rig.until("the second commit", |rig| streamed_reply(rig).as_deref() == Some("第二轮还在写"));
    let card = spawn_live(&mut rig).unwrap();
    assert_eq!(card.item, rig.conversation().contexts[1], "drawn over the card as it is now");
    assert_eq!(card.live.unwrap().status, Some(SubagentLiveStatus::Running));
}

/// The child's text in one of its own rounds.
fn child_text_in(round: usize, delta: &str) -> Value {
    json!({ "type": "subagent_event", "round": 1, "callId": CALL, "event": { "type": "text_delta", "round": round, "delta": delta } })
}

/// The views union a card's record rows with its live rows by id, and live rows carry the renderer's ids: a live row of
/// a round the checkpoint saved would show twice. The rebase keeps the record's copy of a saved round and the live rows
/// of the round in flight.
#[test]
fn task_overlay_a_checkpoint_replaces_the_live_rows_of_the_rounds_it_saved() {
    let mut rig = Rig::new();
    rig.adopted();
    seeded(&mut rig, spawn_card("attested-1", None));
    push(&mut rig, child_text_in(1, "第一轮读完了"));
    push(&mut rig, child_text_in(2, "第二轮在"));
    rig.until("the commit", |rig| streamed_reply(rig).as_deref() == Some("第二轮在"));

    // Round 1 settled and was saved; round 2 is still streaming.
    rig.app.doc.workspaces[0].conversations[0].contexts[1] = spawn_card("attested-2", Some(record("interrupted", json!([
        { "id": "ctx_child_task", "kind": "user", "content": "梳理仓库", "createdAt": AT },
        { "id": "ctx_child_r1", "kind": "assistant", "content": "第一轮读完了", "round": 1, "createdAt": AT },
    ]))));
    push(&mut rig, child_text_in(2, "写"));
    rig.until("the second commit", |rig| streamed_reply(rig).as_deref() == Some("第二轮在写"));
    let live = spawn_live(&mut rig).unwrap().live.unwrap();
    let ids: Vec<&str> = live.contexts.iter().map(LiveContext::id).collect();
    assert_eq!(ids, ["ctx_child_task", "ctx_child_r1", &format!("subagent-stream-{SPAWN}-assistant-2")], "round 1 once, as the record has it");
    assert_eq!(live.status, Some(SubagentLiveStatus::Running));
}

/// A provider that synthesizes its call ids can repeat one across rounds: the round the host's event carries picks the card.
#[test]
fn task_overlay_a_repeated_provider_call_id_is_told_apart_by_its_round() {
    let mut rig = Rig::new();
    rig.adopted();
    let card = |id: &str, round: usize| {
        context(json!({
            "id": id, "kind": "tool", "toolName": "agent_spawn", "round": round, "providerCallId": "call_0",
            "input": { "name": format!("scout-{round}"), "task": "梳理仓库" }, "result": tool_result(true, "已在后台启动"), "attestation": "attested", "createdAt": AT
        }))
    };
    rig.set_contexts(vec![user("ctx_user", "派两个后台代理"), card("ctx_tool_first", 1), card("ctx_tool_second", 2)]);
    push(&mut rig, json!({ "type": "subagent_delta", "round": 1, "callId": "call_0", "channel": "status", "delta": "running" }));
    rig.until("the commit", |rig| overlay(rig).is_some());
    let view = overlay(&mut rig).unwrap();
    assert!(view.get("ctx_tool_first").is_some());
    assert!(view.get("ctx_tool_second").is_none(), "the later card with the same id is another call");
}

/// A record with an outcome is the host's account of how the child ended, even when the child's own terminal status never
/// arrived: the persisted card is the authority again.
#[test]
fn task_overlay_a_record_with_an_outcome_takes_a_running_card_back() {
    let mut rig = Rig::new();
    rig.adopted();
    seeded(&mut rig, spawn_card("attested-1", Some(checkpoint())));
    push(&mut rig, child_text("最后一段"));
    rig.until("the commit", |rig| streamed_reply(rig).is_some());

    rig.app.doc.workspaces[0].conversations[0].contexts[1] = spawn_card("attested-2", Some(record("completed", json!([
        { "id": "ctx_child_first", "kind": "assistant", "content": "第一轮读完了", "createdAt": AT },
    ]))));
    assert!(overlay(&mut rig).is_none());
    assert!(rig.state().task_overlays.is_empty());
}

/// The seam this module exists for. The host settles the run while its child is mid-sentence: the part streamed on the
/// run's stream goes on in the overlay instead of falling back to the persisted card, a part pushed between the host
/// settling the run and the renderer doing so is held rather than dropped, and the reload that settles the run, which
/// brings the child's latest checkpoint, does not start the card over.
#[test]
fn task_overlay_a_settling_run_hands_its_running_child_over_mid_message() {
    let mut rig = Rig::new();
    rig.adopted();
    let run = run_spawning_the_child(&mut rig);
    let asked = context(run.last_context());
    stored(&mut rig, vec![asked.clone(), spawn_card("attested-2", Some(checkpoint())), assistant("ctx_assistant", "已派生，回合结束。")]);
    // The host's body wins over the renderer's flush before the reload, as it does for a card the host re-attested.
    *rig.host.keep_prose.lock().unwrap() = true;

    assert!(rig.state().task_overlays.is_empty(), "the run's own task is the run's to draw");
    host_settles(&mut rig, &run);
    push(&mut rig, child_text("取 crates"));
    // Past the commit that would have folded it, were it not held.
    rig.idle(crate::runs::model_stream::MODEL_STREAM_COMMIT_INTERVAL_MS + 300);
    assert!(overlay(&mut rig).is_none(), "the run's own view is the one on screen until the renderer settles it");
    assert_eq!(run_reply(&mut rig).as_deref(), Some("正在读"), "held, not folded, while the run is settling");
    assert!(rig.state().task_overlays.contains_key(CONV), "held for the handover");

    run.finish(response(json!([json_of(&spawn_card("attested-1", None)), json_of(&assistant("ctx_assistant", "已派生，回合结束。"))])));
    rig.run_exits();
    rig.until("the handover", |rig| streamed_reply(rig).as_deref() == Some("正在读取 crates"));
    let card = spawn_live(&mut rig).unwrap();
    assert_eq!(card.live.as_ref().unwrap().status, Some(SubagentLiveStatus::Running));

    rig.until("the reload", |rig| spawn_attestation(rig).as_deref() == Some("attested-2"));
    push(&mut rig, child_text("，再读 docs"));
    rig.until("the next commit", |rig| streamed_reply(rig).as_deref() == Some("正在读取 crates，再读 docs"));
    let card = spawn_live(&mut rig).unwrap();
    assert_eq!(Some(&card.item), rig.conversation().contexts.iter().find(|context| context.id() == SPAWN));
}

/// A child that reported how it ended inside the run has nothing left to stream: nothing is handed over.
#[test]
fn task_overlay_a_child_that_ended_in_the_run_is_not_handed_over() {
    let mut rig = Rig::new();
    rig.adopted();
    let run = run_spawning_the_child(&mut rig);
    run.emit(child_status("idle"));
    rig.until("the child's end", |rig| {
        rig.app.run_view(CONV).is_some_and(|view| view.contexts.iter().any(|context| context.id() == SPAWN && context.live.as_ref().is_some_and(|live| live.status == Some(SubagentLiveStatus::Idle))))
    });
    let asked = context(run.last_context());
    stored(&mut rig, vec![asked, spawn_card("attested-1", None)]);

    run.finish(response(json!([json_of(&spawn_card("attested-1", None))])));
    rig.run_exits();
    rig.idle(200);
    assert!(overlay(&mut rig).is_none());
    assert!(rig.state().task_overlays.is_empty());
}

fn workflow_card() -> ContextItem {
    context(json!({
        "id": "ctx_tool_workflow", "kind": "tool", "toolName": "workflow", "round": 1, "providerCallId": "call_workflow",
        "input": { "scriptName": "audit" }, "result": tool_result(true, "workflow started"), "createdAt": AT
    }))
}

fn workflow_step(index: usize, label: &str) -> Value {
    json!({ "type": "workflow_progress", "round": 1, "callId": "call_workflow", "runId": "wf_run_1", "entry": { "kind": "agent", "index": index, "state": "start", "label": label } })
}

/// Written under the provider call id the host's event names, read by the context id every view knows the card by: the
/// rows are kept under the context id.
#[test]
fn task_overlay_workflow_progress_is_kept_under_the_cards_context_id() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.set_contexts(vec![user("ctx_user", "跑个工作流"), workflow_card()]);
    push(&mut rig, workflow_step(0, "scan"));
    rig.until("the commit", |rig| overlay(rig).is_some_and(|view| view.workflow_progress_by_call.get("ctx_tool_workflow").is_some_and(|entries| entries.len() == 1)));
    let view = overlay(&mut rig).unwrap();
    assert!(!view.workflow_progress_by_call.contains_key("call_workflow"));
    assert_eq!(view.workflow_run_id_by_call.get("ctx_tool_workflow").map(String::as_str), Some("wf_run_1"));
    assert_eq!(view.get("ctx_tool_workflow").and_then(|card| card.live.as_ref()).and_then(|live| live.status), Some(SubagentLiveStatus::Running));
}

/// A workflow that outlives its run keeps the rows its run streamed, and one reported while the run was settling.
#[test]
fn task_overlay_a_settling_run_hands_its_workflows_progress_over() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.send("跑个工作流");
    let run = rig.wait_for_run(1);
    run.emit(announced(1, "call_workflow", "workflow", "ctx_tool_workflow"));
    run.emit(json!({ "type": "tool_execution_completed", "round": 1, "callId": "call_workflow", "result": tool_result(true, "workflow started") }));
    run.emit(workflow_step(0, "scan"));
    rig.until("the row in the run", |rig| rig.app.run_view(CONV).is_some_and(|view| view.workflow_progress_by_call.get("ctx_tool_workflow").is_some_and(|entries| entries.len() == 1)));
    assert!(!rig.app.run_view(CONV).unwrap().workflow_progress_by_call.contains_key("call_workflow"));
    let asked = context(run.last_context());
    stored(&mut rig, vec![asked, workflow_card()]);

    host_settles(&mut rig, &run);
    push(&mut rig, workflow_step(1, "review"));
    run.finish(response(json!([json_of(&workflow_card())])));
    rig.run_exits();
    rig.until("the handover", |rig| overlay(rig).is_some_and(|view| view.workflow_progress_by_call.get("ctx_tool_workflow").is_some_and(|entries| entries.len() == 2)));
    let view = overlay(&mut rig).unwrap();
    assert_eq!(view.workflow_run_id_by_call.get("ctx_tool_workflow").map(String::as_str), Some("wf_run_1"));
    assert!(view.get("ctx_tool_workflow").is_some());
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
