//! `GlobalSettings.test.tsx`, as far as it tests logic and interaction: every test drives a real host through frames.

mod support;

use std::collections::HashSet;
use std::time::Duration;

use mewrk_client::model::{ApiProvider, FamilySetting, ModelCapability, ModelProfile, ProviderFamily, ReasoningContent};
use mewrk_client::prelude::*;
use mewrk_settings_global::logic::endpoint_meta::API_FORMAT_OPTIONS;
use mewrk_settings_global::logic::provider_ops::create_provider;
use mewrk_settings_global::logic::store;
use mewrk_settings_global::providers::{self, model_id_error, state};
use mewrk_settings_global::{close, global_settings_window, open, SettingsView};
use support::*;

fn view(app: &mut App) -> E {
    div().w_full().h_full().child(global_settings_window(app))
}

fn provider(name: &str, family: ProviderFamily, url: &str) -> ApiProvider {
    ApiProvider { base_url: url.to_owned(), ..create_provider(name, family) }
}

fn model(id: &str) -> ModelProfile {
    ModelProfile { id: id.into(), name: String::new(), group: String::new(), context_window: None, max_output_tokens: None, capabilities: Default::default(), reasoning_content: ReasoningContent::Plaintext, prompt_cache: true, cache_ttl_minutes: None }
}

fn seeded(name: &str, providers_list: Vec<ApiProvider>) -> (TempDir, App, Rig) {
    let dir = TempDir::new(name);
    let mut app = start_app(&dir.0);
    let first = providers_list.first().map(|p| p.id.clone());
    store::change(&mut app, |doc| {
        doc.assets.api_providers = providers_list;
        doc.global_settings.active_provider_id = first;
    });
    open(&mut app, SettingsView::Providers);
    let mut rig = Rig::new();
    rig.settle(&mut app, &view);
    rig.until(&mut app, &view, "the seed to be saved", |app| saved(app));
    (dir, app, rig)
}

/// Whether the host has what's on screen: a flush with nothing to do answers at once.
fn saved(app: &mut App) -> bool {
    use std::sync::atomic::{AtomicBool, Ordering};
    static DONE: AtomicBool = AtomicBool::new(false);
    DONE.store(false, Ordering::SeqCst);
    store::flush(app, |_, _| DONE.store(true, Ordering::SeqCst));
    for _ in 0..200 {
        app.tick();
        if DONE.load(Ordering::SeqCst) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

fn ids(app: &App) -> Vec<String> {
    app.doc.assets.api_providers.iter().map(|p| p.name.clone()).collect()
}

fn find<'a>(app: &'a App, name: &str) -> &'a ApiProvider {
    app.doc.assets.api_providers.iter().find(|p| p.name == name).unwrap()
}

#[test]
fn retired_view_ids_lead_to_their_pages() {
    for id in ["general", "advanced", "memory", "execution_environments"] {
        assert_eq!(SettingsView::from_wire(id), SettingsView::Appearance, "{id}");
    }
    for id in ["hooks", "capability_catalog", "agents", "conversation_presets", "mcp", "skills"] {
        assert_eq!(SettingsView::from_wire(id), SettingsView::Providers, "{id}");
    }
    assert_eq!(SettingsView::from_wire("web_search"), SettingsView::SearchProviders);
}

#[test]
fn the_rail_switches_pages_and_a_new_opening_starts_over() {
    let (_d, mut app, mut rig) = seeded("nav", vec![provider("One", ProviderFamily::OpenaiChat, "https://x.test/v1")]);
    rig.click(&mut app, &view, ("settings-nav", SettingsView::SearchProviders as u64 + 1));
    assert_eq!(mewrk_settings_global::current_view(&mut app), Some(SettingsView::SearchProviders));
    rig.click(&mut app, &view, ("search-row", "exa"));
    assert!(on_screen(("search-enabled", "exa")));
    rig.click(&mut app, &view, "settings-close");
    assert!(!mewrk_settings_global::is_open(&mut app));
    rig.frame(&mut app, &view);
    assert!(!on_screen(("settings-nav", SettingsView::SearchProviders as u64 + 1)));
}

#[test]
fn it_opens_on_the_provider_in_use_rather_than_the_first_row() {
    let (a, b) = (provider("First", ProviderFamily::OpenaiChat, "https://x.test/v1"), provider("Second", ProviderFamily::OpenaiChat, "https://y.test/v1"));
    let second = b.id.clone();
    let dir = TempDir::new("preferred");
    let mut app = start_app(&dir.0);
    store::change(&mut app, |doc| {
        doc.assets.api_providers = vec![a, b];
        doc.global_settings.active_provider_id = Some(second.clone());
    });
    open(&mut app, SettingsView::Providers);
    let mut rig = Rig::new();
    rig.settle(&mut app, &view);
    assert_eq!(state(&mut app).selected_id.as_deref(), Some(second.as_str()));
}

#[test]
fn reorders_providers_by_dragging_the_full_list_row() {
    let list = vec![provider("A", ProviderFamily::OpenaiChat, ""), provider("B", ProviderFamily::OpenaiChat, ""), provider("C", ProviderFamily::OpenaiChat, "")];
    let a_id = list[0].id.clone();
    let c_id = list[2].id.clone();
    let (_d, mut app, mut rig) = seeded("drag", list);
    let from = fluent::el::rect_of(("provider-row", a_id.as_str())).unwrap();
    let to = fluent::el::rect_of(("provider-row", c_id.as_str())).unwrap();
    rig.input.pointer = Some((from.x + 20.0, from.y + 10.0));
    rig.frame(&mut app, &view);
    rig.input.pressed = true;
    rig.input.down = true;
    rig.frame(&mut app, &view);
    rig.input.pointer = Some((to.x + 20.0, to.bottom() - 2.0));
    rig.frame(&mut app, &view);
    rig.frame(&mut app, &view);
    rig.input.down = false;
    rig.input.released = true;
    rig.frame(&mut app, &view);
    rig.settle(&mut app, &view);
    assert_eq!(ids(&app), ["B", "C", "A"]);
}

