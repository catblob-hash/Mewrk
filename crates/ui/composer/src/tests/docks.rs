//! `ToolApprovalDock.test.tsx`, `QueuedMessageList.test.tsx`, and `QuestionDock.test.tsx` through the frame (its logic is tested beside
//! the state machine).

use super::rig::{Rig, CONV};
use crate::approval_dock::{allow_key, always_key, deny_key, pager_key};
use crate::approval_dock::feedback_field_key;
use crate::question_dock::{advance_key, chat_key, close_key, dock_key, dock_state, other_field_key, row_key, submit_key, tab_key, OTHER};
use crate::queued::{delete_key, retry_key, steer_key};
use mewrk_client::prelude::*;
use mewrk_client::runs::{PendingToolPrompt, PromptKind};

pub(super) fn prompt(id: &str, kind: PromptKind) -> PendingToolPrompt {
    PendingToolPrompt { prompt_id: id.to_owned(), tool_name: "run".into(), kind, label: "Run a command".into(), summary: "rm -rf build".into(), risk_level: "High".into(), reason: "It deletes files".into(), requester: None, source_agent: None, source_call_id: None, allow_always_offered: true, mandatory: false, questions: None }
}

pub(super) fn with_prompts(prompts: Vec<PendingToolPrompt>) -> Rig {
    let mut rig = Rig::new();
    rig.app.runs().tool_prompts.insert(CONV.to_owned(), prompts);
    rig.frame();
    rig
}

pub(super) fn question(id: &str, questions: Value) -> PendingToolPrompt {
    PendingToolPrompt { questions: Some(questions), ..prompt(id, PromptKind::Question) }
}

#[test]
fn renders_nothing_when_no_call_is_waiting() {
    let mut rig = Rig::new();
    assert!(!rig.says("Your approval is needed"));
}

#[test]
fn describes_the_call_and_offers_deny_always_allow_and_allow_in_that_order() {
    let mut rig = with_prompts(vec![prompt("p1", PromptKind::Tool)]);
    let words = rig.words();
    for word in ["Your approval is needed", "Run a command", "High risk", "rm -rf build", "It deletes files"] {
        assert!(words.iter().any(|said| said == word), "{word} in {words:?}");
    }
    let (deny, always, allow) = (rig.rect(deny_key("p1")), rig.rect(always_key("p1")), rig.rect(allow_key("p1")));
    assert!(deny.x < always.x && always.x < allow.x);
}

#[test]
fn reports_the_chosen_decision_once_even_when_clicked_repeatedly() {
    let mut rig = with_prompts(vec![prompt("p1", PromptKind::Tool)]);
    rig.click(allow_key("p1"));
    rig.until("the decision", |rig| rig.host.count("resolve_tool_prompt") == 1);
    let args = rig.host.last("resolve_tool_prompt").unwrap();
    assert_eq!(args["decision"], "allow_once");
    assert!(rig.app.tool_prompts(CONV).is_empty(), "the card comes down");
    rig.settle(10);
    assert_eq!(rig.host.count("resolve_tool_prompt"), 1);
}

#[test]
fn reports_a_denial_and_a_blanket_allowance_distinctly() {
    let mut rig = with_prompts(vec![prompt("p1", PromptKind::Tool), prompt("p2", PromptKind::Tool)]);
    rig.click(deny_key("p1"));
    rig.until("the denial", |rig| rig.host.count("resolve_tool_prompt") == 1);
    assert_eq!(rig.host.last("resolve_tool_prompt").unwrap()["decision"], "deny");
    rig.settle(10);
    rig.click(always_key("p2"));
    rig.until("the allowance", |rig| rig.host.count("resolve_tool_prompt") == 2);
    assert_eq!(rig.host.last("resolve_tool_prompt").unwrap()["decision"], "allow_always");
}

#[test]
fn withholds_the_blanket_allowance_the_backend_did_not_offer_and_names_the_subagent_that_asked() {
    let mut rig = with_prompts(vec![PendingToolPrompt { allow_always_offered: false, requester: Some("reviewer".into()), mandatory: true, ..prompt("p1", PromptKind::Tool) }]);
    assert!(!rig.has(always_key("p1")));
    let words = rig.words();
    assert!(words.iter().any(|said| said == "reviewer · Run a command"));
    assert!(words.iter().any(|said| said == "Asked at every security level."));
}

