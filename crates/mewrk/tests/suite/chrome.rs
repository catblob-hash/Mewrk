//! The top row: the nav cluster, the title, the toolbar, and the global shortcuts.

use mewrk_client::prelude::*;
use mewrk_client::side_panes::{pane_is_open, shown_tasks_tab, side_pane_layout_for, TasksPaneTab};
use mewrk_shell::state::shell;

use crate::rig::*;

fn pane_open(app: &mut App, pane: &str) -> bool {
    let id = app.active_conversation_id();
    pane_is_open(side_pane_layout_for(app.side_panes(), id.as_deref()), pane)
}

#[test]
fn the_drawer_hides_the_sidebar_and_the_top_bar_makes_room_for_the_nav() {
    let (mut rig, mut app) = Rig::new("drawer");
    assert!(rig.exists(&mut app, "sidebar"));
    let before = rig.rect(&mut app, "topbar");
    assert_eq!(before.x, 264.0);
    rig.click(&mut app, "shell-nav-drawer");
    rig.frames(&mut app, 6);
    assert!(!rig.exists(&mut app, "sidebar"));
    assert_eq!(rig.rect(&mut app, "topbar").x, 0.0);
    // The cluster is its four buttons now, and the drawer button has not moved from under the pointer.
    assert!(rig.rect(&mut app, "shell-nav").w < 120.0);
    rig.click(&mut app, "shell-nav-drawer");
    assert!(rig.exists(&mut app, "sidebar"));
}

#[test]
fn back_and_forward_walk_what_was_opened_and_search_opens_the_palette() {
    let (mut rig, mut app) = Rig::new("nav");
    let first = rig.add_conversation(&mut app, "ws1", "first");
    let second = rig.add_conversation(&mut app, "ws1", "second");
    for id in [&first, &second] {
        rig.click(&mut app, ("conversation-main", id.as_str()));
    }
    rig.click(&mut app, "shell-nav-back");
    assert_eq!(app.active_conversation_id().as_deref(), Some(first.as_str()));
    rig.click(&mut app, "shell-nav-forward");
    assert_eq!(app.active_conversation_id().as_deref(), Some(second.as_str()));
    rig.click(&mut app, "shell-nav-search");
    assert!(shell(&mut app).search_open);
}

#[test]
fn the_title_renames_in_place_commits_on_enter_and_keeps_on_escape() {
    let (mut rig, mut app) = Rig::new("title");
    let id = rig.add_conversation(&mut app, "ws1", "old");
    rig.click(&mut app, ("conversation-main", id.as_str()));
    rig.click(&mut app, "conversation-title-name");
    assert!(shell(&mut app).title_draft.is_some());
    rig.frames(&mut app, 2);
    rig.cmd(&mut app, 'a');
    rig.type_text(&mut app, "new");
    rig.plain(&mut app, Key::Enter);
    assert_eq!(app.conversation(&id).unwrap().title, "new");
    assert!(shell(&mut app).title_draft.is_none());

    rig.click(&mut app, "conversation-title-name");
    rig.frames(&mut app, 2);
    rig.type_text(&mut app, "dropped");
    rig.plain(&mut app, Key::Escape);
    assert_eq!(app.conversation(&id).unwrap().title, "new");
}

#[test]
fn a_draft_has_no_title_to_rename() {
    let (mut rig, mut app) = Rig::new("draft-title");
    app.open_draft_conversation(Some("ws1"));
    rig.frames(&mut app, 2);
    rig.click(&mut app, "conversation-title-name");
    assert!(shell(&mut app).title_draft.is_none());
}

/// Opens a toolbar menu and presses one of its rows, as a pointer would.
fn pick(rig: &mut Rig, app: &mut App, menu: &str, section: &str, row: &str) {
    let _ = section;
    rig.click(app, mewrk_shell::menu::trigger_key(menu));
    rig.click(app, mewrk_shell::menu::row_key(menu, row));
}

