//! `DraftLanding.test.tsx` through the frame, and the cat of `App.newConversation.test.tsx`.

use super::rig::{conversation, Rig, Seed, CONV, WS};
use crate::landing::{draft_landing_view, task_key, DraftLandingProps};
use crate::draft_landing;
use mewrk_client::prelude::*;
use mewrk_client::runs::ConversationStatus;
use mewrk_client::selection::{DraftLandingTask, DRAFT_LANDING_TASK_LIMIT};
use std::cell::RefCell;
use std::collections::BTreeMap;

pub(super) fn props(tasks: Vec<DraftLandingTask>, can_insert: bool, opened: Rc<RefCell<Vec<String>>>) -> DraftLandingProps {
    DraftLandingProps { recent_tasks: tasks, shortcuts: BTreeMap::new(), send_shortcut: vec!["Enter".to_owned()], newline_shortcut: vec!["Shift".to_owned(), "Enter".to_owned()], can_insert, now: Some(0), on_open_task: Rc::new(move |_, id| opened.borrow_mut().push(id.to_owned())) }
}

pub(super) fn task(id: &str) -> DraftLandingTask {
    DraftLandingTask { id: id.to_owned(), title: format!("Title {id}"), updated_at: "1970-01-01T00:00:00Z".to_owned(), status: ConversationStatus::Idle }
}

#[test]
fn says_a_right_click_inserts_a_context_where_the_timeline_accepts_edits() {
    let mut rig = Rig::new().with_view(|_| draft_landing_view(props(vec![], true, Rc::default())));
    let words = rig.words();
    assert!(words.iter().any(|said| said == "Mew. Work."));
    assert!(words.iter().any(|said| said == "Say what to do below, or right-click to hand-write context."));
    assert!(words.iter().any(|said| said == "Insert context"));
    assert!(words.iter().any(|said| said == "Right-click"));
    assert!(!words.iter().any(|said| said == "Recent tasks"), "no section without tasks, and still the shortcuts");
    assert!(words.iter().any(|said| said == "Keyboard shortcuts"));
}

#[test]
fn says_nothing_of_right_click_on_a_timeline_that_cannot_take_an_insert() {
    let mut locked = Rig::new().with_view(|_| draft_landing_view(props(vec![], false, Rc::default())));
    let words = locked.words();
    assert!(words.iter().any(|said| said == "Say what to do below."));
    assert!(!words.iter().any(|said| said == "Insert context"));
}

#[test]
fn draws_the_tasks_in_the_order_it_is_handed_them_at_most_the_limit_and_opens_the_one_clicked() {
    let opened: Rc<RefCell<Vec<String>>> = Rc::default();
    let sink = opened.clone();
    let tasks: Vec<DraftLandingTask> = (0..DRAFT_LANDING_TASK_LIMIT + 3).map(|index| task(&format!("t{index}"))).collect();
    let mut rig = Rig::new().with_view(move |_| draft_landing_view(props(tasks.clone(), true, sink.clone())));
    rig.settle(10);
    assert!(rig.has(task_key("t0")));
    assert!(rig.has(task_key(&format!("t{}", DRAFT_LANDING_TASK_LIMIT - 1))));
    assert!(!rig.has(task_key(&format!("t{DRAFT_LANDING_TASK_LIMIT}"))));
    assert!(rig.rect(task_key("t0")).y < rig.rect(task_key("t1")).y);
    rig.click(task_key("t1"));
    assert_eq!(*opened.borrow(), ["t1"]);
}

#[test]
fn marks_each_rows_status_names_the_labelled_ones_and_shows_each_age() {
    let tasks = vec![DraftLandingTask { status: ConversationStatus::Running, ..task("a") }, DraftLandingTask { status: ConversationStatus::Blocked, updated_at: "invalid".to_owned(), ..task("b") }, DraftLandingTask { status: ConversationStatus::Completed, ..task("c") }];
    let mut rig = Rig::new().with_view(move |_| draft_landing_view(props(tasks.clone(), true, Rc::default())));
    let words = rig.words();
    for word in ["In progress", "Waiting for you", "Finished", "Just now"] {
        assert!(words.iter().any(|said| said == word), "{word} in {words:?}");
    }
}

#[test]
fn shows_the_keys_the_composer_is_actually_using() {
    let mut rig = Rig::new().with_view(|_| {
        let mut landing = props(vec![], true, Rc::default());
        landing.send_shortcut = vec!["Shift".to_owned(), "Enter".to_owned()];
        landing.newline_shortcut = vec!["Enter".to_owned()];
        draft_landing_view(landing)
    });
    let words = rig.words();
    assert!(words.iter().any(|said| said == "Send"));
    assert!(words.iter().any(|said| said == "New line"));
}

#[test]
fn labels_the_whole_page_in_chinese_under_a_chinese_preference() {
    let mut rig = Rig::seeded(Seed { english: false, ..Seed::default() }).with_view(|_| draft_landing_view(props(vec![task("a")], true, Rc::default())));
    let words = rig.words();
    for word in ["喵。开工。", "最近任务", "快捷键", "发送", "换行", "插入上下文"] {
        assert!(words.iter().any(|said| said == word), "{word} in {words:?}");
    }
}

#[test]
fn lists_the_projects_sent_tasks_on_its_new_task_draft_most_recent_first_and_opens_the_one_clicked() {
    let mut older = conversation("older", "Older");
    older["updatedAt"] = json!("2026-01-01T00:00:00.000Z");
    older["contexts"] = json!([{ "kind": "user", "id": "u1", "content": "hi", "createdAt": "2026-01-01T00:00:00Z" }]);
    let mut newer = conversation("newer", "Newer");
    newer["updatedAt"] = json!("2026-02-01T00:00:00.000Z");
    newer["contexts"] = json!([{ "kind": "user", "id": "u2", "content": "hi", "createdAt": "2026-01-01T00:00:00Z" }]);
    let mut rig = Rig::seeded(Seed { conversations: vec![older, newer], ..Seed::default() });
    rig.app.open_draft_conversation(Some(WS));
    rig = rig.with_view(|app| draft_landing(app));
    rig.settle(10);
    assert!(rig.rect(task_key("newer")).y < rig.rect(task_key("older")).y);
    rig.click(task_key("older"));
    assert_eq!(rig.app.active_conversation_id().as_deref(), Some("older"));
}

#[test]
fn sends_the_cat_away_once_the_conversation_holds_content_and_back_on_a_new_task() {
    let mut rig = Rig::new();
    rig.settle(10);
    assert!(rig.has("composer-cat"), "an unsent conversation is the same empty desk");
    rig.app.update_conversation(WS, CONV, |conversation| {
        conversation.contexts = vec![serde_json::from_value(json!({ "kind": "user", "id": "u1", "content": "hi", "createdAt": "2026-01-01T00:00:00Z" })).unwrap()];
    });
    rig.settle(10);
    assert!(!rig.has("composer-cat"));
    rig.app.open_draft_conversation(Some(WS));
    rig.settle(10);
    assert!(rig.has("composer-cat"));
}

#[test]
fn a_started_conversation_has_no_landing() {
    let mut started = conversation(CONV, "Started");
    started["contexts"] = json!([{ "kind": "user", "id": "u1", "content": "hi", "createdAt": "2026-01-01T00:00:00Z" }]);
    let mut rig = Rig::seeded(Seed { conversations: vec![started], ..Seed::default() }).with_view(|app| draft_landing(app));
    assert!(!rig.says("Mew. Work."));
}