#[test]
fn draws_the_pager_only_for_two_or_more_cards_and_flips_without_answering() {
    let mut rig = with_prompts(vec![prompt("p1", PromptKind::Tool)]);
    rig.frame();
    assert!(!rig.has(pager_key(true)));
    rig.app.runs().tool_prompts.insert(CONV.to_owned(), vec![prompt("p1", PromptKind::Tool), prompt("p2", PromptKind::Tool)]);
    assert!(rig.says("1/2"));
    rig.click(pager_key(false));
    assert!(rig.says("2/2"));
    assert!(rig.has(allow_key("p2")));
    // Stops at the last card.
    rig.click(pager_key(false));
    assert!(rig.says("2/2"));
    assert_eq!(rig.host.count("resolve_tool_prompt"), 0);
}

#[test]
fn enter_presses_the_default_action_that_took_the_focus_on_arrival() {
    let mut rig = with_prompts(vec![prompt("p1", PromptKind::Tool)]);
    rig.frame();
    rig.press(Key::Enter);
    rig.until("the decision", |rig| rig.host.count("resolve_tool_prompt") == 1);
    assert_eq!(rig.host.last("resolve_tool_prompt").unwrap()["decision"], "allow_once");
}

#[test]
fn asks_for_approval_or_feedback_instead_of_a_tool_question_on_a_plan_card() {
    let mut rig = with_prompts(vec![prompt("p1", PromptKind::PlanExit)]);
    let words = rig.words();
    assert!(words.iter().any(|said| said == "Plan ready: approve it or send feedback"));
    assert!(words.iter().any(|said| said == "The full plan is shown on the plan page"));
    assert!(!words.iter().any(|said| said == "High risk"));
    assert!(!rig.has(deny_key("p1")));
    // Nothing written: nothing to send.
    rig.click(always_key("p1"));
    rig.settle(10);
    assert_eq!(rig.host.count("resolve_tool_prompt"), 0);
}

#[test]
fn sends_what_to_change_from_the_cards_reply_box() {
    let mut rig = with_prompts(vec![prompt("p1", PromptKind::PlanExit)]);
    rig.frame();
    rig.click(crate::approval_dock::feedback_field_key("p1").field_id());
    rig.type_text("  use fewer steps ");
    rig.click(always_key("p1"));
    rig.until("the feedback", |rig| rig.host.count("resolve_tool_prompt") == 1);
    let args = rig.host.last("resolve_tool_prompt").unwrap();
    assert_eq!(args["decision"], "deny");
    assert_eq!(args["feedback"], "use fewer steps");
}

#[test]
fn approves_a_plan_without_touching_the_security_level() {
    let mut rig = with_prompts(vec![prompt("p1", PromptKind::PlanExit)]);
    let before = rig.app.doc.workspaces[0].conversations[0].settings.security_level;
    rig.click(allow_key("p1"));
    rig.until("the approval", |rig| rig.host.count("resolve_tool_prompt") == 1);
    assert_eq!(rig.host.last("resolve_tool_prompt").unwrap()["decision"], "allow_once");
    assert_eq!(rig.app.doc.workspaces[0].conversations[0].settings.security_level, before);
}

#[test]
fn a_card_another_pane_owns_is_not_drawn_here() {
    let mut rig = with_prompts(vec![prompt("p1", PromptKind::Tool)]);
    rig.props.owns_tool_prompt = false;
    assert!(!rig.says("Your approval is needed"));
}

pub(super) fn two_questions() -> Value {
    json!([{ "question": "One?", "header": "H1", "options": [{ "label": "A", "description": "first" }, "B"] }, { "question": "Two?", "header": "H2", "options": ["C", "D"] }])
}

#[test]
fn answers_a_question_card_through_the_frame() {
    let mut rig = with_prompts(vec![question("q1", two_questions())]);
    rig.frame();
    let words = rig.words();
    assert!(words.iter().any(|said| said == "One?"));
    assert!(words.iter().any(|said| said == "H1"));
    assert!(words.iter().any(|said| said == "first"));
    rig.click(row_key("q1", 0, 0));
    assert_eq!(dock_state(&mut rig.app, "q1").unwrap().index, 1, "a pick moves on");
    rig.click(row_key("q1", 1, 1));
    assert!(rig.says("Review your answers"));
    assert!(rig.says("→ A") || rig.says("A"));
    rig.click(submit_key("q1"));
    rig.until("the answer", |rig| rig.host.count("resolve_tool_prompt") == 1);
    let answer = rig.host.last("resolve_tool_prompt").unwrap();
    assert_eq!(answer["question"]["action"], "submit");
    assert_eq!(answer["question"]["answers"], json!(["A", "D"]));
}

