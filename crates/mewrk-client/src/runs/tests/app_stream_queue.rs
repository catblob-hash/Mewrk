//! The logic of `App.streamQueue.test.tsx`.

use std::sync::atomic::Ordering;

use serde_json::json;

use super::fixtures::{context, user};
use super::rig::*;
use crate::model::{ContextItem, QueuedMessage};
use crate::runs::composer::Deferred;
use crate::runs::conversation_turns::ConversationTurnStatus;
use crate::runs::model_stream::{ModelRunRetryState, StreamingToolPhase};

fn image_context(id: &str) -> ContextItem {
    context(json!({ "id": id, "kind": "user", "content": "看这张图", "images": [{ "id": "history-image", "name": "history.png", "mime": "image/png", "width": 640, "height": 480, "bytes": 1000 }], "createdAt": "2026-07-20T00:00:00Z" }))
}

#[test]
fn runs_blocks_ordinary_send_when_projected_context_images_meet_a_text_only_model() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.set_contexts(vec![image_context("history-image-user")]);
    rig.send("继续分析");
    rig.idle(30);
    assert_eq!(rig.app.composer_draft(CONV), "继续分析");
    assert_eq!(rig.runs(), 0);
}

#[test]
fn runs_surfaces_a_blocked_lifecycle_hook_without_offering_an_api_retry() {
    let mut rig = Rig::new();
    rig.adopted();
    let mut blocked = response(json!([{ "id": "ctx_hook_failure", "kind": "assistant", "content": "UserPromptSubmit 生命周期钩子已阻止提示词", "createdAt": "2026-07-11T00:00:00Z" }]));
    blocked["stopReason"] = json!("hook_blocked");
    rig.host.auto_reply(blocked);
    rig.host.store.lock().unwrap().get_mut(CONV).unwrap().contexts = vec![context(json!({ "id": "ctx_hook_failure", "kind": "assistant", "content": "UserPromptSubmit 生命周期钩子已阻止提示词", "createdAt": "2026-07-11T00:00:00Z" }))];
    rig.send("触发钩子");
    rig.wait_for_run(1);
    rig.until("the hook's reason on the timeline", |rig| rig.context_ids().contains(&"ctx_hook_failure".to_owned()));
    rig.run_exits();
    assert_eq!(rig.app.model_run_error(CONV), None);
    assert_eq!(rig.turns().last().unwrap().status, ConversationTurnStatus::Interrupted);
}

#[test]
fn runs_renders_streamed_deltas_and_replaces_them_with_the_final_persisted_response() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.send("写点什么");
    let run = rig.wait_for_run(1);
    assert_eq!(rig.app.composer_draft(CONV), "", "the composer clears once the turn is durable");
    run.emit(json!({ "type": "reasoning_delta", "round": 1, "delta": "思考" }));
    run.emit(json!({ "type": "text_delta", "round": 1, "delta": "你" }));
    run.emit(json!({ "type": "text_delta", "round": 1, "delta": "好" }));
    let request_id = run.args["requestId"].as_str().unwrap().to_owned();
    let assistant_id = format!("ctx_assistant_{request_id}_1");
    rig.until("the commit", |rig| rig.app.run_view(CONV).is_some_and(|view| view.contexts.iter().any(|context| context.id() == assistant_id)));
    let view = rig.app.run_view(CONV).unwrap();
    let ContextItem::Assistant { content, .. } = &view.contexts.iter().find(|context| context.id() == assistant_id).unwrap().item else { panic!() };
    assert_eq!(content, "你好");
    // The same projection until the next commit.
    assert!(std::sync::Arc::ptr_eq(&view, &rig.app.run_view(CONV).unwrap()));

    let final_contexts = json!([{ "id": assistant_id, "kind": "assistant", "content": "你好，世界", "createdAt": "2026-07-11T00:00:00Z" }]);
    rig.host.store.lock().unwrap().get_mut(CONV).unwrap().contexts.push(context(final_contexts[0].clone()));
    run.finish(response(final_contexts));
    rig.run_exits();
    assert!(rig.app.run_view(CONV).is_none());
    rig.until("the settled reply", |rig| {
        rig.conversation().contexts.iter().any(|context| matches!(context, ContextItem::Assistant { content, .. } if content == "你好，世界"))
    });
    assert_eq!(rig.conversation().contexts.iter().filter(|context| matches!(context, ContextItem::Assistant { .. })).count(), 1);
    assert_eq!(rig.turns().last().unwrap().status, ConversationTurnStatus::Completed);
}