#[test]
fn reorders_by_keyboard_and_not_while_the_list_is_narrowed() {
    let list = vec![provider("A", ProviderFamily::OpenaiChat, ""), provider("B", ProviderFamily::OpenaiChat, "")];
    let b_id = list[1].id.clone();
    let (_d, mut app, mut rig) = seeded("keys", list);
    rig.click(&mut app, &view, ("provider-row", b_id.as_str()));
    rig.press(&mut app, &view, Key::Up, |k| k.alt = true);
    assert_eq!(ids(&app), ["B", "A"]);
    rig.type_in(&mut app, &view, ("rail-search", "providers"), "b");
    rig.click(&mut app, &view, ("provider-row", b_id.as_str()));
    rig.press(&mut app, &view, Key::Down, |k| k.alt = true);
    assert_eq!(ids(&app), ["B", "A"], "narrowed, list indexes aren't the document's");
}

#[test]
fn search_finds_providers_by_name_and_by_model_and_filters_narrow_them() {
    let mut with_model = provider("Relay", ProviderFamily::OpenaiCompatible, "https://r.test/v1");
    with_model.models.push(model("deep-thought"));
    let mut disabled = provider("Quiet", ProviderFamily::OpenaiChat, "");
    disabled.enabled = false;
    let (relay, quiet) = (with_model.id.clone(), disabled.id.clone());
    let (_d, mut app, mut rig) = seeded("search", vec![with_model, disabled]);
    rig.type_in(&mut app, &view, ("rail-search", "providers"), "deep");
    assert!(on_screen(("provider-row", relay.as_str())));
    assert!(!on_screen(("provider-row", quiet.as_str())));
    rig.press(&mut app, &view, Key::Escape, |_| {});
    assert_eq!(state(&mut app).provider_query, "", "Escape clears the search");
    state(&mut app).rail_filter = providers::RailFilter::Disabled;
    rig.settle(&mut app, &view);
    assert!(!on_screen(("provider-row", relay.as_str())));
    assert!(on_screen(("provider-row", quiet.as_str())));
}

#[test]
fn offers_every_wire_format_in_a_settings_form_and_adds_a_custom_provider() {
    let (_d, mut app, mut rig) = seeded("add", vec![]);
    assert_eq!(API_FORMAT_OPTIONS.len(), 9);
    rig.click(&mut app, &view, "provider-add");
    assert!(state(&mut app).adding_provider);
    // Nothing to add yet: Add stays disabled.
    rig.click(&mut app, &view, "add-provider-submit");
    assert!(state(&mut app).adding_provider);
    rig.type_in(&mut app, &view, "add-provider-name", "My relay");
    assert_eq!(state(&mut app).add_name, "My relay");
    rig.choose(&mut app, &view, "add-provider-family", 2);
    rig.click(&mut app, &view, "add-provider-submit");
    assert!(!state(&mut app).adding_provider);
    let added = find(&app, "My relay").clone();
    assert_eq!(added.family, ProviderFamily::Anthropic);
    assert_eq!(added.base_url, "https://api.anthropic.com/v1");
    assert!(added.enabled);
    assert_eq!(state(&mut app).selected_id.as_deref(), Some(added.id.as_str()));
}

#[test]
fn the_name_is_capped_at_thirty_two_characters() {
    let (_d, mut app, mut rig) = seeded("cap", vec![]);
    rig.click(&mut app, &view, "provider-add");
    rig.type_in(&mut app, &view, "add-provider-name", &"x".repeat(40));
    assert_eq!(state(&mut app).add_name.chars().count(), 32);
}

#[test]
fn enabling_moves_the_active_provider_along() {
    let a = provider("A", ProviderFamily::OpenaiChat, "");
    let b = provider("B", ProviderFamily::OpenaiChat, "");
    let (a_id, b_id) = (a.id.clone(), b.id.clone());
    let (_d, mut app, mut rig) = seeded("enable", vec![a, b]);
    assert_eq!(app.doc.global_settings.active_provider_id.as_deref(), Some(a_id.as_str()));
    rig.click(&mut app, &view, ("provider-enabled", a_id.as_str()));
    assert!(!find(&app, "A").enabled);
    assert_eq!(app.doc.global_settings.active_provider_id.as_deref(), Some(b_id.as_str()));
}

#[test]
fn removes_a_custom_provider_from_its_row_in_two_steps_and_never_a_built_in_one() {
    let a = provider("A", ProviderFamily::OpenaiChat, "");
    let b = provider("B", ProviderFamily::OpenaiChat, "");
    let codex = provider("OpenAI Codex", ProviderFamily::OpenaiCodex, "");
    let (a_id, b_id, codex_id) = (a.id.clone(), b.id.clone(), codex.id.clone());
    let (_d, mut app, mut rig) = seeded("remove", vec![a, b, codex]);
    let delete = ("confirm-delete-btn", fluent::el::hash_key(("provider-delete", a_id.as_str())));
    let row_rect = fluent::el::rect_of(("provider-row", a_id.as_str())).unwrap();
    // It shows on hover, armed by the first press, gone on the second.
    rig.input.pointer = Some((row_rect.x + 10.0, row_rect.y + 10.0));
    rig.settle(&mut app, &view);
    rig.click(&mut app, &view, delete);
    assert_eq!(ids(&app).len(), 3);
    rig.click(&mut app, &view, delete);
    assert_eq!(ids(&app), ["B", "OpenAI Codex"]);
    assert_eq!(state(&mut app).selected_id.as_deref(), Some(b_id.as_str()));
    assert!(fluent::el::rect_of(("confirm-delete-btn", fluent::el::hash_key(("provider-delete", codex_id.as_str())))).is_none());
    providers::remove_provider(&mut app, &codex_id);
    assert_eq!(ids(&app).len(), 2 , "a built-in row refuses");
}