#[test]
fn the_toolbars_menu_toggles_panes_and_each_tab_row_is_that_tab_on_show() {
    let (mut rig, mut app) = Rig::new("toolbar");
    let id = rig.add_conversation(&mut app, "ws1", "first");
    rig.click(&mut app, ("conversation-main", id.as_str()));
    let shown = |app: &mut App| {
        let id = app.active_conversation_id();
        shown_tasks_tab(side_pane_layout_for(app.side_panes(), id.as_deref()))
    };
    pick(&mut rig, &mut app, "pane-toolbar-more", "views", "history");
    assert_eq!(shown(&mut app), Some(TasksPaneTab::History));
    assert!(rig.exists(&mut app, ("pane-tile", "tasks")));
    pick(&mut rig, &mut app, "pane-toolbar-more", "views", "tasks");
    assert_eq!(shown(&mut app), Some(TasksPaneTab::Tasks), "the other tab's row shows that tab");
    pick(&mut rig, &mut app, "pane-toolbar-more", "views", "tasks");
    assert_eq!(shown(&mut app), None, "its own row closes the pane");
    pick(&mut rig, &mut app, "pane-toolbar-more", "views", "files");
    assert!(pane_open(&mut app, "files"));
    assert!(rig.exists(&mut app, ("pane-tile", "files")));
    pick(&mut rig, &mut app, "pane-toolbar-more", "views", "files");
    assert!(!pane_open(&mut app, "files"));
    pick(&mut rig, &mut app, "pane-toolbar-more", "views", "settings");
    assert!(pane_open(&mut app, "settings"));
}

#[test]
fn the_terminal_button_is_a_menu_of_shells_and_its_last_row_hides_the_pane() {
    let (mut rig, mut app) = Rig::new("terminal-menu");
    let id = rig.add_conversation(&mut app, "ws1", "first");
    rig.click(&mut app, ("conversation-main", id.as_str()));
    pick(&mut rig, &mut app, "pane-toolbar-terminal", "pane", "toggle");
    assert!(pane_open(&mut app, "terminal"));
    assert!(rig.exists(&mut app, ("pane-tile", "terminal")));
    pick(&mut rig, &mut app, "pane-toolbar-terminal", "pane", "toggle");
    assert!(!pane_open(&mut app, "terminal"));
}

#[test]
fn the_review_button_says_why_it_is_dead_outside_a_repository() {
    let (mut rig, mut app) = Rig::new("review");
    let id = rig.add_conversation(&mut app, "ws1", "first");
    rig.click(&mut app, ("conversation-main", id.as_str()));
    rig.click(&mut app, "pane-toolbar-review");
    assert!(!pane_open(&mut app, "review"));
}

#[test]
fn a_real_repository_gets_a_review_pane() {
    let (mut rig, mut app) = Rig::new("repo");
    let run = |args: &[&str]| std::process::Command::new("git").args(args).current_dir(&rig.project).output().unwrap();
    run(&["init", "-q"]);
    std::fs::write(rig.project.join("a.txt"), "hello\n").unwrap();
    let id = rig.add_conversation(&mut app, "ws1", "first");
    rig.click(&mut app, ("conversation-main", id.as_str()));
    rig.until(&mut app, "Git to answer", |app| !mewrk_shell::effects::review_pages(app).is_empty());
    rig.click(&mut app, "pane-toolbar-review");
    assert!(pane_open(&mut app, "review"));
    assert!(rig.exists(&mut app, ("pane-tile", "review")));
}

#[test]
fn global_shortcuts_reach_their_commands() {
    let (mut rig, mut app) = Rig::new("shortcuts");
    let first = rig.add_conversation(&mut app, "ws1", "first");
    let second = rig.add_conversation(&mut app, "ws1", "second");
    rig.click(&mut app, ("conversation-main", first.as_str()));
    // Control+Tab, on every platform, as switching tabs is.
    rig.key(&mut app, KeyPress { key: Key::Tab, shift: false, cmd: false, alt: false, ctrl: true });
    assert_ne!(app.active_conversation_id().as_deref(), Some(first.as_str()));
    let _ = second;
    rig.cmd(&mut app, 'n');
    assert!(app.draft_is_active());
    rig.cmd(&mut app, ',');
    assert!(mewrk_settings_global::is_open(&mut app));
    mewrk_settings_global::close(&mut app);
    rig.key(&mut app, KeyPress { key: Key::Char(','), shift: true, cmd: true, alt: false, ctrl: false });
    assert!(pane_open(&mut app, "settings"));
    let zoom = app.doc.global_settings.appearance.zoom;
    rig.cmd(&mut app, '=');
    assert!(app.doc.global_settings.appearance.zoom > zoom);
    rig.cmd(&mut app, '0');
    assert_eq!(app.doc.global_settings.appearance.zoom, 1.0);
}

