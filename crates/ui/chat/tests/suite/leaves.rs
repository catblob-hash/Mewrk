//! The leaves of a timeline: upstream's `TurnChanges.test.tsx`, `App.turnChanges.test.tsx`, `StreamWaitingIndicator.test.tsx`,
//! `InlineToolEditor.test.tsx`, `InlineTextEditor.test.tsx`, `InlineQuestionEditor.test.tsx` and `PlanPage.test.tsx`, as far as
//! they test logic or interaction, against the real host in the rig.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use fluent::el::*;
use fluent::ui::Key;
use mewrk_client::model::{ContextItem, ConversationPlan, FileAttachment, FileAttachmentFormat, ImageAttachment, JsonObject, PlanStatus, ToolDescriptor, ToolResult};
use mewrk_client::prelude::E;
use mewrk_client::runs::{AttachmentRejection, AttachmentRejectionReason, LiveReasoningView, ModelRunRetryState};
use mewrk_client::App;
use mewrk_pane_files::links::PathOpenRequest;
use mewrk_pane_files::path_open::{clear_path_open_handler, set_path_open_handler};
use mewrk_pane_tasks::task_container::TaskItemState;
use mewrk_pane_tasks::workflow_runs::{WorkflowRunPhase, WorkflowRunStep, WorkflowRunStepState, WorkflowRunView};
use mewrk_ui_chat::cat_art::{cat_size, flatten, mood_loops, mood_pose, rasterize_cat, sample_loop, CatPart, CatPose, CatTransform, CAT_CUTS, CAT_LAPTOP, CAT_LID_LIFT, CAT_LIDS, CAT_NEAR_PAW_OVERLAP, CAT_PUPILS, CAT_TAIL, CAT_WRIST, LID_OVERSHOOT};
use mewrk_ui_chat::image_strip::{attachment_preview_layer, file_remove_key, file_tile_key, file_type_label, image_strip_with, image_title, thumbnail_key, thumbnail_remove_key, thumbnail_state, viewer_close_key, ImageStripProps, ThumbnailState};
use mewrk_ui_chat::inline_question_editor::{inline_question_editor, question_field_key, QuestionEditorProps, SaveQuestion};
use mewrk_ui_chat::inline_text_editor::{inline_text_editor, text_field_key, AddAttachments, AddedAttachments, InsertKind, SaveText, TextEditorProps};
use mewrk_ui_chat::inline_tool_editor::{arg_field_key, can_rerun_tool, draft_from_input, draft_messages, draft_tool_context, inline_tool_editor, parse_draft, recorded_fields, result_field_key, Decode, RunTool, SaveToolEdit, ToolDone, ToolEditorProps};
use mewrk_ui_chat::item::Item;
use mewrk_ui_chat::orchestration::{answers_from_formatted_content, questions_from_input};
use mewrk_ui_chat::plan_page::{plan_pane, plan_status_label, plan_title};
use mewrk_ui_chat::question_card::{question_status, question_timeline_card, QuestionCardProps};
use mewrk_ui_chat::reveal::{fresh_alpha, stream_reveal_text};
use mewrk_ui_chat::rolling_number::{rolling_number, rolling_slots, RollSlot};
use mewrk_ui_chat::turn_changes::{message_change_spans, row_open_request, split_path, subagent_change_spans, summarize_span_changes, turn_changes, turn_changes_title, ChangeSpan};
use mewrk_ui_chat::waiting::{announced_label, cat_dwell_choices_ms, cat_mood_cycle_ms, retry_label, stream_waiting_indicator, tasks_label, tasks_line_key, thinking_figures, tool_activities, CatMachine, CatMood, ToolActivity, WaitingProps, CAT_MOODS};
use mewrk_ui_chat::workflow_detail::{workflow_run_detail, workflow_run_summary};
use mewrk_ui_common::popover_menu::probe;
use mewrk_ui_composer::attach::{attach_ports, ClipboardContents};
use serde_json::{json, Value};

use crate::rig::Rig;

const AT: &str = "2026-09-25T00:00:00.000Z";

fn object(value: Value) -> JsonObject {
    match value {
        Value::Object(object) => object,
        _ => panic!("an object"),
    }
}

fn result(output: &str, diff: Option<String>) -> ToolResult {
    ToolResult { success: true, output: output.to_owned(), images: Vec::new(), diff, executed_at: AT.to_owned(), duration_ms: 1 }
}

fn tool(id: &str, name: &str, input: Value, result: ToolResult) -> ContextItem {
    ContextItem::Tool {
        id: id.to_owned(),
        tool_name: name.to_owned(),
        round: None,
        model_turn_id: None,
        provider_call_id: None,
        requested_input: None,
        input: object(input),
        result,
        subagent: None,
        notice: None,
        attestation: String::new(),
        created_at: AT.to_owned(),
    }
}

fn diff(path: &str, additions: usize, deletions: usize) -> String {
    let mut lines = vec![format!("--- {path}"), format!("+++ {path}"), format!("@@ -1,{deletions} +1,{additions} @@")];
    lines.extend((0..deletions).map(|index| format!("-old {index}")));
    lines.extend((0..additions).map(|index| format!("+new {index}")));
    format!("{}\n", lines.join("\n"))
}

fn change(id: &str, path: &str, additions: usize, deletions: usize) -> ContextItem {
    tool(id, "edit", json!({ "path": path, "find": "a", "replace": "b" }), result("ok", Some(diff(path, additions, deletions))))
}

fn user(id: &str) -> ContextItem {
    ContextItem::User { id: id.to_owned(), content: "go".to_owned(), images: Vec::new(), files: Vec::new(), created_at: AT.to_owned() }
}

fn reply(id: &str) -> ContextItem {
    ContextItem::Assistant { id: id.to_owned(), content: "done".to_owned(), round: None, model_turn_id: None, interrupted: false, sources: Vec::new(), created_at: AT.to_owned() }
}

fn summary_of(contexts: &[ContextItem], key: &str) -> Option<mewrk_ui_chat::turn_changes::TurnChangeSummary> {
    let span = ChangeSpan { key: key.to_owned(), context_ids: contexts.iter().map(|item| item.id().to_owned()).collect() };
    summarize_span_changes(&span, |id| contexts.iter().find(|item| item.id() == id).map(Item::settled))
}

fn field_rect(key: impl std::hash::Hash) -> fluent::ui::Rect {
    rect_of(hash_key(("field", key))).expect("the field was on screen last frame")
}

/// Puts the caret in a field, and replaces what it holds.
fn replace_in(rig: &mut Rig, view: &mut dyn FnMut(&mut App) -> E, key: impl std::hash::Hash, typed: &str) {
    let r = field_rect(key);
    rig.click(view, r.x + r.w * 0.5, r.y + r.h * 0.5);
    rig.key(view, Key::Char('a'), true, false);
    rig.type_text(view, typed);
    rig.settle(view);
}

fn image(id: &str, name: &str) -> ImageAttachment {
    ImageAttachment { id: id.to_owned(), name: name.to_owned(), mime: "image/png".to_owned(), width: 100, height: 80, bytes: 500, short_id: None }
}

fn file(id: &str, name: &str, format: FileAttachmentFormat) -> FileAttachment {
    FileAttachment { id: id.to_owned(), name: name.to_owned(), format, bytes: 1200, tokens: 300, pages: if format == FileAttachmentFormat::Pdf { Some(2) } else { None } }
}

// ---------------------------------------------------------------- turn changes

#[test]
fn sums_every_successful_change_per_file_in_the_order_the_span_first_touched_them() {
    let contexts = vec![
        change("a1", "src/a.ts", 3, 1),
        change("b1", "./src/b.ts", 2, 0),
        change("a2", "src/a.ts", 1, 1),
        change("b2", "src/b.ts", 4, 2),
        tool("failed", "edit", json!({ "path": "src/c.ts" }), ToolResult { success: false, ..result("no match", Some(diff("src/c.ts", 9, 9))) }),
        tool("read", "read", json!({ "path": "src/d.ts" }), result("text", None)),
        tool("other-workspace", "edit", json!({ "path": "src/a.ts", "workspace": 2 }), result("ok", Some(diff("src/a.ts", 5, 0)))),
    ];
    let summary = summary_of(&contexts, "span").expect("a summary");
    let rows: Vec<_> = summary.files.iter().map(|file| (file.path.as_str(), file.workspace, file.additions, file.deletions)).collect();
    assert_eq!(rows, [("src/a.ts", None, 4, 2), ("./src/b.ts", None, 6, 2), ("src/a.ts", Some(2), 5, 0)]);
    assert_eq!((summary.additions, summary.deletions), (15, 4));
}

#[test]
fn has_nothing_to_say_for_a_span_that_changed_no_file() {
    assert_eq!(summary_of(&[user("u"), reply("r")], "span"), None);
}

#[test]
fn the_signature_changes_exactly_when_something_the_card_shows_does() {
    let one = summary_of(&[change("a", "a.ts", 1, 0)], "s").unwrap();
    let same = summary_of(&[change("other-id", "a.ts", 1, 0)], "s").unwrap();
    let more = summary_of(&[change("a", "a.ts", 2, 0)], "s").unwrap();
    assert_eq!(one.signature, same.signature);
    assert_ne!(one.signature, more.signature);
}

#[test]
fn a_turn_that_edited_one_tracked_file_and_wrote_a_new_one_totals_both() {
    // `App.turnChanges.test.tsx`'s stage: the card's words and figures before anything opens.
    let _rig = Rig::new();
    let contexts = vec![user("ask"), change("edit-app", "src/App.tsx", 2, 1), change("write-notes", "notes/new.md", 2, 1), reply("done")];
    let items: Vec<Item<'_>> = contexts.iter().map(Item::settled).collect();
    let spans = message_change_spans(&items, |item| matches!(item.item, ContextItem::User { .. }), false);
    assert_eq!(spans.len(), 1);
    let summary = summarize_span_changes(&spans[0], |id| contexts.iter().find(|item| item.id() == id).map(Item::settled)).unwrap();
    assert_eq!(turn_changes_title(summary.files.len()), "Edited 2 files");
    assert_eq!(summary.additions, 4);
}

#[test]
fn runs_each_stretch_from_one_user_message_to_the_next_the_work_before_the_first_included() {
    let contexts = vec![change("e0", "a.ts", 1, 0), user("u1"), change("e1", "a.ts", 1, 0), reply("r1"), user("u2"), user("u3"), change("e3", "b.ts", 1, 0)];
    let items: Vec<Item<'_>> = contexts.iter().map(Item::settled).collect();
    let spans = message_change_spans(&items, |item| matches!(item.item, ContextItem::User { .. }), false);
    let found: Vec<(&str, Vec<&str>)> = spans.iter().map(|span| (span.key.as_str(), span.context_ids.iter().map(String::as_str).collect())).collect();
    assert_eq!(found, [("after:start", vec!["e0"]), ("after:u1", vec!["e1", "r1"]), ("after:u3", vec!["e3"])]);
}

#[test]
fn withholds_the_last_stretch_while_a_round_is_still_writing_it() {
    let contexts = vec![user("u1"), change("e1", "a.ts", 1, 0), reply("r1"), user("u2"), change("e2", "b.ts", 1, 0)];
    let items: Vec<Item<'_>> = contexts.iter().map(Item::settled).collect();
    let spans = message_change_spans(&items, |item| matches!(item.item, ContextItem::User { .. }), true);
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].context_ids, ["e1", "r1"]);
}

fn structured() -> ContextItem {
    tool("result", "structured_output", json!({ "value": 1 }), result("accepted", None))
}

fn ids_of(spans: &[ChangeSpan]) -> Vec<Vec<&str>> {
    spans.iter().map(|span| span.context_ids.iter().map(String::as_str).collect()).collect()
}

#[test]
fn opens_a_subagent_run_at_a_message_that_follows_a_reply_not_at_one_that_lands_between_calls() {
    let contexts = [user("task"), change("e1", "a.ts", 1, 0), user("mid-run"), reply("r1"), user("follow-up"), change("e2", "b.ts", 1, 0), reply("r2")];
    let refs: Vec<&ContextItem> = contexts.iter().collect();
    assert_eq!(ids_of(&subagent_change_spans(&refs, false)), [vec!["task", "e1", "mid-run", "r1"], vec!["follow-up", "e2", "r2"]]);
}

#[test]
fn keeps_a_structured_output_reminder_inside_the_run_it_interrupts() {
    let nudge = ContextItem::User { id: "ctx_structured-output-nudge_1".to_owned(), content: "return it".to_owned(), images: Vec::new(), files: Vec::new(), created_at: AT.to_owned() };
    let contexts = [user("task"), change("e1", "a.ts", 1, 0), reply("r1"), nudge, structured()];
    let refs: Vec<&ContextItem> = contexts.iter().collect();
    assert_eq!(subagent_change_spans(&refs, false).len(), 1);
}

#[test]
fn withholds_a_live_run_until_it_has_returned_its_structured_result() {
    let contexts = [user("task"), change("e1", "a.ts", 1, 0)];
    let refs: Vec<&ContextItem> = contexts.iter().collect();
    assert!(subagent_change_spans(&refs, true).is_empty());
    let finished = structured();
    let mut with_result = refs.clone();
    with_result.push(&finished);
    assert_eq!(subagent_change_spans(&with_result, true).len(), 1);
}

#[test]
fn a_path_splits_into_its_name_and_the_directory_before_it() {
    assert_eq!(split_path("website/content/en/working.md"), ("working.md".to_owned(), "website/content/en".to_owned()));
    assert_eq!(split_path("a.ts"), ("a.ts".to_owned(), String::new()));
    assert_eq!(split_path("dir\\sub\\"), ("sub".to_owned(), "dir".to_owned()));
}

fn many_changes(count: usize) -> Vec<ContextItem> {
    (0..count).map(|index| change(&format!("c{index}"), &format!("src/file-{index}.ts"), index + 1, index)).collect()
}

fn row_key_of<'a>(summary_key: &'a str, file_key: &'a str) -> (&'static str, &'a str, &'a str) {
    ("turn-changes-file", summary_key, file_key)
}

#[test]
fn folds_the_list_from_its_heading_and_keeps_a_list_opened_in_full_across_the_fold() {
    let mut rig = Rig::new();
    let summary = summary_of(&many_changes(6), "turn:t").unwrap();
    assert_eq!((summary.additions, summary.deletions), (21, 15));
    let shown = summary.clone();
    let mut view = move |_: &mut App| turn_changes(&shown, None);
    let view: &mut dyn FnMut(&mut App) -> E = &mut view;
    rig.settle(view);
    let rows_on_screen = |summary: &mewrk_ui_chat::turn_changes::TurnChangeSummary| summary.files.iter().filter(|file| rect_of(row_key_of("turn:t", &file.key)).is_some()).count();
    assert_eq!(rows_on_screen(&summary), 3);

    rig.click_key(view, ("turn-changes-more", "turn:t"));
    assert_eq!(rows_on_screen(&summary), 6);

    rig.click_key(view, ("turn-changes-heading", "turn:t"));
    assert_eq!(rows_on_screen(&summary), 0);

    rig.click_key(view, ("turn-changes-heading", "turn:t"));
    assert_eq!(rows_on_screen(&summary), 6, "a list opened in full comes back in full");
}

#[test]
fn shows_a_list_one_row_longer_than_the_fold_whole() {
    let mut rig = Rig::new();
    let summary = summary_of(&many_changes(4), "turn:t").unwrap();
    let shown = summary.clone();
    let mut view = move |_: &mut App| turn_changes(&shown, None);
    rig.settle(&mut view);
    assert_eq!(summary.files.iter().filter(|file| rect_of(row_key_of("turn:t", &file.key)).is_some()).count(), 4);
    assert!(rect_of(("turn-changes-more", "turn:t")).is_none());
}

#[test]
fn says_where_a_file_is_only_when_another_row_carries_the_same_name() {
    let mut rig = Rig::new();
    let contexts = [change("en", "website/content/en/working.md", 1, 0), change("zh", "website/content/zh-CN/working.md", 1, 0), change("app", "src/App.tsx", 1, 0)];
    let summary = summary_of(&contexts, "turn:t").unwrap();
    let shown = summary.clone();
    let mut view = move |_: &mut App| turn_changes(&shown, None);
    rig.settle(&mut view);
    let directories: Vec<bool> = summary.files.iter().map(|file| rect_of(("turn-changes-directory", "turn:t", file.key.as_str())).is_some()).collect();
    assert_eq!(directories, [true, true, false]);
}

#[test]
fn a_rows_box_spans_the_card() {
    let mut rig = Rig::new();
    let summary = summary_of(&many_changes(6), "turn:t").unwrap();
    let shown = summary.clone();
    let mut view = move |_: &mut App| turn_changes(&shown, None);
    rig.settle(&mut view);
    let card = rect_of(("turn-changes", "turn:t")).unwrap();
    let heading = rect_of(("turn-changes-heading", "turn:t")).unwrap();
    assert!((heading.right() - card.right()).abs() <= 1.0, "the row spans the card");
}

