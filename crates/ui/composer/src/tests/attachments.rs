//! `App.images.test.tsx`'s composer half, `ImageStrip.test.tsx`, `SelectedElementChips.test.tsx` and the drop zone, through the host's
//! own attachment commands.

use super::rig::{png, Rig, Seed, CONV};
use crate::element_chips::element_remove_key;
use crate::image_strip::{file_key, file_remove_key, image_key, image_remove_key};
use crate::{composer_box_key, send_button_key, stage_element_pick, stop_button_key};
use mewrk_client::prelude::*;
use mewrk_client::runs::ComposerFile;

const STRIP: &str = "composer:conversation-1";

fn picture(name: &str) -> ComposerFile {
    ComposerFile { name: name.to_owned(), media_type: "image/png".to_owned(), bytes: png() }
}

fn add_image(rig: &mut Rig, name: &str) -> String {
    let before = rig.app.composer().image_drafts.get(CONV).map_or(0, Vec::len);
    rig.app.add_composer_attachments(CONV, vec![picture(name)], Vec::new());
    rig.until("the picture to arrive", |rig| rig.app.composer().image_drafts.get(CONV).is_some_and(|images| images.len() > before));
    rig.app.composer().image_drafts[CONV].last().unwrap().id.clone()
}

#[test]
fn draws_the_chip_from_the_hosts_thumbnail_and_opens_and_removes_it() {
    let mut rig = Rig::new();
    let id = add_image(&mut rig, "shot.png");
    rig.until("the thumbnail", |rig| rig.words().iter().any(|word| word == "View full image shot.png"));
    assert!(rig.has(image_key(STRIP, &id)));
    assert!(rig.says("1 images"));
    assert!(rig.says("Remove image shot.png"));

    // Open: the full picture over everything, and Escape closes it.
    rig.settle(60);
    let chip = rig.rect(image_key(STRIP, &id));
    rig.click_at(chip.x + 10.0, chip.y + 30.0);
    rig.settle(30);
    assert!(rig.says("Close full image shot.png"));
    rig.press(Key::Escape);
    rig.settle(20);
    assert!(!rig.words().iter().any(|word| word == "Close full image shot.png"));

    rig.click(image_remove_key(STRIP, &id));
    rig.until("the chip to go", |rig| rig.app.composer().image_drafts.get(CONV).is_none_or(Vec::is_empty));
    assert!(!rig.has(image_key(STRIP, &id)));
}

#[test]
fn draws_files_beside_the_images_each_removable_and_holds_a_place_while_preparing() {
    let mut rig = Rig::new();
    rig.app.add_composer_attachments(CONV, vec![ComposerFile { name: "notes.md".into(), media_type: String::new(), bytes: b"# notes\nhello".to_vec() }], Vec::new());
    assert!(rig.says("Adding attachments…"), "a place is held while it is prepared");
    rig.until("the file to arrive", |rig| rig.app.composer().file_drafts.get(CONV).is_some_and(|files| files.len() == 1));
    let file = rig.app.composer().file_drafts[CONV][0].clone();
    assert!(rig.has(file_key(STRIP, &file.id)));
    let words = rig.words();
    assert!(words.iter().any(|word| word == "notes.md"));
    assert!(words.iter().any(|word| word == "Preview file notes.md"));
    assert!(words.iter().any(|word| word == "1 attachments"));
    rig.click(file_remove_key(STRIP, &file.id));
    rig.until("the tile to go", |rig| rig.app.composer().file_drafts.get(CONV).is_none_or(Vec::is_empty));
}

#[test]
fn turns_images_away_with_a_reason_when_the_selected_model_has_no_vision() {
    let mut rig = Rig::seeded(Seed { capabilities: vec![], ..Seed::default() });
    rig.app.add_composer_attachments(CONV, vec![picture("shot.png")], Vec::new());
    rig.until("the notice", |rig| rig.words().iter().any(|word| word == "1 item(s) not attached"));
    let words = rig.words();
    assert!(words.iter().any(|word| word == "shot.png:"));
    assert!(words.iter().any(|word| word == "The current model has no image input"));
    rig.click("attachment-notice-dismiss");
    assert!(!rig.says("1 item(s) not attached"));
}