#[test]
fn glass_lets_the_background_through_the_tiles_and_a_picture_only_through_the_gutter() {
    use mewrk_shell::pane_tiles::{chat_ground, gutter_ground, pane_ground};
    use mewrk_shell::sidebar::sidebar_ground;
    let (mut rig, mut app) = Rig::new("glass");
    let id = rig.add_conversation(&mut app, "ws1", "first");
    rig.click(&mut app, ("conversation-main", id.as_str()));
    rig.frames(&mut app, 2);
    assert_eq!(gutter_ground().a, 255, "the theme's own ground paints the gutter");
    assert_eq!(chat_ground().a, 255);

    app.doc.global_settings.appearance.background = "solid:night".to_owned();
    rig.frames(&mut app, 2);
    assert_eq!(gutter_ground().a, 0, "another ground shows through the gutter");
    assert_eq!(chat_ground().a, 255, "and the tiles stay as they are");

    app.doc.global_settings.appearance.background = "solid".to_owned();
    app.doc.global_settings.appearance.liquid_glass = true;
    rig.frames(&mut app, 2);
    assert_eq!(gutter_ground().a, 0);
    for ground in [chat_ground(), pane_ground(), sidebar_ground()] {
        assert!(ground.a > 127 && ground.a < 255, "glass is a tint: {ground:?}");
    }
}

// glassPlate.test.ts
#[test]
fn saturates_a_colour_as_the_filter_does_leaves_a_grey_grey_and_clamps() {
    use mewrk_shell::glass::filter_color;
    let warm = [150.0, 120.0, 90.0];
    assert_eq!(filter_color(warm, 1.0).map(f32::round), warm);
    let saturated = filter_color(warm, 2.0);
    assert!(saturated[0] > warm[0] && saturated[2] < warm[2], "{saturated:?}");
    for channel in filter_color([100.0, 100.0, 100.0], 2.0) {
        assert!((channel - 100.0).abs() < 1e-3, "{channel}");
    }
    assert_eq!(filter_color([255.0, 0.0, 0.0], 3.0)[1], 0.0);
}

#[test]
fn splits_a_gaussian_into_three_box_blurs_of_the_same_spread() {
    use mewrk_shell::glass::box_radii;
    for sigma in [1.0f32, 3.0, 7.5, 22.0] {
        let variance: f32 = box_radii(sigma).iter().map(|radius| (((2 * radius + 1) * (2 * radius + 1)) as f32 - 1.0) / 12.0).sum();
        assert!((variance - sigma * sigma).abs() <= (sigma * sigma * 0.05).max(1.0), "{sigma}: {variance}");
    }
    assert_eq!(box_radii(0.2), [0, 0, 0]);
}

#[test]
fn blurs_without_moving_or_losing_light_and_leaves_a_flat_field_flat() {
    use mewrk_shell::glass::blur_pixels;
    let grey_row = |levels: &[u8]| levels.iter().flat_map(|level| [*level, *level, *level, 255]).collect::<Vec<u8>>();
    let levels = |pixels: &[u8]| pixels.chunks_exact(4).map(|pixel| pixel[0]).collect::<Vec<u8>>();

    let mut flat = grey_row(&[90; 12]);
    blur_pixels(&mut flat, 12, 1, 2.0);
    assert_eq!(levels(&flat), [90; 12]);

    let mut spot = grey_row(&(0..41).map(|index| if index == 20 { 255 } else { 0 }).collect::<Vec<u8>>());
    blur_pixels(&mut spot, 41, 1, 3.0);
    let blurred = levels(&spot);
    assert!(blurred[20] < 255 && blurred[20] > blurred[17], "{blurred:?}");
    for offset in 1..=10 {
        assert!(blurred[20 - offset].abs_diff(blurred[20 + offset]) <= 1, "{blurred:?}");
    }
    let total: i32 = blurred.iter().map(|level| i32::from(*level)).sum();
    assert!((total - 255).abs() <= 20, "{total}");
    assert_eq!(spot[20 * 4 + 3], 255, "alpha is left as it was");
}