#[test]
fn keeps_only_the_name_format_and_identity_fields_in_provider_settings() {
    let (_d, mut app, mut rig) = seeded("options", vec![provider("Cloud", ProviderFamily::OpenaiChat, "https://custom.test/v1")]);
    let id = app.doc.assets.api_providers[0].id.clone();
    rig.click(&mut app, &view, "provider-options");
    assert!(state(&mut app).options_open);
    assert!(field_rect(("provider-name", id.as_str())).is_some());
    // Switching protocol keeps an entered address and drops identity fields the new one doesn't take.
    let bedrock = API_FORMAT_OPTIONS.iter().position(|o| o.value == ProviderFamily::Bedrock).unwrap();
    rig.choose(&mut app, &view, ("provider-format", id.as_str()), bedrock);
    assert_eq!(app.doc.assets.api_providers[0].family, ProviderFamily::Bedrock);
    assert_eq!(app.doc.assets.api_providers[0].base_url, "https://custom.test/v1");
    // Every label is one of two languages, whichever the host started in.
    let label = mewrk_settings_global::logic::endpoint_meta::family_setting_meta(FamilySetting::Region).label;
    rig.type_in(&mut app, &view, ("provider-setting", id.as_str(), label), "us-east-1");
    assert_eq!(app.doc.assets.api_providers[0].family_settings.get(&FamilySetting::Region).map(String::as_str), Some("us-east-1"));
    let azure = API_FORMAT_OPTIONS.iter().position(|o| o.value == ProviderFamily::Azure).unwrap();
    rig.choose(&mut app, &view, ("provider-format", id.as_str()), azure);
    assert!(app.doc.assets.api_providers[0].family_settings.is_empty());
    rig.press(&mut app, &view, Key::Escape, |_| {});
    assert!(!state(&mut app).options_open, "Escape closes the drawer");
}

#[test]
fn offers_fetch_models_only_where_there_is_a_catalog_to_read() {
    let mut bedrock = provider("Bedrock", ProviderFamily::Bedrock, "");
    bedrock.family_settings.insert(FamilySetting::Region, "us-east-1".into());
    let with_url = provider("Relay", ProviderFamily::OpenaiCompatible, "https://r.test/v1");
    let relay_id = with_url.id.clone();
    let (_d, mut app, mut rig) = seeded("catalog", vec![bedrock, with_url]);
    assert!(!on_screen("models-fetch"));
    assert!(on_screen("models-add"));
    providers::select_provider(&mut app, Some(&relay_id));
    rig.settle(&mut app, &view);
    assert!(on_screen("models-fetch"));
}

#[test]
fn creates_models_in_a_drawer_and_rejects_blank_or_duplicate_ids() {
    let mut p = provider("Relay", ProviderFamily::OpenaiCompatible, "https://r.test/v1");
    p.models.push(model("existing"));
    assert!(says(model_id_error(&p, "  ", None).unwrap(), "不能为空", "is required"));
    assert!(says(model_id_error(&p, "existing", None).unwrap(), "重复", "unique"));
    assert_eq!(model_id_error(&p, "existing", Some("existing")), None);
    assert_eq!(model_id_error(&p, "new", None), None);
    let (_d, mut app, mut rig) = seeded("model", vec![p]);
    rig.click(&mut app, &view, "models-add");
    assert!(state(&mut app).model_editor.is_some());
    rig.click(&mut app, &view, "model-save");
    assert!(state(&mut app).model_editor.is_some(), "a blank id is refused");
    rig.type_in(&mut app, &view, "model-id", "existing");
    rig.click(&mut app, &view, "model-save");
    assert!(state(&mut app).model_editor.is_some(), "a duplicate id is refused");
    rig.type_in(&mut app, &view, "model-id", " brand-new ");
    rig.click(&mut app, &view, "model-save");
    assert!(state(&mut app).model_editor.is_none());
    let saved_models: Vec<String> = find(&app, "Relay").models.iter().map(|m| m.id.clone()).collect();
    assert_eq!(saved_models, ["existing", "brand-new"]);
}

#[test]
fn edits_a_model_from_its_drawer_and_repairs_the_active_selection_on_removal() {
    let mut p = provider("Relay", ProviderFamily::OpenaiCompatible, "https://r.test/v1");
    p.models = vec![model("one"), model("two")];
    p.active_model_id = Some("one".into());
    let (_d, mut app, mut rig) = seeded("edit", vec![p]);
    rig.click(&mut app, &view, ("model-edit", "one"));
    rig.type_in(&mut app, &view, "model-id", "renamed");
    rig.type_in(&mut app, &view, "model-context", "128000");
    rig.click(&mut app, &view, "model-save");
    let p = find(&app, "Relay");
    assert_eq!(p.active_model_id.as_deref(), Some("renamed"), "the active model is renamed along");
    assert_eq!(p.models[0].context_window, Some(128000));
    rig.click(&mut app, &view, ("model-remove", "renamed"));
    let p = find(&app, "Relay");
    assert_eq!(p.models.len(), 1);
    assert_eq!(p.active_model_id.as_deref(), Some("two"));
}

#[test]
fn a_manually_added_model_declares_vision_and_a_relay_leaves_appending_to_the_user() {
    let (_d, mut app, mut rig) = seeded("caps", vec![provider("Relay", ProviderFamily::OpenaiResponses, "https://relay.test/v1")]);
    rig.click(&mut app, &view, "models-add");
    rig.type_in(&mut app, &view, "model-id", "gpt-6-astra");
    let draft = state(&mut app).model_editor.as_ref().unwrap().draft.clone();
    assert!(!draft.capabilities.contains(&ModelCapability::ToolAppend), "behind a relay it's the user's to say: {:?}", draft.capabilities);
    rig.click(&mut app, &view, ("model-capability", ModelCapability::ImageRecognition as u8));
    rig.click(&mut app, &view, ("model-capability", ModelCapability::ToolAppend as u8));
    // Moving a protocol switch stops the filling.
    rig.type_in(&mut app, &view, "model-id", "gpt-7");
    let draft = state(&mut app).model_editor.as_ref().unwrap().draft.clone();
    assert!(draft.capabilities.contains(&ModelCapability::ImageRecognition));
    assert!(draft.capabilities.contains(&ModelCapability::ToolAppend));
    rig.click(&mut app, &view, "model-save");
    assert!(find(&app, "Relay").models[0].capabilities.contains(&ModelCapability::ImageRecognition));
}