/// The cat redraws thirty times a second for as long as a round runs, in an unoptimised build too.
#[test]
fn a_frame_of_the_cat_is_cheap() {
    let pose = CatPose::at(CatMood::Work, 700.0, 13.0, 0.0);
    let started = std::time::Instant::now();
    for _ in 0..30 {
        std::hint::black_box(rasterize_cat(&pose, 57, 48));
    }
    let per_frame = started.elapsed() / 30;
    eprintln!("a cat frame at 3x takes {per_frame:?} in this build");
    assert!(per_frame < std::time::Duration::from_millis(25), "{per_frame:?}");
}

#[test]
fn asks_the_app_to_open_a_rows_file_on_its_diff_where_git_tracks_it() {
    let requests: Rc<RefCell<Vec<PathOpenRequest>>> = Rc::default();
    let heard = requests.clone();
    set_path_open_handler(Rc::new(move |_, request| {
        heard.borrow_mut().push(request.clone());
        true
    }));
    let mut rig = Rig::new();
    let contexts = [change("own", "src/a.ts", 1, 0), tool("other", "edit", json!({ "path": "lib/b.ts", "workspace": 2 }), result("ok", Some(diff("lib/b.ts", 1, 0))))];
    let summary = summary_of(&contexts, "turn:t").unwrap();
    let shown = summary.clone();
    let mut view = move |_: &mut App| turn_changes(&shown, Some("/work/repo"));
    let view: &mut dyn FnMut(&mut App) -> E = &mut view;
    rig.settle(view);
    rig.click_key(view, row_key_of("turn:t", &summary.files[0].key));
    rig.click_key(view, row_key_of("turn:t", &summary.files[1].key));
    clear_path_open_handler();

    let asked = requests.borrow();
    assert_eq!(asked.len(), 2);
    assert_eq!(asked[0], PathOpenRequest { path: "src/a.ts".into(), base_dir: Some("/work/repo".into()), review: true, ..PathOpenRequest::default() });
    // Written against another workspace, so this surface's directory says nothing about it.
    assert_eq!(asked[1], PathOpenRequest { path: "lib/b.ts".into(), base_dir: None, workspace: Some(2), review: true, ..PathOpenRequest::default() });
    assert_eq!(row_open_request(&summary.files[1], Some("/x")).base_dir, None);
}

// ---------------------------------------------------------------- the waiting line

fn props() -> WaitingProps {
    WaitingProps { tools: Rc::new(Vec::new()), thinking: None, retry_notice: None, running_task_count: 0, on_open_tasks: None }
}

fn iso_ms(text: &str) -> i64 {
    mewrk_client::runs::util::parse_iso(text).unwrap()
}

#[test]
fn narrates_live_reasoning_in_the_same_shape_as_a_tool_call() {
    let _rig = Rig::new();
    let thinking = LiveReasoningView { started_at: "2026-09-04T00:00:00.000Z".to_owned(), tokens: 1_240 };
    let (elapsed, tokens) = thinking_figures(&thinking, iso_ms("2026-09-04T00:00:12.400Z"));
    assert_eq!(elapsed.as_deref(), Some("12s"));
    assert_eq!(tokens, "1.2k tokens");
    assert_eq!(announced_label(true, &[], None), "Thinking");
}

#[test]
fn keeps_the_thinking_clock_running_between_stream_commits() {
    let _rig = Rig::new();
    let thinking = LiveReasoningView { started_at: "2026-09-04T00:00:00.000Z".to_owned(), tokens: 0 };
    assert_eq!(thinking_figures(&thinking, iso_ms("2026-09-04T00:00:59.000Z")).0.as_deref(), Some("59s"));
    assert_eq!(thinking_figures(&thinking, iso_ms("2026-09-04T00:01:01.000Z")).0.as_deref(), Some("1m01s"));
}

#[test]
fn shows_the_clock_and_a_token_count_before_the_provider_reports_anything() {
    let mut rig = Rig::new();
    probe::start();
    let thinking = LiveReasoningView { started_at: "2026-09-04T00:00:00.000Z".to_owned(), tokens: 0 };
    let mut view = move |_: &mut App| stream_waiting_indicator("w", &[], WaitingProps { thinking: Some(thinking.clone()), ..props() });
    rig.settle(&mut view);
    let said = probe::take();
    assert!(said.iter().any(|line| line == "Thinking"), "{said:?}");
    assert!(said.iter().any(|line| line == "0 tokens"), "{said:?}");
}

#[test]
fn says_nothing_beside_the_cat_when_no_reasoning_is_running() {
    let mut rig = Rig::new();
    probe::start();
    let mut view = move |_: &mut App| stream_waiting_indicator("w", &[], props());
    rig.settle(&mut view);
    let said = probe::take();
    assert!(said.iter().all(|line| line == "Model is generating"), "{said:?}");
    assert!(rect_of(("stream-cat-drawing", hash_key("w"))).is_some());
}

#[test]
fn narrates_the_tasks_still_running_as_a_line_that_opens_the_tasks_pane() {
    let mut rig = Rig::new();
    let opened = Rc::new(RefCell::new(0));
    let heard = opened.clone();
    let open: Handler<App> = Rc::new(move |_| *heard.borrow_mut() += 1);
    let thinking = LiveReasoningView { started_at: "2026-09-04T00:00:00.000Z".to_owned(), tokens: 0 };
    let mut view = move |_: &mut App| stream_waiting_indicator("w", &[], WaitingProps { thinking: Some(thinking.clone()), running_task_count: 2, on_open_tasks: Some(open.clone()), ..props() });
    let view: &mut dyn FnMut(&mut App) -> E = &mut view;
    rig.settle(view);
    assert_eq!(announced_label(true, &[], tasks_label(2, true).as_deref()), "Thinking; 2 tasks running");
    rig.click_key(view, tasks_line_key(hash_key("w")));
    assert_eq!(*opened.borrow(), 1);
}

#[test]
fn draws_the_task_line_with_nothing_else_to_say_and_none_without_a_pane_to_open() {
    let mut rig = Rig::new();
    assert_eq!(tasks_label(1, true).as_deref(), Some("1 task running"));
    assert_eq!(announced_label(false, &[], tasks_label(1, true).as_deref()), "Model is generating; 1 task running");
    assert_eq!(tasks_label(1, false), None);
    assert_eq!(tasks_label(0, true), None);

    let k = hash_key("w");
    let mut with_pane = |_: &mut App| stream_waiting_indicator("w", &[], WaitingProps { running_task_count: 1, on_open_tasks: Some(Rc::new(|_| {})), ..props() });
    rig.settle(&mut with_pane);
    assert!(rect_of(tasks_line_key(k)).is_some());
    let mut without_pane = |_: &mut App| stream_waiting_indicator("w", &[], WaitingProps { running_task_count: 1, ..props() });
    rig.settle(&mut without_pane);
    assert!(rect_of(tasks_line_key(k)).is_none());
}

#[test]
fn says_when_the_connection_is_being_retried() {
    let mut rig = Rig::new();
    probe::start();
    let notice = ModelRunRetryState { round: 1, attempt: 2, max_attempts: 5, message: "stream reset".to_owned(), dirty: true };
    assert_eq!(retry_label(&notice), "Connection interrupted; retrying 2/5");
    let mut view = move |_: &mut App| stream_waiting_indicator("w", &[], WaitingProps { retry_notice: Some(notice.clone()), ..props() });
    rig.settle(&mut view);
    let said = probe::take();
    assert!(said.iter().any(|line| line == "Connection interrupted; retrying 2/5"), "{said:?}");
    assert!(said.iter().any(|line| line == "stream reset"), "{said:?}");
}

#[test]
fn narrates_only_the_calls_the_timeline_hoisted() {
    let _rig = Rig::new();
    let settled = change("settled", "a.ts", 1, 0);
    let items = [Item::settled(&settled)];
    assert_eq!(tool_activities(&items, &[], &Default::default()), Vec::<ToolActivity>::new());
}

fn no_random() -> impl FnMut() -> f64 {
    || 0.0
}

const fn longest_dwell() -> f64 {
    14_400.0
}

#[test]
fn starts_the_round_at_work() {
    assert_eq!(CatMachine::new(0.0).mood, CatMood::Work);
}

#[test]
fn dwells_only_for_whole_numbers_of_the_moods_own_loop_period() {
    for mood in CAT_MOODS {
        let choices = cat_dwell_choices_ms(mood);
        assert!(!choices.is_empty(), "{mood:?} has no dwell");
        for dwell in choices {
            assert_eq!(dwell % cat_mood_cycle_ms(mood), 0);
        }
    }
    assert_eq!(cat_dwell_choices_ms(CatMood::Work), [9_600, 14_400]);
    assert_eq!(cat_dwell_choices_ms(CatMood::Slack), [4_800, 9_600]);
    assert_eq!(longest_dwell() as u64, CAT_MOODS.iter().flat_map(|mood| cat_dwell_choices_ms(*mood)).max().unwrap());
}

#[test]
fn keeps_every_dwell_long_enough_not_to_distract_and_short_enough_to_be_seen_changing() {
    for mood in CAT_MOODS {
        for dwell in cat_dwell_choices_ms(mood) {
            assert!((4_000..=15_000).contains(&dwell), "{mood:?} dwell {dwell}");
        }
        // More than one dwell per mood, or the timing is a metronome.
        assert!(cat_dwell_choices_ms(mood).len() > 1);
    }
    // The round beside it is working, so the cat mostly is too.
    assert!(cat_dwell_choices_ms(CatMood::Work)[0] > cat_dwell_choices_ms(CatMood::Slack)[0]);
}

#[test]
fn waits_exactly_the_dwell_it_drew_then_changes_to_the_other_mood() {
    let mut machine = CatMachine::new(0.0);
    let mut random = no_random();
    machine.advance(0.0, true, true, &mut random);
    let dwell = cat_dwell_choices_ms(CatMood::Work)[0] as f64;
    machine.advance(dwell - 1.0, true, true, &mut random);
    assert_eq!(machine.mood, CatMood::Work);
    machine.advance(dwell, true, true, &mut random);
    assert_eq!(machine.mood, CatMood::Slack);
}

#[test]
fn waits_for_its_loops_to_come_round_to_rest_before_changing() {
    let mut machine = CatMachine::new(0.0);
    let mut random = no_random();
    machine.advance(0.0, true, true, &mut random);
    let period = cat_mood_cycle_ms(CatMood::Work) as f64;
    let dwell = cat_dwell_choices_ms(CatMood::Work)[0] as f64;
    // Half a cycle behind the timer, which is what a late frame or a spell of not being drawn leaves behind.
    machine.loops_started_ms = -period / 2.0;
    machine.advance(dwell, true, true, &mut random);
    assert_eq!(machine.mood, CatMood::Work, "changing here would tear the loops off mid-move");
    machine.advance(dwell + period / 2.0, true, true, &mut random);
    assert_eq!(machine.mood, CatMood::Slack);
}

#[test]
fn a_frame_arriving_just_after_the_boundary_still_changes_on_it() {
    // The browser's timer tends to fire just before its loop comes round. A frame fires just after, which must not cost a cycle.
    let mut machine = CatMachine::new(0.0);
    let mut random = no_random();
    machine.advance(0.0, true, true, &mut random);
    let dwell = cat_dwell_choices_ms(CatMood::Work)[0] as f64;
    machine.advance(dwell + 20.0, true, true, &mut random);
    assert_eq!(machine.mood, CatMood::Slack);
}

#[test]
fn holds_still_while_the_cat_is_not_laid_out_and_recovers_without_an_event() {
    let mut machine = CatMachine::new(0.0);
    let mut random = no_random();
    machine.advance(0.0, true, true, &mut random);
    let mut now = 0.0;
    for _ in 0..4 {
        now += longest_dwell();
        machine.advance(now, true, false, &mut random);
    }
    assert_eq!(machine.mood, CatMood::Work);
    // Nothing announces a pane being shown again, so the machine finds its own way back.
    now += longest_dwell();
    machine.loops_started_ms = now;
    machine.advance(now, true, true, &mut random);
    assert_eq!(machine.mood, CatMood::Slack);
}

#[test]
fn alternates_between_its_two_moods_for_as_long_as_the_round_lasts() {
    let mut machine = CatMachine::new(0.0);
    let mut random = no_random();
    machine.advance(0.0, true, true, &mut random);
    let mut seen = vec![machine.mood];
    let mut now = 0.0;
    for _ in 0..5 {
        // Only ever on a dwell's own boundary, the way the loops' clock sees it: the first choice for whichever mood is on screen.
        now += cat_dwell_choices_ms(machine.mood)[0] as f64;
        machine.advance(now, true, true, &mut random);
        seen.push(machine.mood);
    }
    assert_eq!(seen, [CatMood::Work, CatMood::Slack, CatMood::Work, CatMood::Slack, CatMood::Work, CatMood::Slack]);
}

#[test]
fn never_changes_mood_while_motion_is_not_allowed_and_a_return_starts_a_fresh_dwell() {
    let mut machine = CatMachine::new(0.0);
    let mut random = no_random();
    let mut now = 0.0;
    for _ in 0..4 {
        now += longest_dwell();
        machine.advance(now, false, true, &mut random);
    }
    assert_eq!(machine.mood, CatMood::Work);
    // Coming back must not spend a deadline that expired meanwhile; the mood moves only after a whole fresh dwell.
    machine.advance(now, true, true, &mut random);
    machine.loops_started_ms = now;
    let dwell = cat_dwell_choices_ms(CatMood::Work)[0] as f64;
    machine.advance(now + dwell - 1.0, true, true, &mut random);
    assert_eq!(machine.mood, CatMood::Work);
    machine.advance(now + dwell, true, true, &mut random);
    assert_eq!(machine.mood, CatMood::Slack);
}

#[test]
fn eases_the_pose_across_a_change_with_the_eyes_on_the_screen_only_at_work() {
    let mut machine = CatMachine::new(0.0);
    let mut random = no_random();
    machine.advance(0.0, true, true, &mut random);
    let dwell = cat_dwell_choices_ms(CatMood::Work)[0] as f64;
    machine.advance(dwell, true, true, &mut random);
    // Fresh loops all start at rest, so what is left is the pose: it has not moved yet.
    let at_change = machine.pose(dwell);
    assert!((at_change.pupils_x - mood_pose(CatMood::Work).0).abs() < 0.01);
    let settled = machine.pose(dwell + 800.0);
    let midway = machine.pose(dwell + 400.0);
    assert!(midway.lids_y > at_change.lids_y && midway.lids_y < settled.lids_y + 0.01);
    // The laptop is to the cat's left as drawn, the viewer's right, so looking at it is looking right.
    assert!(mood_pose(CatMood::Work).0 > 0.0);
    assert!(mood_pose(CatMood::Slack).0 < 0.0);
}

#[test]
fn gives_each_mood_its_own_loops_all_on_one_shared_period() {
    for mood in CAT_MOODS {
        let loops = mood_loops(mood);
        assert!(loops.len() > 2, "{mood:?} has no motion");
        let period = cat_mood_cycle_ms(mood);
        for animation in loops {
            // A loop that does not divide the period is somewhere mid-move when the others come round to rest.
            assert_eq!(period % animation.duration_ms, 0, "{mood:?}: {}", animation.name);
        }
        // One of them has to be the whole period: it is the clock the change is aligned against.
        assert!(loops.iter().any(|animation| animation.duration_ms == period), "{mood:?}");
    }
}

#[test]
fn starts_and_ends_every_loop_at_rest_so_a_change_never_tears_one_off_mid_move() {
    for mood in CAT_MOODS {
        for animation in mood_loops(mood) {
            assert_eq!(animation.stops.first().unwrap().0, 0.0, "{}", animation.name);
            assert_eq!(animation.stops.last().unwrap().0, 100.0, "{}", animation.name);
            assert!(animation.stops.first().unwrap().1.at_rest(), "{}", animation.name);
            assert!(animation.stops.last().unwrap().1.at_rest(), "{}", animation.name);
            assert!(animation.stops.iter().any(|(_, transform)| !transform.at_rest()), "{} never moves", animation.name);
            assert!(sample_loop(animation, 0.0).at_rest(), "{}", animation.name);
        }
    }
}

#[test]
fn types_and_mouses_only_at_work() {
    let moving = |mood: CatMood| mood_loops(mood).iter().map(|animation| animation.part).collect::<Vec<_>>();
    assert!(moving(CatMood::Work).contains(&CatPart::FarPaw));
    assert!(moving(CatMood::Work).contains(&CatPart::NearPaw));
    assert!(!moving(CatMood::Slack).contains(&CatPart::FarPaw));
    assert!(!moving(CatMood::Slack).contains(&CatPart::NearPaw));
    // Both moods wag the tail.
    for mood in CAT_MOODS {
        assert!(moving(mood).contains(&CatPart::Tail));
    }
}

#[test]
fn turns_the_typing_paw_only_upward_about_the_wrist_it_was_cut_round() {
    assert_eq!(CAT_WRIST, (688.0, 624.0));
    let typing = mood_loops(CatMood::Work).iter().find(|animation| animation.part == CatPart::FarPaw).unwrap();
    for (_, transform) in typing.stops {
        match transform {
            // It only ever lifts: pressing down would push it through the keys.
            CatTransform::Rotate(degrees) => assert!(*degrees <= 0.0),
            other => panic!("the far paw only turns: {other:?}"),
        }
    }
}

