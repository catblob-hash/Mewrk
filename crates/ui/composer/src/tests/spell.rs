//! `spellCheck={appearance.spellCheck}` on the composer's textarea, and the rows it adds to the right-click menu, on a checker that
//! leaves the user's dictionary alone. `NSSpellChecker` itself is exercised in `fluent/src/spell.rs`.

use super::rig::Rig;
use crate::editor::{misspelt_words, textarea_key, TEXT_MENU};
use fluent::spell::{install, Speller};
use mewrk_client::prelude::*;
use mewrk_ui_common::context_menu::menu_row_key;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<String>>>);

impl Log {
    fn said(&self, what: &str) -> bool {
        self.0.lock().unwrap().iter().any(|said| said == what)
    }
}

/// Knows two bad words, and has guesses for one of them.
struct Checker(Log);

impl Speller for Checker {
    fn misspelt(&mut self, _paragraph: &str, words: &[String]) -> Vec<String> {
        self.0.0.lock().unwrap().extend(words.iter().map(|word| format!("asked {word}")));
        words.iter().filter(|word| matches!(word.as_str(), "teh" | "wrod")).cloned().collect()
    }
    fn guesses(&mut self, word: &str) -> Vec<String> {
        if word == "teh" { vec!["the".to_owned(), "ten".to_owned()] } else { Vec::new() }
    }
    fn learn(&mut self, word: &str) {
        self.0.0.lock().unwrap().push(format!("learnt {word}"));
    }
    fn ignore(&mut self, word: &str) {
        self.0.0.lock().unwrap().push(format!("ignored {word}"));
    }
}

fn ready(on: bool) -> (Rig, Log) {
    let mut rig = Rig::new();
    let log = Log::default();
    install(Box::new(Checker(log.clone())));
    rig.app.doc.global_settings.appearance.spell_check = on;
    rig.frame();
    rig.focus_text_area();
    (rig, log)
}

fn row(id: &str) -> u64 {
    menu_row_key(TEXT_MENU, &[], id)
}

fn marked(rig: &mut Rig) -> Vec<String> {
    rig.settle(20);
    misspelt_words()
}

/// A right press on the text area's first line, this far along it.
fn right_click(rig: &mut Rig, along: f32) {
    let area = rig.rect(textarea_key());
    rig.input.pointer = Some((area.x + 3.0 + along, area.y + 4.0 + 10.0));
    rig.frame();
    rig.input.right_pressed = true;
    rig.frame();
    rig.settle(30);
}

#[test]
fn marks_a_word_once_it_is_finished_and_only_the_word() {
    let (mut rig, log) = ready(true);
    rig.type_text("teh");
    assert!(marked(&mut rig).is_empty(), "still being written");
    assert!(!log.said("asked teh"), "and nothing has been asked");
    let plain = rig.lines_drawn();
    rig.type_text(" cat teh ");
    assert_eq!(marked(&mut rig), ["teh", "teh"]);
    assert!(rig.lines_drawn() > plain, "a wavy line under each");
    rig.press(Key::Backspace);
    rig.press(Key::Backspace);
    assert_eq!(marked(&mut rig), ["teh"], "a word being changed is being written again");
    assert!(!log.said("asked te"));
    rig.press(Key::Left);
    assert_eq!(marked(&mut rig), ["teh"]);
    assert!(log.said("asked te"), "moving the caret ends that");
}

#[test]
fn the_setting_off_asks_nothing_and_draws_nothing() {
    let (mut rig, log) = ready(false);
    let plain = rig.lines_drawn();
    rig.type_text("teh cat ");
    assert!(marked(&mut rig).is_empty());
    assert_eq!(rig.lines_drawn(), plain);
    assert!(log.0.lock().unwrap().is_empty(), "the checker has not been asked");

    rig.app.doc.global_settings.appearance.spell_check = true;
    assert_eq!(marked(&mut rig), ["teh"], "turned on, what is there is checked");
    rig.app.doc.global_settings.appearance.spell_check = false;
    assert!(marked(&mut rig).is_empty(), "and turned off, it goes");
    assert_eq!(rig.lines_drawn(), plain);
}

