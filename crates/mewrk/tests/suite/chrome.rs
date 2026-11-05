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
    use mewrk_client::model::DEFAULT_GLASS_BLUR;
    use mewrk_shell::glass::{bake, cover_scale, fit_step};
    let picture = Pixels { width: 1024, height: 512, data: (0..1024 * 512).flat_map(|at| [(at % 251) as u8, 40, 90, 255]).collect() };
    // Twice as many pixels in the picture as in the window: the blur is 44 of the picture's, and three of the plate's.
    let scale = cover_scale((512.0, 256.0), (1024, 512));
    assert_eq!(scale, 0.5);
    let plate = bake(&picture, scale, DEFAULT_GLASS_BLUR).unwrap();
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
    rig.until(&mut app, "the bake", |app| bakes(app) == 1);
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
fn the_blur_setting_bakes_the_glass_again_once_it_rests_and_the_transparency_only_moves_the_tint() {
    use mewrk_shell::glass::{bakes, glass_tile, layers, plate, surfaces};
    let (mut rig, mut app) = Rig::new("glass-settings");
    let id = rig.add_conversation(&mut app, "ws1", "first");
    rig.click(&mut app, ("conversation-main", id.as_str()));
    app.doc.global_settings.appearance.liquid_glass = true;
    app.doc.global_settings.appearance.background = "builtin:desk".to_owned();
    rig.until(&mut app, "the bake", |app| bakes(app) == 1);

    let before = glass_tile();
    app.doc.global_settings.appearance.glass_transparency = 60;
    rig.frames(&mut app, 2);
    assert_eq!(layers().transparency, 60);
    assert!(glass_tile().a < before.a, "more shows through: {:?} {before:?}", glass_tile());
    assert_eq!(bakes(&mut app), 1, "the plate has nothing to do with it");

    app.doc.global_settings.appearance.glass_blur = 4;
    rig.frames(&mut app, 2);
    assert_eq!(bakes(&mut app), 1, "a slider still moving bakes nothing yet");
    rig.until(&mut app, "the sharper bake", |app| bakes(app) == 2);
    let sharper = plate(&mut app).unwrap();
    app.doc.global_settings.appearance.glass_blur = 60;
    rig.until(&mut app, "the softer bake", |app| bakes(app) == 3);
    let softer = plate(&mut app).unwrap();
    assert!(softer.size.0 < sharper.size.0, "a finer blur keeps more of the picture: {softer:?} {sharper:?}");

    // At no transparency the tint covers all of it, so nothing is laid under the surfaces at all.
    assert!(!surfaces().is_empty());
    app.doc.global_settings.appearance.glass_transparency = 0;
    rig.frames(&mut app, 2);
    assert_eq!(glass_tile().a, 255);
    assert!(surfaces().is_empty());
}