#[test]
fn runs_commits_a_burst_of_deltas_in_one_batch_rather_than_one_per_event() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.send("写点什么");
    let run = rig.wait_for_run(1);
    let before = rig.state().controller.revision_of(CONV);
    for _ in 0..500 {
        run.emit(json!({ "type": "text_delta", "round": 1, "delta": "x" }));
    }
    rig.until("the burst to land", |rig| rig.state().controller.run(CONV).and_then(|run| run.streamed_text_by_round.get(&1).map(String::len)) == Some(500));
    let commits = rig.state().controller.revision_of(CONV) - before;
    assert!(commits <= 3, "the fixed cadence bounds commits, whatever the arrival rate: {commits}");
}

#[test]
fn runs_queues_composer_messages_during_streaming_steers_one_into_the_live_turn_then_sends_the_rest() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.send("开始长任务");
    let first = rig.wait_for_run(1);
    rig.send("优先检查失败日志");
    assert_eq!(rig.app.composer_draft(CONV), "");
    rig.send("然后补齐回归测试");
    assert_eq!(rig.conversation().queued_messages.len(), 2);
    rig.until("the queue to be durable", |rig| rig.host.calls_of("save_document").iter().filter(|args| args["durable"] == json!(true)).count() >= 3);

    let steered: QueuedMessage = rig.conversation().queued_messages[0].clone();
    let done = Deferred::<bool>::new();
    let heard = done.clone();
    rig.app.steer_queued_message(CONV, &steered.id, move |_, steered| heard.resolve(steered));
    rig.until("the steer", |_| done.is_settled());
    assert!(done.wait());
    let steer = rig.host.calls_of("steer_model_run");
    assert_eq!(steer.len(), 1);
    assert_eq!(steer[0]["requestId"], first.args["requestId"]);
    assert_eq!(steer[0]["content"], json!("优先检查失败日志"));
    // Until it arrives it stays listed, and cannot be steered or deleted a second time.
    assert!(rig.state().composer.steering_message_ids.contains(&steered.id));
    rig.app.delete_queued_message(WS, CONV, &steered.id);
    assert_eq!(rig.conversation().queued_messages.len(), 2);

    first.emit(json!({ "type": "user_input_received", "round": 2, "id": steered.id, "content": steered.content, "createdAt": steered.created_at }));
    rig.until("the delivery", |rig| !rig.state().composer.steering_message_ids.contains(&steered.id));
    // Steering closes the round it landed in, and the request carries on as a new one.
    assert_eq!(rig.turns().last().unwrap().anchor_context_id, steered.id);
    assert_eq!(rig.turns().last().unwrap().status, ConversationTurnStatus::Running);

    let settled = json!([
        { "id": steered.id, "kind": "user", "content": steered.content, "createdAt": steered.created_at },
        { "id": "queued-first-answer", "kind": "assistant", "content": "优先日志已检查", "createdAt": "2026-07-24T00:00:03Z" }
    ]);
    {
        let mut store = rig.host.store.lock().unwrap();
        let stored = store.get_mut(CONV).unwrap();
        stored.contexts.extend(settled.as_array().unwrap().iter().cloned().map(context));
        stored.queued_messages.retain(|message| message.id != steered.id);
    }
    first.finish(response(settled));
    let second = rig.wait_for_run(2);
    assert_eq!(second.last_context()["kind"], json!("user"));
    assert_eq!(second.last_context()["content"], json!("然后补齐回归测试"));
    let reply = reply_context("queued-second", "第二条排队消息已处理");
    rig.host.store.lock().unwrap().get_mut(CONV).unwrap().contexts.push(context(reply.clone()));
    second.finish(response(json!([reply])));
    rig.until("the queue to drain", |rig| !rig.app.model_run_is_active(CONV) && rig.conversation().queued_messages.is_empty());
    rig.until("the reply", |rig| rig.context_ids().contains(&"queued-second".to_owned()));
}

#[test]
fn runs_pauses_the_queue_on_stop_until_the_user_sends_again_then_sends_the_waiting_messages_in_order() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.send("开始长任务");
    let first = rig.wait_for_run(1);
    rig.send("排队的第一条");
    assert_eq!(rig.conversation().queued_messages.len(), 1);
    let stopped = Deferred::<bool>::new();
    let heard = stopped.clone();
    rig.app.cancel_run(CONV, move |_, settled| heard.resolve(settled));
    rig.until("the cancel", |rig| rig.host.count("cancel_conversation_run") == 1);
    assert!(rig.app.model_is_stopping(CONV));
    first.finish(stopped_response("cancelled"));
    rig.until("the stop to settle", |_| stopped.is_settled());
    assert!(stopped.wait());
    assert!(!rig.app.model_is_stopping(CONV));

    // Stopped, so nothing follows on its own: the message waits above the composer,
    // still deletable, until the user sends again.
    rig.idle(50);
    assert_eq!(rig.runs(), 1);
    assert_eq!(rig.conversation().queued_messages.len(), 1);
    assert!(rig.conversation().queue_paused);
    // The pause reaches the host with the conversation, so a restart keeps it too.
    rig.until("the pause to be written", |rig| rig.host.store.lock().unwrap()[CONV].queue_paused);

    rig.send("新的一条");
    let second = rig.wait_for_run(2);
    assert_eq!(second.last_context()["content"], json!("排队的第一条"));
    second.finish(response(json!([])));
    let third = rig.wait_for_run(3);
    assert_eq!(third.last_context()["content"], json!("新的一条"));
    third.finish(response(json!([])));
    rig.run_exits();
}

