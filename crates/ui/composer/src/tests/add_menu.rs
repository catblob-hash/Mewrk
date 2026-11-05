//! `ComposerAddMenu.test.tsx`.

use super::rig::Rig;
use crate::composer_add_files;
use mewrk_client::prelude::*;
use mewrk_ui_common::popover_menu::{panel_key, row_key, trigger_key};
use std::cell::Cell;

const MENU: &str = "add-menu-test";

fn menu_rig(disabled: bool, image_input: bool, reason: Option<&'static str>, chosen: Rc<Cell<u32>>) -> Rig {
    Rig::new().with_view(move |_| {
        let chosen = chosen.clone();
        row().p(40.0).child(composer_add_files(MENU, disabled, image_input, reason.map(str::to_owned), Rc::new(move |_| chosen.set(chosen.get() + 1))))
    })
}

#[test]
fn opens_the_file_picker_from_the_add_menu_which_offers_files_rather_than_images() {
    let chosen = Rc::new(Cell::new(0));
    let mut rig = menu_rig(false, true, None, chosen.clone());
    assert!(!rig.has(panel_key(MENU)));
    rig.click(trigger_key(MENU));
    rig.settle(30);
    let words = rig.words();
    assert!(words.iter().any(|word| word == "Upload files"));
    // Pictures are one kind of file; there is no separate entry for them.
    assert!(!words.iter().any(|word| word.contains("Add image")));
    assert!(words.iter().any(|word| word == "Images, PDFs, or text files; paste and drop also work"));
    rig.click(row_key(MENU, "files"));
    assert_eq!(chosen.get(), 1);
    assert!(!rig.has(panel_key(MENU)));
}

#[test]
fn keeps_uploads_open_for_a_model_without_image_input_and_says_what_it_takes() {
    let mut rig = menu_rig(false, false, None, Rc::default());
    rig.click(trigger_key(MENU));
    rig.settle(30);
    assert!(rig.says("PDFs or text files (the current model takes no images); paste and drop also work"));
}

#[test]
fn disables_the_entry_and_explains_why_when_nothing_can_be_attached() {
    let chosen = Rc::new(Cell::new(0));
    let mut rig = menu_rig(false, true, Some("Preparing attachments…"), chosen.clone());
    rig.click(trigger_key(MENU));
    rig.settle(30);
    assert!(rig.says("Preparing attachments…"));
    rig.click(row_key(MENU, "files"));
    assert_eq!(chosen.get(), 0);
}

#[test]
fn disables_the_whole_control_while_the_composer_is_locked() {
    let mut rig = menu_rig(true, true, None, Rc::default());
    rig.click(trigger_key(MENU));
    rig.settle(20);
    assert!(!rig.has(panel_key(MENU)));
}