#[test]
fn over_a_background_the_tiles_rise_into_the_bars_foot() {
    use mewrk_shell::chrome::TOPBAR_HEIGHT;
    use mewrk_shell::glass::TILE_RISE;
    let (mut rig, mut app) = Rig::new("tile-rise");
    let id = rig.add_conversation(&mut app, "ws1", "first");
    rig.click(&mut app, ("conversation-main", id.as_str()));
    rig.frames(&mut app, 2);
    let plain = rig.rect(&mut app, "pane-tiles-chat");
    assert_eq!(plain.y, TOPBAR_HEIGHT + 8.0);

    app.doc.global_settings.appearance.background = "solid:night".to_owned();
    rig.frames(&mut app, 2);
    let risen = rig.rect(&mut app, "pane-tiles-chat");
    assert_eq!(risen.y, TOPBAR_HEIGHT - TILE_RISE);
    assert_eq!(risen.bottom(), plain.bottom(), "it grows upward");
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

/// Whatever is drawn over the page must stand on something solid of its own, or the page shows through it. Answers what does not: a
/// translucent box, or a word or picture with nothing solid under it, in any layer above the page.
fn blends_over_the_page(rig: &Rig) -> Vec<String> {
    // Corners aside, which an opaque box antialiases over the page as any shape does.
    fn inside(inner: [f32; 4], outer: [f32; 4]) -> bool {
        inner[0] >= outer[0] - 1.0 && inner[1] >= outer[1] - 1.0 && inner[0] + inner[2] <= outer[0] + outer[2] + 1.0 && inner[1] + inner[3] <= outer[1] + outer[3] + 1.0
    }
    fn seen(r: [f32; 4], clip: [f32; 4]) -> Option<[f32; 4]> {
        let (x0, y0, x1, y1) = (r[0].max(clip[0]), r[1].max(clip[1]), (r[0] + r[2]).min(clip[2]), (r[1] + r[3]).min(clip[3]));
        (x1 > x0 && y1 > y0).then_some([x0, y0, x1 - x0, y1 - y0])
    }
    let mut solid: Vec<[f32; 4]> = Vec::new();
    let mut found = Vec::new();
    for layer in rig.scene.layers.over_the_page() {
        for inst in &layer.rects {
            let Some(r) = seen(inst.rect, inst.clip) else { continue };
            let opaque = inst.fill.a == 255 && inst.fill2.a == 255;
            if !solid.iter().any(|under| inside(r, *under)) && !opaque {
                found.push(format!("box {:?} fill {:?} kind {}", inst.rect, inst.fill, inst.params[2]));
            }
            if opaque {
                solid.push(r);
            }
        }
        for draw in &layer.images {
            let Some(r) = seen(draw.inst.rect, draw.inst.clip) else { continue };
            if !solid.iter().any(|under| inside(r, *under)) {
                found.push(format!("picture {:?}", draw.inst.rect));
            }
        }
        for glyph in &layer.glyphs {
            let (x, y) = (glyph.pos[0], glyph.pos[1]);
            let clip = glyph.clip.map(f32::from);
            let Some(r) = seen([x, y, f32::from(glyph.size[0]), f32::from(glyph.size[1])], clip) else { continue };
            if !solid.iter().any(|under| inside(r, *under)) {
                found.push(format!("glyph at {x},{y}"));
            }
        }
    }
    found
}

#[test]
fn nothing_drawn_over_the_page_lets_the_page_through() {
    use mewrk_client::model::ThemePreference;
    use mewrk_shell::scenes::{apply, SCENES};
    for scene in SCENES.iter().filter(|scene| !matches!(**scene, "screen" | "page")) {
        for scheme in [ThemePreference::Day, ThemePreference::Night] {
            let (mut rig, mut app) = Rig::new("opaque-overlays");
            app.doc.global_settings.theme = scheme;
            assert!(apply(&mut app, scene), "{scene}");
            rig.frames(&mut app, 20);
            let found = blends_over_the_page(&rig);
            assert!(found.is_empty(), "{scene}: {} things blend over the page, as {:?}", found.len(), &found[..found.len().min(6)]);
        }
    }
}

#[test]
fn a_dialog_over_a_full_settings_page_lies_over_all_of_it() {
    use mewrk_settings_global::appearance::AppearancePage;
    let (mut rig, mut app) = Rig::new("nested-dialog");
    mewrk_shell::actions::open_global_settings(&mut app, "appearance");
    rig.frames(&mut app, 20);
    let page: usize = rig.scene.layers.over_the_page().count();
    app.local::<AppearancePage>().library_open = true;
    rig.frames(&mut app, 20);
    let done = rig.rect(&mut app, "background-done");
    let layers: Vec<_> = rig.scene.layers.over_the_page().collect();
    let surface = layers.iter().position(|layer| layer.rects.iter().any(|inst| inst.rect[2] == 620.0 && inst.rect[0] <= done.x && done.x < inst.rect[0] + inst.rect[2])).expect("the library's surface");
    assert!(surface >= page, "the settings page spent {page} layers and the library's surface is in layer {surface}");
    assert!(layers[..surface].iter().all(|layer| !layer.rects.iter().any(|inst| inst.rect[2] == 620.0)), "only the library is this wide");
}

/// One thing a pixel is drawn with, and whether it certainly replaces all that is under it there. The shader's own distance, worked out
/// here apart from what the culling assumes.
#[derive(Clone, Debug, PartialEq)]
struct DrawnAt {
    what: String,
    covers: bool,
}

fn round_box_distance(px: f32, py: f32, r: [f32; 4], radii: [f32; 4]) -> f32 {
    let (hw, hh) = (r[2] * 0.5, r[3] * 0.5);
    let (x, y) = (px - r[0] - hw, py - r[1] - hh);
    let radius = if x > 0.0 && y < 0.0 {
        radii[1]
    } else if x > 0.0 && y > 0.0 {
        radii[2]
    } else if x < 0.0 && y > 0.0 {
        radii[3]
    } else {
        radii[0]
    };
    let (qx, qy) = (x.abs() - hw + radius, y.abs() - hh + radius);
    qx.max(qy).min(0.0) + (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() - radius
}

/// All that a pixel is drawn with, in the order it is drawn.
fn drawn_at(rig: &Rig, x: f32, y: f32) -> Vec<DrawnAt> {
    use fluent::gpu::{KIND_ACRYLIC, KIND_RECT};
    let (px, py) = (x + 0.5, y + 0.5);
    let inside = |r: [f32; 4], grow: f32, clip: [f32; 4]| px >= r[0] - grow && py >= r[1] - grow && px < r[0] + r[2] + grow && py < r[1] + r[3] + grow && px >= clip[0] && py >= clip[1] && px < clip[2] && py < clip[3];
    let mut drawn = Vec::new();
    for layer in rig.scene.layers.iter() {
        for inst in layer.rects.iter().filter(|inst| inside(inst.rect, 1.0, inst.clip)) {
            let kind = inst.params[2];
            let solid = (kind == KIND_RECT || kind == KIND_ACRYLIC) && inst.fill.a == 255 && inst.fill2.a == 255;
            drawn.push(DrawnAt { what: format!("box {:?} {:?} {:?} {:?} {kind}", inst.rect, inst.fill, inst.fill2, inst.border), covers: solid && round_box_distance(px, py, inst.rect, inst.radii) <= -0.5 });
        }
        for draw in layer.images.iter().filter(|draw| inside(draw.inst.rect, 0.0, draw.inst.clip)) {
            drawn.push(DrawnAt { what: format!("picture {:?} {:?}", draw.image, draw.inst.rect), covers: false });
        }
        for glyph in &layer.glyphs {
            if inside([glyph.pos[0], glyph.pos[1], f32::from(glyph.size[0]), f32::from(glyph.size[1])], 0.0, glyph.clip.map(f32::from)) {
                drawn.push(DrawnAt { what: format!("glyph {:?} {:?} {:?}", glyph.pos, glyph.uv, glyph.color), covers: false });
            }
        }
    }
    drawn
}

/// A grid over the window, and every corner of every large solid box, which is where a mistake in the arithmetic would show.
fn points_to_look_at(rig: &Rig) -> Vec<(f32, f32)> {
    let mut points = Vec::new();
    let (mut y, height) = (0.0, rig.win.1);
    while y < height {
        let mut x = 0.0;
        while x < rig.win.0 {
            points.push((x, y));
            x += 37.0;
        }
        y += 37.0;
    }
    for inst in rig.scene.layers.iter().flat_map(|layer| &layer.rects).filter(|inst| inst.fill.a == 255 && inst.rect[2] >= 48.0 && inst.rect[3] >= 48.0) {
        let [x, y, w, h] = inst.rect;
        for step in 0..10 {
            let s = step as f32;
            for (cx, cy) in [(x + s, y + s), (x + w - 1.0 - s, y + s), (x + s, y + h - 1.0 - s), (x + w - 1.0 - s, y + h - 1.0 - s), (x - s, y + h * 0.5), (x + w + s, y + h * 0.5), (x + w * 0.5, y - s), (x + w * 0.5, y + h + s)] {
                points.push((cx, cy));
            }
        }
    }
    points
}

/// Every pixel looked at keeps what it was drawn with, in order, but for what lies under something solid that is still drawn there.
fn nothing_shown_was_culled(scene: &str, before: &[DrawnAt], after: &[DrawnAt], at: (f32, f32)) {
    let mut next = 0;
    for draw in before {
        if after.get(next) == Some(draw) {
            next += 1;
        } else {
            assert!(after[next..].iter().any(|over| over.covers), "{scene} at {at:?}: {} went with nothing over it. Left there: {after:?}", draw.what);
        }
    }
    assert_eq!(next, after.len(), "{scene} at {at:?}: something new or out of order. Before: {before:?} after: {after:?}");
}

/// The pixels the GPU is asked to shade, in windows.
fn shaded(rig: &Rig) -> f32 {
    let (win_x, win_y) = rig.win;
    let seen = |x0: f32, y0: f32, x1: f32, y1: f32, clip: [f32; 4]| (x1.min(clip[2]) - x0.max(clip[0])).max(0.0) * (y1.min(clip[3]) - y0.max(clip[1])).max(0.0);
    let mut total = if rig.scene.ground_covered { 0.0 } else { win_x * win_y };
    for layer in rig.scene.layers.iter() {
        total += layer.rects.iter().map(|inst| seen(inst.rect[0] - 1.0, inst.rect[1] - 1.0, inst.rect[0] + inst.rect[2] + 1.0, inst.rect[1] + inst.rect[3] + 1.0, inst.clip)).sum::<f32>();
        total += layer.images.iter().map(|draw| seen(draw.inst.rect[0], draw.inst.rect[1], draw.inst.rect[0] + draw.inst.rect[2], draw.inst.rect[1] + draw.inst.rect[3], draw.inst.clip)).sum::<f32>();
    }
    total / (win_x * win_y)
}

#[test]
fn culling_takes_only_what_is_certainly_covered_from_any_scene() {
    use mewrk_client::model::ThemePreference;
    use mewrk_shell::scenes::{apply, SCENES};
    let mut report = Vec::new();
    for scene in SCENES.iter().filter(|scene| !matches!(**scene, "screen" | "page")) {
        for scheme in [ThemePreference::Day, ThemePreference::Night] {
            let (mut rig, mut app) = Rig::new("culling");
            app.doc.global_settings.theme = scheme;
            assert!(apply(&mut app, scene), "{scene}");
            rig.frames(&mut app, 20);
            let points = points_to_look_at(&rig);
            let before: Vec<_> = points.iter().map(|&(x, y)| drawn_at(&rig, x, y)).collect();
            let shaded_before = shaded(&rig);
            let size = (rig.win.0 as u32, rig.win.1 as u32);
            rig.scene.cull_occluded(size);
            for (at, was) in points.iter().zip(&before) {
                nothing_shown_was_culled(scene, was, &drawn_at(&rig, at.0, at.1), *at);
            }
            let shaded_after = shaded(&rig);
            assert!(shaded_after <= shaded_before, "{scene}: {shaded_before} became {shaded_after}");
            assert!(rig.scene.ground_covered, "{scene}: the window's ground rect covers the ground");
            report.push(format!("{scene}: {shaded_before:.2} -> {shaded_after:.2}"));
        }
    }
    eprintln!("windows' worth of pixels shaded per frame:\n{}", report.join("\n"));
}

fn motion_fixture(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../motion/tests/fixtures").join(name)
}

/// Draws an image over an empty frame, one texel to a pixel, and reads what lands.
fn read_image(renderer: &mut fluent::Renderer, image: fluent::ImageId, size: (u32, u32)) -> Vec<u8> {
    use fluent::gpu::{ImageDraw, ImageInst, SURFACE_FORMAT};
    let gpu = renderer.gpu.clone();
    let target = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("read"),
        size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: SURFACE_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let mut scene = fluent::Scene::default();
    scene.ground_covered = true;
    let (width, height) = (size.0 as f32, size.1 as f32);
    scene.layers[0].images.push(ImageDraw { image, inst: ImageInst { rect: [0.0, 0.0, width, height], clip: [0.0, 0.0, width, height], uv: [0.0, 0.0, 1.0, 1.0], radii: [0.0; 4], opacity: [1.0, 0.0, 0.0, 0.0] }, opaque: false });
    let view = target.create_view(&Default::default());
    renderer.render(&scene, &fluent::Theme::dark(), &view, size);
    let stride = (size.0 * 4).next_multiple_of(256);
    let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor { label: Some("read"), size: u64::from(stride * size.1), usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    encoder.copy_texture_to_buffer(target.as_image_copy(), wgpu::TexelCopyBufferInfo { buffer: &buffer, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(stride), rows_per_image: None } }, wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 });
    let submission = gpu.queue.submit([encoder.finish()]);
    buffer.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::Wait { submission_index: Some(submission), timeout: Some(std::time::Duration::from_secs(10)) }).unwrap();
    let mapped = buffer.slice(..).get_mapped_range().unwrap();
    let mut out = Vec::with_capacity((size.0 * size.1 * 4) as usize);
    for row in mapped.chunks_exact(stride as usize) {
        for pixel in row[..size.0 as usize * 4].chunks_exact(4) {
            out.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
        }
    }
    out
}

