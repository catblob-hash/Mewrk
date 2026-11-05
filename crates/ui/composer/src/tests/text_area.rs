//! The text area: the send and newline shortcuts of `App.tsx`, `PastedTextTags.test.tsx`, and the input method's paths, which no real
//! IME has been through on this stack, so they are pinned here as far as a script can.

use super::rig::{Rig, CONV};
use crate::attach::ClipboardContents;
use crate::editor::textarea_key;
use crate::pasted_text::{forget_last_copy, pasted_text_id_mark};
use mewrk_client::prelude::*;
use mewrk_client::runs::composer::expand_pasted_texts;

const NBSP: char = '\u{a0}';

/// A tag reads as its name alone; the paste's number rides along as invisible marks.
fn tag(id: u32, extra_lines: usize) -> String {
    let lines = if extra_lines > 0 { format!("{NBSP}+{extra_lines}{NBSP}lines") } else { String::new() };
    format!("{NBSP}Pasted{NBSP}text{lines}{}{NBSP}", pasted_text_id_mark(id))
}

fn long() -> String {
    (0..30).map(|index| format!("line {index} {}", "x".repeat(40))).collect::<Vec<_>>().join("\n")
}

fn paste(rig: &mut Rig, text: &str) {
    rig.clipboard.0 = Some(text.to_owned());
    rig.press_with(Key::Char('v'), false, true);
}

fn pastes(rig: &mut Rig) -> Vec<mewrk_client::runs::PastedText> {
    let id = rig.conversation_id();
    rig.app.composer().pasted_texts.get(&id).cloned().unwrap_or_default()
}

fn sends(rig: &mut Rig) -> String {
    let (draft, pastes) = (rig.draft(), pastes(rig));
    expand_pasted_texts(&draft, &pastes)
}

fn ready() -> Rig {
    forget_last_copy();
    let mut rig = Rig::new();
    rig.frame();
    rig.focus_text_area();
    rig
}

#[test]
fn typing_writes_the_draft_and_enter_sends_it() {
    let mut rig = ready();
    rig.type_text("hello");
    assert_eq!(rig.draft(), "hello");
    rig.press(Key::Enter);
    rig.until("the run to start", |rig| rig.host.count("run_model") == 1);
    let request = rig.host.last("run_model").unwrap();
    let contexts = request["request"]["contexts"].as_array().unwrap();
    assert_eq!(contexts.last().unwrap()["content"], "hello");
    assert_eq!(rig.draft(), "", "sending clears the box");
    rig.host.release_runs();
}

#[test]
fn shift_enter_breaks_the_line_and_the_bindings_can_change_places() {
    let mut rig = ready();
    rig.type_text("a");
    rig.press_with(Key::Enter, true, false);
    rig.type_text("b");
    assert_eq!(rig.draft(), "a\nb");
    assert_eq!(rig.host.count("run_model"), 0);

    rig.app.doc.global_settings.appearance.send_shortcut = vec!["Shift".to_owned(), "Enter".to_owned()];
    rig.app.doc.global_settings.appearance.newline_shortcut = vec!["Enter".to_owned()];
    rig.frame();
    rig.press(Key::Enter);
    assert_eq!(rig.draft(), "a\nb\n");
    assert_eq!(rig.host.count("run_model"), 0);
    rig.press_with(Key::Enter, true, false);
    rig.until("the run to start", |rig| rig.host.count("run_model") == 1);
    rig.host.release_runs();
}

#[test]
fn the_command_chord_only_does_something_when_it_is_bound() {
    let mut rig = ready();
    rig.type_text("a");
    rig.press_with(Key::Enter, false, true);
    assert_eq!(rig.draft(), "a");
    assert_eq!(rig.host.count("run_model"), 0);
}