#[test]
fn slides_the_mouse_paw_no_further_than_its_overlap_covers() {
    let mouse = mood_loops(CatMood::Work).iter().find(|animation| animation.part == CatPart::NearPaw).unwrap();
    for (_, transform) in mouse.stops {
        if let CatTransform::Translate(x, _) = transform {
            assert!(x.abs() < CAT_NEAR_PAW_OVERLAP);
        }
    }
}

#[test]
fn closes_the_eyes_fully_on_every_blink_and_never_lowers_the_lids_further() {
    for mood in CAT_MOODS {
        let pose = mood_pose(mood).1;
        // A drowsy lid still leaves some eye showing, and a working one none.
        assert!((0.0..CAT_LID_LIFT).contains(&pose), "{mood:?}");
        let blink = mood_loops(mood).iter().find(|animation| animation.part == CatPart::Blink).unwrap();
        let deepest = blink.stops.iter().map(|(_, transform)| if let CatTransform::TranslateY(y) = transform { *y } else { 0.0 }).fold(0.0, f32::max);
        // Short of this the eye's outline stays as a ghost; past it the lid runs beyond the depth its geometry is checked at.
        assert_eq!(pose + deepest, CAT_LID_LIFT + LID_OVERSHOOT, "{mood:?}");
    }
    assert_eq!(mood_pose(CatMood::Work).1, 0.0);
    assert!(mood_pose(CatMood::Slack).1 > 0.0);
}

#[test]
fn every_path_of_the_drawing_parses_into_closed_contours() {
    for (name, path, contours) in [("tail", CAT_TAIL, 1), ("laptop", CAT_LAPTOP, 1), ("pupils", CAT_PUPILS, 2), ("lids", CAT_LIDS, 2), ("body", CAT_CUTS.body_without_paws.as_str(), 1), ("far paw", CAT_CUTS.far_paw.as_str(), 1), ("near paw", CAT_CUTS.near_paw.as_str(), 1)] {
        let lines = flatten(path);
        assert_eq!(lines.len(), contours, "{name}");
        for point in lines.iter().flatten() {
            // Everything lies in the drawing's own space; a misread number would not.
            assert!((200.0..1100.0).contains(&point[0]) && (280.0..1000.0).contains(&point[1]), "{name}: {point:?}");
        }
    }
}

fn scene_coverage(pose: &CatPose, x: f32, y: f32) -> f32 {
    // One pixel per scene unit, so a point of the drawing can be read off directly.
    let (w, h) = (724usize, 610usize);
    let coverage = rasterize_cat(pose, w, h);
    coverage[(y - 344.0) as usize * w + (x - 292.0) as usize]
}

#[test]
fn the_face_is_a_hole_and_a_lowered_lid_closes_it() {
    // A point in the near eye's white, well clear of its pupil, in the scene's coordinates: the drawing's plus the slab.
    let (x, y) = (530.0, 545.0 + 62.12);
    let open = CatPose { figure_scale_y: 1.0, tail_deg: 0.0, far_paw_deg: 0.0, near_paw: (0.0, 0.0), pupils_x: 13.0, lids_y: 0.0 };
    assert!(scene_coverage(&open, x, y) < 0.5, "the eye is punched out of the head");
    let shut = CatPose { lids_y: CAT_LID_LIFT + LID_OVERSHOOT, ..open };
    assert!(scene_coverage(&shut, x, y) > 0.9, "a closed lid covers it");
    // Solid head elsewhere: the forehead above the eyes.
    assert!(scene_coverage(&open, 620.0, 420.0 + 62.12) > 0.9);
    // Open air above the laptop's screen.
    assert!(scene_coverage(&open, 900.0, 400.0) < 0.1);
}

#[test]
fn the_cat_is_one_line_tall_and_keeps_its_scenes_proportions() {
    let (w, h) = cat_size(16.0);
    assert_eq!(h, 16.0);
    assert!((w - 16.0 * 724.0 / 610.0).abs() < 0.001);
}

// ---------------------------------------------------------------- rolling number and reveal

#[test]
fn pairs_columns_from_the_right_so_a_widening_number_keeps_its_places() {
    assert_eq!(rolling_slots("9s", "10s"), [RollSlot::Digit { from: 1, to: 1 }, RollSlot::Digit { from: 9, to: 0 }, RollSlot::Glyph("s".to_owned())]);
    assert_eq!(rolling_slots("1m23s", "1m24s")[3], RollSlot::Digit { from: 3, to: 4 });
    assert_eq!(rolling_slots("59s", "1m0s")[0], RollSlot::Digit { from: 1, to: 1 });
}

#[test]
fn a_rolling_number_and_a_revealing_line_paint_through_a_change() {
    let mut rig = Rig::new();
    let (mut number, mut line) = ("9s".to_owned(), "reading".to_owned());
    let mut first = |_: &mut App| col().child(rolling_number("n", &number, mewrk_client::prelude::CAPTION, theme().text_primary)).child(stream_reveal_text("l", &line, mewrk_client::prelude::CAPTION, theme().text_primary));
    rig.settle(&mut first);
    number = "10s".to_owned();
    line = "reading the file".to_owned();
    let mut second = |_: &mut App| col().child(rolling_number("n", &number, mewrk_client::prelude::CAPTION, theme().text_primary)).child(stream_reveal_text("l", &line, mewrk_client::prelude::CAPTION, theme().text_primary));
    rig.settle(&mut second);
}

#[test]
fn the_fresh_text_is_uncovered_from_the_left_with_a_soft_edge() {
    assert_eq!(fresh_alpha(0.0, 100.0, 10.0, 0.0), 0.0);
    assert_eq!(fresh_alpha(100.0, 100.0, 10.0, 1.0), 1.0);
    let midway = 0.5;
    assert!(fresh_alpha(0.0, 100.0, 10.0, midway) > fresh_alpha(50.0, 100.0, 10.0, midway));
}

// ---------------------------------------------------------------- the plan

fn plan(status: PlanStatus) -> ConversationPlan {
    ConversationPlan { conversation_id: "conversation-1".to_owned(), markdown: "# Replace the gate\n\nThree steps.".to_owned(), status, created_at: AT.to_owned(), updated_at: AT.to_owned() }
}

#[test]
fn reports_the_status_a_pane_header_shows_with_the_pending_card_outranking_the_stored_one() {
    let _rig = Rig::new();
    assert_eq!(plan_status_label(Some(&plan(PlanStatus::Draft)), true), "Awaiting approval");
    assert_eq!(plan_status_label(Some(&plan(PlanStatus::Approved)), false), "Approved");
    assert_eq!(plan_status_label(Some(&plan(PlanStatus::Rejected)), false), "Changes requested");
    assert_eq!(plan_status_label(Some(&plan(PlanStatus::Draft)), false), "Drafting");
    assert_eq!(plan_status_label(None, false), "Drafting");
}

#[test]
fn titles_the_pane_with_the_plans_own_first_heading() {
    assert_eq!(plan_title(&plan(PlanStatus::Draft).markdown).as_deref(), Some("Replace the gate"));
    assert_eq!(plan_title("no heading here"), None);
    assert_eq!(plan_title("#### too deep\n### right  "), Some("right".to_owned()));
    assert_eq!(plan_title("#no space"), None);
    assert_eq!(plan_title(&format!("{}# past the first forty lines", "\n".repeat(40))), None);
}

#[test]
fn renders_the_plan_with_its_dock_and_no_dock_without_one() {
    let mut rig = Rig::new();
    let document = plan(PlanStatus::Draft);
    let mut view = move |_: &mut App| plan_pane("p", Some(&document), true, Some(accent_button("Approve the plan").key("approve")));
    rig.settle(&mut view);
    assert!(rect_of(("plan-page-dock", hash_key("p"))).is_some());
    assert!(rect_of("approve").is_some());

    let mut empty = |_: &mut App| plan_pane("p", None, true, None);
    rig.settle(&mut empty);
    assert!(rect_of(("plan-page-dock", hash_key("p"))).is_none());
}

// ---------------------------------------------------------------- workflow detail

fn step(key: &str, state: WorkflowRunStepState) -> WorkflowRunStep {
    WorkflowRunStep { key: key.to_owned(), plan_index: None, label: key.to_owned(), state, agent_id: None, role: None, task: None, model_id: None, tokens: None, elapsed_ms: None, cached: false, blocked: false, skipped: false }
}

#[test]
fn a_workflow_run_is_its_figures_its_strip_and_its_phases() {
    let mut rig = Rig::new();
    let steps = vec![step("a", WorkflowRunStepState::Finished), step("b", WorkflowRunStepState::Running), step("c", WorkflowRunStepState::Pending)];
    let view_model = WorkflowRunView {
        id: "w".to_owned(),
        run_id: None,
        name: "review".to_owned(),
        script_name: None,
        description: "Reads everything".to_owned(),
        state: TaskItemState::Running,
        step_count: 3,
        elapsed_ms: Some(84_000),
        tokens: Some(1_240),
        phases: vec![WorkflowRunPhase { key: "p".to_owned(), heading: None, steps: steps.clone(), done: 1, total: 3 }],
        steps,
        logs: Vec::new(),
        running: true,
    };
    assert_eq!(workflow_run_summary(&view_model), "Workflow review: 3 agents, 1 done");
    let mut view = move |_: &mut App| workflow_run_detail("w", &view_model);
    rig.settle(&mut view);
    let k = hash_key("w");
    assert!(rect_of(("workflow-run", k)).is_some());
    assert!(rect_of(("workflow-strip", k)).is_some());
    assert!(rect_of(("workflow-phase", k, "p")).is_some());
}

// ---------------------------------------------------------------- the tool editor

#[derive(Default, Clone)]
struct Calls {
    ran: Rc<RefCell<Vec<(String, JsonObject)>>>,
    saved: Rc<RefCell<Vec<(JsonObject, String, Vec<ImageAttachment>)>>>,
    cancelled: Rc<RefCell<usize>>,
}

impl Calls {
    fn run(&self, answer: Result<(), String>) -> RunTool {
        let ran = self.ran.clone();
        Rc::new(move |app: &mut App, name: String, input: JsonObject, done: ToolDone| {
            ran.borrow_mut().push((name, input));
            done(app, answer.clone());
        })
    }

    fn save(&self) -> SaveToolEdit {
        let saved = self.saved.clone();
        Rc::new(move |app: &mut App, input: JsonObject, output: String, images: Vec<ImageAttachment>, done: ToolDone| {
            saved.borrow_mut().push((input, output, images));
            done(app, Ok(()));
        })
    }

    fn cancel(&self) -> Handler<App> {
        let cancelled = self.cancelled.clone();
        Rc::new(move |_| *cancelled.borrow_mut() += 1)
    }
}

fn descriptor(rig: &Rig, name: &str) -> ToolDescriptor {
    rig.app.doc.tools.iter().find(|tool| tool.name == name).cloned().unwrap_or_else(|| panic!("the host's catalog has {name}"))
}

const EDITOR: &str = "editor";

fn editor_key() -> u64 {
    hash_key(EDITOR)
}

struct EditorSetup {
    item: ContextItem,
    descriptor: Option<ToolDescriptor>,
    inserting: bool,
    on_run: Option<RunTool>,
    on_save: Option<SaveToolEdit>,
    on_cancel: Handler<App>,
}

fn editor_view(setup: EditorSetup) -> impl FnMut(&mut App) -> E {
    move |_| {
        inline_tool_editor(
            EDITOR,
            ToolEditorProps { item: setup.item.clone(), descriptor: setup.descriptor.clone(), inserting: setup.inserting, on_cancel: setup.on_cancel.clone(), on_run: setup.on_run.clone(), on_save: setup.on_save.clone() },
        )
    }
}

fn find_call() -> ContextItem {
    tool("find-call", "find", json!({ "query": "*.ts" }), result("src/a.ts", None))
}

fn write_call() -> ContextItem {
    tool("write-call", "write", json!({ "path": "notes.txt", "content": "hello" }), result("written", None))
}

fn input_of(value: Value) -> JsonObject {
    object(value)
}

#[test]
fn edits_the_arguments_of_a_side_effect_free_call_and_reruns_it() {
    let mut rig = Rig::new();
    let calls = Calls::default();
    let setup = EditorSetup { item: find_call(), descriptor: Some(descriptor(&rig, "find")), inserting: false, on_run: Some(calls.run(Ok(()))), on_save: Some(calls.save()), on_cancel: calls.cancel() };
    let mut view = editor_view(setup);
    let view: &mut dyn FnMut(&mut App) -> E = &mut view;
    rig.settle(view);
    replace_in(&mut rig, view, arg_field_key(editor_key(), "query"), "*.rs");
    rig.click_key(view, ("tool-editor-run", editor_key()));
    let ran = calls.ran.borrow();
    assert_eq!(ran.len(), 1);
    assert_eq!(ran[0].0, "find");
    assert_eq!(ran[0].1.get("query"), Some(&json!("*.rs")));
}

#[test]
fn lays_cancel_run_and_save_out_in_one_actions_group_with_no_footer() {
    let mut rig = Rig::new();
    let calls = Calls::default();
    let setup = EditorSetup { item: find_call(), descriptor: Some(descriptor(&rig, "find")), inserting: false, on_run: Some(calls.run(Ok(()))), on_save: Some(calls.save()), on_cancel: calls.cancel() };
    let mut view = editor_view(setup);
    rig.settle(&mut view);
    let k = editor_key();
    let (cancel, run, save) = (rect_of(("tool-editor-cancel", k)).unwrap(), rect_of(("tool-editor-run", k)).unwrap(), rect_of(("tool-editor-save", k)).unwrap());
    assert!(cancel.x < run.x && run.x < save.x);
    assert_eq!(cancel.y, save.y);
}

#[test]
fn keeps_a_failure_message_beside_the_actions_group_and_the_edited_arguments() {
    let mut rig = Rig::new();
    let calls = Calls::default();
    let setup = EditorSetup { item: find_call(), descriptor: Some(descriptor(&rig, "find")), inserting: false, on_run: Some(calls.run(Err("The workspace is unavailable".to_owned()))), on_save: None, on_cancel: calls.cancel() };
    let mut view = editor_view(setup);
    let view: &mut dyn FnMut(&mut App) -> E = &mut view;
    rig.settle(view);
    let k = editor_key();
    replace_in(&mut rig, view, arg_field_key(k, "query"), "*.rs");
    rig.click_key(view, ("tool-editor-run", k));
    let failure = rect_of(("tool-editor-failure", k)).expect("the failure is on screen");
    let actions = rect_of(("tool-editor-actions", k)).unwrap();
    assert!(!(failure.y >= actions.y && failure.bottom() <= actions.bottom()));
    // A failed rerun leaves the form where it was, ready to be run again.
    rig.click_key(view, ("tool-editor-run", k));
    assert_eq!(calls.ran.borrow().len(), 2);
    assert_eq!(calls.ran.borrow()[1].1.get("query"), Some(&json!("*.rs")));
}

#[test]
fn saves_edited_arguments_and_result_without_executing_anything() {
    let mut rig = Rig::new();
    let calls = Calls::default();
    let setup = EditorSetup { item: write_call(), descriptor: Some(descriptor(&rig, "write")), inserting: false, on_run: Some(calls.run(Ok(()))), on_save: Some(calls.save()), on_cancel: calls.cancel() };
    let mut view = editor_view(setup);
    let view: &mut dyn FnMut(&mut App) -> E = &mut view;
    rig.settle(view);
    let k = editor_key();
    replace_in(&mut rig, view, result_field_key(k), "rewritten by hand");
    rig.click_key(view, ("tool-editor-save", k));
    let saved = calls.saved.borrow();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].0.get("path"), Some(&json!("notes.txt")));
    assert_eq!(saved[0].1, "rewritten by hand");
    assert!(calls.ran.borrow().is_empty());
}

#[test]
fn saves_an_untouched_call_exactly_as_recorded_adding_no_defaults_and_dropping_no_explicit_values() {
    let mut rig = Rig::new();
    let read_tool = descriptor(&rig, "read");
    let read_input = input_of(json!({ "path": "shot.png" }));
    // The declared default is a hint in the empty control, not a value.
    let fields = recorded_fields(&read_input, Some(&read_tool));
    let draft = draft_from_input(&fields, &read_input);
    let start_line = fields.iter().find(|field| field.name == "start_line").expect("read has a start line");
    assert_eq!(start_line.placeholder.as_deref(), Some("1"));
    assert_eq!(draft.iter().find(|(name, _)| name == "start_line").unwrap().1, "");

    let calls = Calls::default();
    let read = tool("read-call", "read", json!({ "path": "shot.png" }), result("image", None));
    let setup = EditorSetup { item: read, descriptor: Some(read_tool), inserting: false, on_run: None, on_save: Some(calls.save()), on_cancel: calls.cancel() };
    let mut view = editor_view(setup);
    rig.settle(&mut view);
    rig.click_key(&mut view, ("tool-editor-save", editor_key()));
    assert_eq!(calls.saved.borrow()[0].0, input_of(json!({ "path": "shot.png" })));

    let shell = rig.app.doc.tools.iter().find(|tool| tool.name == "zsh" || tool.name == "bash").cloned().expect("a shell");
    let shell_input = input_of(json!({ "command": "ls", "run_in_background": false }));
    let fields = recorded_fields(&shell_input, Some(&shell));
    let draft = draft_from_input(&fields, &shell_input);
    assert_eq!(parse_draft(&fields, &draft, &draft_messages(), Some((&shell_input, &draft))).unwrap(), shell_input);
}