/// The bake taking over from the live pass must not show. Whatever the filter is, these two have to agree.
#[test]
fn what_the_gpu_works_out_while_it_waits_is_what_the_bake_comes_out_as() {
    use mewrk_settings_global::logic::background::BUILTIN_PICTURES;
    use mewrk_settings_global::logic::background_image::{area_resize, decode};
    use mewrk_shell::glass::{filtered, plate_spec, GLASS_SATURATE};
    // What is queued for the GPU is queued for the whole process, so another test's renderer would take these images.
    let _one = crate::rig::SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let picture = decode(BUILTIN_PICTURES[0].picture).unwrap();
    let mut renderer = fluent::Renderer::new(std::sync::Arc::new(fluent::Gpu::new(wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle()), None).unwrap()));
    // What the window shows: a tier larger than the plate by a lot. What the bake starts from: the copy kept for it.
    let source = fluent::queue_image(picture.data.clone(), picture.width, picture.height);
    let kept = area_resize(&picture, 1024, 640).unwrap();
    for (window, blur) in [((1440.0, 900.0), 22), ((2560.0, 1440.0), 60), ((1000.0, 700.0), 4)] {
        let spec = plate_spec((picture.width, picture.height), window, blur, false);
        let target = fluent::queue_target_image(spec.size.0, spec.size.1);
        fluent::blur_into(fluent::PlatePass { source, target, sigma: spec.sigma, saturate: GLASS_SATURATE });
        let live = read_image(&mut renderer, target, spec.size);
        let baked = filtered(&kept, spec.size.0, spec.size.1, spec.sigma).unwrap();
        assert_eq!(live.len(), baked.data.len());
        let (mut total, mut worst) = (0u64, 0u8);
        for (a, b) in live.iter().zip(&baked.data) {
            let difference = a.abs_diff(*b);
            total += u64::from(difference);
            worst = worst.max(difference);
        }
        let mean = total as f64 / live.len() as f64;
        assert!(mean < 2.0 && worst < 24, "{window:?} blur {blur}, {:?}: off by {mean:.2} on average and {worst} at worst", spec.size);
        fluent::release_image(target);
    }
}

