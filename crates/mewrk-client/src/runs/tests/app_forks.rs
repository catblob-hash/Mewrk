//! The logic of `App.forkRequests.test.tsx`, `App.autoCompact.test.tsx` and `App.planMode.test.tsx`.

use serde_json::json;

use super::fixtures::{context, user};
use super::rig::*;
use crate::runs::composer::Deferred;
use crate::runs::types::ToolPromptDecision;

const CHILD: &str = "conversation-child";

fn push(rig: &mut Rig, event: serde_json::Value) {
    crate::runs::events::handle_push_event(&mut rig.app, &event);
}

fn forked_child(title: &str) -> crate::model::Conversation {
    let mut child = conversation(CHILD);
    child.title = title.to_owned();
    child.contexts = vec![user("ctx_fork_prompt", "分叉的任务")];
    child
}

fn fork_card(fork_id: &str) -> serde_json::Value {
    json!({ "type": "forkRequested", "forkId": fork_id, "workspaceId": WS, "sourceConversationId": CONV, "sourceTitle": "来源", "prompt": "分叉的任务", "requestedAt": "2026-09-29T00:00:00Z" })
}

fn resolved(fork_id: &str, approved: bool, child: Option<&str>) -> serde_json::Value {
    json!({
        "type": "forkResolved", "forkId": fork_id, "workspaceId": WS, "sourceConversationId": CONV, "approved": approved, "childConversationId": child,
        "decision": { "forkId": fork_id, "workspaceId": WS, "sourceConversationId": CONV, "title": "t", "prompt": "p", "requestedAt": "2026-09-29T00:00:00Z", "decidedAt": "2026-09-29T00:00:01Z", "approved": approved, "childConversationId": child }
    })
}

#[test]
fn runs_draws_a_pushed_fork_request_approves_it_through_the_host_and_starts_the_childs_run_beside_its_source() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.host.store_conversation(forked_child("分叉"));
    push(&mut rig, fork_card("fork-1"));
    assert_eq!(rig.app.fork_requests().len(), 1);
    assert!(rig.app.conversation_is_blocked(CONV), "a fork card waits on the user too");
    rig.app.decide_fork_request("fork-1", true);
    assert!(rig.app.fork_requests().is_empty(), "the card comes down at once");
    rig.until("the answer", |rig| rig.host.count("resolve_fork_request") == 1);
    assert_eq!(rig.host.calls_of("resolve_fork_request")[0], json!({ "forkId": "fork-1", "approved": true }));
    assert_eq!(rig.runs(), 0, "the event, not the answer, starts the run");

    push(&mut rig, resolved("fork-1", true, Some(CHILD)));
    let run = rig.wait_for_run(1);
    assert_eq!(run.args["request"]["conversationId"], json!(CHILD));
    assert_eq!(run.args["forkPromptContextId"], json!("ctx_fork_prompt"));
    assert_eq!(rig.app.fork_decisions(CONV).len(), 1);
    // A new child opens the list, where the host filed it.
    assert_eq!(rig.app.doc.workspaces[0].conversations[0].id, CHILD);
    assert_eq!(rig.host.count("create_conversation"), 0);
    run.finish(response(json!([])));
    rig.until("the child's run to end", |rig| !rig.app.model_run_is_active(CHILD));
}

#[test]
fn runs_takes_a_denied_card_down_without_loading_or_running_anything() {
    let mut rig = Rig::new();
    rig.adopted();
    push(&mut rig, fork_card("fork-1"));
    rig.app.decide_fork_request("fork-1", false);
    rig.until("the answer", |rig| rig.host.count("resolve_fork_request") == 1);
    push(&mut rig, resolved("fork-1", false, None));
    rig.idle(30);
    assert_eq!(rig.host.count("load_conversation"), 0);
    assert_eq!(rig.runs(), 0);
}

#[test]
fn runs_re_lists_open_fork_requests_after_a_reload_and_ignores_a_duplicate_push_of_the_same_card() {
    let mut card = fork_card("fork-relisted");
    card.as_object_mut().unwrap().remove("type");
    let mut rig = Rig::configured(&[], |_, host| host.reply("list_pending_fork_requests", Ok(json!([card]))));
    rig.until("the card", |rig| !rig.app.fork_requests().is_empty());
    push(&mut rig, fork_card("fork-relisted"));
    assert_eq!(rig.app.fork_requests().len(), 1);
}