#[test]
fn edits_the_call_as_the_model_made_it_and_keeps_untouched_values_exactly() {
    let mut rig = Rig::new();
    let calls = Calls::default();
    // A hook ran it with another path; the model wrote `notes.txt`, a number where text belongs, and an explicit null.
    let requested = input_of(json!({ "path": "notes.txt", "content": 5, "mode": null }));
    let rewritten = match tool("write-call", "write", json!({ "path": "safe/notes.txt", "content": "5" }), result("written", None)) {
        ContextItem::Tool { id, tool_name, round, model_turn_id, provider_call_id, input, result, subagent, notice, attestation, created_at, .. } => ContextItem::Tool { id, tool_name, round, model_turn_id, provider_call_id, requested_input: Some(requested.clone()), input, result, subagent, notice, attestation, created_at },
        _ => unreachable!(),
    };
    let setup = EditorSetup { item: rewritten, descriptor: Some(descriptor(&rig, "write")), inserting: false, on_run: None, on_save: Some(calls.save()), on_cancel: calls.cancel() };
    let mut view = editor_view(setup);
    rig.settle(&mut view);
    rig.click_key(&mut view, ("tool-editor-save", editor_key()));
    assert_eq!(calls.saved.borrow()[0].0, requested);
    assert_eq!(calls.saved.borrow()[0].1, "written");
}

#[test]
fn offers_no_rerun_for_a_call_the_host_only_runs_inside_a_model_turn() {
    let mut rig = Rig::new();
    let calls = Calls::default();
    let plan_call = tool("plan-call", "plan", json!({ "action": "set", "content": "step one" }), result("plan updated", None));
    let setup = EditorSetup { item: plan_call, descriptor: rig.app.doc.tools.iter().find(|tool| tool.name == "plan").cloned(), inserting: false, on_run: Some(calls.run(Ok(()))), on_save: Some(calls.save()), on_cancel: calls.cancel() };
    let mut view = editor_view(setup);
    rig.settle(&mut view);
    let k = editor_key();
    assert!(rect_of(("tool-editor-run", k)).is_none());
    assert!(rect_of(("tool-editor-save", k)).is_some());
    // Its arguments stay editable and saveable, one row per recorded key.
    assert!(rect_of(hash_key(("field", arg_field_key(k, "action")))).is_some());
    assert!(!can_rerun_tool("plan"));
    assert!(can_rerun_tool("find"));
    assert!(!can_rerun_tool("zsh"));
}

#[test]
fn shows_a_side_effecting_calls_recorded_arguments_without_a_rerun_button() {
    let mut rig = Rig::new();
    let calls = Calls::default();
    let setup = EditorSetup { item: write_call(), descriptor: Some(descriptor(&rig, "write")), inserting: false, on_run: Some(calls.run(Ok(()))), on_save: Some(calls.save()), on_cancel: calls.cancel() };
    let mut view = editor_view(setup);
    rig.settle(&mut view);
    let k = editor_key();
    assert!(rect_of(("tool-editor-run", k)).is_none());
    assert!(rect_of(hash_key(("field", arg_field_key(k, "path")))).is_some());
    assert!(rect_of(hash_key(("field", result_field_key(k)))).is_some());
}

#[test]
fn saves_removal_of_a_broken_result_image_alongside_the_edited_payload() {
    let mut rig = Rig::new();
    let calls = Calls::default();
    let with_images = tool("find-call", "find", json!({ "query": "*.ts" }), ToolResult { images: vec![image("broken-tool-image", "broken.png"), image("good-tool-image", "good.png")], ..result("src/a.ts", None) });
    let setup = EditorSetup { item: with_images, descriptor: Some(descriptor(&rig, "find")), inserting: false, on_run: None, on_save: Some(calls.save()), on_cancel: calls.cancel() };
    let mut view = editor_view(setup);
    let view: &mut dyn FnMut(&mut App) -> E = &mut view;
    rig.settle(view);
    let strip = hash_key(("tool-editor-images", editor_key()));
    rig.click_key(view, thumbnail_remove_key(strip, 0));
    rig.click_key(view, ("tool-editor-save", editor_key()));
    let saved = calls.saved.borrow();
    assert_eq!(saved[0].2.iter().map(|image| image.id.as_str()).collect::<Vec<_>>(), ["good-tool-image"]);
}

#[test]
fn escape_cancels_unless_the_editor_is_busy() {
    let mut rig = Rig::new();
    let calls = Calls::default();
    let setup = EditorSetup { item: find_call(), descriptor: Some(descriptor(&rig, "find")), inserting: false, on_run: None, on_save: Some(calls.save()), on_cancel: calls.cancel() };
    let mut view = editor_view(setup);
    let view: &mut dyn FnMut(&mut App) -> E = &mut view;
    rig.settle(view);
    let r = field_rect(arg_field_key(editor_key(), "query"));
    rig.click(view, r.x + 2.0, r.y + 2.0);
    rig.key(view, Key::Escape, false, false);
    assert_eq!(*calls.cancelled.borrow(), 1);
}

#[test]
fn says_what_is_wrong_with_each_argument_in_the_fields_order() {
    let mut rig = Rig::new();
    let write = descriptor(&rig, "write");
    let empty = JsonObject::new();
    let fields = recorded_fields(&empty, Some(&write));
    let draft = draft_from_input(&fields, &empty);
    let errors = parse_draft(&fields, &draft, &draft_messages(), Some((&empty, &draft))).unwrap_err();
    assert_eq!(errors[0], ("path".to_owned(), "This field is required".to_owned()));

    // Types decode as typed: a number, a strict boolean, a JSON value.
    let typed = vec![
        mewrk_ui_chat::inline_tool_editor::EditableField { name: "n".into(), label: "n".into(), required: false, decode: Decode::Number, placeholder: None },
        mewrk_ui_chat::inline_tool_editor::EditableField { name: "b".into(), label: "b".into(), required: false, decode: Decode::Boolean, placeholder: None },
        mewrk_ui_chat::inline_tool_editor::EditableField { name: "j".into(), label: "j".into(), required: false, decode: Decode::Json, placeholder: None },
    ];
    let good = vec![("n".to_owned(), " 5 ".to_owned()), ("b".to_owned(), "false".to_owned()), ("j".to_owned(), "[1]".to_owned())];
    assert_eq!(parse_draft(&typed, &good, &draft_messages(), None).unwrap(), input_of(json!({ "n": 5, "b": false, "j": [1] })));
    let bad = vec![("n".to_owned(), "five".to_owned()), ("b".to_owned(), "no".to_owned()), ("j".to_owned(), "[".to_owned())];
    let messages: Vec<String> = parse_draft(&typed, &bad, &draft_messages(), None).unwrap_err().into_iter().map(|(_, message)| message).collect();
    assert_eq!(messages, ["Enter a valid number", "Enter true or false", "Invalid JSON"]);
    let _ = &mut rig;
}

#[test]
fn runs_a_first_execution_of_a_side_effecting_tool_without_a_second_local_confirmation() {
    let mut rig = Rig::new();
    let calls = Calls::default();
    let setup = EditorSetup { item: draft_tool_context("write"), descriptor: Some(descriptor(&rig, "write")), inserting: true, on_run: Some(calls.run(Ok(()))), on_save: None, on_cancel: calls.cancel() };
    let mut view = editor_view(setup);
    let view: &mut dyn FnMut(&mut App) -> E = &mut view;
    rig.settle(view);
    let k = editor_key();
    replace_in(&mut rig, view, arg_field_key(k, "path"), "notes.txt");
    replace_in(&mut rig, view, arg_field_key(k, "content"), "hello");
    rig.click_key(view, ("tool-editor-run", k));
    let ran = calls.ran.borrow();
    assert_eq!(ran[0], ("write".to_owned(), input_of(json!({ "path": "notes.txt", "content": "hello" }))));
}

#[test]
fn writes_out_a_side_effecting_call_result_and_all_without_executing_it() {
    let mut rig = Rig::new();
    let calls = Calls::default();
    let setup = EditorSetup { item: draft_tool_context("write"), descriptor: Some(descriptor(&rig, "write")), inserting: true, on_run: Some(calls.run(Ok(()))), on_save: Some(calls.save()), on_cancel: calls.cancel() };
    let mut view = editor_view(setup);
    let view: &mut dyn FnMut(&mut App) -> E = &mut view;
    rig.settle(view);
    let k = editor_key();
    replace_in(&mut rig, view, arg_field_key(k, "path"), "notes.txt");
    replace_in(&mut rig, view, arg_field_key(k, "content"), "hello");
    replace_in(&mut rig, view, result_field_key(k), "wrote notes.txt");
    rig.click_key(view, ("tool-editor-save", k));
    let saved = calls.saved.borrow();
    assert_eq!(saved[0].0, input_of(json!({ "path": "notes.txt", "content": "hello" })));
    assert_eq!(saved[0].1, "wrote notes.txt");
    // Saving is the whole point: the file on disk is untouched.
    assert!(calls.ran.borrow().is_empty());
}

#[test]
fn the_name_column_sizes_to_the_longest_name_so_every_box_starts_on_the_same_edge() {
    let mut rig = Rig::new();
    let calls = Calls::default();
    let setup = EditorSetup { item: write_call(), descriptor: Some(descriptor(&rig, "write")), inserting: false, on_run: None, on_save: Some(calls.save()), on_cancel: calls.cancel() };
    let mut view = editor_view(setup);
    rig.settle(&mut view);
    let k = editor_key();
    let (path, content, output) = (field_rect(arg_field_key(k, "path")), field_rect(arg_field_key(k, "content")), field_rect(result_field_key(k)));
    assert_eq!(path.x, content.x);
    assert!(path.x > 0.0, "names sit to the left");
    assert!(path.w > 100.0, "the value takes what is left of the row");
    // The result spans the card.
    assert!(output.x < path.x);
    assert!(path.y < content.y && content.y < output.y);
}

// ---------------------------------------------------------------- the text editor

#[derive(Default, Clone)]
struct TextCalls {
    saved: Rc<RefCell<Vec<(String, Vec<ImageAttachment>, Vec<FileAttachment>)>>>,
    cancelled: Rc<RefCell<usize>>,
}

impl TextCalls {
    fn save(&self) -> SaveText {
        let saved = self.saved.clone();
        Rc::new(move |_, content, images, files| saved.borrow_mut().push((content, images, files)))
    }

    fn cancel(&self) -> Handler<App> {
        let cancelled = self.cancelled.clone();
        Rc::new(move |_| *cancelled.borrow_mut() += 1)
    }
}

fn text_view(kind: InsertKind, content: &str, images: Vec<ImageAttachment>, files: Vec<FileAttachment>, calls: &TextCalls, add: Option<AddAttachments>) -> impl FnMut(&mut App) -> E {
    let (content, on_cancel, on_save) = (content.to_owned(), calls.cancel(), calls.save());
    move |_| inline_text_editor("text", TextEditorProps { kind, content: content.clone(), images: images.clone(), files: files.clone(), show_kind: false, image_input: Some(true), on_cancel: on_cancel.clone(), on_save: on_save.clone(), on_add_attachments: add.clone() })
}

fn text_key() -> u64 {
    hash_key("text")
}

#[test]
fn keeps_an_image_only_user_message_savable_without_requiring_replacement_text() {
    let mut rig = Rig::new();
    let calls = TextCalls::default();
    let mut view = text_view(InsertKind::User, "", vec![image("image-one", "screen.png")], Vec::new(), &calls, None);
    let view: &mut dyn FnMut(&mut App) -> E = &mut view;
    rig.settle(view);
    rig.click_key(view, ("text-editor-save", text_key()));
    assert_eq!(calls.saved.borrow()[0], (String::new(), vec![image("image-one", "screen.png")], Vec::new()));
}

#[test]
fn keeps_cancel_and_save_together_in_the_bar_and_the_footer_for_attaching_only() {
    let mut rig = Rig::new();
    let calls = TextCalls::default();
    let k = text_key();
    let mut plain = text_view(InsertKind::Assistant, "x", Vec::new(), Vec::new(), &calls, None);
    rig.settle(&mut plain);
    let (cancel, save) = (rect_of(("text-editor-cancel", k)).unwrap(), rect_of(("text-editor-save", k)).unwrap());
    assert!(cancel.x < save.x);
    // A message that takes no attachments has nothing to put in a footer.
    assert!(rect_of(("text-editor-footer", k)).is_none());

    let add: AddAttachments = Rc::new(|_, _, _, _| {});
    let mut with_files = text_view(InsertKind::User, "x", Vec::new(), Vec::new(), &calls, Some(add));
    rig.settle(&mut with_files);
    assert!(rect_of(("text-editor-footer", k)).is_some());
    assert!(rect_of(("text-editor-bar", k)).is_some());
}

#[test]
fn can_remove_one_broken_attachment_without_deleting_the_message() {
    let mut rig = Rig::new();
    let calls = TextCalls::default();
    let mut view = text_view(InsertKind::User, "keep the words", vec![image("broken-image", "broken.png"), image("good-image", "good.png")], Vec::new(), &calls, None);
    let view: &mut dyn FnMut(&mut App) -> E = &mut view;
    rig.settle(view);
    rig.click_key(view, thumbnail_remove_key(hash_key(("text-editor-images", text_key())), 0));
    rig.click_key(view, ("text-editor-save", text_key()));
    let saved = calls.saved.borrow();
    assert_eq!(saved[0].0, "keep the words");
    assert_eq!(saved[0].1.iter().map(|image| image.id.as_str()).collect::<Vec<_>>(), ["good-image"]);
}

#[test]
fn refuses_to_save_an_empty_message_and_cancels_on_escape() {
    let mut rig = Rig::new();
    let calls = TextCalls::default();
    let mut view = text_view(InsertKind::Assistant, "", Vec::new(), Vec::new(), &calls, None);
    let view: &mut dyn FnMut(&mut App) -> E = &mut view;
    rig.settle(view);
    rig.click_key(view, ("text-editor-save", text_key()));
    assert!(calls.saved.borrow().is_empty());
    let r = field_rect(text_field_key(text_key()));
    rig.click(view, r.x + 2.0, r.y + 2.0);
    rig.key(view, Key::Escape, false, false);
    assert_eq!(*calls.cancelled.borrow(), 1);
}

#[test]
fn saves_on_command_enter_from_the_field_it_focused_on_open() {
    let mut rig = Rig::new();
    let calls = TextCalls::default();
    let mut view = text_view(InsertKind::Assistant, "", Vec::new(), Vec::new(), &calls, None);
    let view: &mut dyn FnMut(&mut App) -> E = &mut view;
    rig.settle(view);
    rig.type_text(view, "typed");
    rig.settle(view);
    rig.key(view, Key::Enter, true, false);
    assert_eq!(calls.saved.borrow()[0].0, "typed");
}

#[test]
fn keeps_its_draft_through_a_stream_flush() {
    let mut rig = Rig::new();
    let calls = TextCalls::default();
    let mut first = text_view(InsertKind::Assistant, "start", Vec::new(), Vec::new(), &calls, None);
    rig.settle(&mut first);
    rig.type_text(&mut first, " more");
    rig.settle(&mut first);
    // The same key with props rebuilt from a newer timeline: nothing the person typed goes.
    let mut flushed = text_view(InsertKind::Assistant, "the stream rewrote this", Vec::new(), Vec::new(), &calls, None);
    rig.settle(&mut flushed);
    rig.click_key(&mut flushed, ("text-editor-save", text_key()));
    assert_eq!(calls.saved.borrow()[0].0, "start more");
}

#[test]
fn hides_the_number_its_own_thumbnail_stands_for_and_writes_it_back_on_save() {
    let mut rig = Rig::new();
    let calls = TextCalls::default();
    let numbered = ImageAttachment { short_id: Some(1), ..image("image-one", "screen.png") };
    let mut view = text_view(InsertKind::User, "look at this [Image #1]", vec![numbered.clone()], Vec::new(), &calls, None);
    let view: &mut dyn FnMut(&mut App) -> E = &mut view;
    rig.settle(view);
    rig.key(view, Key::Enter, true, false);
    let saved = calls.saved.borrow();
    assert_eq!(saved[0], ("look at this [Image #1]".to_owned(), vec![numbered], Vec::new()));
}

