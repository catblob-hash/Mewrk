//! `Sidebar.tsx` driven by the pointer, over a real document.

use fluent::el::hash_key;
use mewrk_client::prelude::*;
use mewrk_client::runs::ConversationStatus;
use mewrk_shell::sidebar::*;

use crate::rig::*;

#[test]
fn a_click_on_a_row_opens_that_conversation() {
    let (mut rig, mut app) = Rig::new("select");
    let first = rig.add_conversation(&mut app, "ws1", "first");
    let second = rig.add_conversation(&mut app, "ws2", "second");
    rig.click(&mut app, ("conversation-main", first.as_str()));
    assert_eq!(app.active_conversation_id().as_deref(), Some(first.as_str()));
    assert_eq!(app.active_workspace_id().as_deref(), Some("ws1"));
    rig.click(&mut app, ("conversation-main", second.as_str()));
    assert_eq!(app.active_conversation_id().as_deref(), Some(second.as_str()));
    assert_eq!(app.active_workspace_id().as_deref(), Some("ws2"));
}

#[test]
fn a_project_folds_at_once_and_new_task_opens_its_draft() {
    let (mut rig, mut app) = Rig::new("fold");
    let id = rig.add_conversation(&mut app, "ws1", "first");
    assert!(rig.exists(&mut app, ("conversation-main", id.as_str())));
    rig.click(&mut app, ("workspace-toggle", "ws1"));
    rig.frames(&mut app, 6);
    assert!(!rig.exists(&mut app, ("conversation-main", id.as_str())));
    rig.click(&mut app, ("workspace-toggle", "ws1"));
    assert!(rig.exists(&mut app, ("conversation-main", id.as_str())));
    // The row's own new-task button is only painted while the row is hovered, but it is there to press.
    rig.click(&mut app, ("workspace-new-task", "ws2"));
    assert!(app.draft_is_active());
    assert_eq!(app.active_workspace_id().as_deref(), Some("ws2"));
}

#[test]
fn a_conversation_is_renamed_in_place_and_escape_drops_the_edit() {
    let (mut rig, mut app) = Rig::new("rename");
    let id = rig.add_conversation(&mut app, "ws1", "old name");
    rig.click(&mut app, ("conversation-rename-button", id.as_str()));
    assert!(rig.exists(&mut app, hash_key(("field", ("conversation-rename", id.as_str())))));
    rig.key(&mut app, KeyPress { key: Key::Char('a'), shift: false, cmd: true, alt: false, ctrl: false });
    rig.type_text(&mut app, "new name");
    rig.plain(&mut app, Key::Enter);
    assert_eq!(conversation_order(&app, "ws1"), ["new name"]);
    rig.frames(&mut app, 6);
    assert!(!rig.exists(&mut app, hash_key(("field", ("conversation-rename", id.as_str())))));

    rig.click(&mut app, ("conversation-rename-button", id.as_str()));
    rig.frames(&mut app, 2);
    rig.type_text(&mut app, "thrown away");
    rig.plain(&mut app, Key::Escape);
    assert_eq!(conversation_order(&app, "ws1"), ["new name"]);
}

#[test]
fn delete_arms_then_confirms_and_a_press_elsewhere_disarms_it() {
    let (mut rig, mut app) = Rig::new("delete");
    let id = rig.add_conversation(&mut app, "ws1", "doomed");
    let key = hash_key(("confirm-delete-button", hash_key(("conversation-delete", id.as_str()))));
    rig.click(&mut app, key);
    let armed_w = rig.rect(&mut app, key).w;
    assert!(armed_w > 22.0, "armed, it gives up the fixed width for the word: {armed_w}");
    // A press on nothing in particular disarms it.
    rig.click_at(&mut app, 700.0, 500.0);
    rig.frames(&mut app, 2);
    assert_eq!(rig.rect(&mut app, key).w, 22.0);
    assert!(app.conversation(&id).is_some());
    // Armed and pressed again, it goes, through the host.
    rig.click(&mut app, key);
    rig.click(&mut app, key);
    rig.until(&mut app, "the conversation to leave", |app| app.conversation(&id).is_none());
}

