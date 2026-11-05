//! The logic of `App.modelRun.test.tsx`.

use serde_json::json;

use super::fixtures::{assistant, context, user};
use super::rig::*;
use crate::model::{ContextItem, ReasoningEffort};
use crate::runs::composer::Deferred;
use crate::runs::conversation_turns::ConversationTurnStatus;
use crate::runs::run_contexts::round_prose_context_id;
use crate::runs::types::{PromptKind, QuestionAction, QuestionResponse, ToolPromptDecision};

fn seeded(rig: &mut Rig) {
    rig.set_contexts(vec![user("ctx_user_seed", "跑个后台任务"), assistant("ctx_assistant_seed", "已派生，回合结束。")]);
}

fn push(rig: &mut Rig, event: serde_json::Value) {
    crate::runs::events::handle_push_event(&mut rig.app, &event);
}

#[test]
fn runs_task_settled_wakes_an_idle_conversation_with_no_user_message() {
    let mut rig = Rig::new();
    rig.adopted();
    seeded(&mut rig);
    rig.host.auto_reply(response(json!([reply_context("ctx_wake_reply", "任务结果已送达")])));
    push(&mut rig, json!({ "type": "taskSettled", "conversationId": CONV }));
    let run = rig.wait_for_run(1);
    assert_eq!(run.args["request"]["conversationId"], json!(CONV));
    // A wake run adds no user message; it uses the existing timeline as request context.
    assert_eq!(run.context_ids(), ["ctx_user_seed", "ctx_assistant_seed"]);
    rig.run_exits();
    rig.idle(30);
    assert_eq!(rig.runs(), 1, "the consumed record starts no second wake");
}

#[test]
fn runs_a_wake_starts_a_conversation_that_is_not_on_screen_and_leaves_the_one_that_is() {
    let mut rig = Rig::configured(&[], |app, host| {
        let mut other = conversation("conversation-2");
        other.contexts = vec![user("other_user", "别的任务")];
        host.store_conversation(other.clone());
        app.doc.workspaces[0].conversations.push(other);
    });
    rig.adopted();
    rig.host.auto_reply(response(json!([])));
    push(&mut rig, json!({ "type": "taskSettled", "conversationId": "conversation-2" }));
    let run = rig.wait_for_run(1);
    assert_eq!(run.args["request"]["conversationId"], json!("conversation-2"));
    assert_eq!(rig.runs(), 1);
}

#[test]
fn runs_a_wake_pending_rescan_restores_what_a_reload_lost() {
    let mut rig = Rig::configured(&[], |_, host| host.reply("list_wake_pending_conversations", Ok(json!([CONV]))));
    seeded(&mut rig);
    rig.host.auto_reply(response(json!([])));
    let run = rig.wait_for_run(1);
    assert_eq!(run.context_ids(), ["ctx_user_seed", "ctx_assistant_seed"]);
}

#[test]
fn runs_a_failed_adoption_does_not_count_and_a_retry_opens_the_latch_for_wakes() {
    let mut rig = Rig::configured(&[], |_, host| {
        host.resumable_failures.store(1, std::sync::atomic::Ordering::SeqCst);
        host.reply("list_wake_pending_conversations", Ok(json!([CONV])));
    });
    seeded(&mut rig);
    rig.host.auto_reply(response(json!([])));
    rig.idle(100);
    assert_eq!(rig.runs(), 0, "nothing wakes while adoption has not completed");
    assert!(!rig.state().resumable_runs_adopted);
    rig.wait_for_run(1);
    assert!(rig.host.count("list_resumable_runs") >= 2);
}

#[test]
fn runs_a_queue_that_will_run_carries_the_wake_but_a_paused_one_does_not() {
    let mut rig = Rig::new();
    rig.adopted();
    seeded(&mut rig);
    rig.app.doc.workspaces[0].conversations[0].queue_paused = true;
    rig.app.doc.workspaces[0].conversations[0].queued_messages = vec![crate::model::QueuedMessage { id: "held".into(), content: "held".into(), images: vec![], files: vec![], created_at: "2026-07-24T00:00:02Z".into() }];
    rig.host.auto_reply(response(json!([])));
    push(&mut rig, json!({ "type": "taskSettled", "conversationId": CONV }));
    let run = rig.wait_for_run(1);
    // A queue the user paused with Stop sends nothing until they send again, so it
    // cannot carry the result: the wake runs on its own.
    assert_eq!(run.context_ids(), ["ctx_user_seed", "ctx_assistant_seed"]);
    assert_eq!(rig.conversation().queued_messages.len(), 1);
}

#[test]
fn runs_keeps_the_draft_and_opens_provider_settings_when_no_model_is_selected() {
    let mut rig = Rig::configured(&[], |app, _| app.doc.global_settings.active_provider_id = None);
    rig.adopted();
    rig.send("没有模型");
    rig.idle(20);
    assert_eq!(rig.app.composer_draft(CONV), "没有模型");
    assert!(rig.app.run_signals().open_provider_settings);
    assert_eq!(rig.runs(), 0);
}

