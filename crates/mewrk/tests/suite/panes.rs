//! `SidePane` around real panes, and their dividers.

use mewrk_client::prelude::*;
use mewrk_client::side_panes::{preview_sessions_for, side_pane_layout_for, SidePanesAction};
use mewrk_pane_browser::browser::BrowserStatus;
use mewrk_pane_browser::controller::BrowserController;
use mewrk_shell::actions::{close_pane, open_pane, preview_tabs};
use mewrk_shell::pane_tiles::GAP;

use crate::rig::*;

#[test]
fn panes_open_as_tiles_beside_the_chat_and_close_from_their_title_bar() {
    let (mut rig, mut app) = Rig::new("tiles");
    let id = rig.add_conversation(&mut app, "ws1", "first");
    rig.click(&mut app, ("conversation-main", id.as_str()));
    open_pane(&mut app, "tasks");
    open_pane(&mut app, "settings");
    rig.frames(&mut app, 3);
    let chat = rig.rect(&mut app, "pane-tiles-chat");
    let tasks = rig.rect(&mut app, ("pane-tile", "tasks"));
    let settings = rig.rect(&mut app, ("pane-tile", "settings"));
    assert!(tasks.x >= chat.right() + GAP - 0.5, "{chat:?} {tasks:?}");
    assert!(settings.x >= chat.right() + GAP - 0.5);
    // A tile on the window's edge runs to it.
    let far = tasks.right().max(settings.right());
    assert!((far - rig.win.0).abs() < 1.0, "{far}");
    rig.click(&mut app, ("side-pane-close", "tasks"));
    let layout = side_pane_layout_for(app.side_panes(), Some(&id)).clone();
    assert_eq!(layout.panes, vec!["settings".to_owned()]);
}

#[test]
fn dragging_the_side_divider_changes_the_split_and_escape_puts_it_back() {
    let (mut rig, mut app) = Rig::new("divider");
    let id = rig.add_conversation(&mut app, "ws1", "first");
    rig.click(&mut app, ("conversation-main", id.as_str()));
    open_pane(&mut app, "tasks");
    rig.frames(&mut app, 3);
    let handle = rig.rect(&mut app, ("pane-tiles-handle", "side"));
    let chat_before = rig.rect(&mut app, "pane-tiles-chat").w;
    let start = (handle.x + handle.w / 2.0, handle.y + 200.0);
    rig.press_and_move(&mut app, start, (start.0 - 100.0, start.1));
    rig.frames(&mut app, 2);
    let chat_after = rig.rect(&mut app, "pane-tiles-chat").w;
    assert!((chat_before - 100.0 - chat_after).abs() < 1.0, "the handle stays under the pointer: {chat_before} {chat_after}");
    assert!(rig.exists(&mut app, "resize-cursor"), "the drag keeps its cursor wherever the pointer goes");
    rig.plain(&mut app, Key::Escape);
    rig.frames(&mut app, 2);
    assert!(!rig.exists(&mut app, "resize-cursor"));
    assert!((rig.rect(&mut app, "pane-tiles-chat").w - chat_before).abs() < 1.0);
    rig.release(&mut app);
}

#[test]
fn a_pane_shown_alone_leaves_nothing_of_the_others_on_screen() {
    let (mut rig, mut app) = Rig::new("solo-covers");
    let id = rig.add_conversation(&mut app, "ws1", "first");
    rig.click(&mut app, ("conversation-main", id.as_str()));
    open_pane(&mut app, "tasks");
    open_pane(&mut app, "settings");
    rig.frames(&mut app, 2);
    let chat = rig.rect(&mut app, "pane-tiles-chat");
    // The lower half of the field, where the composer and the settings pane are and the alone pane's short list is not.
    let in_lower_field = |rig: &Rig| rig.glyphs().iter().filter(|(x, y)| *x >= chat.x && *y > chat.y + chat.h / 2.0).count();
    assert!(in_lower_field(&rig) > 0);

    rig.click(&mut app, ("side-pane-expand", "tasks"));
    rig.frames(&mut app, 2);
    assert_eq!(in_lower_field(&rig), 0);
    assert!(rig.exists(&mut app, ("pane-tile", "settings")), "it stays mounted, with its box");

    // Glass lets through whatever lies under a surface, so nothing but what is on show may lay any.
    app.doc.global_settings.appearance.liquid_glass = true;
    rig.frames(&mut app, 2);
    let (solo, sidebar) = (rig.rect(&mut app, ("pane-tile", "tasks")), rig.rect(&mut app, "sidebar"));
    let laid: Vec<_> = mewrk_shell::glass::surfaces().into_iter().map(|(r, _)| r).collect();
    assert!(laid.iter().all(|r| *r == solo || *r == sidebar), "{laid:?}");
    app.doc.global_settings.appearance.liquid_glass = false;

    rig.click(&mut app, ("side-pane-expand", "tasks"));
    rig.frames(&mut app, 2);
    assert!(in_lower_field(&rig) > 0, "they come back as they were");
}