#[test]
fn runs_starts_an_auto_approved_forks_run_from_the_resolved_event_alone() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.host.store_conversation(forked_child("自动"));
    rig.host.auto_reply(response(json!([])));
    push(&mut rig, resolved("fork-auto", true, Some(CHILD)));
    let run = rig.wait_for_run(1);
    assert_eq!(run.context_ids(), ["ctx_fork_prompt"]);
}

#[test]
fn runs_recovers_a_delivered_fork_intent_after_reload_without_replaying_the_event() {
    let mut rig = Rig::configured(&[], |app, host| {
        // Already listed after the reload, the child keeps the place the host gave it.
        app.doc.workspaces[0].conversations.push(forked_child("重载窗口的首轮"));
        host.store_conversation(forked_child("重载窗口的首轮"));
        host.reply("list_pending_fork_starts", Ok(json!([{ "workspaceId": WS, "conversationId": CHILD, "promptContextId": "ctx_fork_prompt" }])));
    });
    rig.host.auto_reply(response(json!([])));
    let run = rig.wait_for_run(1);
    assert_eq!(run.context_ids().iter().filter(|id| *id == "ctx_fork_prompt").count(), 1);
    assert_eq!(rig.host.count("create_conversation"), 0);
    assert_eq!(rig.app.doc.workspaces[0].conversations.last().unwrap().id, CHILD);
}

#[test]
fn runs_waits_for_adoption_before_starting_a_fork() {
    let mut rig = Rig::configured(&[], |_, host| {
        host.resumable_failures.store(1, std::sync::atomic::Ordering::SeqCst);
        host.store_conversation(forked_child("等收养"));
    });
    push(&mut rig, resolved("lost-edge", true, Some(CHILD)));
    rig.idle(50);
    assert_eq!(rig.runs(), 0);
    rig.host.auto_reply(response(json!([])));
    rig.wait_for_run(1);
}

#[test]
fn runs_deduplicates_the_durable_list_and_a_live_event_and_does_not_restart_a_consumed_intent() {
    let mut rig = Rig::configured(&[], |app, host| {
        app.doc.workspaces[0].conversations.push(forked_child("只运行一次"));
        host.store_conversation(forked_child("只运行一次"));
        host.reply("list_pending_fork_starts", Ok(json!([{ "workspaceId": WS, "conversationId": CHILD, "promptContextId": "ctx_fork_prompt" }])));
    });
    let run = rig.wait_for_run(1);
    push(&mut rig, resolved("duplicate", true, Some(CHILD)));
    rig.idle(50);
    assert_eq!(rig.runs(), 1);
    assert_eq!(rig.host.count("load_conversation"), 1);
    run.finish(response(json!([])));
}

#[test]
fn runs_keeps_a_failed_child_load_visible_and_retries_only_on_user_intent() {
    let mut rig = Rig::configured(&[], |_, host| {
        host.store_conversation(forked_child("重试首轮"));
        host.reply("load_conversation", Err("injected load failure".to_owned()));
        host.reply("list_pending_fork_starts", Ok(json!([{ "workspaceId": WS, "conversationId": CHILD, "promptContextId": "ctx_fork_prompt" }])));
    });
    rig.until("the error", |rig| rig.app.fork_start_error().is_some());
    assert_eq!(rig.app.fork_start_error().as_deref(), Some("injected load failure"));
    rig.idle(30);
    assert_eq!(rig.runs(), 0);
    assert_eq!(rig.host.count("load_conversation"), 1);

    rig.host.reply("list_pending_fork_starts", Ok(json!([{ "workspaceId": WS, "conversationId": CHILD, "promptContextId": "ctx_fork_prompt" }])));
    rig.host.auto_reply(response(json!([])));
    rig.app.retry_fork_starts();
    assert_eq!(rig.app.fork_start_error(), None);
    rig.wait_for_run(1);
    assert_eq!(rig.host.count("create_conversation"), 0);
}