#[test]
fn a_baked_plate_is_small_and_a_blur_finer_than_the_picture_is_not_asked_for_it() {
    use mewrk_settings_global::logic::background_image::Pixels;
    use mewrk_shell::glass::{bake, cover_scale, fit_step};
    let picture = Pixels { width: 1024, height: 512, data: (0..1024 * 512).flat_map(|at| [(at % 251) as u8, 40, 90, 255]).collect() };
    // Twice as many pixels in the picture as in the window: the blur is 44 of the picture's, and three of the plate's.
    let scale = cover_scale((512.0, 256.0), (1024, 512));
    assert_eq!(scale, 0.5);
    let plate = bake(&picture, scale).unwrap();
    assert_eq!((plate.width, plate.height), (70, 35));
    let range = plate.data.chunks_exact(4).map(|pixel| pixel[0]);
    assert!(range.clone().max().unwrap() - range.min().unwrap() < 8, "the pattern is gone");
    // A few percent of fit share a bake, and more do not.
    assert_eq!(fit_step(1.0), fit_step(1.01));
    assert_ne!(fit_step(1.0), fit_step(1.2));
}

#[test]
fn glass_is_baked_once_under_every_surface_and_only_again_when_the_picture_or_the_fit_changes() {
    use mewrk_shell::glass::{bakes, plate, surfaces};
    let (mut rig, mut app) = Rig::new("glass-plate");
    let id = rig.add_conversation(&mut app, "ws1", "first");
    rig.click(&mut app, ("conversation-main", id.as_str()));
    rig.frames(&mut app, 2);
    assert_eq!(plate(&mut app), None, "no glass, no plate");
    assert!(surfaces().is_empty());

    app.doc.global_settings.appearance.liquid_glass = true;
    app.doc.global_settings.appearance.background = "builtin:desk".to_owned();
    rig.until(&mut app, "the bake", |app| plate(app).is_some());
    let baked = plate(&mut app).unwrap();
    assert!(baked.size.0.max(baked.size.1) <= 1024, "{baked:?}");
    assert_eq!(bakes(&mut app), 1);
    let sidebar = rig.rect(&mut app, "sidebar");
    let chat = rig.rect(&mut app, "pane-tiles-chat");
    let laid = surfaces();
    assert!(laid.iter().any(|(r, _)| *r == sidebar), "under the sidebar: {laid:?}");
    assert!(laid.iter().any(|(r, _)| *r == chat), "under the conversation, where it is: {chat:?} {laid:?}");

    // Surfaces coming and going change what shows, not the plate.
    mewrk_shell::actions::open_pane(&mut app, "tasks");
    rig.frames(&mut app, 4);
    let tile = rig.rect(&mut app, ("pane-tile", "tasks"));
    assert!(surfaces().iter().any(|(r, _)| *r == tile), "under a pane too: {tile:?}");
    let moved = rig.rect(&mut app, "pane-tiles-chat");
    assert!(surfaces().iter().any(|(r, _)| *r == moved), "and the conversation's follows it: {moved:?}");
    assert_eq!(plate(&mut app), Some(baked));

    // A window a little wider is still the same fit.
    let win = rig.win;
    rig.win = (win.0 + 4.0, win.1);
    rig.frames(&mut app, 4);
    std::thread::sleep(std::time::Duration::from_millis(200));
    rig.frames(&mut app, 4);
    assert_eq!(bakes(&mut app), 1);

    // One a good deal larger is another, once it has stopped moving.
    rig.win = (win.0 * 1.6, win.1 * 1.6);
    rig.frames(&mut app, 2);
    assert_eq!(bakes(&mut app), 1, "the window is still on its way");
    rig.until(&mut app, "the next bake", |app| bakes(app) == 2);
    assert_ne!(plate(&mut app), Some(baked), "the one texture was replaced");
    rig.win = win;

    // Another picture is another bake, and a solid ground has none: what lies under its tiles is a colour.
    app.doc.global_settings.appearance.background = "builtin:chair".to_owned();
    rig.frames(&mut app, 1);
    assert_eq!(plate(&mut app), None, "the old picture's glass goes with the picture");
    rig.until(&mut app, "the other picture's bake", |app| plate(app).is_some());
    app.doc.global_settings.appearance.background = "solid".to_owned();
    rig.frames(&mut app, 2);
    assert_eq!(plate(&mut app), None);
    assert!(!surfaces().is_empty(), "the surfaces still ask for the ground through the filter");

    app.doc.global_settings.appearance.liquid_glass = false;
    rig.frames(&mut app, 2);
    assert!(surfaces().is_empty());
}