/// The plate of a still background is one frame, kept on disk, so the second time around there's nothing to bake and no picture to decode.
#[test]
fn baked_glass_is_kept_and_found_again() {
    use mewrk_shell::glass::{baked_book, bakes, frames_live, plate};
    let (mut rig, mut app) = Rig::new("glass-kept");
    let id = rig.add_conversation(&mut app, "ws1", "first");
    rig.click(&mut app, ("conversation-main", id.as_str()));
    app.doc.global_settings.appearance.liquid_glass = true;
    app.doc.global_settings.appearance.background = "builtin:desk".to_owned();
    rig.until(&mut app, "the bake", |app| bakes(app) == 1);
    assert!(frames_live(&mut app) >= 1, "the GPU stood in until then");
    let book = baked_book(&mut app).unwrap();
    assert_eq!(book.count(), 1);
    assert_eq!(book.size(), plate(&mut app).unwrap().size);
    let folder = app.app_data_dir().unwrap().join("glass-plates");
    let files = || std::fs::read_dir(&folder).unwrap().flatten().map(|entry| entry.file_name().to_string_lossy().into_owned()).collect::<Vec<_>>();
    assert_eq!(files().len(), 1, "{:?}", files());
    assert!(files()[0].starts_with("builtin_desk-"), "{:?}", files());

    app.doc.global_settings.appearance.background = "solid".to_owned();
    rig.frames(&mut app, 2);
    assert_eq!(baked_book(&mut app).map(|_| ()), None);
    let live_before = frames_live(&mut app);
    app.doc.global_settings.appearance.background = "builtin:desk".to_owned();
    rig.until(&mut app, "the bake from before", |app| bakes(app) == 2 && baked_book(app).is_some());
    rig.frames(&mut app, 2);
    assert_eq!(frames_live(&mut app), live_before, "found on disk, the GPU has nothing to stand in for");
    assert_eq!(files().len(), 1, "and nothing new was written");
}