#[test]
fn digits_and_keys_reach_the_card_and_escape_closes_it() {
    let mut rig = with_prompts(vec![question("q1", two_questions())]);
    rig.frame();
    rig.frame();
    rig.input.text.push('1');
    rig.frame();
    rig.frame();
    assert_eq!(dock_state(&mut rig.app, "q1").unwrap().drafts[0].selected.as_deref(), Some("A"));
    rig.press(Key::Tab);
    assert_eq!(dock_state(&mut rig.app, "q1").unwrap().index, 2, "the review tab");
    rig.press_with(Key::Tab, true, false);
    assert_eq!(dock_state(&mut rig.app, "q1").unwrap().index, 1);
    rig.press(Key::Down);
    assert_eq!(dock_state(&mut rig.app, "q1").unwrap().focus, 1);
    rig.press(Key::Escape);
    rig.until("the close", |rig| rig.host.count("resolve_tool_prompt") == 1);
    assert_eq!(rig.host.last("resolve_tool_prompt").unwrap()["question"]["action"], "close");
}

#[test]
fn clicking_into_other_checks_its_circle_and_typing_answers_with_it() {
    let mut rig = with_prompts(vec![question("q1", two_questions())]);
    rig.frame();
    rig.click(hash_key(("field", other_field_key("q1", 0))));
    rig.settle(10);
    assert_eq!(dock_state(&mut rig.app, "q1").unwrap().drafts[0].selected.as_deref(), Some(crate::question_dock::OTHER));
    rig.type_text("mine");
    assert_eq!(dock_state(&mut rig.app, "q1").unwrap().drafts[0].other, "mine");
    rig.press(Key::Enter);
    assert_eq!(dock_state(&mut rig.app, "q1").unwrap().index, 1, "Enter on a typed Other picks it and moves on");
}

#[test]
fn the_close_button_and_the_chat_row_answer_the_card() {
    let mut rig = with_prompts(vec![question("q1", two_questions()), question("q2", two_questions())]);
    rig.frame();
    assert!(rig.says("1/2"));
    // Flipping to the other card and back is not answering anything.
    rig.click(super::super::approval_dock::pager_key(false));
    assert!(rig.has(tab_key("q2", 0)));
    assert!(rig.has(dock_key("q2")));
    assert!(!dock_state(&mut rig.app, "q2").unwrap().responded);
    rig.click(close_key("q2"));
    rig.until("the close", |rig| rig.host.count("resolve_tool_prompt") == 1);
    assert_eq!(rig.host.last("resolve_tool_prompt").unwrap()["question"]["action"], "close");
    rig.until("the other card", |rig| rig.has(chat_key("q1", 0)));
    rig.click(chat_key("q1", 0));
    rig.until("the chat", |rig| rig.host.count("resolve_tool_prompt") == 2);
    assert_eq!(rig.host.last("resolve_tool_prompt").unwrap()["question"]["action"], "chat");
}

#[test]
fn tells_the_page_what_a_composer_message_would_hand_back() {
    let mut rig = with_prompts(vec![question("q1", two_questions())]);
    rig.frame();
    assert!(rig.words().iter().any(|said| said == "Sending hands back the card's answers first…"));
    rig.click(row_key("q1", 0, 1));
    rig.settle(10);
    assert_eq!(dock_state(&mut rig.app, "q1").unwrap().draft_dirty, false, "told");
}

pub(super) fn queue(rig: &mut Rig, messages: Value) {
    rig.app.update_conversation(super::rig::WS, CONV, |conversation| conversation.queued_messages = serde_json::from_value(messages).unwrap());
    rig.frame();
}

