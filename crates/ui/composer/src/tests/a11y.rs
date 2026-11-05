//! What a screen reader is told about the composer and the cards docked on it, by upstream's `aria-*` and `role`s.

use super::docks::{prompt, question, queue, two_questions, with_prompts};
use super::landing::{props, task};
use super::rig::{Rig, CONV};
use crate::element_chips::element_remove_key;
use crate::landing::draft_landing_view;
use crate::meter::{compact_row_key, context_usage_meter, meter_trigger_key, ContextUsageCounts, ContextUsageProps};
use crate::question_dock::tab_key;
use accesskit::{Action, Node, NodeId};
use mewrk_client::model::{FileAttachment, FileAttachmentFormat};
use mewrk_client::prelude::*;
use mewrk_client::runs::{AttachmentRejection, AttachmentRejectionReason, ElementPick, PromptKind};

type Tree = Vec<(NodeId, Node)>;

/// All of it, as a client that has just arrived would get it.
fn read(rig: &mut Rig) -> Tree {
    set_a11y_active(true);
    rig.frame();
    rig.frame();
    rig.frame();
    take_a11y_update().expect("a client gets the tree").nodes
}

fn named<'a>(tree: &'a Tree, name: &str) -> &'a Node {
    tree.iter().map(|(_, node)| node).find(|node| node.label() == Some(name) || (node.role() == Role::Label && node.value() == Some(name))).unwrap_or_else(|| panic!("nothing says {name:?} in {:?}", tree.iter().map(|(_, node)| (node.role(), node.label(), node.value())).collect::<Vec<_>>()))
}

fn with_role(tree: &Tree, role: Role) -> Vec<&Node> {
    tree.iter().map(|(_, node)| node).filter(|node| node.role() == role).collect()
}

fn role_named<'a>(tree: &'a Tree, role: Role, name: &str) -> &'a Node {
    with_role(tree, role).into_iter().find(|node| node.label() == Some(name)).unwrap_or_else(|| panic!("no {role:?} says {name:?}"))
}

fn id_of(tree: &Tree, name: &str) -> NodeId {
    tree.iter().find(|(_, node)| node.label() == Some(name) || (node.role() == Role::Label && node.value() == Some(name))).unwrap_or_else(|| panic!("nothing says {name:?}")).0
}

/// The sweep: a Tab stop or a press target with no name is a bug.
fn all_named(tree: &Tree) {
    for (_, node) in tree {
        if !(node.supports_action(Action::Click) || node.supports_action(Action::Focus)) {
            continue;
        }
        let said = node.label().is_some_and(|words| !words.trim().is_empty()) || !node.labelled_by().is_empty() || (node.role() == Role::Label && node.value().is_some());
        assert!(said, "a {:?} says nothing: {node:?}", node.role());
    }
}

#[test]
fn the_box_and_the_row_under_it_say_what_they_are() {
    let mut rig = Rig::new();
    let tree = read(&mut rig);
    assert_eq!(named(&tree, "Message the Agent").role(), Role::MultilineTextInput);
    assert_eq!(named(&tree, "Send").role(), Role::Button);
    let plan = named(&tree, "Plan");
    assert_eq!((plan.role(), plan.toggled()), (Role::Button, Some(accesskit::Toggled::False)));
    assert!(plan.description().is_some_and(|words| words.starts_with("Turn on plan mode")), "its title");
    assert_eq!(named(&tree, "Add content").has_popup(), Some(HasPopup::Menu));
    assert_eq!(named(&tree, "Security level: Manual").is_expanded(), Some(false));
    assert!(tree.iter().any(|(_, node)| node.label().is_some_and(|name| name.starts_with("Model: "))));
    assert!(tree.iter().any(|(_, node)| node.label().is_some_and(|name| name.starts_with("Reasoning effort: "))));
    let ring = tree.iter().map(|(_, node)| node).find(|node| node.label().is_some_and(|name| name.starts_with("Context usage: "))).expect("the ring");
    assert_eq!((ring.has_popup(), ring.is_expanded()), (Some(HasPopup::Dialog), Some(false)));
    all_named(&tree);
    set_a11y_active(false);
}

#[test]
fn the_plan_switch_says_it_is_on() {
    let mut rig = Rig::new();
    rig.app.update_conversation(super::rig::WS, CONV, |conversation| conversation.settings.plan_mode_enabled = true);
    let tree = read(&mut rig);
    assert_eq!(named(&tree, "Plan").toggled(), Some(accesskit::Toggled::True));
    set_a11y_active(false);
}

