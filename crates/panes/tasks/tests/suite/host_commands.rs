//! Every command this crate calls, against the real host, so that argument names and answer shapes are the host's and not
//! a fixture's belief about them.

use std::cell::RefCell;
use std::rc::Rc;

use crate::common::*;
use fluent::el::*;
use mewrk_client::model::Conversation;
use mewrk_client::App;
use mewrk_pane_tasks::fork_tray::{self, ForkStore};
use mewrk_pane_tasks::history_pane::*;
use mewrk_pane_tasks::shell_tasks::{self, ShellTaskOutcome, ShellTaskSnapshot, ShellTaskStore};
use mewrk_pane_tasks::task_container::*;
use mewrk_pane_tasks::tasks_pane::*;
use mewrk_pane_tasks::usage_card::*;
use mewrk_pane_tasks::usage_statistics::USAGE_BACKFILL_STORAGE_KEY;
use mewrk_pane_tasks::workflow_panel::*;
use serde_json::{json, Value};

const CONVERSATION: &str = "conv-host-commands";

fn conversation(rig: &mut Rig) {
    let workspace_id = rig.app.doc.workspaces.first().unwrap().id.clone();
    let conversation: Conversation = serde_json::from_value(json!({ "id": CONVERSATION, "title": "New task", "createdAt": "2026-07-24T00:00:00Z", "updatedAt": "2026-07-24T00:00:00Z", "settings": { "enabledTools": ["bash"], "securityLevel": "full_access" }, "contexts": [] })).unwrap();
    rig.app.host.invoke_blocking("create_conversation", json!({ "workspaceId": workspace_id, "conversation": conversation })).expect("create_conversation");
}

#[test]
fn the_history_pane_reads_the_real_store() {
    let mut rig = Rig::new("host-history");
    conversation(&mut rig);
    let mut view = |app: &mut App| -> E { col().child(history_pane(app, HistoryPaneProps::new(CONVERSATION, 1, false))) };
    rig.until(&mut view, "the list", &mut |app| {
        let state = pane_state(app, CONVERSATION, None);
        state.entries.is_some() || state.failure.is_some()
    });
    let state = pane_state(&mut rig.app, CONVERSATION, None);
    assert_eq!(state.failure, None);
    // A child's address asks for that child's rows, and a load of a row nobody wrote comes back empty.
    let mut owned = |app: &mut App| -> E {
        let mut props = HistoryPaneProps::new(CONVERSATION, 1, false);
        props.owners = Some(vec!["reviewer".into()]);
        col().child(history_pane(app, props))
    };
    rig.until(&mut owned, "the owner's list", &mut |app| {
        let state = pane_state(app, CONVERSATION, Some(vec!["reviewer".into()]));
        state.entries.is_some() || state.failure.is_some()
    });
    let state = pane_state(&mut rig.app, CONVERSATION, Some(vec!["reviewer".into()]));
    assert_eq!((state.failure.clone(), state.entries.clone().map(|e| e.len())), (None, Some(0)));
    assert!(on_screen("history-empty"));
    let answer: Value = rig.app.host.invoke_blocking("load_history_entry", json!({ "conversationId": CONVERSATION, "seq": 987654 })).expect("load_history_entry");
    assert!(answer.is_null());
}

#[test]
fn the_usage_page_reads_the_real_ledger_and_marks_its_backfill() {
    let mut rig = Rig::new("host-usage");
    let mut view = |app: &mut App| -> E { col().child(usage_settings(app)) };
    rig.frame(&mut view);
    rig.until(&mut view, "the statistics", &mut |app| !app.local::<UsagePage>().loading);
    let page = rig.app.local::<UsagePage>();
    assert!(page.statistics.generated_at_ms > 0);
    let started = page.statistics.ledger_started_at_ms;
    assert!(started > 0);
    for _ in 0..5 {
        rig.frame(&mut view);
    }
    assert_eq!(rig.app.ui_store().get(USAGE_BACKFILL_STORAGE_KEY), Some(started.to_string().as_str()));
    assert!(on_screen("usage-note"));
    // The host takes a pushed event by the shape this crate writes, and counts it.
    let event = json!({ "id": "turn:conv:t", "occurredAtMs": started - 60_000, "origin": "conversation", "conversationId": "conv", "workspaceId": "ws", "providerId": "p", "providerName": "P", "modelId": "m", "requests": 1, "inputTokens": 5, "cachedInputTokens": 0, "outputTokens": 5, "totalTokens": 10 });
    let written: Value = rig.app.host.invoke_blocking("backfill_token_usage", json!({ "events": [event] })).expect("backfill_token_usage");
    assert_eq!(written, json!(1));
}