#[test]
fn composition_is_not_text_until_it_is_committed_and_its_enter_sends_nothing() {
    let mut rig = ready();
    rig.input.ime_preedit = Some("ni'hao".to_owned());
    rig.frame();
    rig.frame();
    assert_eq!(rig.draft(), "", "what is being composed is only shown");
    // A key that reaches the window mid-composition is the input method's.
    rig.press(Key::Enter);
    rig.press(Key::Backspace);
    assert_eq!(rig.host.count("run_model"), 0);

    // The commit and the Enter that confirmed it can land together.
    rig.input.ime_preedit = None;
    rig.input.ime_commit = Some("你好".to_owned());
    rig.input.keys.push(fluent::ui::KeyPress { key: Key::Enter, shift: false, cmd: false, alt: false, ctrl: false });
    rig.frame();
    rig.frame();
    assert_eq!(rig.draft(), "你好", "committed once, and not sent");
    assert_eq!(rig.host.count("run_model"), 0);
    rig.frame();
    assert_eq!(rig.draft(), "你好", "a commit applies exactly once");
    rig.press(Key::Enter);
    rig.until("the run to start", |rig| rig.host.count("run_model") == 1);
    rig.host.release_runs();
}

#[test]
fn composition_replaces_a_selection_as_it_starts_and_ends_with_the_focus() {
    let mut rig = ready();
    rig.type_text("abc");
    rig.press_with(Key::Char('a'), false, true);
    rig.input.ime_preedit = Some("x".to_owned());
    rig.frame();
    rig.frame();
    assert_eq!(rig.draft(), "");
    // Focus moves away mid-composition: nothing was committed, and nothing of it stays.
    rig.input.ime_preedit = None;
    rig.click_at(5.0, 5.0);
    rig.frame();
    rig.focus_text_area();
    rig.type_text("ok");
    assert_eq!(rig.draft(), "ok");
}

#[test]
fn the_box_grows_with_its_text_up_to_where_it_scrolls() {
    let mut rig = ready();
    let one = rig.rect(textarea_key()).h;
    assert_eq!(one, 29.0);
    for _ in 0..3 {
        rig.press_with(Key::Enter, true, false);
    }
    rig.settle(10);
    let four = rig.rect(textarea_key()).h;
    assert!(four > one, "{four}");
    for _ in 0..30 {
        rig.input.keys.push(fluent::ui::KeyPress { key: Key::Enter, shift: true, cmd: false, alt: false, ctrl: false });
    }
    rig.frame();
    rig.settle(10);
    assert_eq!(rig.rect(textarea_key()).h, 180.0);
}

#[test]
fn each_conversation_keeps_its_own_draft_and_the_box_follows_one_changed_from_outside() {
    let mut rig = ready();
    rig.type_text("mine");
    rig.app.set_composer_draft("elsewhere", "theirs");
    rig.frame();
    assert_eq!(rig.draft(), "mine");
    rig.app.set_composer_draft(CONV, "put back from a message");
    rig.frame();
    rig.frame();
    rig.type_text("!");
    assert_eq!(rig.draft(), "put back from a message!", "the caret ends up after what came in");
}

#[test]
fn undo_takes_back_typing_as_one_step_and_a_tag_comes_back_with_its_text() {
    let mut rig = ready();
    rig.type_text("one");
    rig.type_text(" two");
    rig.press_with(Key::Char('z'), false, true);
    assert_eq!(rig.draft(), "");
    rig.press_with(Key::Char('z'), true, true);
    assert_eq!(rig.draft(), "one two");

    paste(&mut rig, &long());
    let with_tag = rig.draft();
    rig.press(Key::Backspace);
    assert_eq!(rig.draft(), "one two");
    rig.press_with(Key::Char('z'), false, true);
    assert_eq!(rig.draft(), with_tag);
    assert_eq!(sends(&mut rig), format!("one two{}", long()));
}