#[test]
fn an_approval_card_is_a_dialog_named_by_its_heading_with_a_live_region_for_its_answer() {
    let mut rig = with_prompts(vec![prompt("p1", PromptKind::Tool), prompt("p2", PromptKind::Tool)]);
    let tree = read(&mut rig);
    let heading = id_of(&tree, "Your approval is needed");
    assert_eq!((named(&tree, "Your approval is needed").role(), named(&tree, "Your approval is needed").level()), (Role::Heading, Some(2)));
    let dialog = with_role(&tree, Role::Dialog);
    assert_eq!(dialog.len(), 1);
    assert_eq!(dialog[0].labelled_by(), [heading]);
    for name in ["Deny", "Always allow", "Allow", "Previous pending approval", "Next pending approval"] {
        assert_eq!(named(&tree, name).role(), Role::Button, "{name}");
    }
    let status = with_role(&tree, Role::Status);
    assert_eq!(status.len(), 1, "the live region is there before it has anything to say");
    assert_eq!(status[0].live(), Some(Live::Polite));
    all_named(&tree);
    set_a11y_active(false);
}

#[test]
fn a_plan_card_names_its_feedback_box() {
    let mut rig = with_prompts(vec![prompt("p1", PromptKind::PlanExit)]);
    let tree = read(&mut rig);
    assert_eq!(named(&tree, "Plan feedback").role(), Role::MultilineTextInput);
    assert!(named(&tree, "Send feedback").is_disabled(), "nothing written yet");
    all_named(&tree);
    set_a11y_active(false);
}

#[test]
fn a_question_card_has_tabs_and_options_and_says_which_is_chosen() {
    let mut rig = with_prompts(vec![question("q1", two_questions()), question("q2", two_questions())]);
    let tree = read(&mut rig);
    let heading = id_of(&tree, "One?");
    assert_eq!(named(&tree, "One?").role(), Role::Heading);
    assert_eq!(with_role(&tree, Role::Dialog)[0].labelled_by(), [heading]);
    let tabs = named(&tree, "Questions");
    assert_eq!(tabs.role(), Role::TabList);
    let (first, second) = (named(&tree, "H1"), named(&tree, "H2"));
    assert_eq!((first.role(), first.is_selected()), (Role::Tab, Some(true)));
    assert_eq!(second.is_selected(), Some(false));
    assert_eq!(named(&tree, "Submit").role(), Role::Tab, "without its check mark");
    assert_eq!(named(&tree, "Previous question").role(), Role::Button, "not its arrow");
    assert_eq!(named(&tree, "Previous card").role(), Role::Button);
    assert_eq!(named(&tree, "Close questions").role(), Role::Button);
    let list = with_role(&tree, Role::ListBox);
    assert_eq!(list[0].labelled_by(), [heading]);
    let option = named(&tree, "A first");
    assert_eq!((option.role(), option.is_selected()), (Role::ListBoxOption, Some(false)));
    let other = named(&tree, "Choose Other");
    assert_eq!((other.role(), other.toggled()), (Role::RadioButton, Some(accesskit::Toggled::False)));
    assert_eq!(named(&tree, "Other").role(), Role::TextInput);
    all_named(&tree);

    // An answered question says so, and the review page warns of what is left.
    rig.click(crate::question_dock::row_key("q1", 0, 1));
    rig.settle(10);
    let tree = read(&mut rig);
    assert_eq!(named(&tree, "H1 (answered)").role(), Role::Tab);
    rig.click(tab_key("q1", 2));
    let tree = read(&mut rig);
    assert_eq!(named(&tree, "Review your answers").role(), Role::Heading);
    assert_eq!(named(&tree, "You have not answered all questions").role(), Role::Status);
    assert_eq!(named(&tree, "Submit answers").role(), Role::Button);
    all_named(&tree);
    set_a11y_active(false);
}

#[test]
fn a_multiple_choice_question_is_a_group_of_checkboxes() {
    let mut rig = with_prompts(vec![question("q1", json!([{ "question": "Many?", "multiSelect": true, "options": ["A", "B"] }]))]);
    let tree = read(&mut rig);
    assert!(with_role(&tree, Role::ListBox).is_empty());
    assert_eq!(with_role(&tree, Role::Group).iter().filter(|group| group.labelled_by() == [id_of(&tree, "Many?")]).count(), 1);
    let option = named(&tree, "A");
    assert_eq!((option.role(), option.toggled()), (Role::CheckBox, Some(accesskit::Toggled::False)));
    all_named(&tree);
    set_a11y_active(false);
}