/// A video's glass is baked in the background while it plays, and used from the moment it has got far enough. What stands in meanwhile stops by itself.
#[cfg(target_os = "macos")]
#[test]
fn a_videos_glass_is_baked_frame_by_frame_and_replaces_the_live_pass_when_it_gets_there() {
    use mewrk_settings_global::appearance::import_background_file;
    use mewrk_settings_global::effects::{backdrop_media, BackdropMedia};
    use mewrk_shell::glass::{baked_book, bakes, frames_live, plate};
    let (mut rig, mut app) = Rig::new("glass-video");
    let id = rig.add_conversation(&mut app, "ws1", "first");
    rig.click(&mut app, ("conversation-main", id.as_str()));
    app.doc.global_settings.appearance.liquid_glass = true;
    import_background_file(&mut app, motion_fixture("counting.mp4"));
    rig.until(&mut app, "the import", |app| app.doc.global_settings.appearance.background.len() == 64);
    rig.until(&mut app, "the video", |app| matches!(backdrop_media(app), BackdropMedia::Video { .. }));
    rig.until(&mut app, "the glass of every frame", |app| baked_book(app).is_some_and(|book| book.is_complete()));
    let book = baked_book(&mut app).unwrap();
    assert_eq!(book.size(), plate(&mut app).unwrap().size);
    assert_eq!(book.count(), 16, "a frame of glass for every frame of a video this slow");
    assert!((book.duration_us() as i64 - 2_000_000).abs() < 50_000, "{}", book.duration_us());
    // Each frame's own colour survives the blur: the red channel counts up through the video.
    let red = |index: u32| book.frame(index).unwrap().as_ref()[0];
    assert!(red(15) > red(8) && red(8) > red(1), "{} {} {}", red(1), red(8), red(15));
    assert_eq!(bakes(&mut app), 1);

    // From here on, however long it plays, the GPU has nothing to work out.
    let live = frames_live(&mut app);
    let until = std::time::Instant::now() + std::time::Duration::from_millis(600);
    while std::time::Instant::now() < until {
        rig.frames(&mut app, 2);
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    assert_eq!(frames_live(&mut app), live, "the bake is all that shows now");

    // A window a good deal larger is another fit: another bake, and in the meantime the GPU again.
    let win = rig.win;
    rig.win = (win.0 * 2.0, win.1 * 2.0);
    rig.until(&mut app, "the live pass to stand in", |app| frames_live(app) > live);
    rig.until(&mut app, "the next bake", |app| bakes(app) == 2);
    rig.win = win;
}

/// A file that has no room for all a long video's glass spreads it out, rather than running off the end.
#[test]
fn a_long_videos_glass_fits_its_budget_by_going_slower() {
    use mewrk_shell::glass::plate_timing;
    let frame = 200 * 120 * 4;
    let (capacity, step) = plate_timing(10.0, 60.0, frame);
    assert_eq!(capacity, 302, "thirty a second is enough of a blur");
    assert!((step - 1.0 / 30.0).abs() < 1e-6);
    let (capacity, step) = plate_timing(3.0 * 3600.0, 30.0, frame);
    assert!(u64::from(capacity) * frame <= 768 << 20, "{capacity} frames");
    assert!(step > 1.0 / 30.0, "{step}");
    assert!(10_800.0 / step <= f64::from(capacity), "room for all of it: {step} {capacity}");
    let (capacity, _) = plate_timing(2.0, 8.0, frame);
    assert_eq!(capacity, 18);
}

/// Fits within a step are one bake with one blur, so what is on disk holds for all of them.
#[test]
fn fits_within_a_step_share_a_plate_and_what_moves_has_wider_steps() {
    use mewrk_shell::glass::plate_spec;
    let natural = (1920, 1080);
    let still = plate_spec(natural, (1440.0, 900.0), 22, false);
    assert_eq!(still, plate_spec(natural, (1442.0, 901.0), 22, false));
    assert_ne!(still, plate_spec(natural, (1800.0, 1100.0), 22, false));
    let moving = plate_spec(natural, (1600.0, 1000.0), 22, true);
    assert_eq!(moving, plate_spec(natural, (1640.0, 1025.0), 22, true));
    assert_ne!(moving, plate_spec(natural, (2880.0, 1800.0), 22, true));
    assert!(still.sigma > 0.0 && still.sigma <= 3.0, "{still:?}");
    let sharp = plate_spec(natural, (1440.0, 900.0), 0, false);
    assert_eq!(sharp.sigma, 0.0);
    assert_eq!(sharp.size, (1024, 576));
    // The aspect stays, so a plate fits the window as the picture does.
    assert!((still.size.0 as f32 / still.size.1 as f32 - 16.0 / 9.0).abs() < 0.05, "{still:?}");
}

/// `icon.icns` once held a 300-pixel icon in the corner of a white canvas, and macOS drew exactly that: this is the check it needed.
#[test]
fn the_bundled_icon_fills_the_grid_macos_expects() {
    let icns: &[u8] = include_bytes!("../../assets/icon.icns");
    let mut at = 8;
    let png = loop {
        let length = u32::from_be_bytes(icns[at + 4..at + 8].try_into().unwrap()) as usize;
        if &icns[at..at + 4] == b"ic10" {
            break &icns[at + 8..at + length];
        }
        at += length;
    };
    let image = image::load_from_memory(png).unwrap().to_rgba8();
    assert_eq!(image.dimensions(), (1024, 1024));
    for (x, y) in [(0, 0), (1023, 0), (0, 1023), (1023, 1023)] {
        assert_eq!(image.get_pixel(x, y)[3], 0, "the corner at {x},{y} is clear");
    }
    // Solid from edge to edge of the 824-pixel plate, with at most a shadow past it, on all four sides.
    for (x, y) in [(102, 512), (921, 512), (512, 102), (512, 921)] {
        assert_eq!(image.get_pixel(x, y)[3], 255, "the plate reaches {x},{y}");
    }
    for (x, y) in [(90, 512), (933, 512), (512, 90)] {
        assert!(image.get_pixel(x, y)[3] < 64, "the plate stops before {x},{y}");
    }
    let ground = image.get_pixel(160, 512);
    assert!(ground[0] > ground[1] && ground[1] > ground[2] && ground[2] < 128, "the ground is orange: {ground:?}");
}

#[test]
fn every_drawing_of_the_mark_is_the_one_path() {
    use mewrk_ui_common::brand::{ICON_SMALL_SVG, ICON_SVG, MARK_PATH, MARK_SVG, MARK_TRANSFORM, PROMPT_PATH};
    for drawing in [ICON_SVG, ICON_SMALL_SVG, MARK_SVG] {
        assert!(drawing.contains(MARK_PATH));
    }
    // What the page draws without the plate stands exactly where the icon has it.
    assert!(ICON_SVG.contains(PROMPT_PATH) && ICON_SVG.contains(MARK_TRANSFORM));
}