#[test]
fn what_an_input_method_is_composing_is_not_checked() {
    let (mut rig, log) = ready(true);
    rig.type_text("cat ");
    rig.input.ime_preedit = Some("teh".to_owned());
    assert!(marked(&mut rig).is_empty());
    assert!(!log.said("asked teh"), "it isn't text yet");
    rig.input.ime_preedit = None;
    rig.input.ime_commit = Some("teh".to_owned());
    rig.frame();
    rig.type_text(" ");
    assert_eq!(marked(&mut rig), ["teh"], "committed and finished, it is");

    // A word the composition touches is not finished either.
    rig.press_with(Key::Left, false, false);
    rig.input.ime_preedit = Some("zz".to_owned());
    assert!(marked(&mut rig).is_empty());
    rig.input.ime_preedit = None;
    rig.frame();
}

#[test]
fn the_right_click_menu_puts_the_guesses_first_and_a_guess_replaces_the_word_as_one_undo_step() {
    let (mut rig, _log) = ready(true);
    rig.type_text("teh cat ");
    assert_eq!(marked(&mut rig), ["teh"]);
    right_click(&mut rig, 4.0);
    let (first, second, ignore, learn, cut) = (rig.rect(row("guess-0")), rig.rect(row("guess-1")), rig.rect(row("ignore")), rig.rect(row("learn")), rig.rect(row("cut")));
    assert!(first.y < second.y && second.y < ignore.y && ignore.y < learn.y && learn.y < cut.y, "{first:?} {second:?} {ignore:?} {learn:?} {cut:?}");
    assert_eq!(focused_id(), hash_key(row("guess-0")), "the menu opens with the guesses in it, on the likeliest");

    rig.click(row("guess-0"));
    assert_eq!(rig.draft(), "the cat ");
    assert!(!rig.has(row("guess-0")), "the menu closes");
    assert!(marked(&mut rig).is_empty());
    rig.press_with(Key::Char('z'), false, true);
    assert_eq!(rig.draft(), "teh cat ", "one step back");
}

#[test]
fn says_when_it_has_no_guess_and_learning_or_ignoring_takes_the_mark_away() {
    let (mut rig, log) = ready(true);
    rig.type_text("wrod teh ");
    assert_eq!(marked(&mut rig), ["wrod", "teh"]);
    right_click(&mut rig, 4.0);
    assert!(rig.has(row("no-guesses")));
    assert!(!rig.has(row("guess-0")));
    rig.click(row("learn"));
    assert_eq!(marked(&mut rig), ["teh"]);
    assert!(log.said("learnt wrod"));

    let area = rig.rect(textarea_key());
    rig.click_at(area.right() - 10.0, area.y + 14.0);
    let start = 3.0 + 40.0;
    right_click(&mut rig, start);
    assert!(rig.has(row("guess-0")), "the word under the press");
    rig.click(row("ignore"));
    assert!(marked(&mut rig).is_empty());
    assert!(log.said("ignored teh"));
}

#[test]
fn a_good_word_or_the_setting_off_has_only_the_editing_rows_and_those_act_on_the_field() {
    let (mut rig, _log) = ready(false);
    rig.type_text("teh cat");
    right_click(&mut rig, 4.0);
    assert!(rig.has(row("cut")));
    assert!(!rig.has(row("ignore")), "the setting is off, so nothing is offered for a word");
    rig.click(row("select-all"));
    right_click(&mut rig, 4.0);
    rig.click(row("cut"));
    assert_eq!(rig.draft(), "");
    assert_eq!(rig.clipboard.0.as_deref(), Some("teh cat"));
    right_click(&mut rig, 4.0);
    rig.click(row("paste"));
    assert_eq!(rig.draft(), "teh cat");
}
