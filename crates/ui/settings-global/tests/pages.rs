//! The other pages, driven through a real host: Appearance, search providers, shortcuts, dependencies, updates.

mod support;

use std::time::Duration;

use fluent::el::{hash_key, rect_of};
use fluent::theme::Color;
use mewrk_client::model::{RunTarget, SearchProviderKind, ShortcutPreference, SshMachineConfig, ThemePreference};
use mewrk_client::prelude::*;
use mewrk_settings_global::dependencies::{self, tool_name_error};
use mewrk_settings_global::effects::backdrop_size;
use mewrk_settings_global::logic::app_update::{AppRelease, AppUpdateCheck, AppUpdateController, AppUpdateState};
use mewrk_settings_global::logic::store;
use mewrk_settings_global::secret_field::{field_state, Identity};
use mewrk_settings_global::logic::web_search::SearchCredentialSlot;
use mewrk_settings_global::permissions_page::{self, DiskAccess, KeychainAccess, PermissionsSnapshot};
use mewrk_settings_global::{appearance, close, global_settings_window, message_type, on_scheme_changed, open, themed, window_backdrop, zoom, SettingsView};
use support::*;

fn view(app: &mut App) -> E {
    div().w_full().h_full().child(window_backdrop(app)).child(global_settings_window(app))
}

fn on_page(name: &str, page: SettingsView) -> (TempDir, App, Rig) {
    let dir = TempDir::new(name);
    let mut app = start_app(&dir.0);
    open(&mut app, page);
    let mut rig = Rig::new();
    rig.scroller = Some(("settings-content", page as u64 + 1));
    rig.settle(&mut app, &view);
    (dir, app, rig)
}

#[test]
fn the_theme_cards_change_the_theme_and_what_the_shell_picks_up() {
    let (_d, mut app, mut rig) = on_page("theme", SettingsView::Appearance);
    rig.click(&mut app, &view, ("theme-card", ThemePreference::Night as u8));
    assert_eq!(app.doc.global_settings.theme, ThemePreference::Night);
    rig.click(&mut app, &view, ("theme-card", ThemePreference::Day as u8));
    assert_eq!(app.doc.global_settings.theme, ThemePreference::Day);
    rig.click(&mut app, &view, ("theme-card", ThemePreference::System as u8));
    assert_eq!(app.doc.global_settings.theme, ThemePreference::System);
}

#[test]
fn zoom_steps_clamp_and_reset_and_the_driver_reads_them() {
    let (_d, mut app, mut rig) = on_page("zoom", SettingsView::Appearance);
    rig.click(&mut app, &view, "zoom-in");
    assert!((app.doc.global_settings.appearance.zoom - 1.1).abs() < 1e-9);
    assert!((zoom(&app) - 1.1).abs() < 1e-6);
    rig.click(&mut app, &view, "zoom-reset");
    assert_eq!(zoom(&app), 1.0);
    for _ in 0..12 {
        rig.click(&mut app, &view, "zoom-out");
    }
    assert_eq!(app.doc.global_settings.appearance.zoom, 0.5, "it stops at the minimum, disabled");
}

#[test]
fn the_appearance_page_has_no_custom_css_and_the_setting_keeps_what_upstream_wrote() {
    let (_d, mut app, mut rig) = on_page("css", SettingsView::Appearance);
    app.doc.global_settings.appearance.custom_css = ".composer { color: red; }".to_owned();
    rig.settle(&mut app, &view);
    assert!(field_rect("custom-css").is_none(), "left out on purpose");
    assert_eq!(app.doc.global_settings.appearance.custom_css, ".composer { color: red; }");
}

#[test]
fn the_glass_sliders_are_there_with_the_glass_and_apply_as_they_move() {
    use mewrk_client::model::{DEFAULT_GLASS_BLUR, DEFAULT_GLASS_TRANSPARENCY};
    let (_d, mut app, mut rig) = on_page("glass-sliders", SettingsView::Appearance);
    rig.settle(&mut app, &view);
    assert!(rect_of("glass-transparency").is_none() && rect_of("glass-blur").is_none(), "no glass, nothing to set");

    app.doc.global_settings.appearance.liquid_glass = true;
    app.doc.global_settings.appearance.background = "builtin:desk".to_owned();
    rig.settle(&mut app, &view);
    let appearance = |app: &App| (app.doc.global_settings.appearance.glass_transparency, app.doc.global_settings.appearance.glass_blur);
    assert_eq!(appearance(&app), (DEFAULT_GLASS_TRANSPARENCY, DEFAULT_GLASS_BLUR));

    rig.reveal(&mut app, &view, "glass-transparency");
    let track = rect_of("glass-transparency").unwrap();
    rig.input.pointer = Some((track.x + track.w * 0.5, track.y + 10.0));
    rig.frame(&mut app, &view);
    rig.input.pressed = true;
    rig.input.down = true;
    rig.frame(&mut app, &view);
    assert_eq!(appearance(&app).0, 50, "the press alone applies, before any release");
    rig.input.pointer = Some((track.right() + 40.0, track.y + 10.0));
    rig.frame(&mut app, &view);
    assert_eq!(appearance(&app).0, 100, "past the end stays at the end");
    rig.input.pointer = Some((track.x - 40.0, track.y + 10.0));
    rig.frame(&mut app, &view);
    assert_eq!(appearance(&app).0, 0);
    rig.input.down = false;
    rig.input.released = true;
    rig.frame(&mut app, &view);
    rig.settle(&mut app, &view);

    let track = rect_of("glass-blur").unwrap();
    rig.input.pointer = Some((track.x + track.w * 0.25, track.y + 10.0));
    rig.frame(&mut app, &view);
    rig.input.pressed = true;
    rig.input.down = true;
    rig.frame(&mut app, &view);
    rig.input.down = false;
    rig.input.released = true;
    rig.frame(&mut app, &view);
    assert_eq!(appearance(&app).1, 15);

    // A solid ground has nothing to blur: the row stays, and the slider takes no press.
    app.doc.global_settings.appearance.background = "solid".to_owned();
    rig.settle(&mut app, &view);
    let track = rect_of("glass-blur").unwrap();
    rig.input.pointer = Some((track.right() - 1.0, track.y + 10.0));
    rig.frame(&mut app, &view);
    rig.input.pressed = true;
    rig.input.down = true;
    rig.frame(&mut app, &view);
    rig.input.down = false;
    rig.input.released = true;
    rig.frame(&mut app, &view);
    assert_eq!(appearance(&app).1, 15);
}

#[test]
fn the_message_font_size_commits_on_release_only() {
    let (_d, mut app, mut rig) = on_page("font", SettingsView::Appearance);
    rig.reveal(&mut app, &view, "font-size-slider");
    let track = rect_of("font-size-slider").unwrap();
    rig.input.pointer = Some((track.x + 2.0, track.y + 10.0));
    rig.frame(&mut app, &view);
    rig.input.pressed = true;
    rig.input.down = true;
    rig.frame(&mut app, &view);
    rig.input.pointer = Some((track.right() - 1.0, track.y + 10.0));
    rig.frame(&mut app, &view);
    assert_eq!(app.doc.global_settings.appearance.message_font_size, 14, "dragging does not persist every pixel");
    rig.input.down = false;
    rig.input.released = true;
    rig.frame(&mut app, &view);
    rig.settle(&mut app, &view);
    assert_eq!(app.doc.global_settings.appearance.message_font_size, 22);
    assert_eq!(message_type(&app).size, 22.0);
}

#[test]
fn accent_and_code_theme_controls_reach_the_tokens() {
    let (_d, mut app, mut rig) = on_page("accent", SettingsView::Appearance);
    rig.click(&mut app, &view, ("accent", "#EF4444"));
    let tokens = themed(fluent::Theme::dark(), &app.doc.global_settings.appearance);
    assert_eq!(tokens.accent, Color::rgb(0xEF4444));
    rig.type_in(&mut app, &view, "accent-hex", "0a0");
    rig.click_away(&mut app, &view);
    assert_eq!(app.doc.global_settings.appearance.theme_color, "#00AA00");
    rig.type_in(&mut app, &view, "accent-hex", "nonsense");
    rig.click_away(&mut app, &view);
    assert_eq!(app.doc.global_settings.appearance.theme_color, "#00AA00", "what doesn't parse is not saved");
    rig.click(&mut app, &view, "accent-default");
    assert_eq!(app.doc.global_settings.appearance.theme_color, "");

    let before = themed(fluent::Theme::light(), &app.doc.global_settings.appearance).code;
    rig.choose(&mut app, &view, ("code-theme", mewrk_client::appearance::CodeThemeScheme::Day as u8), 1);
    assert_ne!(app.doc.global_settings.appearance.code_theme_day, "mewrk-light");
    let after = themed(fluent::Theme::light(), &app.doc.global_settings.appearance).code;
    assert_ne!(before, after);
    assert!(!after.adaptive);
}