#[test]
fn folds_a_paste_over_800_characters_into_a_tag_at_the_caret_and_sends_it_as_the_text() {
    let mut rig = ready();
    rig.type_text("ab");
    rig.press(Key::Left);
    paste(&mut rig, "short");
    assert_eq!(rig.draft(), "ashortb", "a short paste stays text");
    rig.app.set_composer_draft(CONV, "ab");
    rig.frame();
    rig.frame();
    rig.press(Key::Left);
    paste(&mut rig, &long().replace('\n', "\r\n"));
    let label = tag(1, 29);
    assert_eq!(rig.draft(), format!("a{label}b"));
    // CRLF becomes the LF a text box would have held.
    assert_eq!(sends(&mut rig), format!("a{}b", long()));
    rig.type_text("!");
    assert_eq!(rig.draft(), format!("a{label}!b"), "the caret was left after the tag");
}

#[test]
fn deletes_a_tag_whole_and_moves_the_caret_over_it_in_one_step() {
    let mut rig = ready();
    rig.type_text("ab");
    rig.press(Key::Left);
    paste(&mut rig, &long());
    let label = tag(1, 29);

    // The caret is just after the tag: left goes to its other side, in one step.
    rig.press(Key::Left);
    rig.type_text("<");
    assert_eq!(rig.draft(), format!("a<{label}b"));
    rig.press(Key::Right);
    rig.type_text(">");
    assert_eq!(rig.draft(), format!("a<{label}>b"));

    // Backspace just after it takes all of it, and Delete just before it.
    rig.press(Key::Backspace);
    rig.press(Key::Backspace);
    assert_eq!(rig.draft(), "a<b");
    rig.press_with(Key::Char('z'), false, true);
    rig.press_with(Key::Char('z'), false, true);
    assert_eq!(rig.draft(), format!("a<{label}>b"));
}

#[test]
fn delete_before_a_tag_takes_it_whole_and_shift_selects_over_it() {
    let mut rig = ready();
    paste(&mut rig, &long());
    let label = tag(1, 29);
    rig.press(Key::Home);
    rig.press(Key::Delete);
    assert_eq!(rig.draft(), "");
    rig.press_with(Key::Char('z'), false, true);
    assert_eq!(rig.draft(), label);
    rig.press_with(Key::Left, true, false);
    rig.press(Key::Delete);
    assert_eq!(rig.draft(), "", "a shift-left selected the whole tag");
}

#[test]
fn never_leaves_the_caret_inside_a_tag_when_it_is_clicked() {
    let mut rig = ready();
    rig.type_text("ab");
    rig.press(Key::Left);
    paste(&mut rig, &long());
    let label = tag(1, 29);
    // A click near the tag's start lands before it, one near the end after it.
    let area = rig.rect(textarea_key());
    let glyph = 7.0;
    rig.click_at(area.x + 3.0 + glyph + 8.0, area.y + 14.0);
    rig.type_text("^");
    let draft = rig.draft();
    assert!(draft == format!("a^{label}b") || draft == format!("a{label}^b"), "{draft:?}");
    assert!(draft.contains(&label), "the tag is whole");
}

#[test]
fn copies_what_the_box_shows_as_text_elsewhere_and_as_tags_back_into_a_box() {
    let mut rig = ready();
    rig.type_text("ab");
    rig.press(Key::Left);
    paste(&mut rig, &long());
    rig.press_with(Key::Char('a'), false, true);
    rig.press_with(Key::Char('c'), false, true);
    assert_eq!(rig.clipboard.0.as_deref(), Some(format!("a{}b", long()).as_str()));

    // Back into a box that already holds a paste of its own: the copied tag takes the next number rather than colliding.
    rig.app.set_composer_draft(CONV, "");
    rig.app.set_composer_pasted_texts(CONV, Vec::new());
    rig.frame();
    rig.frame();
    let copied = rig.clipboard.0.clone().unwrap();
    paste(&mut rig, &"y".repeat(900));
    paste(&mut rig, &copied);
    assert_eq!(rig.draft(), format!("{}a{}b", tag(1, 0), tag(2, 29)));
    assert_eq!(sends(&mut rig), format!("{}a{}b", "y".repeat(900), long()));
}