#[test]
fn runs_keeps_a_queue_paused_before_a_restart_waiting_after_it() {
    let mut rig = Rig::configured(&[], |app, _| {
        let conversation = &mut app.doc.workspaces[0].conversations[0];
        conversation.queue_paused = true;
        conversation.queued_messages = vec![QueuedMessage { id: "held".into(), content: "停下时排着的".into(), images: vec![], files: vec![], created_at: "2026-07-24T00:00:00Z".into() }];
    });
    rig.adopted();
    rig.idle(50);
    assert_eq!(rig.runs(), 0);
    assert_eq!(rig.conversation().queued_messages.len(), 1);
}

#[test]
fn runs_waits_for_a_newly_queued_message_to_be_saved_before_steering_it() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.send("启动长任务");
    rig.wait_for_run(1);
    let gate = Deferred::<Result<(), String>>::new();
    *rig.host.save_gate.lock().unwrap() = Some(gate.clone());
    let saves = rig.host.count("save_document");
    rig.send("立刻引导但必须先保存");
    rig.until("the durable save", |rig| rig.host.count("save_document") > saves);
    assert_eq!(rig.host.calls_of("save_document").last().unwrap()["durable"], json!(true));
    let id = rig.conversation().queued_messages[0].id.clone();
    let done = Deferred::<bool>::new();
    let heard = done.clone();
    rig.app.steer_queued_message(CONV, &id, move |_, steered| heard.resolve(steered));
    rig.idle(30);
    assert_eq!(rig.host.count("steer_model_run"), 0);

    *rig.host.save_gate.lock().unwrap() = None;
    gate.resolve(Ok(()));
    rig.until("the steer", |_| done.is_settled());
    assert!(done.wait());
    assert_eq!(rig.host.count("steer_model_run"), 1);
}

#[test]
fn runs_keeps_a_queued_message_and_does_not_steer_it_when_its_save_fails() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.send("启动失败保存测试");
    rig.wait_for_run(1);
    let gate = Deferred::<Result<(), String>>::new();
    *rig.host.save_gate.lock().unwrap() = Some(gate.clone());
    rig.send("保存失败仍留在队列");
    rig.until("the save", |rig| rig.host.calls_of("save_document").iter().filter(|args| args["durable"] == json!(true)).count() >= 2);
    let id = rig.conversation().queued_messages[0].id.clone();
    let done = Deferred::<bool>::new();
    let heard = done.clone();
    rig.app.steer_queued_message(CONV, &id, move |_, steered| heard.resolve(steered));
    rig.idle(20);
    assert_eq!(rig.host.count("steer_model_run"), 0);

    *rig.host.save_gate.lock().unwrap() = None;
    gate.resolve(Err("queue disk unavailable".to_owned()));
    rig.until("the refusal", |_| done.is_settled());
    assert!(!done.wait());
    assert_eq!(rig.host.count("steer_model_run"), 0);
    assert_eq!(rig.conversation().queued_messages[0].content, "保存失败仍留在队列");

    // A second try makes it durable first, then steers.
    let saves = rig.host.count("save_document");
    let retry = Deferred::<bool>::new();
    let heard = retry.clone();
    rig.app.steer_queued_message(CONV, &id, move |_, steered| heard.resolve(steered));
    rig.until("the retry", |_| retry.is_settled());
    assert!(retry.wait());
    assert_eq!(rig.host.calls_of("save_document").get(saves).map(|args| args["durable"].clone()), Some(json!(true)));
    assert_eq!(rig.host.count("steer_model_run"), 1);
}

fn queued(id: &str, content: &str) -> QueuedMessage {
    QueuedMessage { id: id.into(), content: content.into(), images: vec![], files: vec![], created_at: "2026-07-24T00:00:02Z".into() }
}