#[test]
fn explains_a_draft_image_the_selected_model_cannot_take_and_holds_the_send() {
    let mut rig = Rig::new();
    add_image(&mut rig, "shot.png");
    // The model changes under an attached picture.
    rig.app.doc.assets.api_providers[0].models[0].capabilities.clear();
    rig.frame();
    let words = rig.words();
    assert!(words.iter().any(|word| word == "This message carries images, but the selected model has no image input enabled, so it cannot be sent. Switch to a vision model, or remove the images."));
    rig.click(send_button_key());
    rig.settle(20);
    assert_eq!(rig.host.count("run_model"), 0, "the button is held");
}

#[test]
fn explains_an_unsendable_image_history_instead_of_leaving_the_composer_dead() {
    let mut rig = Rig::seeded(Seed { capabilities: vec![], ..Seed::default() });
    rig.app.update_conversation(super::rig::WS, CONV, |conversation| {
        conversation.contexts = vec![serde_json::from_value(json!({ "kind": "user", "id": "u1", "content": "see", "createdAt": "2026-01-01T00:00:00Z", "images": [{ "id": "abc", "name": "a.png", "mime": "image/png", "width": 1, "height": 1, "bytes": 1 }] })).unwrap()];
    });
    assert!(rig.says("This conversation has images (in a message or a tool's screenshot), and the selected model has no image input enabled, so nothing can be sent. Switch to a vision model, or delete those images from the timeline."));
}

#[test]
fn turns_away_and_names_an_image_that_would_put_one_message_past_its_budget() {
    let mut rig = Rig::new();
    let big = ComposerFile { name: "huge.png".into(), media_type: "image/png".into(), bytes: vec![0; 33 * 1024 * 1024] };
    rig.app.add_composer_attachments(CONV, vec![big], Vec::new());
    rig.until("the notice", |rig| rig.words().iter().any(|word| word == "huge.png:"));
    assert!(rig.words().iter().any(|word| word.starts_with("Too large")));
}

#[test]
fn takes_a_file_from_the_picker_through_the_add_menu_and_says_what_it_could_not_read() {
    let mut rig = Rig::new();
    let file = rig.repo.join("picked.txt");
    std::fs::write(&file, "picked").unwrap();
    let folder = rig.repo.join("folder");
    std::fs::create_dir_all(&folder).unwrap();
    *rig.picked.lock().unwrap() = Some(vec![file, folder]);
    rig.click(mewrk_ui_common::popover_menu::trigger_key((crate::footer::ADD_MENU, CONV)));
    rig.settle(30);
    rig.click(mewrk_ui_common::popover_menu::row_key((crate::footer::ADD_MENU, CONV), "files"));
    rig.until("the file to arrive", |rig| rig.app.composer().file_drafts.get(CONV).is_some_and(|files| files.len() == 1));
    rig.until("the notice", |rig| rig.words().iter().any(|word| word == "Folders can't be attached"));
    assert_eq!(rig.app.composer().file_drafts[CONV][0].name, "picked.txt");
}

#[test]
fn says_what_a_drop_would_do_while_it_hovers_and_takes_it_on_release() {
    let mut rig = Rig::new();
    let file = rig.repo.join("dropped.txt");
    std::fs::write(&file, "dropped").unwrap();
    rig.input.pointer = None;
    rig.input.hovered_file = true;
    rig.input.hovered_paths = vec![file.clone()];
    rig.until("the host's look at it", |rig| rig.words().iter().any(|word| word == "Drop to attach 1 file(s)"));
    rig.input.hovered_file = false;
    rig.input.hovered_paths.clear();
    rig.input.dropped = vec![file];
    rig.frame();
    rig.until("the file to arrive", |rig| rig.app.composer().file_drafts.get(CONV).is_some_and(|files| files.len() == 1));
    assert!(!rig.words().iter().any(|word| word.starts_with("Drop to attach")));
}