#[test]
fn a_model_at_the_vendors_endpoint_takes_what_mewrk_knows_as_its_id_is_typed() {
    let (_d, mut app, mut rig) = seeded("known", vec![provider("OpenAI", ProviderFamily::OpenaiResponses, "https://api.openai.com/v1")]);
    rig.click(&mut app, &view, "models-add");
    rig.type_in(&mut app, &view, "model-id", "gpt-6-astra");
    let draft = state(&mut app).model_editor.as_ref().unwrap().draft.clone();
    for c in [ModelCapability::ToolAppend, ModelCapability::SystemAppend, ModelCapability::AsyncTools, ModelCapability::NativeCompaction] {
        assert!(draft.capabilities.contains(&c), "{c:?}");
    }
}

fn open_manager(app: &mut App, rig: &mut Rig) {
    rig.click(app, &view, "models-fetch");
    rig.until(app, &view, "the fetch to end", |app| state(app).busy_action.is_none());
    rig.settle(app, &view);
}

// ModelManagerDrawer.tsx unmounts with its drawer: the search, the disclosure and the row order go with it.
#[test]
fn closing_the_discovery_drawer_lets_what_it_held_go() {
    let mock = MockModels::start(TWO_MODELS);
    let p = provider("Relay", ProviderFamily::OpenaiCompatible, &mock.base_url);
    let (_d, mut app, mut rig) = seeded("manager-close", vec![p]);
    open_manager(&mut app, &mut rig);
    rig.type_in(&mut app, &view, "manager-search", "gpt");
    assert_eq!(state(&mut app).manager.query, "gpt");
    assert!(!state(&mut app).manager.seen.is_empty());
    rig.press(&mut app, &view, Key::Escape, |_| {});
    rig.press(&mut app, &view, Key::Escape, |_| {});
    assert!(!state(&mut app).model_manager_open);
    assert_eq!(state(&mut app).manager.query, "");
    assert!(state(&mut app).manager.seen.is_empty());
}

#[test]
fn fetches_the_catalog_for_a_disabled_provider_and_installs_nothing_on_its_own() {
    let mock = MockModels::start(TWO_MODELS);
    let mut p = provider("Relay", ProviderFamily::OpenaiCompatible, &mock.base_url);
    p.enabled = false;
    let id = p.id.clone();
    let (_d, mut app, mut rig) = seeded("fetch", vec![p]);
    open_manager(&mut app, &mut rig);
    assert_eq!(state(&mut app).discovered.get(&id).map(Vec::len), Some(2));
    assert!(find(&app, "Relay").models.is_empty(), "fetching a catalog changes nothing on its own");
    assert!(mock.requests.lock().unwrap().iter().any(|r| r.starts_with("GET /v1/models")), "{:?}", mock.requests.lock().unwrap());
    // A row is a toggle.
    rig.click(&mut app, &view, ("manager-row", "gpt-5"));
    assert_eq!(find(&app, "Relay").models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["gpt-5"]);
    rig.click(&mut app, &view, ("manager-row", "gpt-5"));
    assert!(find(&app, "Relay").models.is_empty());
    assert!(on_screen(("manager-row", "gpt-5")), "a row stays where it is as it moves in and out of the provider");
    rig.click(&mut app, &view, "manager-all");
    assert_eq!(find(&app, "Relay").models.len(), 2);
    rig.click(&mut app, &view, "manager-all");
    assert!(find(&app, "Relay").models.is_empty());
}

#[test]
fn the_fetch_waits_for_the_address_to_be_saved_and_a_refused_flush_issues_no_request() {
    let mock = MockModels::start(TWO_MODELS);
    let p = provider("Relay", ProviderFamily::OpenaiCompatible, "https://nowhere.test/v1");
    let id = p.id.clone();
    let (_d, mut app, mut rig) = seeded("flush", vec![p]);
    rig.type_in(&mut app, &view, ("provider-address", id.as_str()), &mock.base_url);
    // The host resolves the provider against what it has saved, so a stale address would be refused rather than fetched.
    open_manager(&mut app, &mut rig);
    assert_eq!(state(&mut app).discovered.get(&id).map(Vec::len), Some(2), "{:?}", state(&mut app).discovery_errors);
    let before = mock.request_count();
    store::fail_next_flush(&mut app, "disk full");
    rig.click(&mut app, &view, "manager-refetch");
    rig.until(&mut app, &view, "the fetch to end", |app| state(app).busy_action.is_none());
    assert_eq!(mock.request_count(), before);
    assert_eq!(state(&mut app).discovery_errors.get(&id).map(String::as_str), Some("disk full"));
}

