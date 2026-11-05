//! Add a provider against a local `/models`, change the theme, save, restart: it stays. A restart is a new process here,
//! since that is the one thing that proves what reached the disk.

mod support;

use std::process::Command;

use mewrk_client::model::{ProviderFamily, ThemePreference};
use mewrk_client::prelude::*;
use mewrk_settings_global::logic::endpoint_meta::API_FORMAT_OPTIONS;
use mewrk_settings_global::logic::store;
use mewrk_settings_global::providers::state;
use mewrk_settings_global::{global_settings_window, open, themed, SettingsView};
use support::*;

fn view(app: &mut App) -> E {
    div().w_full().h_full().child(global_settings_window(app))
}

fn data_dir() -> std::path::PathBuf {
    std::env::var_os("MEWRK_SETTINGS_TEST_DIR").expect("the parent test sets this").into()
}

/// Runs in a child process: everything a user does, through the UI.
#[test]
#[ignore = "run by a_provider_and_a_theme_survive_a_restart"]
fn phase_one() {
    let base_url = std::env::var("MEWRK_SETTINGS_TEST_URL").unwrap();
    let dir = data_dir();
    let mut app = start_app(&dir);
    let mut rig = Rig::new();
    let codex = app.doc.assets.api_providers.iter().find(|p| p.family == ProviderFamily::OpenaiCodex).unwrap().id.clone();
    open_without_network(&mut app, &codex);
    open(&mut app, SettingsView::Providers);
    rig.settle(&mut app, &view);
    rig.click(&mut app, &view, "provider-add");
    rig.type_in(&mut app, &view, "add-provider-name", "Local relay");
    let compatible = API_FORMAT_OPTIONS.iter().position(|o| o.value == ProviderFamily::OpenaiCompatible).unwrap();
    rig.choose(&mut app, &view, "add-provider-family", compatible);
    rig.click(&mut app, &view, "add-provider-submit");
    let id = app.doc.assets.api_providers.iter().find(|p| p.name == "Local relay").unwrap().id.clone();
    rig.type_in(&mut app, &view, ("provider-address", id.as_str()), &base_url);
    rig.click_away(&mut app, &view);

    rig.click(&mut app, &view, "models-fetch");
    rig.until(&mut app, &view, "the fetch", |app| state(app).busy_action.is_none());
    rig.settle(&mut app, &view);
    assert_eq!(state(&mut app).discovery_errors.get(&id), None);
    rig.click(&mut app, &view, ("manager-row", "gpt-5"));
    rig.press(&mut app, &view, Key::Escape, |_| {});
    assert!(!state(&mut app).model_manager_open);

    rig.click(&mut app, &view, ("settings-nav", SettingsView::Appearance as u64 + 1));
    rig.scroller = Some(("settings-content", SettingsView::Appearance as u64 + 1));
    rig.settle(&mut app, &view);
    rig.click(&mut app, &view, ("theme-card", ThemePreference::Night as u8));
    rig.click(&mut app, &view, ("accent", "#8B5CF6"));
    rig.choose(&mut app, &view, ("code-theme", mewrk_client::appearance::CodeThemeScheme::Night as u8), 1);
    rig.click(&mut app, &view, "zoom-in");

    use std::sync::atomic::{AtomicBool, Ordering};
    static DONE: AtomicBool = AtomicBool::new(false);
    store::flush_durable(&mut app, |_, result| {
        result.unwrap();
        DONE.store(true, Ordering::SeqCst);
    });
    rig.until(&mut app, &view, "the save to reach the disk", |_| DONE.load(Ordering::SeqCst));
    // The ui-state store has its own debounce, which a real quit flushes.
    app.flush_ui_store();
}

/// Runs in a second child process.
#[test]
#[ignore = "run by a_provider_and_a_theme_survive_a_restart"]
fn phase_two() {
    let dir = data_dir();
    let mut app = start_app(&dir);
    let provider = app.doc.assets.api_providers.iter().find(|p| p.name == "Local relay").expect("the provider came back").clone();
    assert_eq!(provider.family, ProviderFamily::OpenaiCompatible);
    assert_eq!(provider.models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["gpt-5"]);
    assert_eq!(provider.active_model_id.as_deref(), Some("gpt-5"));
    assert_eq!(app.doc.global_settings.theme, ThemePreference::Night);
    let appearance = app.doc.global_settings.appearance.clone();
    assert_eq!(appearance.theme_color, "#8B5CF6");
    assert!((appearance.zoom - 1.1).abs() < 1e-9);
    assert_ne!(appearance.code_theme_night, "mewrk-dark");
    let tokens = themed(fluent::Theme::dark(), &appearance);
    assert_eq!(tokens.accent, fluent::theme::Color::rgb(0x8B5CF6));
    assert!(!tokens.code.adaptive);
    // The page opens on it, and the catalog is still reachable.
    let mut rig = Rig::new();
    open_without_network(&mut app, &provider.id);
    open(&mut app, SettingsView::Providers);
    rig.settle(&mut app, &view);
    assert!(on_screen(("model-row", "gpt-5")));
}

fn run_child(phase: &str, dir: &std::path::Path, url: &str) {
    let output = Command::new(std::env::current_exe().unwrap()).args(["--exact", phase, "--ignored", "--nocapture"]).env("MEWRK_SETTINGS_TEST_DIR", dir).env("MEWRK_SETTINGS_TEST_URL", url).output().unwrap();
    assert!(output.status.success(), "{phase}:\n{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
}

#[test]
fn a_provider_and_a_theme_survive_a_restart() {
    let mock = MockModels::start(TWO_MODELS);
    let dir = TempDir::new("restart");
    run_child("phase_one", &dir.0, &mock.base_url);
    assert!(mock.request_count() > 0, "the catalog came from the mock");
    run_child("phase_two", &dir.0, &mock.base_url);
}