#[test]
fn a_question_with_previews_keeps_its_options_a_listbox_and_names_its_notes() {
    let mut rig = with_prompts(vec![question("q1", json!([{ "question": "Design?", "options": [{ "label": "A", "preview": "**one**" }, { "label": "B", "preview": "two" }] }]))]);
    rig.press(Key::Char('n'));
    let tree = read(&mut rig);
    assert_eq!(with_role(&tree, Role::ListBox).len(), 1);
    assert_eq!(named(&tree, "A").role(), Role::ListBoxOption);
    all_named(&tree);
    set_a11y_active(false);
}

#[test]
fn queued_messages_are_a_named_list_whose_buttons_say_which_message() {
    let mut rig = Rig::new();
    queue(&mut rig, json!([{ "id": "m1", "content": "hello there", "createdAt": "2026-01-01T00:00:00.000Z" }]));
    let tree = read(&mut rig);
    assert_eq!(named(&tree, "Queued messages").role(), Role::Region);
    assert_eq!(with_role(&tree, Role::ListItem).len(), 1);
    let delete = named(&tree, "Delete queued message “hello there”");
    assert_eq!((delete.role(), delete.description()), (Role::Button, Some("Delete queued message")));
    assert!(named(&tree, "Steer “hello there” into the current turn").is_disabled(), "no run to steer into");
    all_named(&tree);
    set_a11y_active(false);
}

fn meter_props() -> ContextUsageProps {
    ContextUsageProps { tokens: 180_000, context_window: Some(1_000_000), counts: ContextUsageCounts::default(), auto_compact: Some(mewrk_client::auto_compact::default_auto_compact_settings()), on_auto_compact_change: Some(Rc::new(|_, _| {})), native_compaction_available: true, on_compaction_method_change: Some(Rc::new(|_, _| {})), ..ContextUsageProps::default() }
}

#[test]
fn the_context_panel_is_a_dialog_and_its_submenu_a_group_of_tabs_and_a_slider() {
    let mut rig = Rig::new().with_view(|_| row().p(200.0).justify_end().child(context_usage_meter(meter_props())));
    set_a11y_active(true);
    rig.frame();
    rig.click(meter_trigger_key());
    rig.frame();
    let tree = read(&mut rig);
    assert_eq!(named(&tree, "Context window usage").role(), Role::Dialog);
    let row_name = "Auto-compact Handoff 80%";
    assert_eq!((named(&tree, row_name).has_popup(), named(&tree, row_name).is_expanded()), (Some(HasPopup::Menu), Some(false)));
    all_named(&tree);

    rig.click(compact_row_key());
    rig.frame();
    let tree = read(&mut rig);
    assert_eq!(named(&tree, row_name).is_expanded(), Some(true));
    assert!(with_role(&tree, Role::Group).iter().any(|group| group.label() == Some("Auto-compact")));
    assert!(with_role(&tree, Role::Switch).iter().any(|switch| switch.label() == Some("Enable auto-compact")));
    assert_eq!(named(&tree, "Compaction method").role(), Role::TabList);
    let handoff = with_role(&tree, Role::Tab).into_iter().find(|tab| tab.label() == Some("Handoff")).expect("the tab");
    assert_eq!(handoff.is_selected(), Some(true));
    assert!(with_role(&tree, Role::TabPanel).iter().any(|panel| panel.label() == Some("Handoff")));
    let slider = role_named(&tree, Role::Slider, "Handoff threshold");
    assert_eq!((slider.role(), slider.numeric_value(), slider.min_numeric_value(), slider.max_numeric_value()), (Role::Slider, Some(80.0), Some(20.0), Some(97.0)));
    assert!(slider.supports_action(Action::Increment) && slider.supports_action(Action::Focus));
    assert_eq!(named(&tree, "Handoff threshold (percent)").role(), Role::TextInput);
    assert_eq!(named(&tree, "Learn more").role(), Role::Link);
    assert!(!tree.iter().any(|(_, node)| node.value() == Some("%") || node.value() == Some("20%")), "the scale is for the eye");
    all_named(&tree);
    set_a11y_active(false);
}