#[test]
fn runs_says_a_fork_could_not_start_when_no_model_is_configured() {
    let mut rig = Rig::configured(&[], |app, host| {
        app.doc.global_settings.active_provider_id = None;
        host.store_conversation(forked_child("没有模型"));
    });
    rig.adopted();
    push(&mut rig, resolved("fork-1", true, Some(CHILD)));
    rig.until("the error", |rig| rig.app.fork_start_error().is_some());
}

fn continuation() -> crate::model::Conversation {
    let mut child = conversation(CHILD);
    child.title = "长任务-handover-1".to_owned();
    child.contexts = vec![context(json!({ "id": "ctx_handoff-start_1", "kind": "user", "content": "接着做", "createdAt": "2026-09-29T00:00:00Z" }))];
    child.handoff_of = serde_json::from_value(json!({ "conversationId": CONV, "contextId": "x", "number": 1 })).ok();
    child
}

fn handed_off(starts_run: Option<bool>) -> serde_json::Value {
    let mut event = json!({ "type": "conversationHandedOff", "workspaceId": WS, "sourceConversationId": CONV, "childConversationId": CHILD });
    if let Some(starts_run) = starts_run {
        event["startsRun"] = json!(starts_run);
    }
    event
}

#[test]
fn runs_follows_a_handed_off_conversation_on_screen_into_its_continuation_and_starts_its_run() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.host.store_conversation(continuation());
    rig.host.auto_reply(response(json!([])));
    push(&mut rig, handed_off(None));
    let run = rig.wait_for_run(1);
    assert_eq!(run.args["request"]["conversationId"], json!(CHILD));
    // Armed on the host's opening message, the continuation's last user message.
    assert_eq!(run.args["forkPromptContextId"], json!("ctx_handoff-start_1"));
    assert_eq!(rig.host.count("create_conversation"), 0);
    // The page moved with the conversation.
    assert_eq!(rig.app.active_conversation_id().as_deref(), Some(CHILD));
    // Listed at the top, where a conversation the user starts goes.
    assert_eq!(rig.app.doc.workspaces[0].conversations[0].id, CHILD);
}

#[test]
fn runs_starts_the_continuation_in_the_background_when_another_conversation_is_on_screen() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.app.set_active(Some(WS), Some("elsewhere"));
    rig.host.store_conversation(continuation());
    rig.host.auto_reply(response(json!([])));
    push(&mut rig, handed_off(Some(true)));
    rig.wait_for_run(1);
    assert_eq!(rig.app.active_conversation_id().as_deref(), Some("elsewhere"));
}

#[test]
fn runs_starts_a_native_compactions_continuation_on_its_card() {
    let mut rig = Rig::new();
    rig.adopted();
    let mut child = conversation(CHILD);
    child.contexts = vec![
        context(json!({ "id": "ctx_compaction-system_1", "kind": "system", "content": "You are the project's reviewer.", "createdAt": "2026-09-29T00:00:00Z" })),
        context(json!({
            "id": "ctx_native-compaction_1", "kind": "system", "content": "", "localOnly": true,
            "nativeCompaction": { "providerId": "provider-1", "model": "model-1", "parts": [], "retained": [{ "role": "user", "sourceId": "u1", "content": "第一问" }], "tokensBefore": 180000, "tokensAfter": 4200 },
            "createdAt": "2026-09-29T00:00:01Z"
        })),
    ];
    rig.host.store_conversation(child);
    rig.host.auto_reply(response(json!([])));
    push(&mut rig, handed_off(Some(true)));
    let run = rig.wait_for_run(1);
    assert_eq!(run.args["forkPromptContextId"], json!("ctx_native-compaction_1"));
}

#[test]
fn runs_compacts_at_once_and_follows_the_continuation_without_starting_it() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.set_contexts(vec![user("u1", "第一问")]);
    rig.app.compact_conversation_now(WS, CONV, |_, _| {});
    let run = rig.wait_for_run(1);
    assert_eq!(run.args["compactNow"], json!(true));
    assert_eq!(run.args["request"]["conversationId"], json!(CONV));
    run.finish(stopped_response("compacted"));
    rig.run_exits();

    rig.host.store_conversation(continuation());
    push(&mut rig, handed_off(Some(false)));
    rig.until("the page to follow", |rig| rig.app.active_conversation_id().as_deref() == Some(CHILD));
    // Nothing is armed there: the continuation waits for the user.
    rig.idle(30);
    assert_eq!(rig.runs(), 1);
}