#[test]
fn runs_a_provider_with_a_derived_endpoint_counts_as_configured_with_no_address() {
    let mut rig = Rig::configured(&[], |app, _| {
        let provider = &mut app.doc.assets.api_providers[0];
        provider.base_url = String::new();
        provider.family = serde_json::from_value(json!("openai_codex")).unwrap();
    });
    rig.adopted();
    rig.host.auto_reply(response(json!([])));
    rig.send("codex 的地址是派生的");
    rig.wait_for_run(1);
}

#[test]
fn runs_keeps_a_persistent_model_error_and_retries_without_duplicating_the_user_context() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.send("只发送一次");
    let first = rig.wait_for_run(1);
    let user_id = first.last_context()["id"].as_str().unwrap().to_owned();
    first.fail("网络中断");
    rig.run_exits();
    let error = rig.app.model_run_error(CONV).unwrap();
    assert_eq!(error.message, "网络中断");
    assert!(error.retryable);
    // A failure a turn renders does not repeat in the composer.
    assert!(error.request_id.is_some());
    assert_eq!(rig.turns().last().unwrap().error.as_ref().unwrap().message, "网络中断");

    rig.app.retry_model_run();
    let second = rig.wait_for_run(2);
    assert_eq!(second.context_ids().iter().filter(|id| **id == user_id).count(), 1);
    assert_eq!(rig.app.model_run_error(CONV), None, "a new attempt retires the failure");
    let reply = reply_context("ctx_retry", "重试成功");
    rig.host.store.lock().unwrap().get_mut(CONV).unwrap().contexts.push(context(reply.clone()));
    second.finish(response(json!([reply])));
    rig.run_exits();
    assert_eq!(rig.conversation().contexts.iter().filter(|context| context.id() == user_id).count(), 1);
    assert_eq!(rig.app.model_run_error(CONV), None);
}

#[test]
fn runs_keeps_a_failed_image_request_available_when_retry_switches_to_a_text_only_model() {
    let mut rig = Rig::configured(&["image_recognition"], |_, _| {});
    rig.adopted();
    rig.set_contexts(vec![context(json!({ "id": "with-image", "kind": "user", "content": "图", "images": [{ "id": "i", "name": "i.png", "mime": "image/png", "width": 1, "height": 1, "bytes": 1 }], "createdAt": "2026-07-20T00:00:00Z" }))]);
    rig.send("再看看");
    let first = rig.wait_for_run(1);
    first.fail("网络中断");
    rig.run_exits();
    assert!(rig.app.model_run_error(CONV).is_some());
    rig.app.doc.assets.api_providers[0].models[0].capabilities.clear();
    rig.app.retry_model_run();
    rig.idle(30);
    assert_eq!(rig.runs(), 1, "a text-only model cannot take the images");
    assert!(rig.app.model_run_error(CONV).is_some(), "and the failed request stays available");
}

#[test]
fn runs_sends_the_selected_reasoning_effort() {
    let mut rig = Rig::configured(&[], |app, _| app.doc.workspaces[0].conversations[0].settings.reasoning_effort = ReasoningEffort::High);
    rig.adopted();
    rig.send("想久一点");
    let run = rig.wait_for_run(1);
    assert_eq!(request_of(&run).reasoning_effort, ReasoningEffort::High);
    assert_eq!(run.args["request"]["model"]["id"], json!("model-1"));
    run.finish(response(json!([])));
}

#[test]
fn runs_keeps_a_stopped_run_visible_until_the_backend_returns_its_settlement() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.send("稍后返回");
    let run = rig.wait_for_run(1);
    run.emit(json!({ "type": "text_delta", "round": 1, "delta": "停止前已经生成的内容" }));
    let assistant_id = round_prose_context_id(run.args["requestId"].as_str().unwrap(), 1, "assistant");
    rig.until("the partial", |rig| rig.app.run_view(CONV).is_some_and(|view| view.contexts.iter().any(|context| context.id() == assistant_id)));
    rig.app.cancel_run(CONV, |_, _| {});
    rig.until("the cancel", |rig| rig.host.count("cancel_conversation_run") == 1);
    assert!(rig.app.model_is_stopping(CONV));
    assert!(rig.app.run_view(CONV).is_some(), "still live until it settles");

    // The host writes streamed prose back itself, so its store holds the fragment a refresh reads.
    rig.host.store.lock().unwrap().get_mut(CONV).unwrap().contexts.push(context(json!({ "id": assistant_id, "kind": "assistant", "content": "停止前已经生成的内容", "interrupted": true, "createdAt": "2026-07-24T00:00:01Z" })));
    run.finish(stopped_response("cancelled"));
    rig.run_exits();
    let partial = rig.conversation().contexts.iter().find(|context| context.id() == assistant_id).unwrap();
    let ContextItem::Assistant { content, interrupted, .. } = partial else { panic!() };
    assert_eq!(content, "停止前已经生成的内容");
    assert!(*interrupted);
    rig.until("the stop button to come back", |rig| !rig.app.model_is_stopping(CONV));
}