#[test]
fn over_a_background_the_tiles_rise_into_the_bars_foot_and_the_strip_lies_over_them() {
    use mewrk_shell::chrome::TOPBAR_HEIGHT;
    use mewrk_shell::glass::{strip_over_tiles, TILE_RISE};
    let (mut rig, mut app) = Rig::new("tile-rise");
    let id = rig.add_conversation(&mut app, "ws1", "first");
    rig.click(&mut app, ("conversation-main", id.as_str()));
    rig.frames(&mut app, 2);
    let plain = rig.rect(&mut app, "pane-tiles-chat");
    assert_eq!(plain.y, TOPBAR_HEIGHT + 8.0);
    assert!(strip_over_tiles(TOPBAR_HEIGHT).is_none());

    app.doc.global_settings.appearance.background = "solid:night".to_owned();
    rig.frames(&mut app, 2);
    let risen = rig.rect(&mut app, "pane-tiles-chat");
    assert_eq!(risen.y, TOPBAR_HEIGHT - TILE_RISE);
    assert_eq!(risen.bottom(), plain.bottom(), "it grows upward");
    assert!(strip_over_tiles(TOPBAR_HEIGHT).is_some());
}

#[test]
fn a_fork_card_floats_where_a_native_page_makes_room_for_it() {
    use mewrk_client::runs::PendingForkRequest;
    use mewrk_pane_tasks::fork_tray::ForkStore;
    let (mut rig, mut app) = Rig::new("fork-tray");
    let request = PendingForkRequest { fork_id: "f1".into(), workspace_id: "ws1".into(), source_conversation_id: "c1".into(), source_title: "Source".into(), prompt: "Try the other way".into(), requested_at: "2026-11-01T00:00:00.000Z".into() };
    app.local::<ForkStore>().requests.push(request);
    rig.frames(&mut app, 3);
    let tray = rig.rect(&mut app, "fork-request-tray");
    let (window_w, _) = window_size();
    assert!((tray.right() - (window_w - 12.0)).abs() < 1.0, "against the window's right edge: {tray:?}");
    assert!((tray.y - (mewrk_shell::chrome::TOPBAR_HEIGHT + 12.0)).abs() < 1.0, "under the top bar: {tray:?}");
    assert!(overlay_rects().iter().any(|rect| *rect == tray), "and among what a native page cuts itself by");
}

#[test]
fn the_title_field_grows_with_what_it_holds_up_to_a_limit() {
    let (mut rig, mut app) = Rig::new("title-width");
    let id = rig.add_conversation(&mut app, "ws1", "first");
    rig.click(&mut app, ("conversation-main", id.as_str()));
    rig.click(&mut app, "conversation-title-name");
    assert!(rig.exists(&mut app, "conversation-title-field"));
    let short = rig.rect(&mut app, "conversation-title-field").w;
    shell(&mut app).title_draft.as_mut().expect("the rename").value = "a title of a good many words that goes on".to_owned();
    let longer = rig.rect(&mut app, "conversation-title-field").w;
    assert!(longer > short + 50.0, "{short} {longer}");
    shell(&mut app).title_draft.as_mut().expect("the rename").value = "w".repeat(400);
    assert_eq!(rig.rect(&mut app, "conversation-title-field").w, 420.0);
}