#[test]
fn what_went_wrong_is_an_alert_and_what_is_happening_a_status() {
    let mut rig = Rig::new();
    rig.app.composer().attachment_notices.insert(CONV.to_owned(), vec![AttachmentRejection { name: Some("big.bin".to_owned()), reason: AttachmentRejectionReason::Unsupported }]);
    rig.app.show_timeline_notice(None, "Deleted 2 messages", Some(("⌘Z".to_owned(), "undo".to_owned())), 5000);
    let tree = read(&mut rig);
    let alert = with_role(&tree, Role::Alert);
    assert_eq!(alert.len(), 1);
    assert!(alert[0].label().is_some_and(|words| words.starts_with("1 item(s) not attached") && words.contains("big.bin")), "{:?}", alert[0].label());
    assert_eq!(alert[0].live(), Some(Live::Assertive));
    assert_eq!(named(&tree, "Dismiss").role(), Role::Button);
    let status = with_role(&tree, Role::Status);
    assert!(status.iter().any(|node| node.label() == Some("Deleted 2 messages ⌘Z undo")), "{status:?}");
    all_named(&tree);
    set_a11y_active(false);
}

#[test]
fn a_send_refused_before_the_run_started_is_an_alert() {
    let mut rig = Rig::new();
    let error_model = rig.app.doc.assets.api_providers[0].models[0].id.clone();
    rig.app.composer().image_drafts.insert(CONV.to_owned(), vec![serde_json::from_value(json!({ "id": "i1", "name": "a.png", "mime": "image/png", "width": 1, "height": 1, "bytes": 1 })).unwrap()]);
    rig.app.doc.assets.api_providers[0].models[0].capabilities.clear();
    let tree = read(&mut rig);
    let alert = with_role(&tree, Role::Alert);
    assert_eq!(alert.len(), 1, "{tree:?}");
    assert!(alert[0].label().is_some_and(|words| words.contains(&error_model) && words.contains("no image input")));
    set_a11y_active(false);
}

#[test]
fn picked_elements_and_attachments_are_lists_that_say_how_to_take_each_off() {
    let mut rig = Rig::new();
    rig.app.set_composer_element_picks(CONV, vec![ElementPick { block: "<element tag=\"a\" has-screenshot=\"false\">".to_owned(), screenshot_image_id: None }]);
    rig.app.composer().file_drafts.insert(CONV.to_owned(), vec![FileAttachment { id: "f1".into(), name: "notes.md".into(), format: FileAttachmentFormat::Text, bytes: 2048, tokens: 1, pages: None }]);
    let tree = read(&mut rig);
    assert_eq!(named(&tree, "Selected page elements").role(), Role::List);
    assert_eq!(named(&tree, "Remove <a />").role(), Role::Button);
    assert_eq!(named(&tree, "1 attachments").role(), Role::List);
    let open = named(&tree, "Preview file notes.md");
    assert_eq!(open.role(), Role::Button);
    assert!(open.description().is_some_and(|words| words.starts_with("notes.md")), "its title");
    assert_eq!(named(&tree, "Remove file notes.md").role(), Role::Button);
    all_named(&tree);
    rig.click(element_remove_key(0));
    set_a11y_active(false);
}

#[test]
fn the_landing_page_names_its_sections_and_a_task_says_its_state() {
    let mut running = task("t1");
    running.status = mewrk_client::runs::ConversationStatus::Running;
    let tasks = vec![running, task("t2")];
    let mut rig = Rig::new().with_view(move |_| draft_landing_view(props(tasks.clone(), true, Rc::default())));
    let tree = read(&mut rig);
    assert_eq!(named(&tree, "Mew. Work.").role(), Role::Heading);
    assert!(with_role(&tree, Role::Region).iter().any(|region| region.label() == Some("Recent tasks")));
    assert!(with_role(&tree, Role::Region).iter().any(|region| region.label() == Some("Keyboard shortcuts")));
    assert!(with_role(&tree, Role::Heading).iter().any(|heading| heading.label() == Some("Recent tasks")), "not in capitals");
    let row = tree.iter().map(|(_, node)| node).find(|node| node.label().is_some_and(|name| name.contains("Title t1"))).expect("the task");
    assert_eq!(row.role(), Role::Button);
    assert!(row.label().is_some_and(|name| name.starts_with("In progress")), "{:?}", row.label());
    all_named(&tree);
    set_a11y_active(false);
}