#[test]
fn runs_discards_the_round_outright_when_a_stop_lands_before_the_first_message() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.send("刚发出去就后悔了");
    let first = rig.wait_for_run(1);
    rig.app.cancel_run(CONV, |_, _| {});
    rig.until("the cancel", |rig| rig.host.count("cancel_conversation_run") == 1);
    let mut stopped = stopped_response("cancelled");
    stopped["durationMs"] = json!(4000);
    first.finish(stopped);
    rig.run_exits();
    // A round that produced nothing leaves nothing on screen; discarded, not merely hidden.
    assert!(rig.turns().is_empty());
    assert!(rig.conversation().contexts.iter().any(|context| matches!(context, ContextItem::User { content, .. } if content == "刚发出去就后悔了")));

    // Sending again therefore opens a round of its own, starting at zero rather
    // than inheriting the four seconds the stopped attempt spent waiting.
    rig.app.send_message();
    let second = rig.wait_for_run(2);
    let turns = rig.turns();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].segment_count, 1);
    assert!(turns[0].duration_ms.unwrap_or(0) < 2000);
    second.finish(response(json!([])));
    rig.run_exits();
}

#[test]
fn runs_continues_the_round_a_stop_left_unfinished_when_the_composer_is_empty_even_after_its_messages_are_deleted() {
    let usage = |input: u64, cached: u64, output: u64| json!({ "inputTokens": input, "cachedInputTokens": cached, "outputTokens": output, "totalTokens": input + cached + output });
    let mut rig = Rig::new();
    rig.adopted();
    rig.send("第一轮问题");
    let first = rig.wait_for_run(1);
    first.emit(json!({ "type": "usage_updated", "round": 1, "usage": usage(100, 20, 7) }));
    first.emit(json!({ "type": "text_delta", "round": 1, "delta": "只写到一半" }));
    rig.until("the partial", |rig| rig.app.run_view(CONV).is_some_and(|view| !view.contexts.is_empty()));
    rig.app.cancel_run(CONV, |_, _| {});
    rig.until("the cancel", |rig| rig.host.count("cancel_conversation_run") == 1);
    let mut stopped = stopped_response("cancelled");
    stopped["usage"] = usage(100, 20, 7);
    stopped["durationMs"] = json!(12_000);
    first.finish(stopped);
    rig.run_exits();
    let turns = rig.turns();
    assert_eq!(turns.len(), 1);
    assert_eq!((turns[0].status, turns[0].duration_ms), (ConversationTurnStatus::Interrupted, Some(12_000)));

    // The user throws the whole stopped round away, anchor message included.
    rig.set_contexts(vec![]);
    // Sending with nothing typed is not a new round; it resumes the one the
    // stop left open, so its elapsed time and token counters keep running.
    rig.app.send_message();
    let second = rig.wait_for_run(2);
    assert!(second.context_ids().is_empty());
    second.emit(json!({ "type": "usage_updated", "round": 1, "usage": usage(140, 30, 5) }));
    // `segment_count` is the discriminator: a second leg of the same round,
    // carrying the first leg's counters forward rather than starting at zero.
    rig.until("the usage to add up", |rig| rig.turns()[0].usage.input_tokens == Some(240));
    let live = rig.turns();
    assert_eq!(live.len(), 1);
    assert_eq!((live[0].status, live[0].segment_count), (ConversationTurnStatus::Running, 2));
    assert_eq!((live[0].usage.cached_input_tokens, live[0].usage.output_tokens), (Some(50), Some(12)));

    let settled = json!([
        { "id": "ctx_continued_reasoning", "kind": "reasoning", "content": "接着上一轮补完", "createdAt": "2026-07-24T00:00:03Z" },
        { "id": "ctx_continued_final", "kind": "assistant", "content": "补完了被打断的那一轮", "createdAt": "2026-07-24T00:00:04Z" }
    ]);
    rig.host.store.lock().unwrap().get_mut(CONV).unwrap().contexts = settled.as_array().unwrap().iter().cloned().map(context).collect();
    let mut done = response(settled);
    done["usage"] = usage(140, 30, 9);
    done["durationMs"] = json!(8_000);
    done["stopReason"] = json!("completed");
    second.finish(done);
    rig.run_exits();
    // 12s + 8s of wall clock, and both requests' input, cached-input and output
    // counters added up — the real cost of the round, not just its last leg.
    let completed = rig.turns();
    assert_eq!(completed.len(), 1);
    assert_eq!((completed[0].status, completed[0].duration_ms), (ConversationTurnStatus::Completed, Some(20_000)));
    assert_eq!((completed[0].usage.input_tokens, completed[0].usage.cached_input_tokens, completed[0].usage.output_tokens), (Some(240), Some(50), Some(16)));
    rig.until("the settled contexts", |rig| rig.context_ids().contains(&"ctx_continued_final".to_owned()));
}

fn attached_request(app: &crate::App) -> serde_json::Value {
    json!({
        "provider": app.doc.assets.api_providers[0], "model": app.doc.assets.api_providers[0].models[0], "reasoningEffort": "medium",
        "conversationId": CONV, "workspacePath": "/workspace", "enabledTools": [], "contexts": [], "tools": []
    })
}