#[test]
fn expanding_a_pane_lets_it_take_the_field_and_toggling_gives_it_back() {
    let (mut rig, mut app) = Rig::new("expand");
    let id = rig.add_conversation(&mut app, "ws1", "first");
    rig.click(&mut app, ("conversation-main", id.as_str()));
    open_pane(&mut app, "tasks");
    rig.frames(&mut app, 2);
    rig.click(&mut app, ("side-pane-expand", "tasks"));
    let tile = rig.rect(&mut app, ("pane-tile", "tasks"));
    assert!(tile.w > rig.win.0 - 264.0 - 20.0, "{tile:?}");
    assert!(side_pane_layout_for(app.side_panes(), Some(&id)).expanded.is_some());
    rig.click(&mut app, ("side-pane-expand", "tasks"));
    assert!(side_pane_layout_for(app.side_panes(), Some(&id)).expanded.is_none());
}

#[test]
fn a_press_inside_a_pane_marks_it_focused() {
    let (mut rig, mut app) = Rig::new("focus");
    let id = rig.add_conversation(&mut app, "ws1", "first");
    rig.click(&mut app, ("conversation-main", id.as_str()));
    open_pane(&mut app, "tasks");
    open_pane(&mut app, "settings");
    rig.frames(&mut app, 3);
    app.dispatch_side_panes(SidePanesAction::Focus { conversation_id: id.clone(), pane: "settings".to_owned() });
    let tasks = rig.rect(&mut app, ("side-pane-header", "tasks"));
    rig.click_at(&mut app, tasks.x + 40.0, tasks.y + 10.0);
    assert_eq!(side_pane_layout_for(app.side_panes(), Some(&id)).focused.as_deref(), Some("tasks"));
    close_pane(&mut app, "tasks");
    close_pane(&mut app, "settings");
}

#[test]
fn the_conversation_tile_is_the_chat_timeline_and_its_questions_are_windows_of_the_root() {
    use mewrk_ui_chat::handlers::{delete_context, TimelineEdits};
    let (mut rig, mut app) = Rig::new("timeline");
    let id = rig.add_conversation(&mut app, "ws1", "first");
    rig.click(&mut app, ("conversation-main", id.as_str()));
    rig.frames(&mut app, 4);
    assert!(rig.exists(&mut app, hash_key(("context-scroll", id.as_str()))), "the timeline's scroller");

    let item = app.conversation(&id).unwrap().contexts[0].clone();
    mewrk_shell::chat::edit_context(&mut app, item.id());
    rig.frames(&mut app, 2);
    assert!(app.local::<TimelineEdits>().editor.is_some(), "the edit shortcut opens the editor on that message");
    app.local::<TimelineEdits>().editor = None;

    // A delete that asks first asks in a window at the root, and Cancel leaves the message where it was.
    app.update_document_if(|doc| {
        doc.global_settings.appearance.confirm_message_delete = true;
        true
    });
    rig.frames(&mut app, 2);
    delete_context(&mut app, &item, false);
    rig.frames(&mut app, 2);
    assert!(app.local::<TimelineEdits>().confirmation.is_some());
    rig.click(&mut app, "timeline-confirm-cancel");
    assert!(app.local::<TimelineEdits>().confirmation.is_none());
    assert_eq!(app.conversation(&id).unwrap().contexts.len(), 1);
}

#[test]
fn a_subagents_pane_draws_its_transcript_on_the_same_surface_read_only() {
    let (mut rig, mut app) = Rig::new("transcript");
    let id = rig.add_conversation(&mut app, "ws1", "first");
    rig.click(&mut app, ("conversation-main", id.as_str()));
    let call: mewrk_client::model::ContextItem = serde_json::from_value(json!({
        "id": "tool-subagent", "kind": "tool", "toolName": "subagent", "input": { "task": "Check the routes", "name": "checker" },
        "result": { "success": true, "output": "done", "executedAt": "2026-07-14T01:31:00Z", "durationMs": 1 },
        "subagent": { "task": "Check the routes", "status": "completed", "updates": [], "contexts": [{ "id": "child-answer", "kind": "assistant", "content": "The routes are fine", "createdAt": "2026-07-14T01:31:00Z" }] },
        "createdAt": "2026-07-14T01:30:00Z"
    }))
    .expect("a subagent call");
    let conversation = id.clone();
    app.update_document_if(move |doc| {
        let conversation = doc.workspaces.iter_mut().flat_map(|workspace| workspace.conversations.iter_mut()).find(|item| item.id == conversation).expect("the conversation");
        conversation.contexts.push(call.clone());
        true
    });
    rig.frames(&mut app, 3);
    let (_, _, agents) = mewrk_shell::effects::active_subagents(&mut app, &id);
    let agent = agents.first().expect("the agent the call started").id.clone();
    mewrk_shell::actions::open_subagent_panel(&mut app, &agent);
    rig.frames(&mut app, 4);
    assert!(rig.exists(&mut app, ("pane-tile", "subagent")), "its pane");
    // A transcript's timeline goes by its first context.
    let timeline = format!("transcript:{}", agents[0].contexts[0].id());
    assert!(rig.exists(&mut app, hash_key(("context-scroll", timeline.as_str()))), "and the transcript in it");
}

