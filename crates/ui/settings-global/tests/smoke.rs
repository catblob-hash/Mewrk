mod support;

use mewrk_settings_global::{global_settings_window, open, SettingsView};
use mewrk_client::prelude::*;
use support::*;

fn view(app: &mut App) -> E {
    div().w_full().h_full().child(global_settings_window(app))
}

#[test]
fn every_page_renders_in_a_real_host() {
    let dir = TempDir::new("smoke");
    let mut app = start_app(&dir.0);
    let mut rig = Rig::new();
    // The seed selects Claude Agent, whose panel starts the CLI and asks it for the login; a test has no business doing that.
    let codex = app.doc.assets.api_providers.iter().find(|p| p.family == mewrk_client::model::ProviderFamily::OpenaiCodex).unwrap().id.clone();
    open_without_network(&mut app, &codex);
    for page in [SettingsView::Appearance, SettingsView::Providers, SettingsView::SearchProviders, SettingsView::Shortcuts, SettingsView::Usage, SettingsView::Dependencies, SettingsView::Permissions] {
        open(&mut app, page);
        rig.settle(&mut app, &view);
        std::thread::sleep(std::time::Duration::from_millis(30));
        rig.settle(&mut app, &view);
        assert!(on_screen(("settings-nav", page as u64 + 1)), "{page:?}");
    }
}

// ---------------------------------------------------------------- accessibility

/// Each page with a client listening: everything a person can reach has a name, and the rail says where they are.
#[test]
fn every_page_names_every_control_it_has() {
    let dir = TempDir::new("a11y-sweep");
    let mut app = start_app(&dir.0);
    let mut rig = Rig::new();
    let codex = app.doc.assets.api_providers.iter().find(|p| p.family == mewrk_client::model::ProviderFamily::OpenaiCodex).unwrap().id.clone();
    open_without_network(&mut app, &codex);
    for page in [SettingsView::Appearance, SettingsView::Providers, SettingsView::SearchProviders, SettingsView::Shortcuts, SettingsView::Usage, SettingsView::Dependencies, SettingsView::Updates, SettingsView::Permissions] {
        open(&mut app, page);
        rig.settle(&mut app, &view);
        std::thread::sleep(std::time::Duration::from_millis(30));
        let tree = a11y_tree(&mut rig, &mut app, &view);
        assert_eq!(a11y_unnamed(&tree), Vec::<String>::new(), "{page:?}");
        assert!(tree.iter().any(|(_, node)| node.role() == accesskit::Role::Navigation && a11y_name(node) == mewrk_client::t("全局设置分类", "Global settings categories")), "{page:?}");
        let stops = a11y_tab_stops(&mut rig, &mut app, &view);
        assert!(stops.len() >= 8, "the rail and the corner at least: {stops:#?}");
        assert!(stops.iter().all(|stop| !stop.ends_with("\"\"")), "{page:?}: {stops:#?}");
        rig.mem.focus = 0;
    }
    fluent::el::set_a11y_active(false);
}