#[test]
fn runs_adopts_a_host_owned_run_on_load_replays_its_buffer_and_applies_the_settlement() {
    let mut rig = Rig::configured(&[], |app, host| {
        host.reply("list_resumable_runs", Ok(json!([{ "conversationId": CONV, "requestId": "run_adopted", "running": true }])));
        let request = attached_request(app);
        *host.attach.lock().unwrap() = Some(Box::new(move |sink| {
            // Buffered events replayed before the attach response must enter the live projection unchanged.
            sink(json!({ "type": "text_delta", "round": 1, "delta": "重放的" }));
            sink(json!({ "type": "text_delta", "round": 1, "delta": "直播内容" }));
            json!({ "status": "running", "requestId": "run_adopted", "request": request, "droppedEvents": 0 })
        }));
        let reply = reply_context("ctx_adopted", "重放的直播内容");
        let mut stored = conversation(CONV);
        stored.contexts = vec![context(reply.clone())];
        host.store_conversation(stored);
        host.reply("take_run_settlement", Ok(json!({ "response": response(json!([reply])) })));
    });
    rig.until("the adopted run", |rig| rig.app.run_view(CONV).is_some_and(|view| view.contexts.iter().any(|context| matches!(&context.item, ContextItem::Assistant { content, .. } if content == "重放的直播内容"))));
    assert_eq!(rig.runs(), 0);
    assert!(rig.state().resumable_runs_adopted);

    // `run_concluded` claims the settlement slot before removing the run while retaining finalized content.
    let sink = rig.host.attach_sink.lock().unwrap().clone().unwrap();
    sink(json!({ "type": "run_concluded", "requestId": "run_adopted" }));
    rig.until("the settlement to be claimed", |rig| rig.host.count("take_run_settlement") == 1);
    rig.run_exits();
    rig.until("the settled reply", |rig| rig.context_ids() == ["ctx_adopted"]);
}

#[test]
fn runs_settles_an_adopted_run_whose_marker_arrived_before_the_attach_answered() {
    let mut rig = Rig::configured(&[], |app, host| {
        host.reply("list_resumable_runs", Ok(json!([{ "conversationId": CONV, "requestId": "run_quick", "running": true }])));
        let request = attached_request(app);
        *host.attach.lock().unwrap() = Some(Box::new(move |sink| {
            sink(json!({ "type": "run_concluded", "requestId": "run_quick" }));
            json!({ "status": "running", "requestId": "run_quick", "request": request, "droppedEvents": 0 })
        }));
        host.reply("take_run_settlement", Ok(json!({ "response": response(json!([])) })));
    });
    rig.until("the settlement to be claimed", |rig| rig.host.count("take_run_settlement") == 1);
    rig.run_exits();
}

#[test]
fn runs_applies_the_retained_settlement_of_a_run_that_finished_while_the_renderer_was_away() {
    let mut rig = Rig::configured(&[], |app, host| {
        host.reply("list_resumable_runs", Ok(json!([{ "conversationId": CONV, "requestId": "run_finished_away", "running": false }])));
        let reply = reply_context("ctx_finished_away", "离场期间完成的回复");
        let mut stored = conversation(CONV);
        stored.contexts = vec![context(reply.clone())];
        host.store_conversation(stored);
        let request = attached_request(app);
        *host.attach.lock().unwrap() = Some(Box::new(move |_| json!({ "status": "finished", "requestId": "run_finished_away", "request": request, "settlement": { "response": response(json!([reply])) } })));
    });
    rig.adopted();
    rig.run_exits();
    rig.until("the reply", |rig| rig.context_ids() == ["ctx_finished_away"]);
    assert_eq!(rig.runs(), 0);
}

#[test]
fn runs_cancels_a_surviving_run_whose_conversation_no_longer_exists() {
    let mut rig = Rig::configured(&[], |_, host| {
        host.reply("list_resumable_runs", Ok(json!([{ "conversationId": "gone", "requestId": "run_gone", "running": true }])));
    });
    rig.adopted();
    rig.until("the orphan's cleanup", |rig| rig.host.calls_of("cancel_conversation_run").iter().any(|args| args["conversationId"] == json!("gone")) && rig.host.count("take_run_settlement") == 1);
}

#[test]
fn runs_keeps_a_run_visible_when_native_cancellation_cleanup_is_not_acknowledged() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.send("保持停止边界");
    let run = rig.wait_for_run(1);
    rig.host.reply("cancel_conversation_run", Ok(json!(false)));
    rig.host.reply("cancel_model_run", Err("input cleanup not acknowledged".to_owned()));
    let stopped = Deferred::<bool>::new();
    let heard = stopped.clone();
    rig.app.cancel_run(CONV, move |_, settled| heard.resolve(settled));
    rig.until("the refusal", |_| stopped.is_settled());
    // If cleanup is not acknowledged, stopping fails silently but the control
    // becomes available again and the run remains visible.
    assert!(!stopped.wait());
    assert!(!rig.app.model_is_stopping(CONV));
    assert!(rig.app.run_view(CONV).is_some());
    // A stop the host never took leaves the run going; its natural end must still hand the queue on.
    assert!(!rig.conversation().queue_paused);
    run.finish(stopped_response("completed"));
    rig.run_exits();
}