#[test]
fn runs_rolls_back_a_failed_automatic_queue_promotion_and_retries_it_only_on_explicit_request() {
    let mut rig = Rig::configured(&[], |app, host| {
        let conversation = &mut app.doc.workspaces[0].conversations[0];
        conversation.contexts = vec![user("prior", "此前请求")];
        conversation.queued_messages = vec![queued("head", "排在最前"), queued("tail", "随后处理尾部消息")];
        let mut stored = conversation.clone();
        stored.queued_messages.clear();
        host.store_conversation(stored);
        host.fail_durable_saves.store(1, Ordering::SeqCst);
    });
    rig.adopted();
    rig.until("the failed promotion's latch", |rig| rig.state().composer.failed_queued_promotion_ids.contains("head"));
    rig.until("the rollback", |rig| !rig.context_ids().contains(&"head".to_owned()));
    rig.idle(50);
    // The message stays queued, in its place, and nothing runs on its own.
    assert_eq!(rig.conversation().queued_messages.iter().map(|message| message.id.as_str()).collect::<Vec<_>>(), ["head", "tail"]);
    assert_eq!(rig.runs(), 0);

    rig.app.retry_failed_queued_promotion(WS, CONV, "head");
    let run = rig.wait_for_run(1);
    assert_eq!(run.last_context()["id"], json!("head"));
    run.finish(response(json!([])));
    let tail = rig.wait_for_run(2);
    assert_eq!(tail.last_context()["content"], json!("随后处理尾部消息"));
    tail.finish(response(json!([])));
    rig.run_exits();
}

#[test]
fn runs_latches_a_permanently_failing_queued_promotion_without_an_automatic_retry_loop() {
    let mut rig = Rig::configured(&[], |app, host| {
        app.doc.workspaces[0].conversations[0].queued_messages = vec![queued("always-fails", "发不出去")];
        host.fail_durable_saves.store(usize::MAX / 2, Ordering::SeqCst);
    });
    rig.adopted();
    rig.until("the latch", |rig| rig.state().composer.failed_queued_promotion_ids.contains("always-fails"));
    rig.idle(100);
    let saves = rig.host.count("save_document");
    rig.idle(200);
    assert_eq!(rig.runs(), 0);
    assert_eq!(rig.host.count("save_document"), saves, "no retry loop");
    assert_eq!(rig.conversation().queued_messages.len(), 1);
    assert!(rig.conversation().contexts.is_empty());

    rig.app.retry_failed_queued_promotion(WS, CONV, "always-fails");
    rig.until("the second attempt", |rig| rig.host.count("save_document") > saves);
    rig.until("the latch again", |rig| rig.state().composer.failed_queued_promotion_ids.contains("always-fails"));
    assert_eq!(rig.runs(), 0);
}

#[test]
fn runs_does_not_hot_loop_when_the_flush_before_a_promotion_keeps_failing() {
    let mut rig = Rig::configured(&[], |app, host| {
        app.doc.workspaces[0].conversations[0].queued_messages = vec![queued("head", "发不出去")];
        for _ in 0..1000 {
            host.reply("save_document", Err("disk gone".to_owned()));
        }
    });
    rig.adopted();
    for _ in 0..50 {
        rig.app.runs_reconcile();
        rig.idle(2);
    }
    assert!(rig.host.count("save_document") <= 3, "{}", rig.host.count("save_document"));
    assert_eq!(rig.runs(), 0);
}

#[test]
fn runs_folds_a_streamed_tool_call_and_a_retry_into_the_view() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.send("调个工具");
    let run = rig.wait_for_run(1);
    run.emit(json!({ "type": "tool_call_announced", "round": 1, "callId": "c1", "toolName": "bash", "contextId": "ctx_tool_1" }));
    run.emit(json!({ "type": "tool_execution_started", "round": 1, "callId": "c1" }));
    rig.until("the row", |rig| rig.app.run_view(CONV).is_some_and(|view| view.contexts.iter().any(|context| context.id() == "ctx_tool_1")));
    let view = rig.app.run_view(CONV).unwrap();
    let row = view.contexts.iter().find(|context| context.id() == "ctx_tool_1").unwrap();
    assert_eq!(row.stream_status, Some(StreamingToolPhase::Running));

    run.emit(json!({ "type": "stream_retry_scheduled", "round": 1, "attempt": 1, "maxAttempts": 3, "delayMs": 0, "message": "overloaded" }));
    rig.until("the notice", |rig| rig.app.run_view(CONV).is_some_and(|view| view.retry.is_some()));
    assert_eq!(rig.app.run_view(CONV).unwrap().retry, Some(ModelRunRetryState { round: 1, attempt: 1, max_attempts: 3, message: "overloaded".into(), dirty: true }));
    run.finish(response(json!([])));
    rig.run_exits();
}