#[test]
fn keeps_the_files_a_message_carries_lets_one_be_removed_and_saves_the_rest() {
    let mut rig = Rig::new();
    let calls = TextCalls::default();
    let mut view = text_view(InsertKind::User, "see the attachments", Vec::new(), vec![file("file-one", "notes.md", FileAttachmentFormat::Text), file("file-two", "paper.pdf", FileAttachmentFormat::Pdf)], &calls, None);
    let view: &mut dyn FnMut(&mut App) -> E = &mut view;
    rig.settle(view);
    rig.click_key(view, file_remove_key(hash_key(("text-editor-images", text_key())), "file-one"));
    rig.click_key(view, ("text-editor-save", text_key()));
    assert_eq!(calls.saved.borrow()[0].2.iter().map(|file| file.id.as_str()).collect::<Vec<_>>(), ["file-two"]);
}

#[test]
fn keeps_a_files_only_message_savable() {
    let mut rig = Rig::new();
    let calls = TextCalls::default();
    let mut view = text_view(InsertKind::User, "", Vec::new(), vec![file("file-one", "notes.md", FileAttachmentFormat::Text)], &calls, None);
    let view: &mut dyn FnMut(&mut App) -> E = &mut view;
    rig.settle(view);
    rig.click_key(view, ("text-editor-save", text_key()));
    assert_eq!(calls.saved.borrow()[0].2.len(), 1);
}

#[test]
fn folds_a_long_paste_into_a_tag_and_saves_it_as_the_text_it_stands_for() {
    mewrk_ui_composer::pasted_text::forget_last_copy();
    let mut rig = Rig::new();
    let calls = TextCalls::default();
    let mut view = text_view(InsertKind::User, "look at this", Vec::new(), Vec::new(), &calls, None);
    let view: &mut dyn FnMut(&mut App) -> E = &mut view;
    rig.settle(view);
    let long: String = (0..120).map(|index| format!("line {index}")).collect::<Vec<_>>().join("\n");
    rig.clipboard.0 = Some(long.clone());
    rig.key(view, Key::Char('v'), true, false);
    rig.settle(view);
    rig.click_key(view, ("text-editor-save", text_key()));
    assert_eq!(calls.saved.borrow()[0].0, format!("look at this{long}"));
}

#[test]
fn leaves_a_short_paste_in_the_box() {
    mewrk_ui_composer::pasted_text::forget_last_copy();
    let mut rig = Rig::new();
    let calls = TextCalls::default();
    let mut view = text_view(InsertKind::User, "", Vec::new(), Vec::new(), &calls, None);
    let view: &mut dyn FnMut(&mut App) -> E = &mut view;
    rig.settle(view);
    rig.clipboard.0 = Some("short".to_owned());
    rig.key(view, Key::Char('v'), true, false);
    rig.settle(view);
    rig.key(view, Key::Enter, true, false);
    assert_eq!(calls.saved.borrow()[0].0, "short");
}

/// An `AddAttachments` that answers at once, recording what it was asked about.
fn answering(answer: AddedAttachments, asked: Rc<RefCell<Vec<(Vec<PathBuf>, Vec<ImageAttachment>)>>>) -> AddAttachments {
    let answer = Rc::new(RefCell::new(Some(answer)));
    Rc::new(move |app, paths, existing, done| {
        asked.borrow_mut().push((paths, existing));
        if let Some(answer) = answer.borrow_mut().take() {
            done(app, answer);
        }
    })
}

#[test]
fn takes_a_file_dropped_on_it_and_saves_the_picture_with_the_number_the_surface_assigned() {
    let mut rig = Rig::new();
    let calls = TextCalls::default();
    let pasted = ImageAttachment { short_id: Some(4), ..image("pasted-image", "pasted.png") };
    let asked: Rc<RefCell<Vec<(Vec<PathBuf>, Vec<ImageAttachment>)>>> = Rc::default();
    let add = answering(AddedAttachments { images: vec![pasted.clone()], files: Vec::new(), rejected: Vec::new() }, asked.clone());
    let mut view = text_view(InsertKind::User, "compare these", Vec::new(), Vec::new(), &calls, Some(add));
    let view: &mut dyn FnMut(&mut App) -> E = &mut view;
    rig.settle(view);
    let zone = rect_of(("text-editor-body", text_key())).unwrap();
    rig.point(zone.x + zone.w * 0.5, zone.y + zone.h * 0.5);
    rig.input.dropped.push(PathBuf::from("/tmp/pasted.png"));
    rig.settle(view);
    assert_eq!(asked.borrow().len(), 1);
    assert_eq!(asked.borrow()[0].0, [PathBuf::from("/tmp/pasted.png")]);
    rig.click_key(view, ("text-editor-save", text_key()));
    assert_eq!(calls.saved.borrow()[0], ("compare these [Image #4]".to_owned(), vec![pasted], Vec::new()));
}