#[test]
fn runs_persists_completed_tools_and_keeps_no_unattested_result_for_an_unfinished_one() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.send("跑两个工具");
    let run = rig.wait_for_run(1);
    run.emit(json!({ "type": "tool_call_announced", "round": 1, "callId": "done", "toolName": "read", "contextId": "ctx_done" }));
    run.emit(json!({ "type": "tool_call_arguments_ready", "round": 1, "callId": "done", "input": { "path": "a" } }));
    run.emit(json!({ "type": "tool_execution_completed", "round": 1, "callId": "done", "result": { "success": true, "output": "ok", "executedAt": "2026-07-24T00:00:01Z", "durationMs": 1 } }));
    run.emit(json!({ "type": "tool_call_announced", "round": 1, "callId": "open", "toolName": "bash", "contextId": "ctx_open" }));
    run.emit(json!({ "type": "tool_execution_started", "round": 1, "callId": "open" }));
    rig.until("the rows", |rig| rig.state().controller.run(CONV).is_some_and(|run| run.streamed_tools_by_round.get(&1).is_some_and(|tools| tools.len() == 2)));
    rig.app.cancel_run(CONV, |_, _| {});
    rig.until("the cancel", |rig| rig.host.count("cancel_conversation_run") == 1);
    run.finish(stopped_response("cancelled"));
    rig.run_exits();
    // Only the backend mints the receipt that authorizes a tool result into the document. A call the stream left
    // unfinished has no attested result, so it goes; the one that finished stays.
    assert!(!rig.context_ids().contains(&"ctx_open".to_owned()));
}

#[test]
fn runs_persists_streamed_output_when_the_model_request_fails() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.send("半路断掉");
    let run = rig.wait_for_run(1);
    run.emit(json!({ "type": "text_delta", "round": 1, "delta": "断之前的内容" }));
    let assistant_id = round_prose_context_id(run.args["requestId"].as_str().unwrap(), 1, "assistant");
    rig.host.store.lock().unwrap().get_mut(CONV).unwrap().contexts.push(context(json!({ "id": assistant_id, "kind": "assistant", "content": "断之前的内容", "interrupted": true, "createdAt": "2026-07-24T00:00:01Z" })));
    // Not committed yet, on purpose: the failure path has to flush what is still buffered.
    run.fail("连接重置");
    rig.run_exits();
    let kept = rig.conversation().contexts.iter().find(|context| context.id() == assistant_id).unwrap();
    let ContextItem::Assistant { content, interrupted, .. } = kept else { panic!() };
    assert_eq!(content, "断之前的内容");
    assert!(*interrupted);
    assert_eq!(rig.app.model_run_error(CONV).unwrap().message, "连接重置");
}

/// The buffer's content reaches the read model before the host's copy does, so nothing it says flickers away.
#[test]
fn runs_a_failed_runs_buffered_output_reaches_the_read_model_at_settlement() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.send("半路断掉");
    let run = rig.wait_for_run(1);
    run.emit(json!({ "type": "text_delta", "round": 1, "delta": "还在缓冲里的" }));
    let assistant_id = round_prose_context_id(run.args["requestId"].as_str().unwrap(), 1, "assistant");
    rig.host.reply("load_conversation", Err("unreachable, so nothing overwrites the read model".to_owned()));
    run.fail("连接重置");
    rig.run_exits();
    assert!(rig.context_ids().contains(&assistant_id));
}

#[test]
fn runs_reads_a_first_round_connection_failure_in_the_timeline_rather_than_a_composer_banner() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.send("连不上");
    let run = rig.wait_for_run(1);
    run.fail("无法连接");
    rig.run_exits();
    let turns = rig.turns();
    assert_eq!(turns.len(), 1, "a round with a notice is a record");
    assert_eq!(turns[0].error.as_ref().unwrap().message, "无法连接");
    assert_eq!(turns[0].status, ConversationTurnStatus::Interrupted);
    assert!(rig.app.model_run_error(CONV).unwrap().request_id.is_some());
}

#[test]
fn runs_drops_an_empty_failed_round_when_the_next_run_retracts_its_notice_instead_of_continuing_it() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.send("连不上");
    let first = rig.wait_for_run(1);
    first.fail("无法连接");
    rig.run_exits();
    assert_eq!(rig.turns().len(), 1);
    rig.app.send_message();
    let second = rig.wait_for_run(2);
    let turns = rig.turns();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].segment_count, 1, "a new round, not a continuation of the failed one");
    assert_eq!(turns[0].error, None);
    second.finish(response(json!([])));
    rig.run_exits();
}

#[test]
fn runs_keeps_a_dismissed_failure_from_leaving_an_empty_round_behind() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.send("连不上");
    let run = rig.wait_for_run(1);
    run.fail("无法连接");
    rig.run_exits();
    rig.app.dismiss_model_run_error(CONV);
    assert_eq!(rig.app.model_run_error(CONV), None);
    assert!(rig.turns().is_empty());
}

#[test]
fn runs_shows_a_structured_request_failure_as_a_transient_notice_and_never_persists_it_as_context() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.send("超额了");
    let run = rig.wait_for_run(1);
    let mut failed = stopped_response("error");
    failed["error"] = json!({ "message": "额度不足", "round": 1, "attempts": 3 });
    run.finish(failed);
    rig.run_exits();
    let error = rig.app.model_run_error(CONV).unwrap();
    assert!(error.message.starts_with("额度不足") && error.message.contains('2'), "{}", error.message);
    assert!(error.retryable);
    assert_eq!(rig.turns().last().unwrap().status, ConversationTurnStatus::Interrupted);
    assert!(!rig.conversation().contexts.iter().any(|context| matches!(context, ContextItem::Assistant { content, .. } if content.contains("额度不足"))));
}