#[test]
fn language_applies_at_once_and_its_resolution_rides_along() {
    let _language = LANGUAGE.lock().unwrap_or_else(|p| p.into_inner());
    let (_d, mut app, mut rig) = on_page("lang", SettingsView::Appearance);
    rig.choose(&mut app, &view, "app-language", 2);
    assert_eq!(app.doc.global_settings.resolved_app_language, mewrk_client::model::ResolvedLanguage::EnUs);
    rig.choose(&mut app, &view, "app-language", 1);
    assert_eq!(app.doc.global_settings.resolved_app_language, mewrk_client::model::ResolvedLanguage::ZhCn);
}

#[test]
fn a_send_shortcut_that_collides_moves_the_newline_shortcut() {
    let (_d, mut app, mut rig) = on_page("shortcut", SettingsView::Appearance);
    assert_eq!(app.doc.global_settings.appearance.newline_shortcut, ["Shift", "Enter"]);
    rig.choose(&mut app, &view, ("composer-shortcut", "send"), 1);
    let a = &app.doc.global_settings.appearance;
    assert_eq!(a.send_shortcut, ["Shift", "Enter"]);
    assert_eq!(a.newline_shortcut, ["Enter"]);
}

#[test]
fn a_built_in_picture_becomes_the_backdrop_and_a_solid_one_follows_the_theme() {
    let (_d, mut app, mut rig) = on_page("backdrop", SettingsView::Appearance);
    rig.click(&mut app, &view, "background-open");
    rig.click(&mut app, &view, ("background-builtin", "builtin:desk"));
    assert_eq!(app.doc.global_settings.appearance.background, "builtin:desk");
    rig.until(&mut app, &view, "the picture to decode", |app| backdrop_size(app).is_some());
    assert_eq!(backdrop_size(&mut app), Some((3840, 2400)));
    rig.click(&mut app, &view, ("background-solid", "solid:night"));
    let on_screen_dark = mewrk_settings_global::effects::resolved_scheme(&app) == mewrk_settings_global::logic::background::BackgroundScheme::Night;
    assert_eq!(app.doc.global_settings.appearance.background, if on_screen_dark { "solid" } else { "solid:night" });
    on_scheme_changed(&mut app, false);
    on_scheme_changed(&mut app, true);
    assert_eq!(app.doc.global_settings.appearance.background, "solid", "a solid ground goes back to the new theme's");
}

#[test]
fn an_imported_picture_is_cut_into_tiers_stored_and_removed() {
    let (_d, mut app, mut rig) = on_page("import", SettingsView::Appearance);
    rig.click(&mut app, &view, "background-open");
    rig.until(&mut app, &view, "the list", |app| app.local::<appearance::BackgroundState>().imported.is_some());
    let mut png = Vec::new();
    let pixels: Vec<u8> = (0..800 * 500).flat_map(|i: u32| [(i % 251) as u8, 30, 90, 255]).collect();
    image::ImageEncoder::write_image(image::codecs::png::PngEncoder::new(&mut png), &pixels, 800, 500, image::ExtendedColorType::Rgba8).unwrap();
    appearance::import_background_bytes(&mut app, png);
    rig.until(&mut app, &view, "the import", |app| !app.local::<appearance::BackgroundState>().importing);
    let state = app.local::<appearance::BackgroundState>();
    assert_eq!(state.error, None);
    let background = app.doc.global_settings.appearance.background.clone();
    assert_eq!(background.len(), 64, "the picture's content hash goes into the settings: {background}");
    rig.until(&mut app, &view, "the list to read again", |app| app.local::<appearance::BackgroundState>().imported.as_ref().is_some_and(|l| l.len() == 1));
    assert_eq!(app.local::<appearance::BackgroundState>().imported.as_ref().unwrap()[0].width, 800);
    rig.until(&mut app, &view, "the backdrop", |app| backdrop_size(app).is_some());
    let tile = ("confirm-delete-btn", hash_key(("background-remove", background.as_str())));
    rig.settle(&mut app, &view);
    rig.click(&mut app, &view, tile);
    rig.click(&mut app, &view, tile);
    rig.until(&mut app, &view, "the removal", |app| app.doc.global_settings.appearance.background == "solid");
    rig.until(&mut app, &view, "the list to empty", |app| app.local::<appearance::BackgroundState>().imported.as_ref().is_some_and(Vec::is_empty));
}

#[test]
fn an_unreadable_picture_says_so_in_the_dialog() {
    let (_d, mut app, mut rig) = on_page("unreadable", SettingsView::Appearance);
    rig.click(&mut app, &view, "background-open");
    appearance::import_background_bytes(&mut app, b"not a picture".to_vec());
    rig.until(&mut app, &view, "the failure", |app| !app.local::<appearance::BackgroundState>().importing);
    let message = app.local::<appearance::BackgroundState>().error.clone().unwrap();
    assert!(says(&message, "无法读取这个文件", "This file can't be read"), "{message}");
}

fn fixture(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../motion/tests/fixtures").join(name)
}