#[test]
fn the_fork_commands_answer_in_the_shapes_the_store_reads() {
    let mut rig = Rig::new("host-forks");
    conversation(&mut rig);
    fork_tray::install(&mut rig.app);
    fork_tray::list_pending_fork_requests(&rig.app);
    fork_tray::list_fork_decisions(&rig.app, CONVERSATION);
    let mut view = |_: &mut App| -> E { div() };
    rig.until(&mut view, "the decisions", &mut |app| app.local::<ForkStore>().decisions.contains_key(CONVERSATION));
    assert!(rig.app.local::<ForkStore>().decisions[CONVERSATION].is_empty());
    assert!(rig.app.local::<ForkStore>().requests.is_empty());
    // A card nobody raised is refused, and comes down all the same.
    fork_tray::decide_fork_request(&mut rig.app, "no-such-fork", true);
    let refused = rig.app.host.invoke_blocking("resolve_fork_request", json!({ "forkId": "no-such-fork", "approved": false }));
    assert!(refused.is_err() || refused.unwrap().is_null());
}

#[test]
fn the_workflow_commands_take_the_arguments_this_crate_sends() {
    let mut rig = Rig::new("host-workflow");
    conversation(&mut rig);
    let answers: Rc<RefCell<Vec<String>>> = Rc::default();
    let _ = &answers;
    // No run by that id: the record reads as nothing or a refusal, never as an argument error.
    let record = rig.app.host.invoke_blocking("workflow_step_record", json!({ "conversationId": CONVERSATION, "runId": "no-such-run", "stepIndex": 0 }));
    if let Err(e) = &record {
        assert!(!e.contains("missing") && !e.contains("invalid args"), "{e}");
    }
    let control = rig.app.host.invoke_blocking("workflow_step_control", json!({ "conversationId": CONVERSATION, "runId": "no-such-run", "stepIndex": 0, "action": action_name(WorkflowStepAction::Skip) }));
    if let Err(e) = &control {
        assert!(!e.contains("missing") && !e.contains("invalid args"), "{e}");
    }
    control_workflow_step(&rig.app, CONVERSATION, "no-such-run", 0, WorkflowStepAction::Retry);
    let mut view = |_: &mut App| -> E { div() };
    rig.settle(&mut view);
}