#[test]
fn runs_keeps_the_failed_partial_during_a_retry_wait_and_replaces_it_when_the_retry_streams() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.send("重试");
    let run = rig.wait_for_run(1);
    let assistant_id = round_prose_context_id(run.args["requestId"].as_str().unwrap(), 1, "assistant");
    let text = |rig: &mut Rig| rig.app.run_view(CONV).and_then(|view| view.contexts.iter().find_map(|context| match &context.item {
        ContextItem::Assistant { id, content, .. } if *id == assistant_id => Some(content.clone()),
        _ => None,
    }));
    run.emit(json!({ "type": "text_delta", "round": 1, "delta": "失败的部分" }));
    run.emit(json!({ "type": "stream_retry_scheduled", "round": 1, "attempt": 1, "maxAttempts": 3, "delayMs": 1000, "message": "overloaded" }));
    rig.until("the notice", |rig| rig.app.run_view(CONV).is_some_and(|view| view.retry.is_some()));
    assert_eq!(text(&mut rig).as_deref(), Some("失败的部分"), "backoff erases nothing");
    run.emit(json!({ "type": "text_delta", "round": 1, "delta": "新的回复" }));
    rig.until("the retry to stream", |rig| rig.app.run_view(CONV).is_some_and(|view| view.retry.is_none()));
    assert_eq!(text(&mut rig).as_deref(), Some("新的回复"));
    run.finish(response(json!([])));
    rig.run_exits();
}

fn approval(prompt_id: &str, kind: &str) -> serde_json::Value {
    json!({ "promptId": prompt_id, "toolName": "bash", "kind": kind, "label": "Shell", "summary": "ls", "riskLevel": "medium", "reason": "writes", "allowAlwaysOffered": false, "mandatory": false })
}

fn stream_approval(prompt_id: &str, kind: &str) -> serde_json::Value {
    let mut event = approval(prompt_id, kind);
    event["type"] = json!("tool_approval_requested");
    event
}

#[test]
fn runs_draws_the_approval_card_for_a_call_the_model_raised_and_resumes_the_run() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.send("需要批准");
    let run = rig.wait_for_run(1);
    run.emit(stream_approval("prompt-1", "tool"));
    rig.until("the card", |rig| !rig.app.tool_prompts(CONV).is_empty());
    assert!(rig.app.conversation_is_blocked(CONV));
    assert_eq!(rig.app.conversation_status(CONV, false), crate::runs::view::ConversationStatus::Blocked);
    assert_eq!(rig.app.active_tool_prompt(CONV).unwrap().0.prompt_id, "prompt-1");
    rig.app.decide_tool_prompt(CONV, "prompt-1", ToolPromptDecision::AllowOnce, None);
    assert!(rig.app.tool_prompts(CONV).is_empty());
    rig.until("the answer", |rig| rig.host.count("resolve_tool_prompt") == 1);
    let answered = &rig.host.calls_of("resolve_tool_prompt")[0];
    assert_eq!(answered["promptId"], json!("prompt-1"));
    assert_eq!(answered["decision"], json!("allow_once"));
    run.emit(json!({ "type": "tool_approval_resolved", "promptId": "prompt-1", "approved": true }));
    run.finish(response(json!([])));
    rig.run_exits();
}

#[test]
fn runs_draws_a_background_tasks_approval_card_with_no_run_in_sight_and_takes_it_down_on_the_push() {
    let mut rig = Rig::new();
    rig.adopted();
    let mut event = approval("prompt-bg", "tool");
    event["type"] = json!("toolApprovalRequested");
    event["conversationId"] = json!(CONV);
    push(&mut rig, event.clone());
    push(&mut rig, event);
    assert_eq!(rig.app.tool_prompts(CONV).len(), 1, "a prompt-id dedupes a repeated push");
    push(&mut rig, json!({ "type": "toolApprovalResolved", "conversationId": CONV, "promptId": "prompt-bg", "approved": false }));
    assert!(rig.app.tool_prompts(CONV).is_empty());
}

#[test]
fn runs_re_raises_a_background_card_the_renderer_missed_after_a_reload() {
    let mut row = approval("prompt-missed", "tool");
    row["conversationId"] = json!(CONV);
    let mut rig = Rig::configured(&[], |_, host| host.reply("list_pending_tool_prompts", Ok(json!([row]))));
    rig.until("the rescan's card", |rig| !rig.app.tool_prompts(CONV).is_empty());
    assert_eq!(rig.app.tool_prompts(CONV)[0].prompt_id, "prompt-missed");
}