#[test]
fn lists_queued_messages_steers_when_a_run_goes_and_deletes() {
    let mut rig = Rig::new();
    queue(&mut rig, json!([{ "id": "m1", "content": "first   message", "createdAt": "2026-01-01T00:00:00Z" }, { "id": "m2", "content": "", "createdAt": "2026-01-01T00:00:00Z", "images": [{ "id": "i", "name": "a.png", "mime": "image/png", "width": 1, "height": 1, "bytes": 1 }] }]));
    let words = rig.words();
    assert!(words.iter().any(|said| said == "first message"));
    assert!(words.iter().any(|said| said == "a.png"));
    assert!(words.iter().any(|said| said == "1 images"));
    assert!(words.iter().any(|said| said == "Delete queued message “first message”"));
    assert!(!rig.has(retry_key("m1")));
    // No run: nothing to steer into.
    rig.click(steer_key("m1"));
    rig.settle(10);
    assert_eq!(rig.host.count("steer_model_run"), 0);
    rig.click(delete_key("m1"));
    rig.until("the row to go", |rig| !rig.has(delete_key("m1")));
    assert_eq!(rig.app.doc.workspaces[0].conversations[0].queued_messages.len(), 1);
}

#[test]
fn offers_one_quiet_explicit_retry_action_only_for_a_failed_promotion() {
    let mut rig = Rig::new();
    queue(&mut rig, json!([{ "id": "m1", "content": "first", "createdAt": "2026-01-01T00:00:00Z" }, { "id": "m2", "content": "second", "createdAt": "2026-01-01T00:00:00Z" }]));
    rig.app.composer().failed_queued_promotion_ids.insert("m1".to_owned());
    rig.settle(10);
    assert!(rig.has(retry_key("m1")));
    assert!(!rig.has(retry_key("m2")));
    assert!(rig.says("Retry queued message “first”"));
}

#[test]
fn tab_in_the_text_area_goes_to_the_card_docked_above_it() {
    let mut rig = with_prompts(vec![question("q1", two_questions())]);
    rig.frame();
    rig.focus_text_area();
    rig.type_text("x");
    assert_eq!(rig.draft(), "x", "the box takes typing while the card is up");
    rig.press(Key::Tab);
    rig.input.text.push('1');
    rig.frame();
    rig.frame();
    assert_eq!(rig.draft(), "x");
    assert_eq!(dock_state(&mut rig.app, "q1").unwrap().drafts[0].selected.as_deref(), Some("A"));
}

// ---------------------------------------------------------------- what is where, and what holds the keyboard

#[test]
fn a_plan_cards_reply_action_stands_at_the_start_with_the_approval_at_the_end() {
    let mut rig = with_prompts(vec![prompt("p1", PromptKind::PlanExit)]);
    rig.settle(10);
    // `minmax(90px, 1fr) auto auto`: the first column takes the room, and the third stands empty past the gap.
    let (send, approve, reply) = (rig.rect(always_key("p1")), rig.rect(allow_key("p1")), rig.rect(feedback_field_key("p1").field_id()));
    assert!((send.x - reply.x).abs() < 1.0, "from the card's start: {send:?} {reply:?}");
    assert!(send.right() < approve.x - 100.0, "with the room after it: {send:?} {approve:?}");
    assert!((reply.right() - approve.right() - 12.0).abs() < 1.0, "the empty column's gap at the end: {approve:?} {reply:?}");
}

#[test]
fn a_tool_cards_actions_fill_the_same_three_columns() {
    let mut rig = with_prompts(vec![prompt("p1", PromptKind::Tool)]);
    rig.settle(10);
    let (deny, always, allow) = (rig.rect(deny_key("p1")), rig.rect(always_key("p1")), rig.rect(allow_key("p1")));
    assert!(deny.right() < always.x - 100.0);
    assert!((allow.x - always.right() - 12.0).abs() < 1.0);
    let whole = rig.rect(("tool-approval-dock", "p1"));
    assert!((whole.right() - allow.right()).abs() < 1.0, "the last of three ends the row");
}

#[test]
fn the_last_column_stands_empty_when_only_two_actions_are_offered() {
    let mut rig = with_prompts(vec![PendingToolPrompt { allow_always_offered: false, ..prompt("p1", PromptKind::Tool) }]);
    rig.settle(10);
    let whole = rig.rect(("tool-approval-dock", "p1"));
    assert!((whole.right() - rig.rect(allow_key("p1")).right() - 12.0).abs() < 1.0);
}

#[test]
fn a_long_summary_scrolls_past_about_eight_lines() {
    let summary = (0..40).map(|at| format!("line {at}")).collect::<Vec<_>>().join("\n");
    let mut rig = with_prompts(vec![PendingToolPrompt { summary, ..prompt("p1", PromptKind::Tool) }]);
    rig.settle(10);
    let shown = rig.rect(("approval-summary", "p1"));
    // `max-height: 12.8em` of a 12 px face.
    assert!((shown.h - 153.6).abs() < 1.0, "{shown:?}");
}