#[test]
fn runs_compact_now_is_refused_while_a_run_is_in_the_way() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.send("跑着");
    let run = rig.wait_for_run(1);
    let done = Deferred::<bool>::new();
    let heard = done.clone();
    rig.app.compact_conversation_now(WS, CONV, move |_, started| heard.resolve(started));
    assert!(!done.wait());
    assert_eq!(rig.runs(), 1);
    run.finish(response(json!([])));
}

fn stream_card(prompt_id: &str, kind: &str) -> serde_json::Value {
    json!({ "type": "tool_approval_requested", "promptId": prompt_id, "toolName": "exit_plan_mode", "kind": kind, "label": "Plan", "summary": "", "riskLevel": "low", "reason": "", "allowAlwaysOffered": false, "mandatory": true })
}

#[test]
fn runs_a_plan_exit_card_is_answered_like_any_other_and_carries_the_users_feedback() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.send("先做个计划");
    let run = rig.wait_for_run(1);
    run.emit(stream_card("plan-1", "plan_exit"));
    rig.until("the card", |rig| !rig.app.tool_prompts(CONV).is_empty());
    assert_eq!(rig.app.tool_prompts(CONV)[0].kind, crate::runs::types::PromptKind::PlanExit);
    // It goes straight to the plan pane, once.
    assert_eq!(rig.app.next_tool_prompt_navigation(CONV, |_| None::<String>), Some(crate::runs::types::ToolPromptNavigation::PlanPane));
    assert_eq!(rig.app.next_tool_prompt_navigation(CONV, |_| None::<String>), None);
    rig.app.decide_tool_prompt(CONV, "plan-1", ToolPromptDecision::Deny, Some("再加一个回滚步骤"));
    rig.until("the answer", |rig| rig.host.count("resolve_tool_prompt") == 1);
    assert_eq!(rig.host.calls_of("resolve_tool_prompt")[0]["feedback"], json!("再加一个回滚步骤"));
    run.finish(response(json!([])));
}

#[test]
fn runs_keeps_a_pushed_plan_and_drops_it_when_the_plan_is_cleared() {
    let mut rig = Rig::new();
    rig.adopted();
    push(&mut rig, json!({ "type": "conversationPlanUpdated", "conversationId": CONV, "plan": { "conversationId": CONV, "markdown": "# 计划", "status": "draft", "createdAt": "2026-09-29T00:00:00Z", "updatedAt": "2026-09-29T00:00:00Z" } }));
    assert_eq!(rig.app.conversation_plan(CONV).unwrap().markdown, "# 计划");
    push(&mut rig, json!({ "type": "conversationPlanUpdated", "conversationId": CONV, "plan": null }));
    assert_eq!(rig.app.conversation_plan(CONV), None);
}

#[test]
fn runs_a_plan_push_beats_a_load_that_was_already_in_flight() {
    let mut rig = Rig::new();
    rig.adopted();
    let plan = |markdown: &str| json!({ "conversationId": CONV, "markdown": markdown, "status": "draft", "createdAt": "2026-09-29T00:00:00Z", "updatedAt": "2026-09-29T00:00:00Z" });
    // Becoming the active conversation already asked once; let that answer land first.
    rig.idle(30);
    let asked = rig.host.count("load_conversation_plan");
    rig.host.reply("load_conversation_plan", Ok(plan("陈旧")));
    rig.app.load_conversation_plan(CONV);
    push(&mut rig, json!({ "type": "conversationPlanUpdated", "conversationId": CONV, "plan": plan("推送的") }));
    rig.until("the load", |rig| rig.host.count("load_conversation_plan") > asked);
    rig.idle(30);
    assert_eq!(rig.app.conversation_plan(CONV).unwrap().markdown, "推送的");
}

#[test]
fn runs_turns_the_switch_back_off_when_the_host_says_an_approved_plan_ended_plan_mode() {
    let mut rig = Rig::configured(&[], |app, _| app.doc.workspaces[0].conversations[0].settings.plan_mode_enabled = true);
    rig.adopted();
    push(&mut rig, json!({ "type": "conversationPlanModeChanged", "conversationId": CONV, "enabled": false }));
    assert!(!rig.conversation().settings.plan_mode_enabled);
    // Written back all the same: a settings write still in the queue carries the old value.
    rig.until("the write-back", |rig| rig.host.calls_of("update_conversation").iter().any(|args| args["conversation"]["settings"]["planModeEnabled"] == json!(false)));
}