#[cfg(target_os = "macos")]
#[test]
fn a_video_goes_in_whole_with_its_first_frame_as_its_picture_and_goes_out_again() {
    use mewrk_settings_global::effects::{backdrop_media, BackdropMedia};
    let (_d, mut app, mut rig) = on_page("import-video", SettingsView::Appearance);
    rig.click(&mut app, &view, "background-open");
    appearance::import_background_file(&mut app, fixture("counting.mp4"));
    rig.until(&mut app, &view, "the import", |app| !app.local::<appearance::BackgroundState>().importing);
    assert_eq!(app.local::<appearance::BackgroundState>().error, None);
    let background = app.doc.global_settings.appearance.background.clone();
    assert_eq!(background.len(), 64, "{background}");
    rig.until(&mut app, &view, "the list to read again", |app| app.local::<appearance::BackgroundState>().imported.as_ref().is_some_and(|l| l.len() == 1));
    let listed = app.local::<appearance::BackgroundState>().imported.as_ref().unwrap()[0].clone();
    assert_eq!(listed.motion.as_deref(), Some("video"));
    assert_eq!((listed.width, listed.height), (64, 36));
    rig.until(&mut app, &view, "the first frame", |app| backdrop_size(app).is_some());
    rig.until(&mut app, &view, "to know it moves", |app| matches!(backdrop_media(app), BackdropMedia::Video { .. }));

    // The first real frame takes over from the picture, which the scene on hand can't know, so it asks for a new one. After that it needn't.
    use mewrk_settings_global::{set_backdrop_covered, set_window_visible, tick_backdrop};
    use std::time::{Duration, Instant};
    let next_change = |app: &mut App| {
        let begun = Instant::now();
        loop {
            let tick = tick_backdrop(app, Instant::now());
            if tick.changed {
                return tick;
            }
            assert!(begun.elapsed() < Duration::from_secs(5), "no frame came");
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    assert!(next_change(&mut app).rebuild, "the first frame");
    assert!(!next_change(&mut app).rebuild, "the next");

    // Out of sight, or under something solid, nothing moves.
    set_window_visible(&mut app, false);
    let now = Instant::now();
    let hidden = tick_backdrop(&mut app, now);
    assert!(!hidden.changed);
    assert!(hidden.next.is_some_and(|next| next > now + Duration::from_secs(1)), "it only comes back to give up the decoder: {:?}", hidden.next.map(|next| next - now));
    std::thread::sleep(Duration::from_millis(300));
    assert!(!tick_backdrop(&mut app, Instant::now()).changed);
    set_window_visible(&mut app, true);
    next_change(&mut app);
    assert!(!set_backdrop_covered(&mut app, true));
    assert!(!tick_backdrop(&mut app, Instant::now()).changed);
    assert!(set_backdrop_covered(&mut app, false), "uncovered, and with something to catch up on");
    next_change(&mut app);

    let tile = ("confirm-delete-btn", hash_key(("background-remove", background.as_str())));
    rig.settle(&mut app, &view);
    rig.click(&mut app, &view, tile);
    rig.click(&mut app, &view, tile);
    rig.until(&mut app, &view, "the removal", |app| app.doc.global_settings.appearance.background == "solid");
    assert!(matches!(backdrop_media(&mut app), BackdropMedia::Still));
}

#[test]
fn an_animated_gif_is_decoded_into_frames_and_a_still_one_stays_a_picture() {
    use mewrk_settings_global::effects::{backdrop_media, BackdropMedia};
    let (dir, mut app, mut rig) = on_page("import-gif", SettingsView::Appearance);
    rig.click(&mut app, &view, "background-open");
    let animated = dir.0.join("moves.gif");
    std::fs::write(&animated, mewrk_motion::animated::test_gif(16, 9, &[[255, 0, 0], [0, 255, 0], [0, 0, 255]], 40)).unwrap();
    appearance::import_background_file(&mut app, animated);
    rig.until(&mut app, &view, "the import", |app| !app.local::<appearance::BackgroundState>().importing);
    assert_eq!(app.local::<appearance::BackgroundState>().error, None);
    rig.until(&mut app, &view, "the frames", |app| matches!(backdrop_media(app), BackdropMedia::Animated { .. }));
    let BackdropMedia::Animated { book } = backdrop_media(&mut app) else { unreachable!() };
    assert_eq!(book.count(), 3);
    assert_eq!(book.duration_us(), 120_000);

    let still = dir.0.join("still.gif");
    std::fs::write(&still, mewrk_motion::animated::test_gif(16, 9, &[[9, 9, 9]], 40)).unwrap();
    appearance::import_background_file(&mut app, still);
    rig.until(&mut app, &view, "the second import", |app| !app.local::<appearance::BackgroundState>().importing);
    rig.until(&mut app, &view, "to know it holds still", |app| matches!(backdrop_media(app), BackdropMedia::Still));
    rig.until(&mut app, &view, "the list to read again", |app| app.local::<appearance::BackgroundState>().imported.as_ref().is_some_and(|l| l.len() == 2));
    let listed = app.local::<appearance::BackgroundState>().imported.clone().unwrap();
    assert_eq!(listed.iter().filter(|image| image.motion.as_deref() == Some("animated")).count(), 1, "the still GIF is just a picture");
}

#[test]
fn what_only_looks_like_a_video_is_refused_and_leaves_nothing_in_the_library() {
    let (dir, mut app, mut rig) = on_page("import-not-video", SettingsView::Appearance);
    rig.click(&mut app, &view, "background-open");
    rig.until(&mut app, &view, "the list", |app| app.local::<appearance::BackgroundState>().imported.is_some());
    let fake = dir.0.join("fake.mp4");
    std::fs::write(&fake, b"\0\0\0\x18ftypisom not a video at all").unwrap();
    appearance::import_background_file(&mut app, fake);
    rig.until(&mut app, &view, "the failure", |app| !app.local::<appearance::BackgroundState>().importing);
    let message = app.local::<appearance::BackgroundState>().error.clone().unwrap();
    assert!(says(&message, "无法读取这个文件", "This file can't be read"), "{message}");
    assert_eq!(app.doc.global_settings.appearance.background, "solid");
    assert!(app.local::<appearance::BackgroundState>().imported.as_ref().unwrap().is_empty());
}

#[test]
fn search_provider_rows_write_the_whole_catalog_and_keys_live_in_their_slot() {
    let (_d, mut app, mut rig) = on_page("search", SettingsView::SearchProviders);
    rig.click(&mut app, &view, ("search-row", "searxng"));
    rig.click(&mut app, &view, ("search-enabled", "searxng"));
    assert_eq!(app.doc.assets.web_search.providers.len(), 10);
    assert!(app.doc.assets.web_search.providers.iter().find(|p| p.kind == SearchProviderKind::Searxng).unwrap().enabled);
    // A trailing comma survives in the field, and what's stored is the parse.
    rig.type_in(&mut app, &view, ("search-engines", "searxng"), "google, ");
    assert_eq!(app.doc.assets.web_search.providers.iter().find(|p| p.kind == SearchProviderKind::Searxng).unwrap().engines, ["google"]);
    rig.type_in(&mut app, &view, ("search-basic-user", "searxng"), "me");
    assert_eq!(app.doc.assets.web_search.providers.iter().find(|p| p.kind == SearchProviderKind::Searxng).unwrap().basic_auth_username, "me");

    rig.click(&mut app, &view, ("search-row", "tavily"));
    assert!(field_rect(("search-host", "tavily")).is_some());
    let id = Identity { provider_kind: "tavily".into(), slot: SearchCredentialSlot::ApiKey };
    rig.type_in(&mut app, &view, ("secret", "tavily", "apiKey"), "search-test-key");
    rig.click_away(&mut app, &view);
    let wait = id.clone();
    rig.until(&mut app, &view, "the save", move |app| field_state(app, &wait).0.is_none() && field_state(app, &wait).1 == 15);
    let status = app.host.invoke_blocking("get_search_key_status", json!({ "providerKind": "tavily", "slot": "apiKey" })).unwrap();
    assert_eq!(status["configured"], json!(true));
    rig.click(&mut app, &view, ("secret-reveal", "tavily", "apiKey"));
    let wait = id.clone();
    rig.until(&mut app, &view, "the read", move |app| field_state(app, &wait).2);
    assert_eq!(field_state(&mut app, &id).0.as_deref(), Some("search-test-key"));
    rig.select_all_and_type(&mut app, &view, "");
    let r = field_rect(("secret", "tavily", "apiKey")).unwrap();
    rig.click_at(&mut app, &view, r.x + 10.0, r.y + 5.0);
    rig.select_all_and_type(&mut app, &view, "");
    rig.click_away(&mut app, &view);
    let wait = id.clone();
    rig.until(&mut app, &view, "the deletion", move |app| field_state(app, &wait).1 == 0);
    let status = app.host.invoke_blocking("get_search_key_status", json!({ "providerKind": "tavily", "slot": "apiKey" })).unwrap();
    assert_eq!(status["configured"], json!(false));
}

fn shortcuts(app: &App) -> &std::collections::BTreeMap<String, ShortcutPreference> {
    &app.doc.global_settings.shortcuts
}

// ShortcutSettings.test.tsx
#[test]
fn refuses_a_recorded_binding_that_conflicts_with_another_enabled_command() {
    let (_d, mut app, mut rig) = on_page("conflict", SettingsView::Shortcuts);
    rig.click(&mut app, &view, ("shortcut-binding", "conversation.create"));
    assert!(on_screen(("shortcut-recording", "conversation.create")));
    rig.press(&mut app, &view, Key::Char(','), |k| k.cmd = true);
    assert!(shortcuts(&app).is_empty(), "nothing changes");
    let page = app.local::<mewrk_settings_global::shortcuts_page::ShortcutsState>();
    assert_eq!(page.conflict, Some(("conversation.create", "app.settings.open")));
}

#[test]
fn persists_a_valid_free_binding_and_enables_its_command() {
    let (_d, mut app, mut rig) = on_page("record", SettingsView::Shortcuts);
    rig.click(&mut app, &view, ("shortcut-binding", "conversation.create"));
    rig.press(&mut app, &view, Key::Char('Y'), |k| {
        k.ctrl = true;
        k.shift = true;
    });
    assert_eq!(shortcuts(&app).get("conversation.create"), Some(&ShortcutPreference { binding: vec!["Control".into(), "Shift".into(), "KeyY".into()], enabled: true }));
    assert!(on_screen(("shortcut-undo", "conversation.create")));
    assert!(!on_screen(("shortcut-recording", "conversation.create")));
}

#[test]
fn a_command_with_no_binding_cannot_be_enabled() {
    let (_d, mut app, mut rig) = on_page("nobinding", SettingsView::Shortcuts);
    rig.click(&mut app, &view, ("shortcut-enabled", "conversation.stop"));
    assert!(shortcuts(&app).is_empty());
}

#[test]
fn undoes_a_command_by_removing_its_sparse_override() {
    let (_d, mut app, mut rig) = on_page("undo", SettingsView::Shortcuts);
    store::change(&mut app, |doc| {
        doc.global_settings.shortcuts.insert("app.settings.open".into(), ShortcutPreference { binding: vec!["Control".into(), "KeyK".into()], enabled: false });
    });
    rig.settle(&mut app, &view);
    rig.click(&mut app, &view, ("shortcut-undo", "app.settings.open"));
    assert!(shortcuts(&app).is_empty());
}

#[test]
fn cancels_recording_on_escape_without_changing_the_binding() {
    let (_d, mut app, mut rig) = on_page("escape", SettingsView::Shortcuts);
    rig.click(&mut app, &view, ("shortcut-binding", "conversation.create"));
    rig.press(&mut app, &view, Key::Escape, |_| {});
    assert!(shortcuts(&app).is_empty());
    assert!(on_screen(("shortcut-binding", "conversation.create")));
    assert_eq!(mewrk_settings_global::current_view(&mut app), Some(SettingsView::Shortcuts), "Escape went to the recorder, not the window");
}

#[test]
fn enable_all_refuses_a_partial_update_on_a_conflict() {
    let (_d, mut app, mut rig) = on_page("bulk", SettingsView::Shortcuts);
    store::change(&mut app, |doc| {
        doc.global_settings.shortcuts.insert("message.copy_last".into(), ShortcutPreference { binding: vec!["Meta".into(), "Comma".into()], enabled: false });
        doc.global_settings.shortcuts.insert("panel.close".into(), ShortcutPreference { binding: vec!["Control".into(), "Comma".into()], enabled: false });
    });
    rig.settle(&mut app, &view);
    let before = shortcuts(&app).clone();
    let visible: Vec<_> = mewrk_client::shortcuts::SHORTCUT_COMMANDS.iter().collect();
    mewrk_settings_global::shortcuts_page::set_visible_enabled(&mut app, &visible, true);
    rig.settle(&mut app, &view);
    assert_eq!(shortcuts(&app), &before, "nothing is written");
    assert!(app.local::<mewrk_settings_global::shortcuts_page::ShortcutsState>().conflict.is_some());
    mewrk_settings_global::shortcuts_page::set_visible_enabled(&mut app, &visible, false);
    assert!(shortcuts(&app).values().all(|p| !p.enabled));
}

// DependencySettings.test.tsx
#[test]
fn the_dependencies_page_detects_and_offers_no_install() {
    let (_d, mut app, mut rig) = on_page("deps", SettingsView::Dependencies);
    rig.until(&mut app, &view, "the probe", |app| app.local::<dependencies::DependenciesState>().snapshots.is_some());
    rig.settle(&mut app, &view);
    let snapshots = app.local::<dependencies::DependenciesState>().snapshots.clone().unwrap();
    assert!(!snapshots.is_empty());
    assert!(snapshots.iter().all(|s| s.builtin), "{snapshots:?}");
    assert!(!on_screen(("tool-delete", snapshots[0].name.as_str())), "built-ins can't be deleted");
    let existing = snapshots[0].name.to_lowercase();
    rig.click(&mut app, &view, "tool-add");
    rig.type_in(&mut app, &view, "tool-name", &existing);
    rig.type_in(&mut app, &view, "tool-executable", "another");
    rig.click(&mut app, &view, "tool-submit");
    assert!(app.doc.global_settings.environment_tools.is_empty(), "a name that exists is refused");
    let known = [existing].into_iter().collect();
    assert_eq!(tool_name_error("custom", &known), "");
    rig.type_in(&mut app, &view, "tool-name", "custom-cli");
    rig.type_in(&mut app, &view, "tool-version-args", "-V --short");
    rig.click(&mut app, &view, "tool-submit");
    assert_eq!(app.doc.global_settings.environment_tools.len(), 1);
    assert_eq!(app.doc.global_settings.environment_tools[0].version_args, ["-V", "--short"]);
    rig.until(&mut app, &view, "the new tool in the snapshot", |app| app.local::<dependencies::DependenciesState>().snapshots.as_ref().is_some_and(|s| s.iter().any(|t| t.name == "custom-cli")));
    rig.settle(&mut app, &view);
    rig.click(&mut app, &view, ("tool-delete", "custom-cli"));
    assert!(app.doc.global_settings.environment_tools.is_empty());
}

// UpdateSettings.test.tsx: a check already made, so nothing here goes to GitHub.
#[test]
fn the_updates_page_reads_the_version_and_shows_a_finished_check() {
    let dir = TempDir::new("updates");
    let mut app = start_app(&dir.0);
    let check = AppUpdateCheck { current_version: "1.0.0".into(), latest_version: "1.1.0".into(), update_available: true, release: AppRelease { tag: "v1.1.0".into(), name: "Mewrk 1.1.0".into(), html_url: "https://x.test/release".into(), notes: "**Fixed** things".into(), published_at: "2026-01-01T00:00:00Z".into() }, asset: None, checksums_asset: None, checked_at: "2026-01-02T00:00:00Z".into(), in_app_install: false };
    {
        let controller = app.local::<AppUpdateController>();
        let sequence = controller.begin_check(true, mewrk_settings_global::logic::usage::now_ms()).unwrap();
        controller.finish_check(sequence, Ok(check), mewrk_settings_global::logic::usage::now_ms());
    }
    app.local::<mewrk_settings_global::updates::UpdatesPage>().release_channel = true;
    open(&mut app, SettingsView::Updates);
    let mut rig = Rig::new();
    rig.settle(&mut app, &view);
    rig.until(&mut app, &view, "the version", |app| app.local::<mewrk_settings_global::updates::UpdatesPage>().info.is_some());
    rig.settle(&mut app, &view);
    assert!(on_screen("update-release-page"), "no asset here: the release page is the way");
    assert!(!on_screen("update-download"));
    assert!(on_screen("update-check"));
}

// This build isn't published on upstream's release page, so an upstream release is not an update of it.
#[test]
fn a_build_with_no_release_channel_shows_its_version_and_asks_github_nothing() {
    let (_d, mut app, mut rig) = on_page("no-channel", SettingsView::Updates);
    rig.until(&mut app, &view, "the version", |app| app.local::<mewrk_settings_global::updates::UpdatesPage>().info.is_some());
    rig.settle(&mut app, &view);
    assert!(on_screen("current-version"));
    assert!(on_screen("update-source"));
    assert!(!on_screen("update-releases"), "nothing to point at");
    assert!(!on_screen("update-check"));
    assert_eq!(app.local::<AppUpdateController>().state, AppUpdateState::Idle, "no check was started");
}

#[test]
fn the_usage_page_reads_the_ledger_and_switches_views() {
    let (_d, mut app, mut rig) = on_page("usage", SettingsView::Usage);
    std::thread::sleep(Duration::from_millis(50));
    rig.settle(&mut app, &view);
    assert!(on_screen("usage-heatmap"));
    rig.click(&mut app, &view, ("usage-seg", 2u64));
    rig.settle(&mut app, &view);
    assert!(!on_screen("usage-heatmap"));
    close(&mut app);
    rig.settle(&mut app, &view);
}

// CodexLoginPanel.test.tsx, as far as a real host answers without a browser or a network.
#[test]
fn the_codex_row_shows_its_sign_in_and_has_no_key_or_address_of_its_own() {
    let dir = TempDir::new("codex");
    let mut app = start_app(&dir.0);
    let row = mewrk_client::model::ApiProvider { id: "provider_codex".into(), name: "OpenAI Codex".into(), enabled: false, family: mewrk_client::model::ProviderFamily::OpenaiCodex, base_url: String::new(), family_settings: Default::default(), notes: String::new(), models: vec![], active_model_id: None };
    store::change(&mut app, |doc| {
        doc.assets.api_providers = vec![row];
        doc.global_settings.active_provider_id = None;
    });
    open(&mut app, SettingsView::Providers);
    let mut rig = Rig::new();
    rig.settle(&mut app, &view);
    rig.until(&mut app, &view, "the sign-in status", |app| mewrk_settings_global::providers::state(app).codex.status.is_some() || mewrk_settings_global::providers::state(app).codex.error.is_some());
    rig.settle(&mut app, &view);
    assert_eq!(mewrk_settings_global::providers::state(&mut app).codex.error, None);
    assert!(on_screen("codex-sign-in"));
    assert!(field_rect(("provider-key", "provider_codex")).is_none());
    assert!(field_rect(("provider-address", "provider_codex")).is_none());
    // The drawer holds the one address a Codex row has: a local test stub's.
    rig.click(&mut app, &view, "provider-options");
    assert!(field_rect(("provider-stub", "provider_codex")).is_some());
}

fn workspace_view(app: &mut App) -> E {
    use mewrk_settings_global::{workspace_settings_dialog, WorkspaceSettingsProps};
    #[derive(Default)]
    struct Stored {
        vars: std::collections::BTreeMap<String, String>,
        sandbox: Option<mewrk_client::model::SandboxSettings>,
    }
    let stored = app.local::<Stored>();
    let (vars, sandbox) = (stored.vars.clone(), stored.sandbox.clone());
    let dialog = workspace_settings_dialog(
        app,
        WorkspaceSettingsProps {
            name: "proj".into(),
            machine: None,
            path: "/tmp/proj".into(),
            ssh_machines: vec![],
            vars,
            sandbox,
            on_change_vars: Rc::new(|app, v| app.local::<Stored>().vars = v),
            on_change_sandbox: Rc::new(|app, v| app.local::<Stored>().sandbox = Some(v)),
            on_close: Rc::new(|_| {}),
        },
    );
    // The test reads back what the handlers wrote.
    let (count, enabled) = {
        let s = app.local::<Stored>();
        (s.vars.len(), s.sandbox.as_ref().is_some_and(|b| b.enabled))
    };
    app.local::<WorkspaceProbe>().vars = count;
    app.local::<WorkspaceProbe>().sandbox_on = enabled;
    div().w_full().h_full().child(dialog)
}

#[derive(Default)]
struct WorkspaceProbe {
    vars: usize,
    sandbox_on: bool,
}

// SandboxSettings.test.tsx and WorkspaceSettings.tsx
#[test]
fn a_workspaces_window_asks_its_machine_and_saves_only_what_parses() {
    let dir = TempDir::new("workspace");
    let mut app = start_app(&dir.0);
    let mut rig = Rig::new();
    rig.settle(&mut app, &workspace_view);
    rig.until(&mut app, &workspace_view, "the machine's answer", |app| app.local::<mewrk_settings_global::sandbox::SandboxPageState>().support.is_some());
    // A test binary has no agent next to it, so this may read as "not available"; either way the page has the answer.
    let support = app.local::<mewrk_settings_global::sandbox::SandboxPageState>().support.clone().unwrap();
    eprintln!("sandbox here: {support:?}");
    assert!(support.available || !support.detail.is_empty(), "{support:?}");
    rig.click(&mut app, &workspace_view, "sandbox-enabled");
    rig.settle(&mut app, &workspace_view);
    assert!(app.local::<WorkspaceProbe>().sandbox_on);
    let scope = hash_key("/tmp/proj");
    rig.scroller = Some(("workspace-settings-page", scope));
    rig.type_in(&mut app, &workspace_view, ("env-input", scope), "A=1");
    rig.press(&mut app, &workspace_view, Key::Enter, |_| {});
    rig.input.text = "B=2".to_owned();
    rig.settle(&mut app, &workspace_view);
    assert_eq!(app.local::<WorkspaceProbe>().vars, 2);
    // A draft that doesn't parse keeps the last that did.
    rig.select_all_and_type(&mut app, &workspace_view, "BASH_ENV=x");
    rig.settle(&mut app, &workspace_view);
    assert_eq!(app.local::<WorkspaceProbe>().vars, 2);
}

fn machine_view(app: &mut App) -> E {
    use mewrk_settings_global::{machine_settings_dialog, MachineSettingsProps};
    let dialog = machine_settings_dialog(
        app,
        MachineSettingsProps {
            machine: None,
            ssh_machines: vec![],
            projects: 0,
            conversations: 0,
            on_set_wsl_agent_shell: Rc::new(|_, _| {}),
            on_save_ssh_machine: Rc::new(|_, _| {}),
            on_delete_ssh_machine: Rc::new(|_, _| {}),
            on_close: Rc::new(|_| {}),
        },
    );
    div().w_full().h_full().child(dialog)
}

#[test]
fn this_machines_window_probes_its_shells() {
    let dir = TempDir::new("machine");
    let mut app = start_app(&dir.0);
    let mut rig = Rig::new();
    rig.settle(&mut app, &machine_view);
    rig.click(&mut app, &machine_view, "machine-probe");
    rig.until(&mut app, &machine_view, "the probe", |_| !on_screen("machine-probe") || fluent::el::rect_of("machine-probe").is_some());
    for _ in 0..100 {
        rig.frame(&mut app, &machine_view);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(on_screen("machine-close"));
}

fn ssh_view(app: &mut App) -> E {
    use mewrk_settings_global::{machine_settings_dialog, MachineSettingsProps};
    let machine: SshMachineConfig = serde_json::from_value(json!({ "id": "sshm_1", "name": "build", "host": "me@build", "port": 2222, "identityFile": "", "createdAt": "2026-01-01T00:00:00.000Z", "updatedAt": "2026-01-01T00:00:00.000Z" })).unwrap();
    let dialog = machine_settings_dialog(
        app,
        MachineSettingsProps {
            machine: Some(RunTarget::Ssh { machine_id: machine.id.clone() }),
            ssh_machines: vec![machine],
            projects: 2,
            conversations: 1,
            on_set_wsl_agent_shell: Rc::new(|_, _| {}),
            on_save_ssh_machine: Rc::new(|app, saved| app.local::<SavedMachine>().0 = Some(saved)),
            on_delete_ssh_machine: Rc::new(|_, _| {}),
            on_close: Rc::new(|app| app.local::<MachineDialogShown>().0 = false),
        },
    );
    div().w_full().h_full().child(dialog)
}

#[derive(Default)]
struct SavedMachine(Option<SshMachineConfig>);

struct MachineDialogShown(bool);

impl Default for MachineDialogShown {
    fn default() -> Self {
        Self(true)
    }
}

/// The dialog only while something has it open, as the shell's `machine_settings` does.
fn ssh_window(app: &mut App) -> E {
    if app.local::<MachineDialogShown>().0 { ssh_view(app) } else { div() }
}

#[test]
fn an_ssh_machines_window_starts_over_each_time_it_opens() {
    let dir = TempDir::new("ssh-window");
    let mut app = start_app(&dir.0);
    let mut rig = Rig::new();
    rig.settle(&mut app, &ssh_window);
    rig.type_in(&mut app, &ssh_window, ("ssh", "name"), "edited");
    rig.click(&mut app, &ssh_window, "ssh-cancel");
    rig.settle(&mut app, &ssh_window);
    assert!(!on_screen("ssh-save"), "cancelled");
    app.local::<MachineDialogShown>().0 = true;
    rig.settle(&mut app, &ssh_window);
    // What was typed went with the window it was typed in.
    rig.click(&mut app, &ssh_window, "ssh-save");
    let saved = app.local::<SavedMachine>().0.clone().expect("saved");
    assert_eq!(saved.name, "build");
    assert_eq!(saved.port, 2222);
}

#[test]
fn an_ssh_machines_delete_stands_apart_from_the_other_two_and_asks_first() {
    let dir = TempDir::new("ssh-delete");
    let mut app = start_app(&dir.0);
    let mut rig = Rig::new();
    rig.settle(&mut app, &ssh_window);
    let (delete, cancel, save) = (rect_of("ssh-delete").unwrap(), rect_of("ssh-cancel").unwrap(), rect_of("ssh-save").unwrap());
    assert!(cancel.x - delete.right() > 50.0, "`margin-right: auto`: {delete:?} {cancel:?}");
    assert_eq!(save.x - cancel.right(), 8.0, "{cancel:?} {save:?}");
    rig.click(&mut app, &ssh_window, "ssh-delete");
    assert!(on_screen("ssh-delete-confirm"));
    rig.click(&mut app, &ssh_window, "ssh-delete-cancel");
    assert!(on_screen("ssh-save"));
}

// AppearanceSettings.test.tsx: the local model, as far as a real host answers offline.
#[test]
fn turning_a_use_on_without_a_model_asks_first_and_declining_leaves_it_off() {
    use mewrk_settings_global::appearance::local_model_status;
    let (_d, mut app, mut rig) = on_page("local", SettingsView::Appearance);
    rig.until(&mut app, &view, "the status", |app| local_model_status(app).is_some());
    rig.settle(&mut app, &view);
    let status = local_model_status(&mut app).unwrap();
    rig.click(&mut app, &view, ("local-use", "subagents"));
    if status.recommended.is_none() {
        assert!(!app.doc.global_settings.appearance.local_model.subagents, "this machine can't run it, so every switch is off and disabled");
        return;
    }
    assert!(app.doc.global_settings.appearance.local_model.subagents, "it turns no use on by itself, so nothing to download");
    assert!(!on_screen("variant-cancel"));
    rig.click(&mut app, &view, ("local-use", "titles"));
    assert!(on_screen("variant-cancel"), "a use needs a model: which build?");
    rig.click(&mut app, &view, "variant-cancel");
    assert!(!app.doc.global_settings.appearance.local_model.titles);
    rig.click(&mut app, &view, "local-manage");
    rig.until(&mut app, &view, "the default prompts", |app| field_rect(("local-prompt", "title")).is_some() && !app.doc.global_settings.appearance.local_model.title_prompt.is_empty() || field_rect(("local-prompt", "title")).is_some());
    rig.scroller = None;
    rig.type_in(&mut app, &view, ("local-prompt", "title"), "Name it briefly.");
    assert_eq!(app.doc.global_settings.appearance.local_model.title_prompt, "Name it briefly.");
    rig.click(&mut app, &view, ("local-prompt-reset", "title"));
    assert_eq!(app.doc.global_settings.appearance.local_model.title_prompt, "");
}

// AppearanceSettings/index.tsx `FontCombobox`: what was typed decides, an empty value included.
#[test]
fn a_custom_font_goes_back_to_the_list_when_what_is_typed_is_a_preset() {
    let (_d, mut app, mut rig) = on_page("font", SettingsView::Appearance);
    assert!(field_rect(("font-custom", "ui")).is_none());
    rig.choose(&mut app, &view, ("font", "ui"), mewrk_client::appearance::UI_FONT_PRESETS.len());
    assert!(field_rect(("font-custom", "ui")).is_some(), "choosing Custom opens the input");
    assert_eq!(app.doc.global_settings.appearance.ui_font_family, "", "and saves nothing until it changes");
    rig.type_in(&mut app, &view, ("font-custom", "ui"), "Foo Sans");
    assert_eq!(app.doc.global_settings.appearance.ui_font_family, "Foo Sans");
    assert!(field_rect(("font-custom", "ui")).is_some());
    rig.type_in(&mut app, &view, ("font-custom", "ui"), "");
    assert_eq!(app.doc.global_settings.appearance.ui_font_family, "");
    assert!(field_rect(("font-custom", "ui")).is_none(), "an empty family is the Default preset");
}

// feedback.css and menus.css: 190px, 154px at max-width 860, and a strip across the top at 720.
#[test]
fn a_narrower_window_narrows_the_rail_and_then_lays_it_across_the_top() {
    let (_d, mut app, mut rig) = on_page("strip", SettingsView::Appearance);
    let item = |view_id: SettingsView| rect_of(("settings-nav", view_id as u64 + 1)).unwrap();
    let content = || rect_of(("settings-content", SettingsView::Appearance as u64 + 1)).unwrap();
    let (first, second) = (item(SettingsView::Providers), item(SettingsView::SearchProviders));
    assert_eq!(first.x, second.x, "a column of pages");
    assert!(content().x >= first.x + 172.0, "beside a 190 px rail");
    rig.size = (800.0, 600.0);
    rig.settle(&mut app, &view);
    assert_eq!(item(SettingsView::Providers).x, item(SettingsView::SearchProviders).x);
    assert!(content().x - item(SettingsView::Providers).x <= 150.0, "beside a 154 px rail");
    rig.size = (700.0, 520.0);
    rig.settle(&mut app, &view);
    let (first, second) = (item(SettingsView::Providers), item(SettingsView::SearchProviders));
    assert!(second.x > first.x, "side by side in a strip");
    assert!(content().y >= item(SettingsView::Updates).bottom(), "with the page under every one of them");
}

// SecretField.tsx: its identity is what resets it, so a key on screen doesn't wait there for the row to come back.
#[test]
fn a_secret_on_screen_goes_when_another_row_is_selected() {
    let (_d, mut app, mut rig) = on_page("secret-identity", SettingsView::SearchProviders);
    rig.click(&mut app, &view, ("search-row", "tavily"));
    let id = Identity { provider_kind: "tavily".into(), slot: SearchCredentialSlot::ApiKey };
    rig.type_in(&mut app, &view, ("secret", "tavily", "apiKey"), "search-test-key");
    rig.click_away(&mut app, &view);
    let wait = id.clone();
    rig.until(&mut app, &view, "the save", move |app| field_state(app, &wait).1 == 15);
    rig.click(&mut app, &view, ("secret-reveal", "tavily", "apiKey"));
    let wait = id.clone();
    rig.until(&mut app, &view, "the read", move |app| field_state(app, &wait).2);
    rig.click(&mut app, &view, ("search-row", "exa"));
    rig.click(&mut app, &view, ("search-row", "tavily"));
    let (draft, _, visible) = field_state(&mut app, &id);
    assert_eq!((draft, visible), (None, false));
    let wait = id.clone();
    rig.until(&mut app, &view, "the mask", move |app| field_state(app, &wait).1 > 0);
}

// CodexLoginPanel.tsx unmounts with its provider: no poll goes on behind another row, and coming back reads the status again.
#[test]
fn leaving_the_codex_row_ends_its_panel_and_coming_back_starts_it_over() {
    use mewrk_client::model::{ApiProvider, ProviderFamily};
    let dir = TempDir::new("codex-leave");
    let mut app = start_app(&dir.0);
    let row = |id: &str, family| ApiProvider { id: id.into(), name: id.into(), enabled: false, family, base_url: String::new(), family_settings: Default::default(), notes: String::new(), models: vec![], active_model_id: None };
    let rows = vec![row("provider_codex", ProviderFamily::OpenaiCodex), row("provider_other", ProviderFamily::OpenaiChat)];
    store::change(&mut app, |doc| {
        doc.assets.api_providers = rows;
        doc.global_settings.active_provider_id = None;
    });
    open(&mut app, SettingsView::Providers);
    let mut rig = Rig::new();
    rig.settle(&mut app, &view);
    let read = |app: &mut App| mewrk_settings_global::providers::state(app).codex.status.is_some();
    rig.until(&mut app, &view, "the sign-in status", read);
    rig.click(&mut app, &view, ("provider-row", "provider_other"));
    assert!(mewrk_settings_global::providers::state(&mut app).codex.status.is_none(), "the panel's state went with it");
    assert!(mewrk_settings_global::providers::state(&mut app).codex.for_provider.is_none());
    rig.click(&mut app, &view, ("provider-row", "provider_codex"));
    assert_eq!(mewrk_settings_global::providers::state(&mut app).codex.for_provider.as_deref(), Some("provider_codex"), "asked again");
    rig.until(&mut app, &view, "the status again", read);
}

// DependencySettings' dialog is one form: Enter in any field submits it.
#[test]
fn enter_in_any_field_of_the_add_tool_dialog_submits_it() {
    let (_d, mut app, mut rig) = on_page("deps-enter", SettingsView::Dependencies);
    rig.click(&mut app, &view, "tool-add");
    rig.type_in(&mut app, &view, "tool-name", "custom-cli");
    rig.type_in(&mut app, &view, "tool-executable", "custom");
    rig.press(&mut app, &view, Key::Enter, |_| {});
    assert_eq!(app.doc.global_settings.environment_tools.last().map(|t| t.name.as_str()), Some("custom-cli"));
    assert!(!on_screen("tool-submit"), "and closes");
}

#[test]
fn at_720_and_under_the_rail_is_a_strip_that_scrolls_sideways_rather_than_wraps() {
    let (_d, mut app, mut rig) = on_page("strip", SettingsView::Appearance);
    rig.size = (560.0, 900.0);
    rig.settle(&mut app, &view);
    let (first, last) = (rect_of(("settings-nav", SettingsView::Providers.key())).expect("the first page"), rect_of(("settings-nav", SettingsView::Updates.key())).expect("the last page"));
    assert_eq!(first.y, last.y, "one line");
    assert!(last.right() > rig.size.0, "and what doesn't fit runs on past the edge: {last:?}");
    rig.input.pointer = Some((first.x + 4.0, first.y + 4.0));
    rig.frame(&mut app, &view);
    rig.input.scroll_x = -200.0;
    rig.frame(&mut app, &view);
    rig.frame(&mut app, &view);
    assert_eq!(rect_of(("settings-nav", SettingsView::Updates.key())).unwrap().x, last.x - 200.0);
}

#[test]
fn a_font_menu_sets_each_family_in_itself() {
    let (_d, mut app, mut rig) = on_page("font-menu", SettingsView::Appearance);
    rig.click(&mut app, &view, hash_key(("select", hash_key(("font", "ui")))));
    rig.settle(&mut app, &view);
    assert!(on_screen(("select-panel", hash_key(("select", hash_key(("font", "ui")))))), "the menu is open");
    rig.click(&mut app, &view, hash_key(("select", hash_key(("font", "ui")))));
    // Set in the family they name: the rows have no text leaf of the interface font for these, only for Default and Custom.
    rig.choose(&mut app, &view, ("font", "ui"), 7);
    assert_eq!(app.doc.global_settings.appearance.ui_font_family, "Arial");
}

// ---------------------------------------------------------------- accessibility

#[test]
fn the_appearance_page_says_what_each_control_sets() {
    let _language = LANGUAGE.lock().unwrap_or_else(|p| p.into_inner());
    let (_d, mut app, mut rig) = on_page("a11y-appearance", SettingsView::Appearance);
    let tree = a11y_tree(&mut rig, &mut app, &view);
    assert!(a11y_is(&tree, Role::Button, t("深色", "Dark")).toggled().is_some(), "a theme card says whether it is the one");
    a11y_is(&tree, Role::Button, t("默认主色", "Default accent color"));
    assert!(a11y_named(&tree, &tf("主色 {color}", "Accent color {color}", &[("color", &"#356AE6")])).toggled().is_some());
    a11y_is(&tree, Role::TextInput, t("十六进制主色", "Hex accent color"));
    a11y_is(&tree, Role::ComboBox, t("浅色主题下的代码外观", "Code appearance in the light theme"));
    a11y_is(&tree, Role::ComboBox, t("应用语言", "Application language"));
    a11y_is(&tree, Role::ComboBox, t("界面字体", "Interface font"));
    a11y_is(&tree, Role::ComboBox, t("发送快捷键", "Send shortcut"));
    a11y_is(&tree, Role::Switch, t("液态玻璃", "Liquid glass"));
    a11y_is(&tree, Role::Switch, t("也用于子代理", "Also for subagents"));
    let size = a11y_is(&tree, Role::Slider, t("消息字号", "Message font size"));
    assert!(size.numeric_value().is_some() && size.min_numeric_value().is_some() && size.max_numeric_value().is_some());
    fluent::el::set_a11y_active(false);
}

#[test]
fn the_background_library_is_a_named_dialog_of_choices_that_say_which_is_on() {
    let _language = LANGUAGE.lock().unwrap_or_else(|p| p.into_inner());
    let (_d, mut app, mut rig) = on_page("a11y-background", SettingsView::Appearance);
    rig.click(&mut app, &view, "background-open");
    let tree = a11y_tree(&mut rig, &mut app, &view);
    let dialog = a11y_is(&tree, Role::Dialog, t("背景", "Background"));
    assert!(dialog.is_modal());
    assert_eq!(dialog.described_by().len(), 1, "its lede describes it");
    a11y_is(&tree, Role::Region, t("内置背景", "Built-in backgrounds"));
    assert!(a11y_is(&tree, Role::Button, t("浅色纯色", "Light solid")).toggled().is_some());
    assert!(a11y_is(&tree, Role::Button, t("桌子下", "Under the desk")).toggled().is_some());
    assert_eq!(a11y_unnamed(&tree), Vec::<String>::new());
    fluent::el::set_a11y_active(false);
}

#[test]
fn shortcuts_say_what_a_binding_does_and_a_conflict_is_an_alert() {
    let _language = LANGUAGE.lock().unwrap_or_else(|p| p.into_inner());
    let (_d, mut app, mut rig) = on_page("a11y-shortcuts", SettingsView::Shortcuts);
    let tree = a11y_tree(&mut rig, &mut app, &view);
    a11y_is(&tree, Role::ComboBox, t("筛选快捷键分组", "Filter shortcut groups"));
    assert_eq!(a11y_named(&tree, t("更多快捷键操作", "More shortcut actions")).has_popup(), Some(accesskit::HasPopup::Menu));
    let command = &mewrk_client::shortcuts::SHORTCUT_COMMANDS[0];
    let label = command.label();
    a11y_is(&tree, Role::Button, &tf("修改「{name}」的快捷键", "Change shortcut for “{name}”", &[("name", &label)]));
    a11y_is(&tree, Role::Switch, &tf("启用「{name}」", "Enable “{name}”", &[("name", &label)]));
    fluent::el::set_a11y_active(false);
}

#[test]
fn the_usage_page_is_tabs_a_range_and_a_picture_that_says_what_it_shows() {
    let _language = LANGUAGE.lock().unwrap_or_else(|p| p.into_inner());
    let (_d, mut app, mut rig) = on_page("a11y-usage", SettingsView::Usage);
    let tree = a11y_tree(&mut rig, &mut app, &view);
    a11y_is(&tree, Role::Region, t("使用统计", "Usage statistics"));
    a11y_is(&tree, Role::TabList, t("统计视图", "Statistics view"));
    assert_eq!(a11y_is(&tree, Role::Tab, t("总览", "Overview")).is_selected(), Some(true));
    assert_eq!(a11y_is(&tree, Role::Tab, t("模型", "Models")).is_selected(), Some(false));
    a11y_is(&tree, Role::Group, t("统计区间", "Statistics range"));
    assert!(a11y_named(&tree, t("30 天", "30d")).toggled().is_some());
    let heatmap = tree.iter().map(|(_, node)| node).find(|node| node.role() == Role::Image && a11y_name(node).contains("26")).expect("the heatmap has a name");
    assert!(heatmap.children().is_empty(), "its cells are one picture");
    fluent::el::set_a11y_active(false);
}

#[test]
fn a_dependency_with_a_bad_name_says_so_on_the_field_and_out_loud() {
    let _language = LANGUAGE.lock().unwrap_or_else(|p| p.into_inner());
    let (_d, mut app, mut rig) = on_page("a11y-deps", SettingsView::Dependencies);
    rig.click(&mut app, &view, "tool-add");
    rig.type_in(&mut app, &view, "tool-name", "bad name");
    let tree = a11y_tree(&mut rig, &mut app, &view);
    let name = a11y_is(&tree, Role::TextInput, t("名称", "Name"));
    assert!(name.invalid().is_some());
    assert_eq!(name.described_by().len(), 1);
    let alert = tree.iter().map(|(_, node)| node).find(|node| node.role() == Role::Alert).expect("the error is an alert");
    assert!(alert.live().is_some());
    assert_eq!(a11y_unnamed(&tree), Vec::<String>::new());
    fluent::el::set_a11y_active(false);
}

#[test]
fn the_windows_opened_from_a_gear_name_their_controls_and_tab_goes_round_them() {
    let _language = LANGUAGE.lock().unwrap_or_else(|p| p.into_inner());
    let dir = TempDir::new("a11y-windows");
    let mut app = start_app(&dir.0);
    let mut rig = Rig::new();
    rig.settle(&mut app, &workspace_view);
    rig.until(&mut app, &workspace_view, "the machine's answer", |app| app.local::<mewrk_settings_global::sandbox::SandboxPageState>().support.is_some());
    let tree = a11y_tree(&mut rig, &mut app, &workspace_view);
    a11y_is(&tree, Role::Region, t("工作区设置", "Workspace settings"));
    a11y_is(&tree, Role::Navigation, t("工作区设置分类", "Workspace settings categories"));
    a11y_is(&tree, Role::MultilineTextInput, t("环境变量", "Environment variables"));
    a11y_is(&tree, Role::Switch, t("在沙箱中运行命令", "Run commands in the sandbox"));
    a11y_is(&tree, Role::ComboBox, t("网络", "Network"));
    a11y_is(&tree, Role::MultilineTextInput, t("额外可写目录", "Further writable directories"));
    assert_eq!(a11y_unnamed(&tree), Vec::<String>::new());
    let stops = a11y_tab_stops(&mut rig, &mut app, &workspace_view);
    assert!(stops.iter().all(|stop| !stop.ends_with("\"\"")), "{stops:#?}");
    fluent::el::set_a11y_active(false);

    let dir = TempDir::new("a11y-machine");
    let mut app = start_app(&dir.0);
    let mut rig = Rig::new();
    let tree = a11y_tree(&mut rig, &mut app, &machine_view);
    assert_eq!(a11y_unnamed(&tree), Vec::<String>::new());
    assert!(tree.iter().any(|(_, node)| node.role() == Role::Dialog && !a11y_name(node).is_empty()));
    a11y_is(&tree, Role::Region, t("Shell 后端", "Shell backends"));
    fluent::el::set_a11y_active(false);

    let dir = TempDir::new("a11y-ssh");
    let mut app = start_app(&dir.0);
    let mut rig = Rig::new();
    let tree = a11y_tree(&mut rig, &mut app, &ssh_view);
    a11y_is(&tree, Role::TextInput, t("主机", "Host"));
    a11y_is(&tree, Role::TextInput, t("身份文件（可选）", "Identity file (optional)"));
    assert_eq!(a11y_unnamed(&tree), Vec::<String>::new());
    fluent::el::set_a11y_active(false);
}

#[test]
fn the_local_models_window_names_its_builds_and_prompts() {
    let _language = LANGUAGE.lock().unwrap_or_else(|p| p.into_inner());
    let (_d, mut app, mut rig) = on_page("a11y-local", SettingsView::Appearance);
    rig.click(&mut app, &view, "local-manage");
    let tree = a11y_tree(&mut rig, &mut app, &view);
    a11y_is(&tree, Role::Dialog, t("本地小模型", "Local model"));
    a11y_is(&tree, Role::Region, t("模型", "Models"));
    a11y_is(&tree, Role::MultilineTextInput, t("会话标题的前置提示词", "Prompt for conversation titles"));
    assert_eq!(a11y_unnamed(&tree), Vec::<String>::new());
    fluent::el::set_a11y_active(false);
}

// ---------------------------------------------------------------- permissions

fn with_sheet(app: &mut App) -> E {
    div().w_full().h_full().child(global_settings_window(app)).child(mewrk_settings_global::first_launch_dialog(app))
}

fn permission_answers(app: &mut App, full_disk_access: DiskAccess, keychain: KeychainAccess) {
    let s = permissions_page::state(app);
    s.started = true;
    s.snapshot = Some(PermissionsSnapshot { full_disk_access, keychain, app_bundle: Some("/Applications/Mewrk.app".to_owned()), ad_hoc_signed: false, prompted: false });
}

#[test]
fn permissions_is_the_last_page_of_the_rail() {
    let (_d, mut app, mut rig) = on_page("permissions-rail", SettingsView::Appearance);
    let nav = |page: SettingsView| rect_of(("settings-nav", page as u64 + 1)).unwrap();
    assert!(nav(SettingsView::Permissions).y > nav(SettingsView::Updates).y);
    rig.click(&mut app, &view, ("settings-nav", SettingsView::Permissions as u64 + 1));
    assert_eq!(mewrk_settings_global::current_view(&mut app), Some(SettingsView::Permissions));
    assert_eq!(SettingsView::from_wire("permissions"), SettingsView::Permissions);
}

#[test]
fn the_page_offers_what_is_missing_and_nothing_that_is_not() {
    let (_d, mut app, mut rig) = on_page("permissions-rows", SettingsView::Appearance);
    permission_answers(&mut app, DiskAccess::Denied, KeychainAccess::NeedsPermission);
    open(&mut app, SettingsView::Permissions);
    rig.settle(&mut app, &view);
    assert!(on_screen(("permission-disk", "page")));
    assert!(on_screen(("permission-reveal", "page")), "the app is there to drag into the list");
    assert!(on_screen(("permission-keychain", "page")));

    permission_answers(&mut app, DiskAccess::Granted, KeychainAccess::Granted);
    rig.settle(&mut app, &view);
    assert!(on_screen(("permission-disk", "page")), "the way to turn it back off stays");
    assert!(!on_screen(("permission-reveal", "page")));
    assert!(!on_screen(("permission-keychain", "page")));

    // "Allow" works until the app quits, and the row says so and offers the way to make it stick.
    permission_answers(&mut app, DiskAccess::Granted, KeychainAccess::ThisLaunchOnly);
    rig.settle(&mut app, &view);
    assert!(on_screen(("permission-keychain", "page")));
}

/// The host's answer, over the real command: the keychain's is "not here", since a test never installs the vault.
#[test]
fn the_page_asks_the_host_and_allow_comes_back_without_the_real_keychain() {
    let (_d, mut app, mut rig) = on_page("permissions-host", SettingsView::Permissions);
    rig.until(&mut app, &view, "the first answer", |app| permissions_page::state(app).snapshot.is_some());
    assert_eq!(permissions_page::state(&mut app).snapshot.as_ref().unwrap().keychain, KeychainAccess::NotApplicable);
    assert!(!permissions_page::state(&mut app).snapshot.as_ref().unwrap().prompted);

    permissions_page::state(&mut app).snapshot.as_mut().unwrap().keychain = KeychainAccess::NeedsPermission;
    rig.settle(&mut app, &view);
    rig.click(&mut app, &view, ("permission-keychain", "page"));
    rig.until(&mut app, &view, "the answer to Allow", |app| !permissions_page::state(app).asking_keychain);
    assert_eq!(permissions_page::state(&mut app).snapshot.as_ref().unwrap().keychain, KeychainAccess::NotApplicable);

    // Only the panes the app declares reach the system.
    let refused = app.host.invoke_blocking("open_privacy_settings", json!({ "pane": "https://example.com" })).unwrap_err();
    assert!(refused.contains("隐私设置页面"), "the pane itself is what's refused, not the call: {refused}");
}

#[test]
fn the_sheet_asks_once_and_any_answer_settles_it() {
    let dir = TempDir::new("permissions-sheet");
    let marker = dir.0.join("permissions-prompted");
    let mut app = start_app(&dir.0);
    let mut rig = Rig::new();
    rig.settle(&mut app, &with_sheet);
    assert!(!on_screen("permissions-first-launch-done"), "only the driver starts it");

    // What the host says depends on this machine's Full Disk Access, so the state is written in once it has answered.
    mewrk_settings_global::first_launch(&mut app);
    rig.until(&mut app, &with_sheet, "the first answer", |app| permissions_page::state(app).snapshot.is_some());
    permission_answers(&mut app, DiskAccess::Denied, KeychainAccess::NeedsPermission);
    permissions_page::state(&mut app).first_launch_open = true;
    rig.settle(&mut app, &with_sheet);
    assert!(on_screen(("permission-disk", "sheet")));
    assert!(on_screen(("permission-keychain", "sheet")));

    rig.click(&mut app, &with_sheet, "permissions-first-launch-done");
    assert!(!permissions_page::state(&mut app).first_launch_open);
    rig.until(&mut app, &with_sheet, "the answer to be kept", |_| marker.exists());

    // What the next launch asks: the host remembers, whatever this machine has granted.
    permissions_page::state(&mut app).snapshot = None;
    mewrk_settings_global::first_launch(&mut app);
    rig.until(&mut app, &with_sheet, "the next launch's answer", |app| permissions_page::state(app).snapshot.is_some());
    assert!(permissions_page::state(&mut app).snapshot.as_ref().unwrap().prompted);
    rig.settle(&mut app, &with_sheet);
    assert!(!permissions_page::state(&mut app).first_launch_open);
    assert!(!on_screen("permissions-first-launch-done"));
}

#[test]
fn escape_answers_the_sheet_and_all_granted_says_done() {
    let dir = TempDir::new("permissions-sheet-escape");
    let mut app = start_app(&dir.0);
    let mut rig = Rig::new();
    permission_answers(&mut app, DiskAccess::Granted, KeychainAccess::Granted);
    permissions_page::state(&mut app).first_launch_open = true;
    rig.settle(&mut app, &with_sheet);
    assert!(on_screen("permissions-first-launch-done"));
    rig.press(&mut app, &with_sheet, Key::Escape, |_| {});
    assert!(!permissions_page::state(&mut app).first_launch_open);
    let marker = dir.0.join("permissions-prompted");
    rig.until(&mut app, &with_sheet, "the answer to be kept", |_| marker.exists());
}