#[test]
fn dragging_a_row_reorders_conversations_and_a_project_reorders_projects() {
    let (mut rig, mut app) = Rig::new("reorder");
    let first = rig.add_conversation(&mut app, "ws1", "first");
    let second = rig.add_conversation(&mut app, "ws1", "second");
    rig.frames(&mut app, 3);
    // New conversations open the list.
    assert_eq!(conversation_order(&app, "ws1"), ["second", "first"]);
    let (top, bottom) = (rig.rect(&mut app, ("conversation-main", second.as_str())), rig.rect(&mut app, ("conversation-main", first.as_str())));
    rig.drag(&mut app, (top.x + 40.0, top.y + top.h / 2.0), (bottom.x + 40.0, bottom.bottom() - 2.0));
    assert_eq!(conversation_order(&app, "ws1"), ["first", "second"]);
    // The click that ended the drag opened nothing.
    assert_ne!(app.active_conversation_id().as_deref(), Some(second.as_str()), "selection is what it was");

    // The temporary project stays pinned: a project dropped below it lands just above it.
    let order = |app: &App| app.doc.workspaces.iter().map(|workspace| workspace.id.clone()).collect::<Vec<_>>();
    assert_eq!(&order(&app)[..2], ["ws1", "ws2"]);
    let (a, b) = (rig.rect(&mut app, ("workspace-toggle", "ws1")), rig.rect(&mut app, ("workspace-toggle", "ws2")));
    rig.drag(&mut app, (a.x + 40.0, a.y + a.h / 2.0), (b.x + 40.0, b.bottom() - 2.0));
    assert_eq!(&order(&app)[..2], ["ws2", "ws1"]);
}

#[test]
fn the_drop_target_rules_read_as_upstreams() {
    let group = |id: &str, y: f32, pinned: bool, rows: &[&str]| GroupGeometry { id: id.to_owned(), pinned, rect: rect(0.0, y, 200.0, 28.0 * (1 + rows.len()) as f32), heading: rect(0.0, y, 200.0, 28.0), conversations: rows.iter().enumerate().map(|(i, row)| ((*row).to_owned(), rect(0.0, y + 28.0 * (1 + i) as f32, 200.0, 28.0))).collect() };
    let geometry = ListGeometry { list: rect(0.0, 0.0, 200.0, 300.0), groups: vec![group("a", 0.0, false, &["c1", "c2"]), group("b", 100.0, false, &[]), group("temp", 140.0, true, &[])] };
    let project = DragItem::Workspace { workspace_id: "a".into() };
    assert_eq!(sidebar_drop_target((10.0, 110.0), &project, &geometry), Some(DropTarget::Workspace { workspace_id: "b".into(), after: false }));
    // Below everything is after the last project that is not pinned.
    assert_eq!(sidebar_drop_target((10.0, 290.0), &project, &geometry), Some(DropTarget::Workspace { workspace_id: "b".into(), after: true }));
    assert_eq!(sidebar_drop_target((500.0, 110.0), &project, &geometry), None, "well outside the list");
    let row = DragItem::Conversation { workspace_id: "a".into(), conversation_id: "c1".into() };
    assert_eq!(sidebar_drop_target((10.0, 60.0), &row, &geometry), Some(DropTarget::Conversation { workspace_id: "a".into(), conversation_id: "c2".into(), after: false }));
    assert_eq!(sidebar_drop_target((10.0, 10.0), &row, &geometry), None, "on the project's own heading");
    assert_eq!(sidebar_drop_target((10.0, 120.0), &row, &geometry), None, "another project's rows are not a target");
}

#[test]
fn the_handle_resizes_clamps_restores_and_double_clicks_home() {
    let (mut rig, mut app) = Rig::new("resize");
    let handle = "sidebar-resize-handle";
    let r = rig.rect(&mut app, handle);
    let at = (r.x + 4.0, 300.0);
    rig.drag(&mut app, at, (at.0 + 40.0, 300.0));
    assert_eq!(app.sidebar_width(), 304.0);
    let r = rig.rect(&mut app, handle);
    let at = (r.x + 4.0, 300.0);
    rig.drag(&mut app, at, (at.0 + 1000.0, 300.0));
    assert_eq!(app.sidebar_width(), 420.0);
    // Escape in the middle of a drag puts it back.
    let r = rig.rect(&mut app, handle);
    let at = (r.x + 4.0, 300.0);
    rig.press_and_move(&mut app, at, (at.0 - 100.0, 300.0));
    assert_eq!(app.sidebar_width(), 320.0);
    rig.plain(&mut app, Key::Escape);
    assert_eq!(app.sidebar_width(), 420.0);
    rig.release(&mut app);
    rig.double_click(&mut app, handle);
    assert_eq!(app.sidebar_width(), 264.0);
    assert_eq!(clamp_sidebar_width(100.0), 220.0);
}

#[test]
fn the_status_mark_follows_a_conversation_and_the_settings_row_opens_settings() {
    let (mut rig, mut app) = Rig::new("status");
    let id = rig.add_conversation(&mut app, "ws1", "first");
    assert_eq!(app.sidebar_conversation_status(&id), ConversationStatus::Idle);
    rig.click(&mut app, "sidebar-settings");
    assert!(mewrk_settings_global::is_open(&mut app));
}