#[test]
fn brings_a_cut_back_as_it_was_since_the_tags_text_is_still_this_drafts() {
    let mut rig = ready();
    rig.type_text("ab");
    rig.press(Key::Left);
    paste(&mut rig, &long());
    let value = rig.draft();
    rig.press_with(Key::Char('a'), false, true);
    rig.press_with(Key::Char('x'), false, true);
    assert_eq!(rig.draft(), "");
    let copied = rig.clipboard.0.clone().unwrap();
    assert_eq!(copied, format!("a{}b", long()));
    paste(&mut rig, &copied);
    assert_eq!(rig.draft(), value);
}

#[test]
fn tells_two_tags_that_read_alike_apart_and_sends_each_ones_own_text() {
    let mut rig = ready();
    let other = long().replace("line 0", "LINE 0");
    paste(&mut rig, &long());
    paste(&mut rig, &other);
    assert_eq!(rig.draft(), format!("{}{}", tag(1, 29), tag(2, 29)));
    assert_eq!(sends(&mut rig), format!("{}{other}", long()));
}

#[test]
fn does_not_fold_long_plain_text_copied_out_of_a_box() {
    let mut rig = ready();
    let typed = "t".repeat(1000);
    rig.app.set_composer_draft(CONV, typed.clone());
    rig.frame();
    rig.frame();
    rig.press_with(Key::Char('a'), false, true);
    rig.press_with(Key::Char('c'), false, true);
    let copied = rig.clipboard.0.clone().unwrap();
    paste(&mut rig, &copied);
    assert_eq!(rig.draft(), typed);
    assert!(pastes(&mut rig).is_empty());
}

#[test]
fn a_clipboard_picture_is_attached_and_a_clipboard_with_both_keeps_its_text() {
    let mut rig = ready();
    // Only a picture: nothing lands in the box.
    *rig.system_clipboard.borrow_mut() = ClipboardContents { image: Some((vec![200; 16], 2, 2)), paths: Vec::new() };
    rig.clipboard.0 = None;
    rig.press_with(Key::Char('v'), false, true);
    rig.until("the picture to arrive", |rig| rig.app.composer().image_drafts.get(CONV).is_some_and(|images| images.len() == 1));
    assert_eq!(rig.draft(), "");
    let image = rig.app.composer().image_drafts[CONV][0].clone();
    assert_eq!(image.name, "image.png");

    // Both: the text keeps going where text goes, and the file rides along.
    let file = rig.repo.join("notes.md");
    std::fs::write(&file, "# notes").unwrap();
    *rig.system_clipboard.borrow_mut() = ClipboardContents { image: None, paths: vec![file] };
    paste(&mut rig, "notes.md");
    assert_eq!(rig.draft(), "notes.md");
    rig.until("the file to arrive", |rig| rig.app.composer().file_drafts.get(CONV).is_some_and(|files| files.len() == 1));
}

#[test]
fn a_held_composer_takes_no_input() {
    let mut rig = ready();
    rig.type_text("before");
    mewrk_pane_git::git_controller(&mut rig.app).acquire_mutation_lease(CONV, &[CONV.to_owned()]);
    rig.frame();
    rig.frame();
    rig.type_text(" after");
    assert_eq!(rig.draft(), "before");
    mewrk_pane_git::git_controller(&mut rig.app).release_mutation_lease(CONV);
}

#[test]
fn pressing_the_send_button_leaves_the_focus_in_the_box() {
    let mut rig = ready();
    rig.type_text("first");
    rig.click(crate::send_button_key());
    rig.until("the run to start", |rig| rig.host.count("run_model") == 1);
    rig.type_text("next");
    assert_eq!(rig.draft(), "next");
    rig.host.release_runs();
}

#[test]
fn clicking_elsewhere_takes_the_focus_and_the_mention_brings_it_back_with_the_caret_after_it() {
    let mut rig = ready();
    rig.type_text("look");
    rig.click_at(5.0, 5.0);
    rig.type_text("lost");
    assert_eq!(rig.draft(), "look");
    crate::put_mention_in_composer(&mut rig.app, CONV, "@src/main.rs:12");
    rig.frame();
    rig.frame();
    rig.type_text(" here");
    assert_eq!(rig.draft(), "look\n@src/main.rs:12 here");
}