#[test]
fn a_card_flipped_to_takes_the_focus_as_if_it_had_just_arrived() {
    let mut rig = with_prompts(vec![prompt("p1", PromptKind::Tool), prompt("p2", PromptKind::Tool)]);
    rig.frame();
    rig.click(pager_key(false));
    rig.settle(10);
    assert!(rig.says("2/2"));
    // Back again, to a card it has already shown.
    rig.click(pager_key(true));
    rig.settle(10);
    assert!(rig.says("1/2"));
    rig.press(Key::Enter);
    rig.until("the decision", |rig| rig.host.count("resolve_tool_prompt") == 1);
    assert_eq!(rig.host.last("resolve_tool_prompt").unwrap()["promptId"], "p1");
    assert_eq!(rig.host.last("resolve_tool_prompt").unwrap()["decision"], "allow_once");
}

#[test]
fn a_card_rises_as_it_arrives_and_then_stands_where_it_belongs() {
    let mut rig = with_prompts(vec![prompt("p0", PromptKind::Tool), prompt("p1", PromptKind::Tool)]);
    rig.settle(400);
    let at_rest = rig.rect(allow_key("p0"));
    rig.click(pager_key(false));
    let arriving = rig.rect(allow_key("p1"));
    assert!(arriving.y > at_rest.y, "below where it will stand: {arriving:?} {at_rest:?}");
    rig.settle(400);
    assert_eq!(rig.rect(allow_key("p1")).y, at_rest.y);
}

fn lone_question() -> Value {
    json!([{ "question": "Which?", "header": "Pick", "options": ["A", "B"] }])
}

#[test]
fn enter_on_a_card_that_only_just_took_the_keyboard_answers_nothing() {
    let mut rig = with_prompts(vec![question("q1", lone_question())]);
    rig.frame();
    rig.frame();
    rig.press(Key::Enter);
    rig.settle(10);
    assert_eq!(rig.host.count("resolve_tool_prompt"), 0, "the focus is on the card, and a section is not a button");
    assert!(!dock_state(&mut rig.app, "q1").unwrap().responded);
}

#[test]
fn enter_presses_the_row_the_arrows_moved_to() {
    let mut rig = with_prompts(vec![question("q1", lone_question())]);
    rig.frame();
    rig.frame();
    rig.press(Key::Down);
    rig.frame();
    rig.press(Key::Enter);
    rig.until("the answer", |rig| rig.host.count("resolve_tool_prompt") == 1);
    assert_eq!(rig.host.last("resolve_tool_prompt").unwrap()["question"]["answers"], json!(["B"]));
}

/// As the shell delivers a space: a key press for whatever acts on keys, and text for whatever reads text.
fn press_space(rig: &mut super::rig::Rig) {
    rig.input.text.push(' ');
    rig.press(Key::Char(' '));
}

#[test]
fn space_on_a_focused_row_presses_it_once() {
    let mut rig = with_prompts(vec![question("q1", json!([{ "question": "Many?", "multiSelect": true, "options": ["A", "B"] }]))]);
    rig.frame();
    rig.frame();
    rig.press(Key::Down);
    rig.frame();
    press_space(&mut rig);
    rig.frame();
    assert_eq!(dock_state(&mut rig.app, "q1").unwrap().drafts[0].checked, ["B"], "a toggle pressed twice would have undone itself");
    // On the card itself, nothing is pressed.
    rig.click(tab_key("q1", 0));
    rig.frame();
    press_space(&mut rig);
    rig.frame();
    assert_eq!(dock_state(&mut rig.app, "q1").unwrap().drafts[0].checked, ["B"]);
}

