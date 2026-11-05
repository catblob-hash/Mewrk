//! Port of `lib/modelRunController.test.ts`.

use std::sync::Arc;

use serde_json::json;

use super::fixtures::*;
use crate::runs::model_run_controller::ModelRunController;
use crate::runs::model_stream::{ModelRunState, StreamingToolPhase, StreamingToolState};

fn run_with_id(request_id: &str) -> ModelRunState {
    let mut run = run_fixture();
    run.request_id = request_id.into();
    run
}

fn preview_tool(status: StreamingToolPhase) -> StreamingToolState {
    StreamingToolState {
        id: "tool-b".into(),
        call_id: "call-1".into(),
        tool_name: "preview_click".into(),
        requested_input: None,
        input: json!({ "selector": "button.primary" }).as_object().unwrap().clone(),
        result: serde_json::from_value(tool_result(true, "")).unwrap(),
        stream_status: status,
        live: Default::default(),
        created_at: AT.into(),
    }
}

#[test]
fn publishes_updates_synchronously_and_counts_one_commit() {
    let mut controller = ModelRunController::default();
    controller.insert("conv", run_fixture());
    assert_eq!(controller.run("conv"), Some(&run_fixture()));
    assert_eq!(controller.revision(), 1);
    assert_eq!(controller.revision_of("conv"), 1);
}

#[test]
fn counts_no_commit_when_the_updater_changes_nothing() {
    let mut controller = ModelRunController::default();
    controller.insert("conv", run_fixture());
    let before = controller.revision();
    controller.update_run("conv", |_| false);
    controller.update_run("missing", |_| true);
    assert_eq!(controller.revision(), before);
}

#[test]
fn keeps_summary_identity_across_streamed_delta_only_commits() {
    let mut controller = ModelRunController::default();
    controller.insert("conv", run_fixture());
    assert_eq!(controller.summary_revision(), 1);
    let before = controller.summaries()["conv"].clone();
    controller.update_run("conv", |run| {
        run.streamed_text_by_round.insert(0, "正在输出的文本".into());
        true
    });
    assert!(Arc::ptr_eq(&controller.summaries()["conv"], &before));
    assert_eq!(controller.summary_revision(), 1);
    assert_eq!(controller.revision(), 2);
}

#[test]
fn counts_summaries_when_a_run_appears_and_when_it_is_removed() {
    let mut controller = ModelRunController::default();
    controller.insert("conv", run_fixture());
    assert!(controller.remove("conv"));
    assert_eq!(controller.summary_revision(), 2);
    assert!(controller.summaries().get("conv").is_none());
    assert!(!controller.remove("conv"));
}

#[test]
fn updates_the_summary_when_a_steered_message_is_delivered() {
    let mut controller = ModelRunController::default();
    controller.insert("conv", run_fixture());
    let before = controller.summaries()["conv"].clone();
    controller.update_run("conv", |run| {
        run.steered_inputs_by_round.insert(1, vec![user("queued-1", "queued-1")]);
        true
    });
    assert!(!Arc::ptr_eq(&controller.summaries()["conv"], &before));
    assert_eq!(controller.summaries()["conv"].steered_message_ids, ["queued-1"]);
}

#[test]
fn surfaces_browser_automation_transitions_in_the_summary() {
    let mut controller = ModelRunController::default();
    controller.insert("conv", run_fixture());
    assert_eq!(controller.summaries()["conv"].browser_automation_tool, None);
    controller.update_run("conv", |run| {
        run.streamed_tools_by_round.insert(0, vec![preview_tool(StreamingToolPhase::Running)]);
        true
    });
    assert_eq!(controller.summaries()["conv"].browser_automation_tool.as_deref(), Some("preview_click"));
}

#[test]
fn an_announced_preview_call_is_not_driving_the_page_yet() {
    let mut controller = ModelRunController::default();
    let mut run = run_fixture();
    run.streamed_tools_by_round.insert(0, vec![preview_tool(StreamingToolPhase::Announced)]);
    controller.insert("conv", run);
    assert_eq!(controller.summaries()["conv"].browser_automation_tool, None);
}

#[test]
fn preserves_untouched_conversations_summary_objects_when_another_run_changes() {
    let mut controller = ModelRunController::default();
    controller.insert("quiet", run_with_id("run_quiet"));
    controller.insert("busy", run_with_id("run_busy"));
    let quiet_before = controller.summaries()["quiet"].clone();
    controller.update_run("busy", |run| {
        run.steered_inputs_by_round.insert(0, vec![user("queued-2", "queued-2")]);
        true
    });
    assert!(Arc::ptr_eq(&controller.summaries()["quiet"], &quiet_before));
    assert_eq!(controller.summaries()["busy"].steered_message_ids, ["queued-2"]);
}

#[test]
fn tracks_run_tokens_and_pipeline_guards_without_counting_commits() {
    let mut controller = ModelRunController::default();
    controller.set_run_token("conv", "run_1");
    assert_eq!(controller.run_token("conv"), Some("run_1"));
    assert!(controller.has_run_token("conv"));
    controller.delete_run_token("conv");
    assert!(!controller.has_run_token("conv"));
    controller.add_preparing_run("conv");
    assert!(controller.has_preparing_run("conv"));
    controller.delete_preparing_run("conv");
    assert!(!controller.has_preparing_run("conv"));
    controller.add_performing_run("conv");
    assert!(controller.has_performing_run("conv"));
    controller.delete_performing_run("conv");
    assert!(!controller.has_performing_run("conv"));
    assert_eq!(controller.revision(), 0);
    assert_eq!(controller.summary_revision(), 0);
}

#[test]
fn waits_for_the_exact_run_generation_to_leave_both_live_stores() {
    let mut controller = ModelRunController::default();
    controller.set_run_token("conv", "run_waiting");
    controller.insert("conv", run_with_id("run_waiting"));
    let exit = controller.wait_for_run_exit("conv", "run_waiting");
    controller.insert("unrelated", run_with_id("run_unrelated"));
    assert!(exit.try_recv().is_err());
    controller.delete_run_token("conv");
    assert!(exit.try_recv().is_err());
    controller.remove("conv");
    assert!(exit.try_recv().is_ok());
    // A generation already gone answers at once.
    assert!(controller.wait_for_run_exit("conv", "run_waiting").try_recv().is_ok());
}