#[test]
fn runs_mirrors_a_title_the_host_wrote_without_writing_it_back() {
    let mut rig = Rig::new();
    rig.adopted();
    push(&mut rig, json!({ "type": "conversationTitleChanged", "conversationId": CONV, "title": "助手起的名字", "settled": false }));
    assert_eq!(rig.conversation().title, "助手起的名字");
    rig.idle(300);
    assert_eq!(rig.host.count("update_conversation"), 0);
}

#[test]
fn runs_keeps_what_the_helper_model_explains_by_card_and_by_call() {
    let mut rig = Rig::new();
    rig.adopted();
    push(&mut rig, json!({ "type": "toolExplained", "conversationId": CONV, "contextId": "tool_1", "callId": "call_9", "text": "列出文件", "error": false }));
    push(&mut rig, json!({ "type": "toolExplained", "conversationId": CONV, "contextId": "tool_1", "callId": "call_9", "text": "没有安装 pnpm", "error": true }));
    assert_eq!(rig.app.tool_explanation("tool_1", None, None).as_deref(), Some("列出文件"));
    assert_eq!(rig.app.tool_explanation("other", Some(CONV), Some("call_9")).as_deref(), Some("列出文件"));
    assert_eq!(rig.app.tool_explanation("other", None, Some("call_9")), None);
    assert_eq!(rig.app.tool_error_explanation("tool_1").as_deref(), Some("没有安装 pnpm"));
    assert!(rig.app.tool_explanation_version() >= 2);
}

#[test]
fn runs_the_context_gauge_reads_the_live_run_then_the_providers_report_then_the_estimate() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.set_contexts(vec![user("u", &"a".repeat(400))]);
    let estimate = rig.app.context_usage_view(CONV);
    assert_eq!((estimate.tokens, estimate.estimated), (100, true));
    rig.send("继续");
    let run = rig.wait_for_run(1);
    run.emit(json!({ "type": "usage_updated", "round": 1, "usage": { "inputTokens": 5000, "outputTokens": 300 } }));
    rig.until("the snapshot", |rig| rig.state().controller.run(CONV).is_some_and(|run| !run.usage_by_round.is_empty()));
    let live = rig.app.context_usage_view(CONV);
    assert_eq!((live.tokens, live.estimated), (5300, false));
    let mut done = response(json!([]));
    done["contextTokens"] = json!(6000);
    run.finish(done);
    rig.run_exits();
    let reported = rig.app.context_usage_view(CONV);
    assert_eq!((reported.tokens, reported.estimated), (6000, false));
    // Another model's count does not project onto this one.
    rig.app.doc.assets.api_providers[0].id = "provider-2".to_owned();
    rig.app.doc.global_settings.active_provider_id = Some("provider-2".to_owned());
    assert!(rig.app.context_usage_view(CONV).estimated);
}

#[test]
fn runs_takes_a_subagents_card_to_its_page_once_that_page_exists() {
    let mut rig = Rig::new();
    rig.adopted();
    rig.send("子代理要批准");
    let run = rig.wait_for_run(1);
    let mut card = stream_card("agent-1", "tool");
    card["sourceAgent"] = json!("a1");
    run.emit(stream_card("plain", "tool"));
    run.emit(card);
    rig.until("the cards", |rig| rig.app.tool_prompts(CONV).len() == 2);
    // The agent's page does not exist yet: the card waits, and the main session's card needs no taking.
    assert_eq!(rig.app.next_tool_prompt_navigation(CONV, |_| None::<String>), None);
    let found = rig.app.next_tool_prompt_navigation(CONV, |prompt| prompt.source_agent.clone());
    assert_eq!(found, Some(crate::runs::types::ToolPromptNavigation::Subagent("a1".to_owned())));
    assert_eq!(rig.app.active_tool_prompt(CONV).unwrap().1, 1, "the stack shows the card that was followed");
    run.finish(response(json!([])));
}