#[test]
fn the_arrows_reach_the_chat_row_and_the_next_button_and_keep_the_keyboard_there() {
    let mut rig = with_prompts(vec![question("q1", json!([{ "question": "Many?", "multiSelect": true, "options": ["A"] }, { "question": "Two?", "options": ["C"] }]))]);
    rig.frame();
    rig.frame();
    // Rows go A, Other, chat, Next. Into the Other box, where the arrows move the caret and Tab is what leaves it.
    rig.press(Key::Down);
    rig.frame();
    assert_eq!(dock_state(&mut rig.app, "q1").unwrap().focus, 1);
    rig.press(Key::Tab);
    rig.frame();
    assert_eq!(fluent::el::focused_id(), hash_key(advance_key("q1", 0)));
    assert_eq!(dock_state(&mut rig.app, "q1").unwrap().focus, 3, "the mark goes where the keyboard went");
    rig.press(Key::Up);
    rig.frame();
    assert_eq!(fluent::el::focused_id(), hash_key(chat_key("q1", 0)), "a key still reaches the card from the last row");
    rig.press(Key::Down);
    rig.frame();
    assert_eq!(fluent::el::focused_id(), hash_key(advance_key("q1", 0)));
    rig.press(Key::Enter);
    rig.frame();
    let state = dock_state(&mut rig.app, "q1").unwrap();
    assert_eq!(state.index, 1, "Enter on the Next button moves on");
    // What held the keyboard went with its page, and the card holds it now.
    assert_eq!(fluent::el::focused_id(), hash_key(dock_key("q1")));
    rig.input.text.push('1');
    rig.frame();
    rig.frame();
    assert_eq!(dock_state(&mut rig.app, "q1").unwrap().drafts[1].selected.as_deref(), Some("C"), "a digit still reaches the next page");
}

#[test]
fn the_pointer_mark_follows_what_a_click_put_the_keyboard_on() {
    let mut rig = with_prompts(vec![question("q1", json!([{ "question": "Many?", "multiSelect": true, "options": ["A", "B", "C"] }]))]);
    rig.frame();
    rig.click(row_key("q1", 0, 2));
    rig.frame();
    let state = dock_state(&mut rig.app, "q1").unwrap();
    assert_eq!(state.focus, 2);
    assert_eq!(state.drafts[0].checked, ["C"]);
    rig.press(Key::Up);
    assert_eq!(dock_state(&mut rig.app, "q1").unwrap().focus, 1, "the arrows go on from there");
}

#[test]
fn tab_leaves_the_other_box_for_the_control_after_it() {
    let mut rig = with_prompts(vec![question("q1", two_questions())]);
    rig.frame();
    rig.click(hash_key(("field", other_field_key("q1", 0))));
    rig.settle(10);
    assert_eq!(dock_state(&mut rig.app, "q1").unwrap().drafts[0].selected.as_deref(), Some(OTHER));
    rig.press(Key::Tab);
    rig.frame();
    assert_eq!(fluent::el::focused_id(), hash_key(chat_key("q1", 0)));
    assert_eq!(dock_state(&mut rig.app, "q1").unwrap().index, 0, "typing, Tab is the browser's and not the card's");
}

fn with_previews() -> Value {
    json!([{ "question": "Layout?", "header": "Layout", "options": [{ "label": "A", "preview": "$\\frac{a}{b}$" }, { "label": "B", "preview": "plain" }] }])
}

#[test]
fn a_preview_is_set_in_the_small_code_face_and_follows_the_math_setting() {
    let mut rig = with_prompts(vec![question("q1", with_previews())]);
    rig.settle(10);
    let as_math = rig.rect(("question-preview", "q1", 0usize));
    rig.app.update_document(|doc| doc.global_settings.appearance.single_dollar_math = false);
    rig.settle(10);
    let as_text = rig.rect(("question-preview", "q1", 0usize));
    assert!(as_math.h > as_text.h, "a fraction stands taller than a line of its source: {as_math:?} {as_text:?}");
    // One line of the face, and the box's padding and border: `.question-dock__preview-box`.
    assert!(as_text.h < 16.275 + 16.0 + 2.0 + 4.0, "{as_text:?}");
}

#[test]
fn hovering_a_row_in_the_preview_layout_shows_its_preview_and_enter_still_presses_what_holds_the_keyboard() {
    let mut rig = with_prompts(vec![question("q1", with_previews())]);
    rig.frame();
    rig.frame();
    let b = rig.rect(row_key("q1", 0, 1));
    rig.input.pointer = Some((b.x + 4.0, b.y + 4.0));
    rig.settle(10);
    assert_eq!(dock_state(&mut rig.app, "q1").unwrap().focus, 1);
    assert!(rig.says("plain"));
    rig.press(Key::Enter);
    rig.settle(10);
    assert_eq!(rig.host.count("resolve_tool_prompt"), 0, "the card holds the keyboard, whatever the pointer rests on");
}