#[test]
fn runs_stacks_concurrent_cards_behind_a_pager_instead_of_hiding_them() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.send("并发");
    let run = rig.wait_for_run(1);
    run.emit(stream_approval("p1", "tool"));
    run.emit(stream_approval("p2", "tool"));
    rig.until("both cards", |rig| rig.app.tool_prompts(CONV).len() == 2);
    let (prompt, index, total) = rig.app.active_tool_prompt(CONV).unwrap();
    assert_eq!((prompt.prompt_id.as_str(), index, total), ("p1", 0, 2));
    rig.app.navigate_tool_prompt(CONV, 1);
    assert_eq!(rig.app.active_tool_prompt(CONV).unwrap().1, 1);
    rig.app.navigate_tool_prompt(CONV, 5);
    assert_eq!(rig.app.active_tool_prompt(CONV).unwrap().1, 1);
    // The cursor is clamped when the stack shrinks.
    rig.app.decide_tool_prompt(CONV, "p2", ToolPromptDecision::Deny, None);
    assert_eq!(rig.app.active_tool_prompt(CONV).unwrap(), (rig.app.tool_prompts(CONV)[0].clone(), 0, 1));
    run.finish(response(json!([])));
    rig.run_exits();
}

#[test]
fn runs_keeps_a_background_card_the_host_still_holds_when_a_foreground_run_ends_and_drops_one_it_does_not() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.send("结束");
    let run = rig.wait_for_run(1);
    run.emit(stream_approval("kept", "tool"));
    run.emit(stream_approval("gone", "tool"));
    rig.until("both cards", |rig| rig.app.tool_prompts(CONV).len() == 2);
    let mut alive = approval("kept", "tool");
    alive["conversationId"] = json!(CONV);
    rig.host.reply("list_pending_tool_prompts", Ok(json!([alive])));
    run.finish(response(json!([])));
    rig.until("the reconcile", |rig| rig.app.tool_prompts(CONV).len() == 1);
    assert_eq!(rig.app.tool_prompts(CONV)[0].prompt_id, "kept");
}

#[test]
fn runs_a_composer_message_is_a_way_out_of_a_question_card() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.send("问我");
    let run = rig.wait_for_run(1);
    run.emit(stream_approval("question-1", "question"));
    rig.until("the card", |rig| rig.app.question_is_pending(CONV));
    assert_eq!(rig.app.tool_prompts(CONV)[0].kind, PromptKind::Question);
    // Steering first is what keeps a close from ending the turn: the host only ends it when no message is waiting.
    rig.send("算了，换个方向");
    rig.until("the answer", |rig| rig.host.count("resolve_tool_prompt") == 1);
    assert_eq!(rig.host.count("steer_model_run"), 1);
    let order: Vec<String> = rig.host.calls.lock().unwrap().iter().map(|(name, _)| name.clone()).filter(|name| name == "steer_model_run" || name == "resolve_tool_prompt").collect();
    assert_eq!(order, ["steer_model_run", "resolve_tool_prompt"]);
    let answered = &rig.host.calls_of("resolve_tool_prompt")[0];
    assert_eq!(answered["question"]["action"], json!("close"));
    assert_eq!(answered["decision"], json!("deny"));
    rig.until("the card to come down", |rig| !rig.app.question_is_pending(CONV));
    run.finish(response(json!([])));
}

#[test]
fn runs_answers_a_question_card_with_what_was_filled_in_and_keeps_it_when_the_host_refuses() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.send("问我");
    let run = rig.wait_for_run(1);
    run.emit(stream_approval("question-1", "question"));
    rig.until("the card", |rig| rig.app.question_is_pending(CONV));
    rig.host.reply("resolve_tool_prompt", Err("gone".to_owned()));
    let response_body = QuestionResponse { action: QuestionAction::Submit, answers: vec![Some("Postgres".into())], ..Default::default() };
    let first = Deferred::<bool>::new();
    let heard = first.clone();
    rig.app.answer_question_prompt(CONV, "question-1", response_body.clone(), move |_, taken| heard.resolve(taken));
    rig.until("the refusal", |_| first.is_settled());
    assert!(!first.wait());
    assert!(rig.app.question_is_pending(CONV), "the card stays up until the host took the answer");
    let second = Deferred::<bool>::new();
    let heard = second.clone();
    rig.app.answer_question_prompt(CONV, "question-1", response_body, move |_, taken| heard.resolve(taken));
    rig.until("the answer", |_| second.is_settled());
    assert!(second.wait());
    assert!(!rig.app.question_is_pending(CONV));
    assert_eq!(rig.host.calls_of("resolve_tool_prompt")[1]["question"]["answers"], json!(["Postgres"]));
    run.finish(response(json!([])));
}

#[test]
fn runs_waits_on_the_card_before_running_a_tool_call_the_user_assembled_by_hand() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.host.reply("request_tool_approval", Ok(json!({ "expiresInMs": 0, "prompt": approval("manual-1", "tool") })));
    rig.host.reply("resolve_tool_prompt", Ok(json!({ "nonce": "minted", "expiresInMs": 90000 })));
    rig.host.reply("execute_tool", Ok(json!({ "success": true, "output": "done", "executedAt": "2026-07-24T00:00:01Z", "durationMs": 1 })));
    let request: crate::model::ToolExecutionRequest = serde_json::from_value(json!({ "conversationId": CONV, "workspacePath": "/workspace", "toolName": "bash", "input": { "command": "ls" } })).unwrap();
    let result = Deferred::<Result<String, String>>::new();
    let heard = result.clone();
    rig.app.run_manual_tool(CONV, request, move |_, outcome| heard.resolve(outcome.map(|result| result.output)));
    rig.until("the card", |rig| !rig.app.tool_prompts(CONV).is_empty());
    assert_eq!(rig.host.count("execute_tool"), 0);
    rig.app.decide_tool_prompt(CONV, "manual-1", ToolPromptDecision::AllowOnce, None);
    rig.until("the tool", |_| result.is_settled());
    assert_eq!(result.wait(), Ok("done".to_owned()));
    assert_eq!(rig.host.calls_of("execute_tool")[0]["approvalNonce"], json!("minted"));
}