#[test]
fn shows_why_the_catalog_request_failed_and_clears_it_once_a_fetch_succeeds() {
    let mock = MockModels::start(TWO_MODELS);
    *mock.status.lock().unwrap() = (401, r#"{"error":{"message":"bad key"}}"#.to_owned());
    let p = provider("Relay", ProviderFamily::OpenaiCompatible, &mock.base_url);
    let id = p.id.clone();
    let (_d, mut app, mut rig) = seeded("fail", vec![p]);
    open_manager(&mut app, &mut rig);
    assert!(state(&mut app).discovery_errors.contains_key(&id));
    assert!(on_screen("manager-error"));
    assert!(on_screen("manager-empty"));
    *mock.status.lock().unwrap() = (200, TWO_MODELS.to_owned());
    rig.click(&mut app, &view, "manager-refetch");
    rig.until(&mut app, &view, "the fetch to end", |app| state(app).busy_action.is_none());
    rig.settle(&mut app, &view);
    assert!(!state(&mut app).discovery_errors.contains_key(&id));
    assert!(!on_screen("manager-error"));
}

#[test]
fn keeps_curated_values_when_a_discovery_result_is_installed_over_them() {
    let mock = MockModels::start(TWO_MODELS);
    let mut p = provider("Relay", ProviderFamily::OpenaiCompatible, &mock.base_url);
    let mut curated = model("gpt-5");
    curated.name = "My name".into();
    curated.group = "mine".into();
    curated.context_window = Some(7);
    p.models.push(curated);
    let (_d, mut app, mut rig) = seeded("curated", vec![p]);
    open_manager(&mut app, &mut rig);
    rig.click(&mut app, &view, "manager-all");
    let models = &find(&app, "Relay").models;
    let kept = models.iter().find(|m| m.id == "gpt-5").unwrap();
    assert_eq!((kept.name.as_str(), kept.group.as_str(), kept.context_window), ("My name", "mine", Some(7)));
    assert!(models.iter().any(|m| m.id == "qwen-mini"));
}

fn frames_for(app: &mut App, rig: &mut Rig, ms: u64) {
    let begun = std::time::Instant::now();
    while begun.elapsed() < Duration::from_millis(ms) {
        rig.frame(app, &view);
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn discards_discovery_results_after_the_selection_changes() {
    let mock = MockModels::start(TWO_MODELS);
    *mock.delay.lock().unwrap() = Duration::from_millis(200);
    let a = provider("A", ProviderFamily::OpenaiCompatible, &mock.base_url);
    let b = provider("B", ProviderFamily::OpenaiCompatible, "https://b.test/v1");
    let (a_id, b_id) = (a.id.clone(), b.id.clone());
    let (_d, mut app, mut rig) = seeded("stale", vec![a, b]);
    providers::discover_models(&mut app);
    rig.until(&mut app, &view, "the request to go out", |_| mock.request_count() > 0);
    // Moving to another provider while the request is out leaves its answer with nowhere to go.
    providers::select_provider(&mut app, Some(&b_id));
    frames_for(&mut app, &mut rig, 300);
    assert!(!state(&mut app).discovered.contains_key(&a_id));
    assert_eq!(state(&mut app).busy_action, None);
}

#[test]
fn discards_discovery_results_after_the_endpoint_changes() {
    let mock = MockModels::start(TWO_MODELS);
    *mock.delay.lock().unwrap() = Duration::from_millis(200);
    let a = provider("A", ProviderFamily::OpenaiCompatible, &mock.base_url);
    let a_id = a.id.clone();
    let (_d, mut app, mut rig) = seeded("stale-url", vec![a]);
    providers::discover_models(&mut app);
    rig.until(&mut app, &view, "the request to go out", |_| mock.request_count() > 0);
    providers::replace_provider(&mut app, &a_id, |p| p.base_url = "https://elsewhere.test/v1".into(), None);
    frames_for(&mut app, &mut rig, 300);
    assert!(!state(&mut app).discovered.contains_key(&a_id), "the catalog answered for the old address");
}

fn type_key(app: &mut App, rig: &mut Rig, id: &str, secret: &str) {
    rig.type_in(app, &view, ("provider-key", id), secret);
}

fn key_settled(app: &mut App, id: &str) -> bool {
    !state(app).saving_keys.contains(id) && !state(app).revealing_keys.contains(id)
}

fn seeded_key_provider(name: &str) -> (TempDir, App, Rig, String) {
    let p = provider("Relay", ProviderFamily::OpenaiCompatible, "https://r.test/v1");
    let id = p.id.clone();
    let (d, app, rig) = seeded(name, vec![p]);
    (d, app, rig, id)
}

// The credential store is one per process, and some tests refuse its writes, so whatever saves a key takes turns.
static STORE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn store_turn() -> std::sync::MutexGuard<'static, ()> {
    STORE_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

#[test]
fn saves_a_hidden_draft_keeps_only_its_length_and_restores_that_mask_after_reopening() {
    let _turn = store_turn();
    let (_d, mut app, mut rig, id) = seeded_key_provider("save");
    type_key(&mut app, &mut rig, &id, "test-key-123");
    assert_eq!(state(&mut app).key_drafts.get(&id).map(String::as_str), Some("test-key-123"));
    rig.click_away(&mut app, &view);
    let wait_id = id.clone();
    rig.until(&mut app, &view, "the save", move |app| key_settled(app, &wait_id) && !state(app).key_drafts.contains_key(&wait_id));
    assert_eq!(state(&mut app).key_lengths.get(&id), Some(&12));
    close(&mut app);
    rig.settle(&mut app, &view);
    assert!(state(&mut app).key_lengths.is_empty(), "the page's state went with the window");
    open(&mut app, SettingsView::Providers);
    rig.settle(&mut app, &view);
    assert_eq!(state(&mut app).key_lengths.get(&id), Some(&12), "the length is remembered, the key is not");
    assert!(state(&mut app).key_drafts.is_empty());
}

#[test]
fn reads_the_credential_only_while_explicitly_visible() {
    let _turn = store_turn();
    let (_d, mut app, mut rig, id) = seeded_key_provider("reveal");
    type_key(&mut app, &mut rig, &id, "test-key-123");
    rig.click_away(&mut app, &view);
    let wait_id = id.clone();
    rig.until(&mut app, &view, "the save", move |app| key_settled(app, &wait_id) && !state(app).key_drafts.contains_key(&wait_id));
    rig.click(&mut app, &view, ("provider-key-reveal", id.as_str()));
    let wait_id = id.clone();
    rig.until(&mut app, &view, "the read", move |app| state(app).visible_keys.contains(&wait_id));
    assert_eq!(state(&mut app).key_drafts.get(&id).map(String::as_str), Some("test-key-123"));
    rig.click(&mut app, &view, ("provider-key-reveal", id.as_str()));
    let wait_id = id.clone();
    rig.until(&mut app, &view, "hiding", move |app| !state(app).visible_keys.contains(&wait_id));
    assert!(state(&mut app).key_drafts.get(&id).is_none(), "hiding it forgets the plaintext");
}

#[test]
fn keeps_an_edited_key_temporary_while_visible_and_saves_it_automatically() {
    let _turn = store_turn();
    let (_d, mut app, mut rig, id) = seeded_key_provider("visible");
    rig.click(&mut app, &view, ("provider-key-reveal", id.as_str()));
    let wait_id = id.clone();
    rig.until(&mut app, &view, "the field to open", move |app| state(app).visible_keys.contains(&wait_id));
    type_key(&mut app, &mut rig, &id, "edited-while-visible");
    rig.click_away(&mut app, &view);
    let wait_id = id.clone();
    rig.until(&mut app, &view, "the save", move |app| key_settled(app, &wait_id));
    assert_eq!(state(&mut app).key_lengths.get(&id), Some(&20));
    assert_eq!(state(&mut app).key_drafts.get(&id).map(String::as_str), Some("edited-while-visible"), "visible, the draft stays on screen");
}

#[test]
fn deletes_the_stored_credential_when_an_emptied_field_loses_focus() {
    let _turn = store_turn();
    let (_d, mut app, mut rig, id) = seeded_key_provider("delete");
    type_key(&mut app, &mut rig, &id, "test-key-123");
    rig.click_away(&mut app, &view);
    let wait_id = id.clone();
    rig.until(&mut app, &view, "the save", move |app| key_settled(app, &wait_id) && state(app).key_lengths.contains_key(&wait_id));
    type_key(&mut app, &mut rig, &id, "");
    rig.click_away(&mut app, &view);
    let wait_id = id.clone();
    rig.until(&mut app, &view, "the deletion", move |app| key_settled(app, &wait_id) && !state(app).key_lengths.contains_key(&wait_id));
    assert!(state(&mut app).key_errors.is_empty(), "{:?}", state(&mut app).key_errors);
}

#[test]
fn keeps_the_configured_key_when_the_format_or_address_changes() {
    let _turn = store_turn();
    let (_d, mut app, mut rig, id) = seeded_key_provider("rebind");
    type_key(&mut app, &mut rig, &id, "test-key-123");
    rig.click_away(&mut app, &view);
    let wait_id = id.clone();
    rig.until(&mut app, &view, "the save", move |app| key_settled(app, &wait_id) && state(app).key_lengths.contains_key(&wait_id));
    rig.type_in(&mut app, &view, ("provider-address", id.as_str()), "https://other.test/v1");
    rig.click_away(&mut app, &view);
    assert_eq!(state(&mut app).key_lengths.get(&id), Some(&12));
    let snapshot = find(&app, "Relay").clone();
    rig.until(&mut app, &view, "saving the address", saved);
    let secret = app.host.invoke_blocking("reveal_api_key", json!({ "provider": snapshot })).unwrap();
    assert_eq!(secret, json!("test-key-123"));
}

#[test]
fn keeps_a_successful_key_save_after_switching_providers() {
    let _turn = store_turn();
    let a = provider("A", ProviderFamily::OpenaiCompatible, "https://a.test/v1");
    let b = provider("B", ProviderFamily::OpenaiCompatible, "https://b.test/v1");
    let (a_id, b_id) = (a.id.clone(), b.id.clone());
    let (_d, mut app, mut rig) = seeded("switch", vec![a, b]);
    type_key(&mut app, &mut rig, &a_id, "test-key-for-a");
    // Pressing the other row ends the edit, which saves, and selects B.
    rig.click(&mut app, &view, ("provider-row", b_id.as_str()));
    assert_eq!(state(&mut app).selected_id.as_deref(), Some(b_id.as_str()));
    let wait_id = a_id.clone();
    rig.until(&mut app, &view, "the save", move |app| key_settled(app, &wait_id) && state(app).key_lengths.contains_key(&wait_id));
    let snapshot = find(&app, "A").clone();
    assert_eq!(app.host.invoke_blocking("reveal_api_key", json!({ "provider": snapshot })).unwrap(), json!("test-key-for-a"));
}

#[test]
fn reports_a_refused_key_save_keeps_the_draft_and_clears_on_a_retry() {
    let _turn = store_turn();
    let (_d, mut app, mut rig, id) = seeded_key_provider("refused");
    *REFUSE_WRITES.lock().unwrap() = Some("keychain says no to retry-key-1".to_owned());
    type_key(&mut app, &mut rig, &id, "retry-key-1");
    rig.click_away(&mut app, &view);
    let wait_id = id.clone();
    rig.until(&mut app, &view, "the refusal", move |app| state(app).key_errors.contains_key(&wait_id));
    *REFUSE_WRITES.lock().unwrap() = None;
    let message = state(&mut app).key_errors.get(&id).cloned().unwrap();
    assert!(says(&message, "保存 API Key 失败", "Could not save the API key"), "{message}");
    assert!(!message.contains("retry-key-1"), "the secret is scrubbed: {message}");
    assert!(!state(&mut app).key_lengths.contains_key(&id), "a refused save must not look like a stored key");
    assert_eq!(state(&mut app).key_drafts.get(&id).map(String::as_str), Some("retry-key-1"), "the draft stays so it can be retried");
    let field = field_rect(("provider-key", id.as_str())).unwrap();
    rig.click_at(&mut app, &view, field.x + 10.0, field.y + 5.0);
    rig.click_away(&mut app, &view);
    let wait_id = id.clone();
    rig.until(&mut app, &view, "the retry", move |app| !state(app).key_errors.contains_key(&wait_id));
    assert_eq!(state(&mut app).key_lengths.get(&id), Some(&11));
}

#[test]
fn reports_a_flush_failure_without_reaching_the_credential_store() {
    let _turn = store_turn();
    let (_d, mut app, mut rig, id) = seeded_key_provider("flushfail");
    type_key(&mut app, &mut rig, &id, "never-stored-key");
    store::fail_next_flush(&mut app, "disk full");
    rig.click_away(&mut app, &view);
    let wait_id = id.clone();
    rig.until(&mut app, &view, "the failure", move |app| state(app).key_errors.contains_key(&wait_id));
    let message = state(&mut app).key_errors.get(&id).cloned().unwrap();
    assert!(says(&message, "保存 API Key 失败：disk full", "Could not save the API key: disk full"), "{message}");
    assert!(!state(&mut app).key_lengths.contains_key(&id));
    let snapshot = find(&app, "Relay").clone();
    rig.until(&mut app, &view, "saving", saved);
    assert!(app.host.invoke_blocking("reveal_api_key", json!({ "provider": snapshot })).is_err(), "nothing was stored");
}

#[test]
fn reports_a_refused_deletion_and_a_refused_read() {
    let _turn = store_turn();
    let (_d, mut app, mut rig, id) = seeded_key_provider("refused-rw");
    type_key(&mut app, &mut rig, &id, "test-key-123");
    rig.click_away(&mut app, &view);
    let wait_id = id.clone();
    rig.until(&mut app, &view, "the save", move |app| key_settled(app, &wait_id) && state(app).key_lengths.contains_key(&wait_id));
    *REFUSE_WRITES.lock().unwrap() = Some("locked".to_owned());
    *REFUSE_READS.lock().unwrap() = Some("locked".to_owned());
    type_key(&mut app, &mut rig, &id, "");
    rig.click_away(&mut app, &view);
    let wait_id = id.clone();
    rig.until(&mut app, &view, "the refused deletion", move |app| state(app).key_errors.contains_key(&wait_id));
    assert!(says(state(&mut app).key_errors.get(&id).unwrap(), "删除 API Key 失败", "Could not delete the API key"));
    assert_eq!(state(&mut app).key_lengths.get(&id), Some(&12), "the stored key is still there");
    state(&mut app).key_errors.clear();
    rig.click(&mut app, &view, ("provider-key-reveal", id.as_str()));
    let wait_id = id.clone();
    rig.until(&mut app, &view, "the refused read", move |app| state(app).key_errors.contains_key(&wait_id));
    *REFUSE_WRITES.lock().unwrap() = None;
    *REFUSE_READS.lock().unwrap() = None;
    assert!(says(state(&mut app).key_errors.get(&id).unwrap(), "读取 API Key 失败", "Could not read the API key"));
    assert!(!state(&mut app).visible_keys.contains(&id));
}

#[test]
fn does_not_surface_a_key_error_after_the_page_is_gone() {
    let _turn = store_turn();
    let (_d, mut app, mut rig, id) = seeded_key_provider("gone");
    *REFUSE_WRITES.lock().unwrap() = Some("locked".to_owned());
    type_key(&mut app, &mut rig, &id, "late-key");
    rig.click_away(&mut app, &view);
    close(&mut app);
    for _ in 0..40 {
        rig.frame(&mut app, &view);
        std::thread::sleep(Duration::from_millis(5));
    }
    *REFUSE_WRITES.lock().unwrap() = None;
    assert!(state(&mut app).key_errors.is_empty());
}

#[test]
fn a_missing_key_opens_the_field_empty_and_editable() {
    let _turn = store_turn();
    let (_d, mut app, mut rig, id) = seeded_key_provider("missing");
    rig.click(&mut app, &view, ("provider-key-reveal", id.as_str()));
    let wait_id = id.clone();
    rig.until(&mut app, &view, "the read", move |app| !state(app).revealing_keys.contains(&wait_id));
    assert!(state(&mut app).visible_keys.contains(&id), "{:?}", state(&mut app).key_errors);
    assert_eq!(state(&mut app).key_drafts.get(&id).map(String::as_str), Some(""));
    assert!(state(&mut app).key_errors.is_empty());
}

#[test]
fn models_group_by_family_collapse_and_search_forces_matches_open() {
    let mut p = provider("Relay", ProviderFamily::OpenaiCompatible, "https://r.test/v1");
    p.models = vec![model("gpt-5"), model("gpt-4o"), model("claude-opus")];
    let (_d, mut app, mut rig) = seeded("groups", vec![p]);
    assert!(on_screen(("model-group", "gpt")));
    assert!(on_screen(("model-group", "claude")));
    rig.click(&mut app, &view, ("model-group", "gpt"));
    assert!(!on_screen(("model-row", "gpt-5")));
    rig.click(&mut app, &view, "models-search-open");
    rig.type_in(&mut app, &view, "models-search", "gpt-5");
    assert!(on_screen(("model-row", "gpt-5")), "a match shows through a collapsed group");
    assert!(!on_screen(("model-row", "claude-opus")));
    rig.press(&mut app, &view, Key::Escape, |_| {});
    assert_eq!(state(&mut app).section.query, "");
    rig.click(&mut app, &view, "models-toggle-all");
    let groups: HashSet<String> = state(&mut app).section.collapsed.clone().into_iter().collect();
    assert!(groups.is_empty(), "expanding everything clears what was collapsed");
}

#[test]
fn removing_a_group_removes_every_model_in_it() {
    let mut p = provider("Relay", ProviderFamily::OpenaiCompatible, "https://r.test/v1");
    p.models = vec![model("gpt-5"), model("gpt-4o"), model("claude-opus")];
    let (_d, mut app, mut rig) = seeded("group-remove", vec![p]);
    let r = fluent::el::rect_of(("model-group", "gpt")).unwrap();
    rig.input.pointer = Some((r.x + 10.0, r.y + 5.0));
    rig.settle(&mut app, &view);
    rig.click(&mut app, &view, ("model-group-remove", "gpt"));
    assert_eq!(find(&app, "Relay").models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["claude-opus"]);
}

// ---------------------------------------------------------------- accessibility

#[test]
fn a_provider_row_is_a_button_named_for_it_and_its_panes_controls_all_say_what_they_are() {
    let mut mine = provider("Relay", ProviderFamily::OpenaiChat, "https://relay.test/v1");
    mine.models = vec![model("gpt-5")];
    let other = provider("Other", ProviderFamily::OpenaiChat, "https://other.test/v1");
    let (_d, mut app, mut rig) = seeded("a11y-rail", vec![mine, other]);
    let tree = a11y_tree(&mut rig, &mut app, &view);
    let row = a11y_is(&tree, Role::Button, "Relay");
    assert!(row.aria_current().is_some(), "the selected row is the current one");
    assert!(row.keyboard_shortcut().is_some());
    assert!(a11y_is(&tree, Role::Button, "Other").aria_current().is_none());
    a11y_is(&tree, Role::TextInput, t("搜索提供商", "Search providers"));
    assert_eq!(a11y_named(&tree, t("筛选提供商", "Filter providers")).has_popup(), Some(accesskit::HasPopup::Menu));
    a11y_is(&tree, Role::Switch, &tf("{name} 启用状态", "{name} enabled state", &[("name", &"Relay")]));
    a11y_is(&tree, Role::PasswordInput, "API Key");
    a11y_is(&tree, Role::TextInput, t("API 地址", "API address"));
    assert!(a11y_named(&tree, &model_group_of("gpt-5")).is_expanded().is_some());
    assert_eq!(a11y_unnamed(&tree), Vec::<String>::new());
    fluent::el::set_a11y_active(false);
}

fn model_group_of(id: &str) -> String {
    mewrk_settings_global::logic::model_caps::model_group(&model(id))
}

#[test]
fn a_models_switches_say_whose_they_are_and_a_blank_one_is_a_new_model() {
    let mut mine = provider("Relay", ProviderFamily::OpenaiChat, "https://relay.test/v1");
    mine.models = vec![model("gpt-5")];
    let (_d, mut app, mut rig) = seeded("a11y-model", vec![mine]);
    rig.click(&mut app, &view, ("model-edit", "gpt-5"));
    let tree = a11y_tree(&mut rig, &mut app, &view);
    a11y_is(&tree, Role::Dialog, t("模型属性", "Model properties"));
    a11y_is(&tree, Role::TextInput, t("模型 ID", "Model ID"));
    a11y_is(&tree, Role::TextInput, t("最大输出 Token", "Maximum output tokens"));
    a11y_is(&tree, Role::ComboBox, t("思考字段形态", "Reasoning field form"));
    let vision = tf("{name} {capability}", "{name} {capability}", &[("name", &"gpt-5"), ("capability", &t("视觉输入", "Vision"))]);
    a11y_is(&tree, Role::Switch, &vision);
    assert_eq!(a11y_unnamed(&tree), Vec::<String>::new());

    rig.type_in(&mut app, &view, "model-id", "");
    let tree = a11y_tree(&mut rig, &mut app, &view);
    assert!(a11y_is(&tree, Role::TextInput, t("模型 ID", "Model ID")).invalid().is_some());
    let vision = tf("{name} {capability}", "{name} {capability}", &[("name", &t("新模型", "New model")), ("capability", &t("视觉输入", "Vision"))]);
    a11y_is(&tree, Role::Switch, &vision);
    fluent::el::set_a11y_active(false);
}

#[test]
fn the_model_manager_says_what_a_row_would_do_and_whether_it_is_in() {
    let mut mine = provider("Relay", ProviderFamily::OpenaiChat, "");
    mine.models = vec![model("gpt-5")];
    let (_d, mut app, mut rig) = seeded("a11y-manager", vec![mine]);
    state(&mut app).model_manager_open = true;
    let tree = a11y_tree(&mut rig, &mut app, &view);
    a11y_is(&tree, Role::Dialog, t("发现模型", "Discover models"));
    a11y_is(&tree, Role::TextInput, t("搜索模型", "Search models"));
    let row = a11y_is(&tree, Role::Button, &tf("从提供商移除 {id}", "Remove {id} from the provider", &[("id", &"gpt-5")]));
    assert!(row.toggled().is_some());
    assert_eq!(a11y_unnamed(&tree), Vec::<String>::new());
    fluent::el::set_a11y_active(false);
}

#[test]
fn adding_a_provider_and_its_options_are_named_dialogs_with_named_fields() {
    let mine = provider("Relay", ProviderFamily::OpenaiChat, "https://relay.test/v1");
    let (_d, mut app, mut rig) = seeded("a11y-dialogs", vec![mine]);
    rig.click(&mut app, &view, "provider-add");
    let tree = a11y_tree(&mut rig, &mut app, &view);
    a11y_is(&tree, Role::Dialog, t("添加提供商", "Add provider"));
    a11y_is(&tree, Role::TextInput, t("提供商名称", "Provider name"));
    a11y_is(&tree, Role::ComboBox, t("对话协议", "Chat protocol"));
    assert_eq!(a11y_unnamed(&tree), Vec::<String>::new());
    fluent::el::set_a11y_active(false);
    rig.click(&mut app, &view, "add-provider-cancel");
    rig.click(&mut app, &view, "provider-options");
    let tree = a11y_tree(&mut rig, &mut app, &view);
    a11y_is(&tree, Role::Dialog, t("提供商设置", "Provider settings"));
    a11y_is(&tree, Role::ComboBox, t("API 格式", "API format"));
    assert_eq!(a11y_unnamed(&tree), Vec::<String>::new());
    fluent::el::set_a11y_active(false);
}