#[test]
fn escape_ends_a_drag_where_it_began_and_the_release_opens_nothing() {
    let (mut rig, mut app) = Rig::new("drag-escape");
    let first = rig.add_conversation(&mut app, "ws1", "first");
    let second = rig.add_conversation(&mut app, "ws1", "second");
    rig.frames(&mut app, 3);
    let active = app.active_conversation_id();
    let (top, bottom) = (rig.rect(&mut app, ("conversation-main", second.as_str())), rig.rect(&mut app, ("conversation-main", first.as_str())));
    rig.press_and_move(&mut app, (top.x + 40.0, top.y + top.h / 2.0), (bottom.x + 40.0, bottom.bottom() - 2.0));
    assert!(sort_drag_active(), "the drag is on");
    rig.plain(&mut app, Key::Escape);
    assert!(!sort_drag_active(), "Escape ended it");
    // Back over the row it started on, which is a click if nothing stops it.
    rig.point(&mut app, top.x + 40.0, top.y + top.h / 2.0);
    rig.release(&mut app);
    assert_eq!(conversation_order(&app, "ws1"), ["second", "first"]);
    assert_eq!(app.active_conversation_id(), active, "the release opened nothing");
}

#[test]
fn a_drag_let_go_over_something_else_does_not_eat_the_next_click() {
    let (mut rig, mut app) = Rig::new("drag-click");
    let first = rig.add_conversation(&mut app, "ws1", "first");
    let second = rig.add_conversation(&mut app, "ws1", "second");
    rig.frames(&mut app, 3);
    let (top, bottom) = (rig.rect(&mut app, ("conversation-main", second.as_str())), rig.rect(&mut app, ("conversation-main", first.as_str())));
    rig.drag(&mut app, (top.x + 40.0, top.y + top.h / 2.0), (bottom.x + 40.0, bottom.bottom() - 2.0));
    assert_eq!(conversation_order(&app, "ws1"), ["first", "second"]);
    // The temporary project's row takes no drags, so nothing resets the watch but the time it runs for.
    std::thread::sleep(std::time::Duration::from_millis(400));
    let temporary = mewrk_client::workspaces::TEMPORARY_WORKSPACE_ID;
    let unfolded = rig.rect(&mut app, hash_key(("workspace-group", temporary))).h;
    rig.click(&mut app, ("workspace-toggle", temporary));
    rig.frames(&mut app, 3);
    let folded = rig.rect(&mut app, hash_key(("workspace-group", temporary))).h;
    assert!(folded < unfolded, "the click folded the project: {folded} of {unfolded}");
}

#[test]
fn what_a_project_folded_outlasts_the_sidebar_closing() {
    let (mut rig, mut app) = Rig::new("fold-close");
    let id = rig.add_conversation(&mut app, "ws1", "first");
    rig.click(&mut app, ("workspace-toggle", "ws1"));
    rig.frames(&mut app, 3);
    assert!(!rig.exists(&mut app, ("conversation-main", id.as_str())));
    rig.click(&mut app, "shell-nav-drawer");
    rig.frames(&mut app, 8);
    rig.click(&mut app, "shell-nav-drawer");
    rig.frames(&mut app, 3);
    assert!(rig.exists(&mut app, ("workspace-toggle", "ws1")));
    assert!(!rig.exists(&mut app, ("conversation-main", id.as_str())), "still folded");
}

#[test]
fn a_title_cut_short_comes_up_whole_on_hover_and_a_short_one_does_not() {
    let long = "a-conversation-title-long-enough-that-no-sidebar-could-ever-hold-all-of-it-on-one-line";
    let (mut rig, mut app) = Rig::new("hover-title");
    let cut = rig.add_conversation(&mut app, "ws1", long);
    let short = rig.add_conversation(&mut app, "ws1", "ok");
    assert_eq!(rig.rest_on(&mut app, ("conversation-main", short.as_str())), 0);
    assert_eq!(rig.rest_on(&mut app, ("conversation-main", cut.as_str())), long.chars().count());
}

#[test]
fn a_disabled_toolbar_button_says_why_and_an_enabled_row_button_says_nothing() {
    let (mut rig, mut app) = Rig::new("hover-reason");
    let id = rig.add_conversation(&mut app, "ws1", "first");
    rig.click(&mut app, ("conversation-main", id.as_str()));
    assert_eq!(rig.rest_on(&mut app, ("workspace-new-task", "ws1")), 0, "a name is not a reason");
    let why = t("这个任务的工作区都不是 Git 仓库", "None of this task's workspaces is a Git repository");
    assert_eq!(rig.rest_on(&mut app, "pane-toolbar-review"), why.chars().filter(|c| !c.is_whitespace()).count());
}