#[test]
fn runs_a_manual_cards_retraction_tells_the_caller_rather_than_leaving_it_hanging() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.host.reply("request_tool_approval", Ok(json!({ "expiresInMs": 0, "prompt": approval("manual-2", "tool") })));
    let request: crate::model::ToolExecutionRequest = serde_json::from_value(json!({ "conversationId": CONV, "workspacePath": "/workspace", "toolName": "bash", "input": {} })).unwrap();
    let result = Deferred::<Result<String, String>>::new();
    let heard = result.clone();
    rig.app.run_manual_tool(CONV, request, move |_, outcome| heard.resolve(outcome.map(|result| result.output)));
    rig.until("the card", |rig| !rig.app.tool_prompts(CONV).is_empty());
    push(&mut rig, json!({ "type": "toolApprovalResolved", "conversationId": CONV, "promptId": "manual-2", "approved": false }));
    rig.until("the refusal", |_| result.is_settled());
    assert!(result.wait().is_err());
}

#[test]
fn runs_refuses_a_send_the_store_silently_dropped_rather_than_run_on_a_history_without_it() {
    let mut rig = Rig::new();
    rig.adopted();
    // The host keeps its own prose when the read model is stale: the durable write succeeds without the message.
    *rig.host.keep_prose.lock().unwrap() = true;
    rig.send("被吞掉的消息");
    rig.until("the refusal", |rig| rig.app.model_run_error(CONV).is_some());
    assert_eq!(rig.runs(), 0);
    let error = rig.app.model_run_error(CONV).unwrap();
    assert!(!error.retryable);
    assert_eq!(error.request_id, None, "no turn renders it, so the composer does");
    assert_eq!(rig.app.composer_draft(CONV), "被吞掉的消息", "the message stays with the user");
}

#[test]
fn runs_says_why_when_the_host_refused_the_write_outright() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.host.refuse_updates.lock().unwrap().insert(CONV.to_owned(), "storage is read-only".to_owned());
    rig.send("写不进去");
    rig.until("the refusal", |rig| rig.app.model_run_error(CONV).is_some());
    assert!(rig.app.model_run_error(CONV).unwrap().message.contains("storage is read-only"));
    assert_eq!(rig.runs(), 0);
}

#[test]
fn runs_rolls_a_turn_back_when_its_durable_write_fails() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.host.fail_durable_saves.store(1, std::sync::atomic::Ordering::SeqCst);
    rig.send("存不下来");
    rig.until("the rollback", |rig| {
        !rig.app.model_run_is_active(CONV) && rig.host.calls_of("save_document").iter().filter(|args| args["durable"] == json!(true)).count() >= 2
    });
    assert!(rig.conversation().contexts.is_empty());
    assert_eq!(rig.conversation().title, "New task");
    assert_eq!(rig.runs(), 0);
    assert_eq!(rig.app.composer_draft(CONV), "存不下来");
}

#[test]
fn runs_titles_a_new_conversation_from_its_first_message() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.host.auto_reply(response(json!([])));
    rig.send("帮我看看这段代码为什么这么慢");
    let run = rig.wait_for_run(1);
    assert_eq!(run.last_context()["content"], json!("帮我看看这段代码为什么这么慢"));
    assert_eq!(rig.conversation().title, "帮我看看这段代码为什么这么慢");
}

#[test]
fn runs_marks_a_conversation_that_finished_off_screen_until_it_is_opened() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.send("后台跑完");
    let run = rig.wait_for_run(1);
    rig.app.runs_reconcile();
    assert_eq!(rig.app.conversation_status(CONV, false), crate::runs::view::ConversationStatus::Running);
    rig.app.set_active(Some(WS), Some("elsewhere"));
    run.finish(response(json!([])));
    rig.run_exits();
    rig.app.runs_reconcile();
    assert_eq!(rig.app.conversation_status(CONV, false), crate::runs::view::ConversationStatus::Completed);
    rig.app.set_active(Some(WS), Some(CONV));
    rig.app.runs_reconcile();
    assert_eq!(rig.app.conversation_status(CONV, false), crate::runs::view::ConversationStatus::Idle);
}

#[test]
fn runs_the_turn_record_survives_a_reload_as_upstream_stored_it() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.send("存一条");
    let run = rig.wait_for_run(1);
    let raw = rig.app.ui_store().get(crate::runs::conversation_turns::CONVERSATION_TURNS_STORAGE_KEY).map(str::to_owned).unwrap();
    let reloaded = crate::runs::conversation_turns::load_conversation_turns(Some(&raw), crate::runs::util::now_ms());
    // A turn stored as running reads as interrupted until adoption resumes it.
    assert_eq!(reloaded[CONV][0].status, ConversationTurnStatus::Interrupted);
    run.finish(response(json!([])));
    rig.run_exits();
}