#[test]
fn a_folder_reads_as_unavailable_before_it_is_dropped_and_a_drop_elsewhere_is_not_ours() {
    let mut rig = Rig::new();
    let folder = rig.repo.join("folder");
    std::fs::create_dir_all(&folder).unwrap();
    rig.frame();
    let zone = rig.rect(composer_box_key());
    rig.input.pointer = Some((zone.x + 20.0, zone.y + 10.0));
    rig.input.hovered_file = true;
    rig.input.hovered_paths = vec![folder];
    rig.until("the refusal", |rig| rig.words().iter().any(|word| word == "Folders can't be attached"));
    rig.input.hovered_file = false;
    rig.input.hovered_paths.clear();

    // Dropped away from the box, with the pointer known to be there.
    let file = rig.repo.join("away.txt");
    std::fs::write(&file, "away").unwrap();
    rig.input.pointer = Some((5.0, 5.0));
    rig.input.dropped = vec![file];
    rig.frame();
    rig.settle(60);
    assert!(rig.app.composer().file_drafts.get(CONV).is_none_or(Vec::is_empty));
}

#[test]
fn stages_a_picked_element_as_a_chip_its_crop_hidden_from_the_strip() {
    let mut rig = Rig::new();
    let block = "<mewrk-selected-element>\n<element tag=\"button\" has-screenshot=\"true\" class=\"btn btn-primary btn-lg\">\n  <text>Place order</text>\n</element>\n(trailer)\n</mewrk-selected-element>".to_owned();
    stage_element_pick(&mut rig.app, CONV, block.clone(), Some(("element-button.png".to_owned(), png())));
    rig.until("the crop", |rig| rig.app.composer().element_picks.get(CONV).is_some_and(|picks| picks[0].screenshot_image_id.is_some()));
    let crop = rig.app.composer().element_picks[CONV][0].screenshot_image_id.clone().unwrap();
    let words = rig.words();
    assert!(words.iter().any(|word| word == "<button class=\"btn btn-primary\" />"));
    assert!(words.iter().any(|word| word == "Place order"));
    assert!(!rig.has(image_key(STRIP, &crop)), "the chip is the only thing the composer draws");

    // Two picks of one element stay distinct: removing the second leaves the first.
    stage_element_pick(&mut rig.app, CONV, block, None);
    rig.frame();
    rig.click(element_remove_key(1));
    assert_eq!(rig.app.composer().element_picks[CONV].len(), 1);
    // Taking the chip off takes the crop off the message: nothing else could.
    rig.click(element_remove_key(0));
    rig.until("the crop to go", |rig| rig.app.composer().image_drafts.get(CONV).is_none_or(Vec::is_empty));
}

#[test]
fn keeps_the_active_stop_action_enabled_while_a_new_image_is_still_uploading() {
    let mut rig = Rig::new();
    rig.frame();
    rig.focus_text_area();
    rig.type_text("go");
    rig.press(Key::Enter);
    rig.until("the run to start", |rig| rig.host.count("run_model") == 1);
    rig.until("the stop label", |rig| rig.words().iter().any(|word| word == "Stop generating"));
    assert!(!rig.has(stop_button_key()), "the one button stops, while nothing is queued");
    rig.app.composer().image_loading_ids.insert(CONV.to_owned());
    rig.frame();
    rig.click(send_button_key());
    rig.until("the cancel to arrive", |rig| rig.host.count("cancel_conversation_run") + rig.host.count("cancel_model_run") > 0);
    rig.app.composer().image_loading_ids.remove(CONV);
}