#[test]
fn a_timeline_comes_back_where_it_was_left_and_a_new_one_opens_on_its_end() {
    let (mut rig, mut app) = Rig::new("scroll-memory");
    let long = |id: String| -> Vec<mewrk_client::model::ContextItem> {
        (0..120).map(|n| serde_json::from_value(json!({ "kind": "user", "id": format!("{id}-{n}"), "content": format!("message {n}"), "createdAt": "2026-01-01T00:00:00.000Z" })).unwrap()).collect()
    };
    let first = rig.add_conversation(&mut app, "ws1", "first");
    let second = rig.add_conversation(&mut app, "ws1", "second");
    let (one, two) = (first.clone(), second.clone());
    let (fill_one, fill_two) = (long(one.clone()), long(two.clone()));
    app.update_document_if(move |doc| {
        for conversation in doc.workspaces.iter_mut().flat_map(|workspace| workspace.conversations.iter_mut()) {
            if conversation.id == one {
                conversation.contexts = fill_one.clone();
            } else if conversation.id == two {
                conversation.contexts = fill_two.clone();
            }
        }
        true
    });
    let scroller = |id: &str| hash_key(("context-scroll", id));
    rig.click(&mut app, ("conversation-main", first.as_str()));
    rig.frames(&mut app, 12);
    let (top, content, viewport) = scroll_state(scroller(&first)).expect("the first timeline");
    assert!(content > viewport * 2.0, "long enough to scroll: {content} {viewport}");
    assert!((top - (content - viewport)).abs() < 2.0, "it opens on its end: {top} of {content} in {viewport}");

    scroll_request(scroller(&first), ScrollRequest::To(200.0));
    rig.frames(&mut app, 120);
    let left_at = scroll_state(scroller(&first)).expect("the first timeline").0;
    assert!(left_at < content - viewport - 50.0, "scrolled away from the end: {left_at}");

    rig.click(&mut app, ("conversation-main", second.as_str()));
    rig.frames(&mut app, 12);
    let (top, content, viewport) = scroll_state(scroller(&second)).expect("the second timeline");
    assert!((top - (content - viewport)).abs() < 2.0, "the other one opens on its end: {top} of {content} in {viewport}");

    rig.click(&mut app, ("conversation-main", first.as_str()));
    rig.frames(&mut app, 12);
    let back = scroll_state(scroller(&first)).expect("the first timeline").0;
    assert!((back - left_at).abs() < 2.0, "and the first comes back where it was: {back} {left_at}");
}

#[test]
fn a_session_showing_nothing_has_no_tab_and_goes_once_it_is_left_behind() {
    let (mut rig, mut app) = Rig::new("blank-pages");
    let id = rig.add_conversation(&mut app, "ws1", "first");
    rig.click(&mut app, ("conversation-main", id.as_str()));
    let owner = app.host_conversation_id(&id);
    let (shown, left, waiting) = (format!("{owner}#shown"), format!("{owner}#left"), format!("{owner}#waiting"));
    for session in [&owner, &shown, &left, &waiting] {
        app.dispatch_side_panes(SidePanesAction::RegisterPreview { conversation_id: id.clone(), session_id: session.clone(), workspace: None });
    }
    let page = BrowserStatus { url: "http://localhost:5173/".to_owned(), has_page: true, ..BrowserStatus::default() };
    let controller = app.local::<BrowserController>();
    controller.statuses.insert(shown.clone(), page);
    controller.statuses.insert(left.clone(), BrowserStatus::default());
    controller.statuses.insert(owner.clone(), BrowserStatus::default());
    assert_eq!(preview_tabs(&mut app, &id), vec![shown.clone()], "only what shows something has a tab");
    let (conversation, gone) = (id.clone(), left.clone());
    rig.until(&mut app, "the start page left behind to be reclaimed", move |app| !preview_sessions_for(app.side_panes(), Some(&conversation)).contains(&gone));
    // The conversation's own page is what the model's tools fill, and one not heard from yet may be about to show something.
    assert_eq!(preview_sessions_for(app.side_panes(), Some(&id)), [owner, shown, waiting]);
}