/// Runs frames until something that happens on a thread has happened.
fn until(rig: &mut Rig, view: &mut dyn FnMut(&mut App) -> E, what: &str, done: &dyn Fn() -> bool) {
    for _ in 0..500 {
        rig.frame(view);
        if done() {
            rig.settle(view);
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("{what} never happened");
}

#[test]
fn takes_a_pasted_image_and_saves_it_with_the_number_the_surface_assigned() {
    let mut rig = Rig::new();
    attach_ports(&mut rig.app).read_clipboard = Rc::new(|| ClipboardContents { image: Some((vec![255; 4], 1, 1)), paths: Vec::new() });
    let calls = TextCalls::default();
    let pasted = ImageAttachment { short_id: Some(4), ..image("pasted-image", "pasted.png") };
    let answer = Rc::new(RefCell::new(Some(pasted.clone())));
    let seen: Rc<RefCell<Vec<(String, bool)>>> = Rc::default();
    let heard = seen.clone();
    let add: AddAttachments = Rc::new(move |app, paths, _, done| {
        // The picture is a file by the time the surface is told about it.
        heard.borrow_mut().extend(paths.iter().map(|path| (path.file_name().unwrap().to_string_lossy().into_owned(), path.exists())));
        done(app, AddedAttachments { images: answer.borrow_mut().take().into_iter().collect(), files: Vec::new(), rejected: Vec::new() });
    });
    let mut view = text_view(InsertKind::User, "compare these", Vec::new(), Vec::new(), &calls, Some(add));
    let view: &mut dyn FnMut(&mut App) -> E = &mut view;
    rig.settle(view);
    rig.key(view, Key::Char('v'), true, false);
    let waiting = seen.clone();
    until(&mut rig, view, "the surface hearing of the picture", &move || !waiting.borrow().is_empty());
    assert_eq!(*seen.borrow(), [("image.png".to_owned(), true)]);
    rig.click_key(view, ("text-editor-save", text_key()));
    assert_eq!(calls.saved.borrow()[0], ("compare these [Image #4]".to_owned(), vec![pasted], Vec::new()));
}

#[test]
fn files_on_the_clipboard_join_the_message_and_any_text_with_them_stays_text() {
    mewrk_ui_composer::pasted_text::forget_last_copy();
    let mut rig = Rig::new();
    attach_ports(&mut rig.app).read_clipboard = Rc::new(|| ClipboardContents { image: None, paths: vec![PathBuf::from("/tmp/a.txt")] });
    let calls = TextCalls::default();
    let asked: Rc<RefCell<Vec<(Vec<PathBuf>, Vec<ImageAttachment>)>>> = Rc::default();
    let add = answering(AddedAttachments { images: Vec::new(), files: vec![file("f", "a.txt", FileAttachmentFormat::Text)], rejected: Vec::new() }, asked.clone());
    let mut view = text_view(InsertKind::User, "", Vec::new(), Vec::new(), &calls, Some(add));
    let view: &mut dyn FnMut(&mut App) -> E = &mut view;
    rig.settle(view);
    rig.clipboard.0 = Some("a.txt".to_owned());
    rig.key(view, Key::Char('v'), true, false);
    let waiting = asked.clone();
    until(&mut rig, view, "the surface hearing of the file", &move || !waiting.borrow().is_empty());
    assert_eq!(asked.borrow()[0].0, [PathBuf::from("/tmp/a.txt")]);
    rig.key(view, Key::Enter, true, false);
    assert_eq!(calls.saved.borrow()[0].0, "a.txt");
    assert_eq!(calls.saved.borrow()[0].2.len(), 1);
}

#[test]
fn leaves_a_paste_alone_where_nothing_can_be_attached() {
    mewrk_ui_composer::pasted_text::forget_last_copy();
    let mut rig = Rig::new();
    attach_ports(&mut rig.app).read_clipboard = Rc::new(|| panic!("a message that takes no attachments has no reason to look"));
    let calls = TextCalls::default();
    let mut view = text_view(InsertKind::User, "just words", Vec::new(), Vec::new(), &calls, None);
    let view: &mut dyn FnMut(&mut App) -> E = &mut view;
    rig.settle(view);
    rig.clipboard.0 = None;
    rig.key(view, Key::Char('v'), true, false);
    rig.settle(view);
    rig.key(view, Key::Enter, true, false);
    assert_eq!(calls.saved.borrow()[0].0, "just words");
}

#[test]
fn offers_the_add_menu_on_a_user_message_and_attaches_what_the_dialog_picked() {
    let mut rig = Rig::new();
    attach_ports(&mut rig.app).pick_files = std::sync::Arc::new(|| Some(vec![PathBuf::from("/tmp/a.txt")]));
    let calls = TextCalls::default();
    let asked: Rc<RefCell<Vec<(Vec<PathBuf>, Vec<ImageAttachment>)>>> = Rc::default();
    let add = answering(AddedAttachments { images: Vec::new(), files: vec![file("f", "a.txt", FileAttachmentFormat::Text)], rejected: Vec::new() }, asked.clone());
    let mut view = text_view(InsertKind::User, "x", Vec::new(), Vec::new(), &calls, Some(add));
    let view: &mut dyn FnMut(&mut App) -> E = &mut view;
    rig.settle(view);
    let menu = ("text-editor-add", text_key());
    rig.click_key(view, mewrk_ui_common::popover_menu::trigger_key(menu));
    rig.settle(view);
    rig.click_key(view, mewrk_ui_common::popover_menu::row_key(menu, "files"));
    let waiting = asked.clone();
    until(&mut rig, view, "the surface hearing of the pick", &move || !waiting.borrow().is_empty());
    assert_eq!(asked.borrow()[0].0, [PathBuf::from("/tmp/a.txt")]);
    assert!(rect_of(file_tile_key(hash_key(("text-editor-images", text_key())), "f")).is_some());
}

#[test]
fn a_drop_elsewhere_in_the_window_is_not_for_it() {
    let mut rig = Rig::new();
    let calls = TextCalls::default();
    let asked: Rc<RefCell<Vec<(Vec<PathBuf>, Vec<ImageAttachment>)>>> = Rc::default();
    let add = answering(AddedAttachments { images: Vec::new(), files: Vec::new(), rejected: Vec::new() }, asked.clone());
    let mut view = text_view(InsertKind::User, "x", Vec::new(), Vec::new(), &calls, Some(add));
    let view: &mut dyn FnMut(&mut App) -> E = &mut view;
    rig.settle(view);
    rig.point(990.0, 790.0);
    rig.input.dropped.push(PathBuf::from("/tmp/elsewhere.png"));
    rig.settle(view);
    assert!(asked.borrow().is_empty());
}

#[test]
fn reports_what_an_attempt_left_out_and_why() {
    let mut rig = Rig::new();
    let calls = TextCalls::default();
    let asked: Rc<RefCell<Vec<(Vec<PathBuf>, Vec<ImageAttachment>)>>> = Rc::default();
    let add = answering(AddedAttachments { images: Vec::new(), files: Vec::new(), rejected: vec![AttachmentRejection { name: Some("archive.zip".to_owned()), reason: AttachmentRejectionReason::Unsupported }] }, asked);
    let mut view = text_view(InsertKind::User, "x", Vec::new(), Vec::new(), &calls, Some(add));
    let view: &mut dyn FnMut(&mut App) -> E = &mut view;
    rig.settle(view);
    let zone = rect_of(("text-editor-body", text_key())).unwrap();
    rig.point(zone.x + zone.w * 0.5, zone.y + zone.h * 0.5);
    rig.input.dropped.push(PathBuf::from("/tmp/archive.zip"));
    rig.settle(view);
    assert!(rect_of(("text-editor-notice", text_key())).is_some());
    rig.click_key(view, ("text-editor-notice-dismiss", text_key()));
    assert!(rect_of(("text-editor-notice", text_key())).is_none());
}

#[test]
fn a_file_answered_by_the_surface_joins_the_strip() {
    let mut rig = Rig::new();
    let calls = TextCalls::default();
    let asked: Rc<RefCell<Vec<(Vec<PathBuf>, Vec<ImageAttachment>)>>> = Rc::default();
    let add = answering(AddedAttachments { images: Vec::new(), files: vec![file("f", "a.txt", FileAttachmentFormat::Text)], rejected: Vec::new() }, asked);
    let mut view = text_view(InsertKind::User, "x", Vec::new(), Vec::new(), &calls, Some(add));
    let view: &mut dyn FnMut(&mut App) -> E = &mut view;
    rig.settle(view);
    let zone = rect_of(("text-editor-body", text_key())).unwrap();
    rig.point(zone.x + zone.w * 0.5, zone.y + zone.h * 0.5);
    rig.input.dropped.push(PathBuf::from("/tmp/a.txt"));
    rig.settle(view);
    rig.settle(view);
    assert!(rect_of(file_tile_key(hash_key(("text-editor-images", text_key())), "f")).is_some());
}

#[test]
fn has_no_add_menu_where_nothing_can_be_attached() {
    let mut rig = Rig::new();
    let calls = TextCalls::default();
    let mut view = text_view(InsertKind::Assistant, "x", Vec::new(), Vec::new(), &calls, None);
    rig.settle(&mut view);
    assert!(rect_of(("text-editor-footer", text_key())).is_none());
}

#[test]
fn the_field_takes_focus_when_it_opens() {
    let mut rig = Rig::new();
    let calls = TextCalls::default();
    let mut view = text_view(InsertKind::User, "", Vec::new(), Vec::new(), &calls, None);
    rig.settle(&mut view);
    assert_eq!(focused_id(), hash_key(("field", text_field_key(text_key()))));
}

// ---------------------------------------------------------------- the question editor and card

fn question_input() -> Value {
    json!({
        "questions": [
            { "header": "Plan", "question": "Which plan?", "multiSelect": false, "options": [{ "label": "Safe", "description": "Compatible first", "preview": "safe()" }, { "label": "Fast", "description": "Speed first" }] },
            { "header": "Powers", "question": "Which powers?", "multiSelect": true, "options": [{ "label": "Search", "description": "Look things up" }, { "label": "Export", "description": "Make files" }] }
        ]
    })
}

fn question_context(input: Value) -> ContextItem {
    tool("ask-1", "ask_user", input, result("Waiting for user input.", None))
}

fn answer_context(content: &str) -> ContextItem {
    ContextItem::User { id: "answer-1".to_owned(), content: content.to_owned(), images: Vec::new(), files: Vec::new(), created_at: AT.to_owned() }
}

const FORMATTED_ANSWER: &str = "User has answered your questions: \"Which plan?\"=\"Safe\", \"Which powers?\"=\"Search, Export\"";

type QuestionSaves = Rc<RefCell<Vec<(JsonObject, Option<String>)>>>;

fn question_saver(saves: &QuestionSaves, answer: Result<(), String>) -> SaveQuestion {
    let saves = saves.clone();
    Rc::new(move |app: &mut App, input, content, done: ToolDone| {
        saves.borrow_mut().push((input, content));
        done(app, answer.clone());
    })
}

fn question_view(input: Value, answer: Option<ContextItem>, on_save: SaveQuestion, on_close: Handler<App>) -> impl FnMut(&mut App) -> E {
    move |_| inline_question_editor("q", QuestionEditorProps { item: question_context(input.clone()), answer: answer.clone(), on_close: on_close.clone(), on_save: on_save.clone() })
}

fn q_key() -> u64 {
    hash_key("q")
}

/// The form is taller than the rig's window, and what is past its bottom takes no clicks; upstream's scrolls with the timeline.
fn question_rig() -> Rig {
    let mut rig = Rig::new();
    rig.size = (1000.0, 4000.0);
    rig
}

#[test]
fn edits_fixed_questions_options_and_answers_as_one_message() {
    let mut rig = question_rig();
    let saves: QuestionSaves = Rc::default();
    let mut view = question_view(question_input(), Some(answer_context(FORMATTED_ANSWER)), question_saver(&saves, Ok(())), Rc::new(|_| {}));
    let view: &mut dyn FnMut(&mut App) -> E = &mut view;
    rig.settle(view);
    let k = q_key();
    replace_in(&mut rig, view, question_field_key(k, "q-question", 0, 0), "Adopt which plan?");
    replace_in(&mut rig, view, question_field_key(k, "q-option-label", 0, 0), "Compatible");
    replace_in(&mut rig, view, question_field_key(k, "q-option-preview", 0, 0), "compatible()");
    replace_in(&mut rig, view, question_field_key(k, "q-answer", 0, 0), "Compatible");
    rig.click_key(view, ("question-editor-save", k));

    let saves = saves.borrow();
    assert_eq!(saves.len(), 1);
    let (input, content) = &saves[0];
    assert_eq!(
        Value::Object(input.clone()),
        json!({
            "questions": [
                { "header": "Plan", "question": "Adopt which plan?", "multiSelect": false, "options": [{ "label": "Compatible", "description": "Compatible first", "preview": "compatible()" }, { "label": "Fast", "description": "Speed first" }] },
                { "header": "Powers", "question": "Which powers?", "multiSelect": true, "options": [{ "label": "Search", "description": "Look things up" }, { "label": "Export", "description": "Make files" }] }
            ]
        })
    );
    assert_eq!(answers_from_formatted_content(content.as_deref().unwrap(), &questions_from_input(input)), Some(vec!["Compatible".to_owned(), "Search, Export".to_owned()]));
}

#[test]
fn validates_headers_question_content_option_fields_and_completed_answers() {
    let mut rig = question_rig();
    probe::start();
    let saves: QuestionSaves = Rc::default();
    let mut view = question_view(question_input(), Some(answer_context(FORMATTED_ANSWER)), question_saver(&saves, Ok(())), Rc::new(|_| {}));
    let view: &mut dyn FnMut(&mut App) -> E = &mut view;
    rig.settle(view);
    let k = q_key();
    replace_in(&mut rig, view, question_field_key(k, "q-header", 0, 0), "1234567890123");
    replace_in(&mut rig, view, question_field_key(k, "q-question", 0, 0), " ");
    replace_in(&mut rig, view, question_field_key(k, "q-option-label", 0, 0), " ");
    replace_in(&mut rig, view, question_field_key(k, "q-option-description", 0, 1), " ");
    replace_in(&mut rig, view, question_field_key(k, "q-answer", 0, 0), " ");
    rig.click_key(view, ("question-editor-save", k));
    assert!(saves.borrow().is_empty());
    assert!(rect_of(("question-editor-invalid", k)).is_some());
}

#[test]
fn the_validation_says_each_thing_wrong_at_once() {
    use mewrk_ui_chat::inline_question_editor::{question_drafts_from_input, validate};
    let _rig = Rig::new();
    let input = object(question_input());
    let mut drafts = question_drafts_from_input(&input);
    drafts[0].header = "1234567890123".to_owned();
    drafts[0].question = String::new();
    drafts[0].options[0].label = String::new();
    drafts[0].options[1].description = String::new();
    let (errors, validated) = validate(&drafts, &[String::new(), "Search".to_owned()], true);
    assert!(validated.is_none());
    let first = &errors.questions[0];
    assert_eq!(first.header.as_deref(), Some("Header cannot exceed 12 characters"));
    assert_eq!(first.question.as_deref(), Some("Question cannot be empty"));
    assert_eq!(first.option_errors[0].label.as_deref(), Some("Option label cannot be empty"));
    assert_eq!(first.option_errors[1].description.as_deref(), Some("Option description cannot be empty"));
    assert_eq!(first.answer.as_deref(), Some("Answer cannot be empty"));
    assert_eq!(errors.questions[1].answer, None);
}

#[test]
fn allows_option_counts_to_change_without_changing_the_number_of_questions() {
    let mut rig = question_rig();
    let saves: QuestionSaves = Rc::default();
    let mut view = question_view(question_input(), None, question_saver(&saves, Ok(())), Rc::new(|_| {}));
    let view: &mut dyn FnMut(&mut App) -> E = &mut view;
    rig.settle(view);
    let k = q_key();
    rig.click_key(view, question_field_key(k, "q-add", 0, 0));
    replace_in(&mut rig, view, question_field_key(k, "q-option-label", 0, 2), "Balanced");
    replace_in(&mut rig, view, question_field_key(k, "q-option-description", 0, 2), "Both");
    rig.click_key(view, ("question-editor-save", k));
    let saves = saves.borrow();
    let questions = questions_from_input(&saves[0].0);
    assert_eq!(questions.len(), 2);
    assert_eq!(questions[0].options.iter().map(|option| option.label.as_str()).collect::<Vec<_>>(), ["Safe", "Fast", "Balanced"]);
}

#[test]
fn keeps_an_unparseable_completed_answer_editable_instead_of_discarding_it() {
    let mut rig = question_rig();
    let saves: QuestionSaves = Rc::default();
    let one = json!({ "questions": [question_input()["questions"][0].clone()] });
    let mut view = question_view(one, Some(answer_context("an old free-text answer")), question_saver(&saves, Ok(())), Rc::new(|_| {}));
    let view: &mut dyn FnMut(&mut App) -> E = &mut view;
    rig.settle(view);
    rig.click_key(view, ("question-editor-save", q_key()));
    assert!(saves.borrow()[0].1.as_deref().unwrap().contains("an old free-text answer"));
}

#[test]
fn locks_the_form_during_an_async_save_and_displays_save_failures() {
    let mut rig = question_rig();
    let done: Rc<RefCell<Option<ToolDone>>> = Rc::default();
    let waiting = done.clone();
    let on_save: SaveQuestion = Rc::new(move |_, _, _, finish| *waiting.borrow_mut() = Some(finish));
    let closed = Rc::new(RefCell::new(0));
    let heard = closed.clone();
    let mut view = question_view(question_input(), None, on_save, Rc::new(move |_| *heard.borrow_mut() += 1));
    let view: &mut dyn FnMut(&mut App) -> E = &mut view;
    rig.settle(view);
    let k = q_key();
    rig.click_key(view, ("question-editor-save", k));
    assert!(done.borrow().is_some());
    // Locked: Cancel does nothing, and neither does Save.
    rig.click_key(view, ("question-editor-cancel", k));
    assert_eq!(*closed.borrow(), 0);

    let finish = done.borrow_mut().take().unwrap();
    finish(&mut rig.app, Err("Write conflict".to_owned()));
    rig.settle(view);
    assert!(rect_of(("question-editor-failed", k)).is_some());
    rig.click_key(view, ("question-editor-cancel", k));
    assert_eq!(*closed.borrow(), 1);
}

#[test]
fn escape_closes_the_form_from_a_field() {
    let mut rig = question_rig();
    let saves: QuestionSaves = Rc::default();
    let closed = Rc::new(RefCell::new(0));
    let heard = closed.clone();
    let mut view = question_view(question_input(), None, question_saver(&saves, Ok(())), Rc::new(move |_| *heard.borrow_mut() += 1));
    let view: &mut dyn FnMut(&mut App) -> E = &mut view;
    rig.settle(view);
    let r = field_rect(question_field_key(q_key(), "q-header", 0, 0));
    rig.click(view, r.x + 2.0, r.y + 2.0);
    rig.key(view, Key::Escape, false, false);
    assert_eq!(*closed.borrow(), 1);
}

#[test]
fn a_form_with_no_questions_says_so_and_offers_no_save() {
    let mut rig = question_rig();
    let saves: QuestionSaves = Rc::default();
    let mut view = question_view(json!({}), None, question_saver(&saves, Ok(())), Rc::new(|_| {}));
    let view: &mut dyn FnMut(&mut App) -> E = &mut view;
    rig.settle(view);
    let k = q_key();
    assert!(rect_of(("question-editor-empty", k)).is_some());
    rig.click_key(view, ("question-editor-save", k));
    assert!(saves.borrow().is_empty());
}

#[test]
fn the_card_says_where_a_question_stands() {
    use mewrk_ui_chat::orchestration::QuestionOutcome;
    let _rig = Rig::new();
    assert_eq!(question_status(QuestionOutcome::Legacy, true), "Answered");
    assert_eq!(question_status(QuestionOutcome::Legacy, false), "Unanswered");
    assert_eq!(question_status(QuestionOutcome::Answered, false), "Answered");
    assert_eq!(question_status(QuestionOutcome::Closed, false), "Closed");
    assert_eq!(question_status(QuestionOutcome::Declined, false), "Wanted to chat first");
    assert_eq!(question_status(QuestionOutcome::Failed, false), "Failed");
}

#[test]
fn a_right_click_on_the_card_inserts_before_or_after_it_by_the_half_it_landed_in() {
    let mut rig = Rig::new();
    let context = question_context(question_input());
    let inserts: Rc<RefCell<Vec<usize>>> = Rc::default();
    let heard = inserts.clone();
    let on_open: Rc<dyn Fn(&mut App, PointerEvent, usize)> = Rc::new(move |_, _, at| heard.borrow_mut().push(at));
    let mut view = move |_: &mut App| {
        let item = Item::settled(&context);
        question_timeline_card(QuestionCardProps {
            item: item.tool().unwrap(),
            index: 3,
            answer: None,
            answer_index: Some(7),
            read_only: false,
            editing: false,
            selected: false,
            on_edit: None,
            on_delete: Some(Rc::new(|_| {})),
            on_cancel_edit: None,
            on_save_question: None,
            on_open_insert: Some(on_open.clone()),
        })
    };
    let view: &mut dyn FnMut(&mut App) -> E = &mut view;
    rig.settle(view);
    let r = rect_of(("question-card", "ask-1")).unwrap();
    rig.right_click(view, r.x + 8.0, r.y + 4.0);
    rig.right_click(view, r.x + 8.0, r.bottom() - 4.0);
    assert_eq!(*inserts.borrow(), [3, 8]);
}

#[test]
fn a_read_only_card_offers_no_actions_and_opens_no_menu() {
    let mut rig = Rig::new();
    let context = question_context(question_input());
    let inserts: Rc<RefCell<Vec<usize>>> = Rc::default();
    let heard = inserts.clone();
    let on_open: Rc<dyn Fn(&mut App, PointerEvent, usize)> = Rc::new(move |_, _, at| heard.borrow_mut().push(at));
    let mut view = move |_: &mut App| {
        let item = Item::settled(&context);
        question_timeline_card(QuestionCardProps { item: item.tool().unwrap(), index: 0, answer: None, answer_index: None, read_only: true, editing: false, selected: true, on_edit: None, on_delete: None, on_cancel_edit: None, on_save_question: None, on_open_insert: Some(on_open.clone()) })
    };
    let view: &mut dyn FnMut(&mut App) -> E = &mut view;
    rig.settle(view);
    let r = rect_of(("question-card", "ask-1")).unwrap();
    assert!(rect_of(("question-actions", "ask-1")).is_none());
    rig.right_click(view, r.x + 8.0, r.y + 4.0);
    assert!(inserts.borrow().is_empty());
}

#[test]
fn editing_a_card_in_place_swaps_its_body_for_the_form_and_takes_away_its_actions() {
    let mut rig = Rig::new();
    let context = question_context(question_input());
    let saves: QuestionSaves = Rc::default();
    let on_save = question_saver(&saves, Ok(()));
    let mut view = move |_: &mut App| {
        let item = Item::settled(&context);
        question_timeline_card(QuestionCardProps { item: item.tool().unwrap(), index: 0, answer: None, answer_index: None, read_only: false, editing: true, selected: false, on_edit: Some(Rc::new(|_| {})), on_delete: Some(Rc::new(|_| {})), on_cancel_edit: Some(Rc::new(|_| {})), on_save_question: Some(on_save.clone()), on_open_insert: None })
    };
    rig.settle(&mut view);
    assert!(rect_of(("question-actions", "ask-1")).is_none());
    assert!(rect_of(("question-editor", hash_key(("question-editor", "ask-1")))).is_some());
}

// ---------------------------------------------------------------- the image strip

#[test]
fn a_tile_says_its_picture_and_size_and_a_file_its_type() {
    assert_eq!(image_title(&ImageAttachment { width: 1280, height: 720, ..image("a", "screen.png") }), "screen.png · 1280 × 720");
    assert_eq!(image_title(&ImageAttachment { width: 0, height: 0, ..image("a", "screen.png") }), "screen.png");
    assert_eq!(file_type_label(&file("f", "paper.pdf", FileAttachmentFormat::Pdf)), "PDF");
    assert_eq!(file_type_label(&file("f", "table.csv", FileAttachmentFormat::Text)), "CSV");
    assert_eq!(file_type_label(&file("f", "Makefile", FileAttachmentFormat::Text)), "TXT");
}

#[test]
fn a_picture_the_host_cannot_find_fails_on_its_own_and_can_be_retried() {
    let mut rig = Rig::new();
    let missing = image("0000000000000000000000000000000000000000000000000000000000000000", "gone.png");
    let shown = missing.clone();
    let mut view = move |_: &mut App| image_strip_with("s", ImageStripProps { images: std::slice::from_ref(&shown), files: &[], compact: false, busy: false, on_remove: None, on_remove_file: None }).unwrap_or_else(div);
    let view: &mut dyn FnMut(&mut App) -> E = &mut view;
    let strip = hash_key("s");
    for _ in 0..500 {
        rig.settle(view);
        if thumbnail_state(&mut rig.app, &missing) == ThumbnailState::Failed {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(thumbnail_state(&mut rig.app, &missing), ThumbnailState::Failed);
    assert!(rect_of(thumbnail_key(strip, 0)).is_some(), "the strip stays, with its one chip failed");
    rig.click_key(view, thumbnail_key(strip, 0));
    // The retry forgot the failure, so the chip is asking again, or has already been told again.
    let after = thumbnail_state(&mut rig.app, &missing);
    assert!(after == ThumbnailState::Loading || after == ThumbnailState::Failed);
    let _ = viewer_close_key;
}

#[test]
fn a_strip_with_nothing_to_show_is_nothing_and_a_busy_one_holds_a_place() {
    let mut rig = Rig::new();
    assert!(image_strip_with("s", ImageStripProps { images: &[], files: &[], compact: false, busy: false, on_remove: None, on_remove_file: None }).is_none());
    let mut view = |_: &mut App| image_strip_with("s", ImageStripProps { images: &[], files: &[], compact: true, busy: true, on_remove: None, on_remove_file: None }).unwrap_or_else(div);
    rig.settle(&mut view);
    assert!(rect_of(("image-strip-pending", hash_key("s"))).is_some());
}

#[test]
fn a_file_tile_opens_its_preview_where_a_timeline_draws_the_layer() {
    let mut rig = Rig::new();
    let attached = file("f".repeat(64).as_str(), "table.csv", FileAttachmentFormat::Text);
    let shown = attached.clone();
    let opened = Rc::new(RefCell::new(false));
    let seen = opened.clone();
    let mut view = move |app: &mut App| {
        let strip = image_strip_with("s", ImageStripProps { images: &[], files: std::slice::from_ref(&shown), compact: false, busy: false, on_remove: None, on_remove_file: None }).unwrap_or_else(div);
        let layer = attachment_preview_layer(app, false);
        *seen.borrow_mut() = layer.is_some();
        col().child(strip).child(layer)
    };
    let view: &mut dyn FnMut(&mut App) -> E = &mut view;
    rig.settle(view);
    assert!(!*opened.borrow());
    rig.click_key(view, file_tile_key(hash_key("s"), &attached.id));
    assert!(*opened.borrow());
}

// ---------------------------------------------------------------- pictures

/// Draws the leaves through the real GPU path to PNGs, for looking at:
/// `MEWRK_SHOT_DIR=/some/dir cargo test -p mewrk-ui-chat --test suite leaves::shots -- --ignored`.
mod shots {
    use std::sync::Arc;

    use mewrk_ui_chat::tool_view::get_tool_presentation;
    use std::time::Duration;

    use fluent::gpu::SURFACE_FORMAT;
    use fluent::ui::{Input, NoClipboard, UiMemory};
    use fluent::{Gpu, Renderer, Scene, TextCtx, Theme, Ui};

    use super::*;

    struct Shot {
        gpu: Arc<Gpu>,
        renderer: Renderer,
        target: wgpu::Texture,
        scene: Scene,
        text: TextCtx,
        mem: UiMemory,
        input: Input,
        app: App,
        theme: Theme,
        size: (u32, u32),
        scale: f32,
        _data: crate::rig::TempDir,
    }

    impl Shot {
        fn new(width: f32, height: f32, scale: f32, theme: Theme) -> Self {
            let data = crate::rig::TempDir::new("shot");
            let app = crate::rig::start_app(&data.0);
            let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
            let gpu = Arc::new(Gpu::new(instance, None).expect("a GPU"));
            let renderer = Renderer::new(gpu.clone());
            let size = ((width * scale) as u32, (height * scale) as u32);
            let target = gpu.device.create_texture(&wgpu::TextureDescriptor { label: Some("shot"), size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 }, mip_level_count: 1, sample_count: 1, dimension: wgpu::TextureDimension::D2, format: SURFACE_FORMAT, usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC, view_formats: &[] });
            Self { gpu, renderer, target, scene: Scene::default(), text: TextCtx::new(scale), mem: UiMemory::default(), input: Input::default(), app, theme, size, scale, _data: data }
        }

        fn frame(&mut self, view: &mut dyn FnMut(&mut App) -> E, time: f32) {
            self.app.tick();
            self.scene.clear();
            let logical = (self.size.0 as f32 / self.scale, self.size.1 as f32 / self.scale);
            set_window_size(logical.0, logical.1);
            set_theme(self.theme);
            let body = view(&mut self.app);
            let root = col::<App>().size(logical.0, logical.1).bg(self.theme.layer).p(12.0).child(body);
            let mut clipboard = NoClipboard;
            let mut ui = Ui::new(&mut self.scene, &mut self.text, self.theme, &self.input, &mut self.mem, &mut clipboard, self.scale, logical, 0.016, time);
            render(root, &mut self.app, &mut ui);
            self.input.end_frame();
            self.text.flush_atlases(&self.renderer);
            let target = self.target.create_view(&Default::default());
            let submission = self.renderer.render(&self.scene, &self.theme, &target, self.size);
            let _ = self.gpu.device.poll(wgpu::PollType::Wait { submission_index: Some(submission), timeout: Some(Duration::from_secs(5)) });
        }

        fn save(&mut self, name: &str) {
            let dir = std::env::var("MEWRK_SHOT_DIR").expect("MEWRK_SHOT_DIR");
            let (w, h) = self.size;
            let device = &self.gpu.device;
            let bytes_per_row = (w * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
            let buffer = device.create_buffer(&wgpu::BufferDescriptor { label: None, size: (bytes_per_row * h) as u64, usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
            let mut encoder = device.create_command_encoder(&Default::default());
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo { texture: &self.target, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                wgpu::TexelCopyBufferInfo { buffer: &buffer, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(bytes_per_row), rows_per_image: None } },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
            self.gpu.queue.submit([encoder.finish()]);
            buffer.slice(..).map_async(wgpu::MapMode::Read, |_| {});
            let _ = device.poll(wgpu::PollType::Wait { submission_index: None, timeout: Some(Duration::from_secs(5)) });
            let data = buffer.slice(..).get_mapped_range().expect("mapped");
            let mut picture = image::RgbaImage::new(w, h);
            for y in 0..h {
                for x in 0..w {
                    let at = (y * bytes_per_row + x * 4) as usize;
                    picture.put_pixel(x, y, image::Rgba([data[at + 2], data[at + 1], data[at], 255]));
                }
            }
            picture.save(format!("{dir}/{name}.png")).expect("save");
        }

        fn draw(&mut self, name: &str, view: &mut dyn FnMut(&mut App) -> E) {
            for frame in 0..6 {
                self.frame(view, frame as f32 * 0.016);
            }
            self.save(name);
        }
    }

    fn thinking() -> LiveReasoningView {
        LiveReasoningView { started_at: mewrk_client::runs::util::iso_from_ms(mewrk_client::runs::util::now_ms() - 84_000), tokens: 1_240 }
    }

    fn waiting_props(tasks: usize) -> WaitingProps {
        WaitingProps { tools: Rc::new(Vec::new()), thinking: Some(thinking()), retry_notice: None, running_task_count: tasks, on_open_tasks: Some(Rc::new(|_| {})) }
    }

    fn running_call() -> ContextItem {
        tool("running", "read", json!({ "path": "/Users/someone/work/project/src/components/StreamWaitingIndicator.tsx" }), result("", None))
    }

    #[test]
    #[ignore = "draws to MEWRK_SHOT_DIR, for a person to look at"]
    fn draw_a_drag() {
        use mewrk_ui_chat::conversation_view;
        use mewrk_ui_chat::frame::unit_key;
        use mewrk_ui_chat::props::ConversationViewProps;
        let read = |id: &str| crate::fixture::live(crate::fixture::tool(id, "read", serde_json::json!({ "path": format!("src/{id}.rs") }), "x"));
        let contexts = vec![crate::fixture::live(crate::fixture::assistant("top", "A reply above the block.")), read("r1"), read("r2"), read("r3"), read("r4"), read("r5"), crate::fixture::live(crate::fixture::user("next", "A message below it."))];
        let props = ConversationViewProps {
            contexts: Rc::new(contexts),
            timeline_id: Some("shot-drag".to_owned()),
            on_delete_contexts: Some(Rc::new(|_, _| {})),
            on_move_contexts: Some(Rc::new(|_, _, _| {})),
            ..ConversationViewProps::default()
        };
        let mut view = |app: &mut App| col().w_full().h(496.0).child(conversation_view(app, props.clone()));
        let mut shot = Shot::new(760.0, 520.0, 2.0, Theme::light());
        let mut time = 0.0;
        let mut step = |shot: &mut Shot, view: &mut dyn FnMut(&mut App) -> E, frames: usize| {
            for _ in 0..frames {
                time += 0.016;
                shot.frame(view, time);
            }
        };
        step(&mut shot, &mut view, 6);
        for id in ["r2", "r4"] {
            let r = rect_of(unit_key(0, id)).expect("built");
            shot.input.zoom_modifier = true;
            shot.input.pointer = Some((r.x + 10.0, r.y + 3.0));
            step(&mut shot, &mut view, 1);
            shot.input.pressed = true;
            shot.input.down = true;
            step(&mut shot, &mut view, 1);
            shot.input.down = false;
            shot.input.released = true;
            step(&mut shot, &mut view, 2);
            shot.input.zoom_modifier = false;
        }
        let r = rect_of(unit_key(0, "r2")).expect("built");
        let at = (r.x + 40.0, r.y + r.h * 0.5);
        shot.input.pointer = Some(at);
        step(&mut shot, &mut view, 1);
        shot.input.pressed = true;
        shot.input.down = true;
        step(&mut shot, &mut view, 1);
        let wait = |shot: &mut Shot, view: &mut dyn FnMut(&mut App) -> E, step: &mut dyn FnMut(&mut Shot, &mut dyn FnMut(&mut App) -> E, usize)| {
            std::thread::sleep(mewrk_ui_chat::drag::MOTION + Duration::from_millis(30));
            step(shot, view, 3);
        };
        shot.input.pointer = Some((at.0 + 90.0, 380.0));
        step(&mut shot, &mut view, 2);
        wait(&mut shot, &mut view, &mut step);
        shot.save("drag-gathered");
        shot.input.pointer = Some((at.0, at.1 - 4.0));
        step(&mut shot, &mut view, 2);
        wait(&mut shot, &mut view, &mut step);
        shot.save("drag-docked-in-a-block");
        shot.input.pointer = Some((at.0 + 90.0, 380.0));
        step(&mut shot, &mut view, 2);
        wait(&mut shot, &mut view, &mut step);
        shot.input.down = false;
        shot.input.released = true;
        step(&mut shot, &mut view, 2);
        shot.save("drag-landing");
    }

    /// One card of each family and state, in the two columns a wide window would give them.
    #[test]
    #[ignore = "draws to MEWRK_SHOT_DIR, for a person to look at"]
    fn draw_tool_cards() {
        use mewrk_ui_chat::tool_detail::tool_detail_renderer;
        let call = |name: &str, input: Value, output: &str, success: bool, images: Value| -> ContextItem {
            serde_json::from_value(json!({ "id": format!("c-{name}-{success}"), "kind": "tool", "toolName": name, "round": 1, "input": input, "result": { "success": success, "output": output, "executedAt": AT, "durationMs": 12, "images": images }, "createdAt": AT })).expect("a tool card")
        };
        let ok = |name: &str, input: Value, output: &str| call(name, input, output, true, json!([]));
        let long = "src/components/a/very/deeply/nested/directory/that/goes/on/for/quite/a/while/file.rs:12:    let answer = compute_something_that_takes_many_arguments(first, second, third, fourth, fifth, sixth);";
        let left = vec![
            ok("ls", json!({ "path": "src" }), "main.rs\nlib.rs\nbin/\ntarget/ (ignored)\n(Showing 4 of 14)\n"),
            ok("grep", json!({ "pattern": "needle" }), "src/a.rs\nsrc/b.rs\n… 3 more\n"),
            ok("grep", json!({ "pattern": "needle", "output_mode": "content", "context": 1 }), &format!("{long}\nsrc/a.rs-13-context line\n--\nsrc/b.rs:4:another\n")),
            ok("grep", json!({ "pattern": "needle", "output_mode": "count" }), "src/a.rs:3\nsrc/b.rs:1\nTotal: 4\n"),
            ok("read", json!({ "path": "a.rs", "start_line": 40 }), "fn a() {}\n\nfn b() {}\n… 12 more lines\n"),
            call("read", json!({ "path": "gone.rs" }), "No such file", false, json!([])),
            ok("preview_console_logs", json!({ "level": "error" }), "[error] boom\n[warn] careful\n[log] fine\n(3 of 9)\n"),
            ok("preview_network", json!({}), "[1] GET https://example.com/a → 200 OK\n[2] GET https://example.com/b [FAILED: refused]\n"),
            ok("preview_snapshot", json!({}), "- heading \"Title\"\n  - button \"Go\" [uid=4]\n"),
        ];
        let right = vec![
            ok("preview_inspect", json!({ "selector": "#go", "styles": ["color"] }), r#"{"tag":"button","rect":{"x":1,"y":2}}"#),
            ok("preview_eval", json!({ "expression": "1 + 1" }), "2"),
            ok("preview_upload_image", json!({ "selector": "#file" }), r#"{"page":{"url":"https://example.com","title":"Example","loading":false},"notices":["uploaded"],"newConsoleErrors":["boom"],"snapshot":{"tree":"- button"},"own":"value"}"#),
            ok("web_search", json!({ "query": "rust" }), r#"{"findings":"Prints to stdout.","sources":[{"url":"https://doc.rust-lang.org/std","title":"std"},{"url":"https://www.example.org/x"}]}"#),
            ok("web_search", json!({ "query": "rust" }), r#"{"results":[{"title":"The book","url":"https://doc.rust-lang.org/book","content":"Chapter one."},{"url":"https://example.org/x","content":""}]}"#),
            ok("task_wait", json!({}), "Timed out once.\n\n[scout · completed]\nFound it.\n\nCurrent status: 1 running\n"),
            ok("read_global_memory", json!({ "name": "notes" }), "# Notes\nRemember this."),
            ok("mcp__docs_0123456789__look_up__abc", json!({ "term": "macro", "limit": 3 }), "A macro."),
            ok("subagent_update", json!({ "message": "Halfway through." }), "ok"),
            ok("plan", json!({ "action": "read" }), "# The plan"),
        ];
        for (suffix, theme_value) in [("light", Theme::light()), ("dark", Theme::dark())] {
            let mut shot = Shot::new(1240.0, 1500.0, 1.5, theme_value);
            let cards = |column: Vec<ContextItem>| -> Vec<E> {
                column.iter().enumerate().map(|(at, item)| {
                    let tool = Item::settled(item).tool().expect("a tool card");
                    let id = format!("{}-{at}", item.id());
                    col().min_w(0.0).child(text(get_tool_presentation(&tool, None, None, None).title).ty(fluent::theme::FINE).color(theme().text_secondary).mb(2.0)).child(tool_detail_renderer(&id, &tool, None))
                }).collect()
            };
            let (a, b) = (left.clone(), right.clone());
            let mut view = move |_: &mut App| -> E { row().gap(16.0).items_start().child(col().flex1().min_w(0.0).gap(8.0).children(cards(a.clone()))).child(col().flex1().min_w(0.0).gap(8.0).children(cards(b.clone()))) };
            shot.draw(&format!("tool-cards-{suffix}"), &mut view);
        }
    }

    #[test]
    #[ignore = "draws to MEWRK_SHOT_DIR, for a person to look at"]
    fn draw_them() {
        // One cat, large, in each mood: what the rasteriser makes of the drawing, poses and all.
        for (name, mood) in [("cat-work", CatMood::Work), ("cat-slack", CatMood::Slack)] {
            let mut large = Shot::new(190.0, 160.0, 4.0, Theme::light());
            let (gaze, lids) = mood_pose(mood);
            let pose = CatPose::at(mood, 0.0, gaze, lids);
            let mut view = move |_: &mut App| {
                let t_ = theme();
                custom(move |_: &mut App, ui: &mut Ui<'_>, r| mewrk_ui_chat::cat_art::paint_cat(ui, r, &pose, t_.text_primary, t_.control_strong_stroke)).size(160.0 * 724.0 / 610.0, 130.0 * 1.0).min_h(130.0)
            };
            large.draw(name, &mut view);
        }

        for (suffix, theme_value) in [("light", Theme::light()), ("dark", Theme::dark())] {
            let mut shot = Shot::new(760.0, 420.0, 2.0, theme_value);

            let call = running_call();
            let streaming = Item { item: &call, streaming: true, stream_status: Some(mewrk_client::runs::StreamingToolPhase::Running), live: None, started_at: None };
            let items = vec![streaming];
            let notice = ModelRunRetryState { round: 1, attempt: 2, max_attempts: 5, message: "The stream was reset by the server before the reply finished".to_owned(), dirty: true };
            let changes = summary_of(&[change("own", "src/components/App.tsx", 12, 3), change("en", "website/content/en/working.md", 1, 0), change("zh", "website/content/zh-CN/working.md", 4, 4), change("d", "notes/new.md", 7, 0), change("e", "src/lib.rs", 2, 1)], "turn").unwrap();
            let mut view = |_: &mut App| {
                col()
                    .gap(10.0)
                    .child(stream_waiting_indicator("alone", &[], WaitingProps { tools: Rc::new(Vec::new()), thinking: None, retry_notice: None, running_task_count: 0, on_open_tasks: None }))
                    .child(stream_waiting_indicator("thinking", &items, waiting_props(2)))
                    .child(stream_waiting_indicator("retry", &[], WaitingProps { retry_notice: Some(notice.clone()), ..waiting_props(0) }))
                    .child(turn_changes(&changes, Some("/work/repo")))
            };
            shot.draw(&format!("waiting-{suffix}"), &mut view);

            let steps = vec![step("a", WorkflowRunStepState::Finished), step("b", WorkflowRunStepState::Running), step("c", WorkflowRunStepState::Pending), step("d", WorkflowRunStepState::Failed), WorkflowRunStep { cached: true, ..step("e", WorkflowRunStepState::Finished) }];
            let run = WorkflowRunView { id: "w".into(), run_id: None, name: "review".into(), script_name: None, description: "Reads every changed file and writes up what it found.".into(), state: TaskItemState::Running, step_count: 5, elapsed_ms: Some(84_000), tokens: Some(12_400), phases: vec![WorkflowRunPhase { key: "p".into(), heading: Some("Read".into()), steps: steps.clone(), done: 1, total: 5 }, WorkflowRunPhase { key: "q".into(), heading: None, steps: steps[..1].to_vec(), done: 1, total: 1 }], steps, logs: Vec::new(), running: true };
            let question = question_context(question_input());
            let answer = answer_context(FORMATTED_ANSWER);
            let plan_document = plan(PlanStatus::Draft);
            let mut view = |_: &mut App| {
                let item = Item::settled(&question);
                col()
                    .gap(10.0)
                    .child(workflow_run_detail("w", &run))
                    .child(question_timeline_card(QuestionCardProps { item: item.tool().unwrap(), index: 0, answer: Some(&answer), answer_index: Some(1), read_only: false, editing: false, selected: false, on_edit: None, on_delete: None, on_cancel_edit: None, on_save_question: None, on_open_insert: None }))
                    .child(div().h(90.0).child(plan_pane("p", Some(&plan_document), true, None)))
            };
            shot.draw(&format!("cards-{suffix}"), &mut view);

            let mut text_shot = Shot::new(760.0, 420.0, 2.0, theme_value);
            let write = write_call();
            let descriptor = text_shot.app.doc.tools.iter().find(|tool| tool.name == "write").cloned();
            let (images, files) = (vec![image("a", "screen.png")], vec![file("f", "notes.md", FileAttachmentFormat::Text), file("p", "paper.pdf", FileAttachmentFormat::Pdf)]);
            let add: AddAttachments = Rc::new(|_, _, _, _| {});
            let mut view = |_: &mut App| {
                col()
                    .gap(14.0)
                    .child(inline_text_editor("text", TextEditorProps { kind: InsertKind::User, content: "Please look at this\nand tell me what is wrong.".to_owned(), images: images.clone(), files: files.clone(), show_kind: true, image_input: Some(true), on_cancel: Rc::new(|_| {}), on_save: Rc::new(|_, _, _, _| {}), on_add_attachments: Some(add.clone()) }))
                    .child(inline_tool_editor("tool", ToolEditorProps { item: write.clone(), descriptor: descriptor.clone(), inserting: false, on_cancel: Rc::new(|_| {}), on_run: None, on_save: Some(Rc::new(|_, _, _, _, _| {})) }))
            };
            text_shot.draw(&format!("editors-{suffix}"), &mut view);

            let mut form = Shot::new(760.0, 1100.0, 1.5, theme_value);
            let question = question_context(question_input());
            let answer = answer_context(FORMATTED_ANSWER);
            let mut view = |_: &mut App| inline_question_editor("q", QuestionEditorProps { item: question.clone(), answer: Some(answer.clone()), on_close: Rc::new(|_| {}), on_save: Rc::new(|_, _, _, _| {}) });
            form.draw(&format!("question-form-{suffix}"), &mut view);
        }
    }
}

#[test]
fn a_plan_page_follows_the_inline_math_setting() {
    let mut rig = Rig::new();
    let document = ConversationPlan { markdown: "The ratio is $\\frac{\\frac{a}{b}}{\\frac{c}{d}}$ here.".to_owned(), ..plan(PlanStatus::Draft) };
    let height = |rig: &mut Rig, single_dollar: bool| {
        let shown = document.clone();
        let mut view = move |_: &mut App| col().w_full().h_full().child(plan_pane("p", Some(&shown), single_dollar, None));
        rig.settle(&mut view);
        mewrk_ui_common::markdown::probe::height(("plan-markdown", hash_key("p")))
    };
    let as_math = height(&mut rig, true);
    let as_text = height(&mut rig, false);
    assert!(as_math > as_text, "a fraction stands taller than a line of its source: {as_math} {as_text}");
}

// ---------------------------------------------------------------- what a reader gets

mod a11y {
    use std::sync::{Arc, Mutex};

    use mewrk_client::model::{ToolCategory, ToolParameter};

    use super::*;
    use super::image;
    use crate::rig::{id_of, nameless, said, says, Rig};

    /// A real PNG in the host's store, so a chip gets as far as being ready.
    fn stored_image(rig: &mut Rig) -> ImageAttachment {
        const PIXEL: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=";
        let answer = Arc::new(Mutex::new(None));
        let held = answer.clone();
        rig.app.invoke::<ImageAttachment>("image_attachment_upload", json!({ "name": "pixel.png", "data": PIXEL }), move |_, result| *held.lock().unwrap() = Some(result));
        for _ in 0..1000 {
            rig.app.tick();
            if let Some(result) = answer.lock().unwrap().take() {
                return result.expect("the host took the picture");
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("the host never answered");
    }

    fn strip_view(images: Vec<ImageAttachment>, files: Vec<FileAttachment>, removable: bool) -> impl FnMut(&mut App) -> E {
        move |_| {
            let remove: Option<Rc<dyn Fn(&mut App, &str)>> = removable.then(|| Rc::new(|_: &mut App, _: &str| {}) as Rc<dyn Fn(&mut App, &str)>);
            image_strip_with("s", ImageStripProps { images: &images, files: &files, compact: false, busy: false, on_remove: remove.clone(), on_remove_file: remove }).unwrap_or_else(div)
        }
    }

    #[test]
    fn a_chip_says_what_pressing_it_would_do_at_each_stage_and_the_list_counts_them() {
        let mut rig = Rig::new();
        let missing = image("0000000000000000000000000000000000000000000000000000000000000000", "gone.png");
        let mut view = strip_view(vec![missing.clone()], Vec::new(), true);
        let view: &mut dyn FnMut(&mut App) -> E = &mut view;
        set_a11y_active(true);
        for _ in 0..500 {
            rig.settle(view);
            if thumbnail_state(&mut rig.app, &missing) == ThumbnailState::Failed {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // What the failure announced went by while the frames ran; the chip stands as failed.
        let tree = rig.a11y_tree(view);
        let chip = said(&tree, "Retry loading image gone.png");
        assert_eq!(chip.role(), Role::Button);
        assert_eq!(chip.description(), Some("gone.png failed to load"));
        assert!(chip.supports_action(accesskit::Action::Click));
        assert_eq!(said(&tree, "1 images").role(), Role::List);
        assert_eq!(said(&tree, "Remove image gone.png").role(), Role::Button);
        assert_eq!(nameless(&tree), Vec::<String>::new());

        // A retry that fails again says so again, as a live region a reader hears without going to it.
        let id = id_of(&tree, "Retry loading image gone.png");
        a11y_action(accesskit::ActionRequest { action: accesskit::Action::Click, target_tree: accesskit::TreeId::ROOT, target_node: id, data: None });
        let mut heard = false;
        for _ in 0..500 {
            rig.frame(view);
            if let Some(update) = take_a11y_update() {
                heard |= update.nodes.iter().any(|(_, node)| node.live() == Some(accesskit::Live::Polite) && node.value() == Some("gone.png failed to load"));
            }
            if heard {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(heard);
        set_a11y_active(false);
    }

    #[test]
    fn the_viewer_is_a_named_dialog_that_takes_the_focus_and_gives_it_back() {
        let mut rig = Rig::new();
        let stored = stored_image(&mut rig);
        let mut view = strip_view(vec![stored.clone()], Vec::new(), false);
        let view: &mut dyn FnMut(&mut App) -> E = &mut view;
        set_a11y_active(true);
        for _ in 0..500 {
            rig.settle(view);
            if thumbnail_state(&mut rig.app, &stored) == ThumbnailState::Ready {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let tree = rig.a11y_tree(view);
        let open = format!("View full image {}", stored.name);
        assert_eq!(said(&tree, &open).description(), Some(format!("{} loaded", stored.name).as_str()));
        rig.a11y_press(view, id_of(&tree, &open));

        let tree = rig.a11y_tree(view);
        let dialog = said(&tree, &open);
        assert_eq!(dialog.role(), Role::Dialog);
        assert!(dialog.is_modal());
        assert_eq!(said(&tree, &format!("Full image {}", stored.name)).role(), Role::Image);
        assert_eq!(said(&tree, &format!("Close full image {}", stored.name)).role(), Role::Button);
        assert_eq!(said(&tree, &stored.name).role(), Role::Label);
        assert!(says(&tree, "1 × 1").is_none(), "the size is `aria-hidden`");
        assert_eq!(focused_id(), hash_key(viewer_close_key(hash_key("s"))));

        rig.key(view, Key::Escape, false, false);
        rig.settle(view);
        assert_ne!(focused_id(), hash_key(viewer_close_key(hash_key("s"))), "the focus went back to the chip");
        assert_ne!(focused_id(), 0);
        set_a11y_active(false);
    }

    #[test]
    fn a_file_tile_is_named_as_a_preview_and_its_remove_as_a_removal() {
        let mut rig = Rig::new();
        let attached = file("f", "table.csv", FileAttachmentFormat::Text);
        let mut view = strip_view(Vec::new(), vec![attached], true);
        let tree = rig.a11y_tree(&mut view);
        assert_eq!(said(&tree, "Preview file table.csv").role(), Role::Button);
        assert_eq!(said(&tree, "Remove file table.csv").role(), Role::Button);
        assert_eq!(said(&tree, "1 attachments").role(), Role::List);
        assert_eq!(tree.iter().filter(|(_, n)| n.role() == Role::ListItem).count(), 1);
        assert_eq!(nameless(&tree), Vec::<String>::new());
        set_a11y_active(false);
    }

    /// `.image-strip__file`: the icon in its corner and the words at the far end, not an icon in the middle over words right under it.
    #[test]
    fn a_stacked_file_tile_keeps_its_icon_left_and_its_words_at_the_bottom() {
        let mut rig = Rig::new();
        let attached = file("f", "table.csv", FileAttachmentFormat::Text);
        let mut view = strip_view(Vec::new(), vec![attached.clone()], false);
        rig.settle(&mut view);
        let tile = rect_of(file_tile_key(hash_key("s"), &attached.id)).unwrap();
        assert_eq!((tile.w, tile.h), (184.0, 88.0));
        let glyphs: Vec<_> = rig.scene.layers.iter().flat_map(|layer| layer.glyphs.iter()).collect();
        let top = glyphs.iter().min_by(|a, b| a.pos[1].total_cmp(&b.pos[1])).expect("something drawn");
        assert!(top.pos[0] < tile.x + 12.0 + 20.0, "the icon stands at the left: {:?} in {tile:?}", top.pos);
        let bottom = glyphs.iter().map(|glyph| glyph.pos[1] + glyph.size[1] as f32).fold(0.0, f32::max);
        assert!(bottom > tile.bottom() - 10.0 - 5.0, "the words end at the bottom: {bottom} in {tile:?}");
    }

    #[test]
    fn the_busy_placeholder_is_a_status_a_reader_hears() {
        let mut rig = Rig::new();
        let mut view = |_: &mut App| image_strip_with("s", ImageStripProps { images: &[], files: &[], compact: true, busy: true, on_remove: None, on_remove_file: None }).unwrap_or_else(div);
        let tree = rig.a11y_tree(&mut view);
        assert_eq!(said(&tree, "Adding attachments…").role(), Role::Status);
        assert!(said(&tree, "0 images").is_busy());
        set_a11y_active(false);
    }

    fn tool_descriptor(required: &[&str]) -> ToolDescriptor {
        let parameters: Vec<ToolParameter> = required.iter().map(|name| serde_json::from_value(json!({ "name": name, "label": format!("The {name}"), "type": "string", "required": true })).expect("a parameter")).collect();
        serde_json::from_value(json!({ "name": "find", "label": "Find", "description": "", "category": ToolCategory::Filesystem, "dangerous": false, "parameters": parameters })).expect("a descriptor")
    }

    #[test]
    fn an_arguments_field_goes_by_its_label_and_says_when_it_is_wrong() {
        let mut rig = Rig::new();
        let calls = Calls::default();
        let setup = EditorSetup { item: tool("find-call", "find", json!({}), result("ok", None)), descriptor: Some(tool_descriptor(&["query"])), inserting: false, on_run: None, on_save: Some(calls.save()), on_cancel: calls.cancel() };
        let mut view = editor_view(setup);
        let view: &mut dyn FnMut(&mut App) -> E = &mut view;
        let tree = rig.a11y_tree(view);
        let field = said(&tree, "The query *");
        assert_eq!(field.role(), Role::MultilineTextInput);
        assert!(field.invalid().is_none());
        assert_eq!(said(&tree, "Result").role(), Role::MultilineTextInput);
        assert_eq!(said(&tree, "Cancel").role(), Role::Button);
        assert_eq!(said(&tree, "Save").role(), Role::Button);
        assert_eq!(nameless(&tree), Vec::<String>::new());

        rig.click_key(view, ("tool-editor-save", editor_key()));
        let tree = rig.a11y_tree(view);
        assert!(said(&tree, "The query *").invalid().is_some());
        assert_eq!(said(&tree, "This field is required").role(), Role::Alert);
        set_a11y_active(false);
    }

    /// `useMemo(fields, [callInput, descriptor])`: a catalog that answers after the editor opened still brings its rows.
    #[test]
    fn a_descriptor_that_arrives_late_brings_its_arguments_and_they_take_typing() {
        let mut rig = Rig::new();
        let calls = Calls::default();
        let descriptor_now = Rc::new(RefCell::new(None::<ToolDescriptor>));
        let (shown, save, cancel) = (descriptor_now.clone(), calls.save(), calls.cancel());
        let mut view = move |_: &mut App| inline_tool_editor(EDITOR, ToolEditorProps { item: tool("find-call", "find", json!({}), result("ok", None)), descriptor: shown.borrow().clone(), inserting: false, on_cancel: cancel.clone(), on_run: None, on_save: Some(save.clone()) });
        let view: &mut dyn FnMut(&mut App) -> E = &mut view;
        rig.settle(view);
        let k = editor_key();
        let field_key = hash_key(("field", arg_field_key(k, "query")));
        assert!(rect_of(field_key).is_none());
        *descriptor_now.borrow_mut() = Some(tool_descriptor(&["query"]));
        rig.settle(view);
        assert!(rect_of(field_key).is_some());
        let r = rect_of(field_key).unwrap();
        rig.click(view, r.x + 4.0, r.y + 4.0);
        rig.type_text(view, "*.rs");
        rig.settle(view);
        rig.click_key(view, ("tool-editor-save", k));
        assert_eq!(calls.saved.borrow()[0].0, object(json!({ "query": "*.rs" })));
    }

    #[test]
    fn a_failed_result_is_coloured_as_upstream_colours_it() {
        let mut rig = Rig::new();
        let calls = Calls::default();
        let failed = tool("find-call", "find", json!({}), ToolResult { success: false, ..result("boom", None) });
        let setup = EditorSetup { item: failed, descriptor: None, inserting: false, on_run: None, on_save: Some(calls.save()), on_cancel: calls.cancel() };
        let mut view = editor_view(setup);
        rig.settle(&mut view);
        let critical = theme().critical;
        assert!(rig.scene.layers.iter().flat_map(|layer| layer.glyphs.iter()).any(|glyph| glyph.color == critical), "the words themselves are red");
    }

    #[test]
    fn the_question_form_names_each_field_by_its_question_and_option() {
        let mut rig = question_rig();
        let saves: QuestionSaves = Rc::default();
        let mut view = question_view(question_input(), Some(answer_context(FORMATTED_ANSWER)), question_saver(&saves, Ok(())), Rc::new(|_| {}));
        let view: &mut dyn FnMut(&mut App) -> E = &mut view;
        let tree = rig.a11y_tree(view);
        assert_eq!(said(&tree, "Edit questions and answers").role(), Role::Form);
        for (name, role) in [
            ("Question 1 header", Role::MultilineTextInput),
            ("Question 1 text", Role::MultilineTextInput),
            ("Question 2 option 2 label", Role::MultilineTextInput),
            ("Question 2 option 2 description", Role::MultilineTextInput),
            ("Question 1 option 1 preview", Role::MultilineTextInput),
            ("Answer to question 2", Role::MultilineTextInput),
            ("Allow multiple selections for question 1", Role::Switch),
            ("Add an option to question 1", Role::Button),
            ("Remove option 1 from question 2", Role::Button),
        ] {
            assert_eq!(said(&tree, name).role(), role, "{name}");
        }
        assert!(tree.iter().any(|(_, n)| n.role() == Role::Heading && n.level() == Some(3)));
        assert!(tree.iter().any(|(_, n)| n.role() == Role::Region && !n.labelled_by().is_empty()), "a question's section goes by its heading");
        assert_eq!(nameless(&tree), Vec::<String>::new());

        let k = q_key();
        replace_in(&mut rig, view, question_field_key(k, "q-header", 0, 0), "1234567890123");
        rig.click_key(view, ("question-editor-save", k));
        let tree = rig.a11y_tree(view);
        assert!(said(&tree, "Question 1 header").invalid().is_some());
        assert_eq!(said(&tree, "Fix the marked fields before saving.").role(), Role::Alert);
        set_a11y_active(false);
    }

    #[test]
    fn a_text_editor_goes_by_what_it_edits() {
        let mut rig = Rig::new();
        let mut view = |_: &mut App| inline_text_editor("t", TextEditorProps { kind: InsertKind::Assistant, content: "hi".to_owned(), images: Vec::new(), files: Vec::new(), show_kind: false, image_input: None, on_cancel: Rc::new(|_| {}), on_save: Rc::new(|_, _, _, _| {}), on_add_attachments: None });
        let tree = rig.a11y_tree(&mut view);
        let field = said(&tree, "Model reply");
        assert_eq!(field.role(), Role::MultilineTextInput);
        assert_eq!(field.value(), Some("hi"));
        assert_eq!(nameless(&tree), Vec::<String>::new());
        set_a11y_active(false);
    }

    #[test]
    fn the_waiting_line_is_a_polite_status_with_all_it_says_in_its_name() {
        let mut rig = Rig::new();
        let mut view = |_: &mut App| stream_waiting_indicator("w", &[], WaitingProps { tools: Rc::new(Vec::new()), thinking: None, retry_notice: None, running_task_count: 0, on_open_tasks: None });
        let tree = rig.a11y_tree(&mut view);
        let status = said(&tree, "Model is generating");
        assert_eq!((status.role(), status.live()), (Role::Status, Some(accesskit::Live::Polite)));
        set_a11y_active(false);
    }

    #[test]
    fn a_turns_changed_files_are_a_named_section_that_says_when_it_is_open() {
        let mut rig = Rig::new();
        let summary = mewrk_ui_chat::turn_changes::TurnChangeSummary { key: "s".to_owned(), files: vec![mewrk_ui_chat::turn_changes::TurnFileChange { key: "k".to_owned(), path: "src/a.rs".to_owned(), workspace: None, additions: 1, deletions: 2 }], additions: 1, deletions: 2, signature: String::new() };
        let mut view = move |_: &mut App| turn_changes(&summary, None);
        let view: &mut dyn FnMut(&mut App) -> E = &mut view;
        let tree = rig.a11y_tree(view);
        assert_eq!(said(&tree, "Edited 1 file").role(), Role::Region);
        let heading = tree.iter().map(|(_, n)| n).find(|n| n.role() == Role::Button && n.is_expanded() == Some(true)).expect("the fold says it is open");
        assert_eq!(heading.label(), Some("Edited 1 file +1 −2"));
        said(&tree, "a.rs +1 −2");
        assert_eq!(nameless(&tree), Vec::<String>::new());
        set_a11y_active(false);
    }

    #[test]
    fn a_workflow_detail_says_the_sentence_and_hides_the_squares() {
        let mut rig = Rig::new();
        let view_of_run = WorkflowRunView { id: "w".to_owned(), name: "Plan".to_owned(), description: String::new(), state: TaskItemState::Running, running: true, script_name: None, run_id: None, step_count: 2, elapsed_ms: None, tokens: None, steps: Vec::new(), phases: Vec::new(), logs: Vec::new() };
        let mut view = move |_: &mut App| workflow_run_detail("w", &view_of_run);
        let tree = rig.a11y_tree(&mut view);
        let section = said(&tree, "Workflow Plan: 2 agents, 0 done");
        assert_eq!(section.role(), Role::Region);
        assert!(section.is_busy());
        set_a11y_active(false);
    }
}