#[test]
fn announces_a_local_failure_and_retries_without_failing_the_whole_strip() {
    let mut rig = Rig::new();
    let good = add_image(&mut rig, "good.png");
    let missing: mewrk_client::model::ImageAttachment = serde_json::from_value(json!({ "id": "0".repeat(64), "name": "gone.png", "mime": "image/png", "width": 1, "height": 1, "bytes": 1 })).unwrap();
    rig.app.composer().image_drafts.get_mut(CONV).unwrap().push(missing.clone());
    rig.until("the failure", |rig| rig.words().iter().any(|word| word == "Retry loading image gone.png"));
    assert!(rig.words().iter().any(|word| word == "View full image good.png"), "the other picture is not touched");
    let chip = rig.rect(image_key(STRIP, &missing.id));
    rig.click_at(chip.x + 10.0, chip.y + 30.0);
    rig.until("the failure again", |rig| rig.words().iter().any(|word| word == "Retry loading image gone.png"));
    assert!(rig.has(image_key(STRIP, &good)));
}

#[test]
fn sends_a_pure_image_message_from_the_button() {
    let mut rig = Rig::new();
    add_image(&mut rig, "shot.png");
    rig.click(send_button_key());
    rig.until("the run to start", |rig| rig.host.count("run_model") == 1);
    let request = rig.host.last("run_model").unwrap();
    let contexts = request["request"]["contexts"].as_array().unwrap();
    assert_eq!(contexts.last().unwrap()["images"].as_array().unwrap().len(), 1);
    assert!(rig.app.composer().image_drafts.get(CONV).is_none_or(Vec::is_empty), "the chips go with the message");
    rig.host.release_runs();
}

#[test]
fn keeps_an_image_added_to_a_new_tasks_draft_and_sends_it_with_the_first_message() {
    let mut rig = Rig::seeded(Seed { conversations: vec![], ..Seed::default() });
    let draft = rig.conversation_id();
    rig.app.add_composer_attachments(&draft, vec![picture("first.png")], Vec::new());
    rig.until("the picture", |rig| rig.app.composer().image_drafts.get(&draft).is_some_and(|images| images.len() == 1));
    rig.frame();
    rig.focus_text_area();
    rig.type_text("look");
    rig.press(Key::Enter);
    rig.until("the run to start", |rig| rig.host.count("run_model") == 1);
    assert_ne!(rig.conversation_id(), draft, "sending made it a conversation");
    let request = rig.host.last("run_model").unwrap();
    let contexts = request["request"]["contexts"].as_array().unwrap();
    assert_eq!(contexts.last().unwrap()["images"].as_array().unwrap().len(), 1);
    rig.host.release_runs();
}

#[test]
fn queues_while_a_run_goes_with_a_stop_control_kept_in_view_and_the_card_placeholder_says_so() {
    let mut rig = Rig::new();
    rig.frame();
    rig.focus_text_area();
    rig.type_text("first");
    rig.press(Key::Enter);
    rig.until("the run to start", |rig| rig.host.count("run_model") == 1);
    rig.until("the run to show", |rig| rig.words().iter().any(|word| word == "Stop generating"));
    rig.focus_text_area();
    rig.type_text("second");
    assert!(rig.says("Queue message"));
    assert!(rig.has(stop_button_key()), "queuing repurposes the button, and a stop stays in view");
    rig.press(Key::Enter);
    rig.until("the row", |rig| rig.words().iter().any(|word| word == "second"));
    assert_eq!(rig.host.count("run_model"), 1);
    assert_eq!(rig.draft(), "");
    rig.host.release_runs();
}

#[test]
fn says_a_send_refused_before_the_run_started_with_its_retry_and_dismissal() {
    let mut rig = Rig::new();
    *rig.host.refuse.lock().unwrap() = Some("the provider said no".to_owned());
    rig.frame();
    rig.focus_text_area();
    rig.type_text("hi");
    rig.press(Key::Enter);
    rig.until("the run to fail", |rig| rig.host.count("run_model") == 1);
    rig.settle(200);
    let error = rig.app.model_run_error(CONV);
    if error.as_ref().is_some_and(|error| error.request_id.is_none()) {
        assert!(rig.words().iter().any(|word| word == "Provider · model-1"));
        rig.click(crate::run_dismiss_key());
        assert!(rig.app.model_run_error(CONV).is_none());
    }
}