#[test]
fn a_bare_shortcut_yields_to_a_field_and_not_to_a_focused_row() {
    let (mut rig, mut app) = Rig::new("shortcut-focus");
    let id = rig.add_conversation(&mut app, "ws1", "first");
    app.update_global_settings(|doc| {
        doc.global_settings.shortcuts.insert("conversation.create".to_owned(), mewrk_client::model::ShortcutPreference { binding: vec!["Shift".to_owned(), "KeyN".to_owned()], enabled: true });
    });
    let shift_n = KeyPress { key: Key::Char('n'), shift: true, cmd: false, alt: false, ctrl: false };
    rig.click(&mut app, ("conversation-main", id.as_str()));
    assert_ne!(rig.focused(), 0, "the row took the keyboard");
    rig.key(&mut app, shift_n);
    assert!(app.draft_is_active(), "a row is not typed into");
    // The draft's composer is.
    rig.click(&mut app, ("conversation-main", id.as_str()));
    rig.frames(&mut app, 2);
    mewrk_ui_composer::focus_composer();
    rig.frames(&mut app, 2);
    rig.key(&mut app, shift_n);
    assert!(!app.draft_is_active(), "Shift+N is a capital in a text box");
}

#[test]
fn a_plan_card_takes_the_reader_to_the_plan_once() {
    let (mut rig, mut app) = Rig::new("plan-card");
    let id = rig.add_conversation(&mut app, "ws1", "first");
    rig.click(&mut app, ("conversation-main", id.as_str()));
    rig.frames(&mut app, 3);
    let plan: mewrk_client::model::ConversationPlan = serde_json::from_value(serde_json::json!({ "conversationId": id, "markdown": "# Plan", "status": "draft", "createdAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-01T00:00:00Z" })).unwrap();
    app.runs().plans.insert(id.clone(), Some(plan));
    let card: mewrk_client::runs::PendingToolPrompt = serde_json::from_value(serde_json::json!({ "promptId": "plan-1", "toolName": "exit_plan_mode", "kind": "plan_exit", "label": "Plan", "summary": "", "riskLevel": "low", "reason": "", "allowAlwaysOffered": false })).unwrap();
    app.runs().tool_prompts.insert(id.clone(), vec![card]);
    rig.frames(&mut app, 3);
    assert!(pane_open(&mut app, "plan"), "the answer is read beside the plan");
    assert_eq!(mewrk_shell::effects::approval_dock_owner(&mut app, &id), mewrk_shell::effects::DockOwner::Plan);
    mewrk_shell::actions::close_pane(&mut app, "plan");
    rig.frames(&mut app, 3);
    assert!(!pane_open(&mut app, "plan"), "once: closing it is the reader's to do");
}

#[test]
fn a_narrow_window_floats_the_sidebar_over_the_tiles() {
    let (mut rig, mut app) = Rig::new("narrow");
    let id = rig.add_conversation(&mut app, "ws1", "first");
    rig.click(&mut app, ("conversation-main", id.as_str()));
    rig.frames(&mut app, 3);
    let wide = rig.rect(&mut app, "pane-tiles-chat");
    let sidebar = rig.rect(&mut app, "sidebar");
    assert_eq!(wide.x, sidebar.right() + 8.0, "beside it, past the gutter");

    rig.win = (800.0, 700.0);
    rig.frames(&mut app, 3);
    let chat = rig.rect(&mut app, "pane-tiles-chat");
    let sidebar = rig.rect(&mut app, "sidebar");
    assert_eq!((chat.x, chat.right()), (0.0, 800.0), "the tiles run to the window's left edge under it");
    assert_eq!(sidebar.x, 0.0);
    assert!(sidebar.w > 200.0, "still there, over them: {sidebar:?}");

    // A press on the sidebar is the sidebar's, whatever lies under it.
    rig.click(&mut app, "new-task-button");
    assert!(app.draft_is_active());

    // Closed, it takes nothing: the top bar makes room for the nav as it always does.
    rig.click(&mut app, "shell-nav-drawer");
    rig.frames(&mut app, 3);
    assert!(!rig.exists(&mut app, "sidebar"));
    let chat = rig.rect(&mut app, "pane-tiles-chat");
    assert_eq!((chat.x, chat.right()), (0.0, 800.0));
}