#[test]
fn a_task_rows_stop_goes_through_the_host_and_clears_its_pending_mark() {
    let mut rig = Rig::new("host-stop");
    conversation(&mut rig);
    shell_tasks::install(&mut rig.app);
    let request = json!({ "conversationId": CONVERSATION, "workspacePath": "", "toolName": "bash", "input": { "command": "sleep 60" } });
    rig.app.invoke::<Value>("execute_tool", json!({ "request": request, "approvalNonce": null }), |_, _| {});
    let mut view = |_: &mut App| -> E { div() };
    rig.until(&mut view, "the row", &mut |app| !app.local::<ShellTaskStore>().for_conversation(CONVERSATION).is_empty());
    let sources = TaskSources { conversation_id: Some(CONVERSATION.into()), shell_tasks: rig.app.local::<ShellTaskStore>().for_conversation(CONVERSATION), ..Default::default() };
    let item = derive_task_items(&sources, &task_container_messages()).remove(0);
    assert_eq!(item.state, TaskItemState::Running);
    stop_task_item(&mut rig.app, &item, &StopHooks::default());
    assert_eq!(rig.app.local::<StoppingTasks>().for_conversation(CONVERSATION), [item.id.clone()]);
    // A second press while the first is pending asks for nothing more.
    stop_task_item(&mut rig.app, &item, &StopHooks::default());
    rig.until(&mut view, "the pending mark", &mut |app| app.local::<StoppingTasks>().for_conversation(CONVERSATION).is_empty());
    rig.until(&mut view, "the end", &mut |app| app.local::<ShellTaskStore>().for_conversation(CONVERSATION).iter().any(|t: &ShellTaskSnapshot| t.outcome == Some(ShellTaskOutcome::Stopped)));
    // A row with nothing left to stop answers false rather than failing.
    let again: Value = rig.app.host.invoke_blocking("stop_shell_task", json!({ "conversationId": CONVERSATION, "shellTaskId": item.id })).expect("stop_shell_task");
    assert_eq!(again, json!(false));
    let unknown: Value = rig.app.host.invoke_blocking("stop_conversation_task", json!({ "conversationId": CONVERSATION, "task": "no-such-agent" })).expect("stop_conversation_task");
    assert_eq!(unknown, json!(false));
    // `list_shell_tasks` reads back the row the events built.
    shell_tasks::reconcile(&rig.app, CONVERSATION);
    for _ in 0..30 {
        rig.frame(&mut view);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(rig.app.local::<ShellTaskStore>().for_conversation(CONVERSATION).len(), 1);
}

#[test]
fn a_backfill_pushes_only_what_predates_the_ledger_and_only_once_per_ledger() {
    use mewrk_client::runs::conversation_turns::CONVERSATION_TURNS_STORAGE_KEY;
    use mewrk_pane_tasks::usage_statistics::UsageStatistics;
    let mut rig = Rig::new("host-backfill");
    let first: Value = rig.app.host.invoke_blocking("token_usage_statistics", json!({})).expect("token_usage_statistics");
    let started = serde_json::from_value::<UsageStatistics>(first).unwrap().ledger_started_at_ms;
    let turn = |id: &str, at: i64, total: u64| json!({ "id": id, "requestId": "r", "anchorContextId": "c", "modelId": "m", "startedAt": iso(at), "endedAt": iso(at), "status": "completed", "contextIds": ["ctx-reply"], "usage": { "inputTokens": total, "outputTokens": 0, "totalTokens": total }, "usageOffset": {}, "usageBaseline": {}, "usageRevisionAtStart": 0, "segmentCount": 1 });
    let turns = json!({ "conv": [turn("before", started - 60_000, 10), turn("after", started + 60_000, 900)] });
    rig.app.ui_store().set(CONVERSATION_TURNS_STORAGE_KEY, turns.to_string());
    let mut view = |app: &mut App| -> E { col().child(usage_settings(app)) };
    rig.frame(&mut view);
    rig.until(&mut view, "the statistics", &mut |app| !app.local::<UsagePage>().loading);
    let tokens = |rig: &mut Rig| rig.app.local::<UsagePage>().statistics.usage.iter().map(|b| b.total_tokens).sum::<i64>();
    // The gateway recorded what came after its start itself, so only the older turn is pushed.
    assert_eq!(tokens(&mut rig), 10);
    assert_eq!(rig.app.ui_store().get(USAGE_BACKFILL_STORAGE_KEY), Some(started.to_string().as_str()));
    // Coming back to the page reads again and has nothing to push: the marker says this ledger had its backfill.
    let mut away = |_: &mut App| -> E { div() };
    for _ in 0..4 {
        rig.frame(&mut away);
    }
    rig.app.ui_store().set(CONVERSATION_TURNS_STORAGE_KEY, json!({ "conv": [turn("late", started - 30_000, 77)] }).to_string());
    rig.frame(&mut view);
    rig.until(&mut view, "the second read", &mut |app| !app.local::<UsagePage>().loading);
    assert_eq!(tokens(&mut rig), 10);
}
