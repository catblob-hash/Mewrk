//! `ProviderSettings/index.tsx` and `ProviderRail.tsx`: the provider rail and the selected provider's pane.

mod claude_agent;
mod codex;
mod drawers;
mod model_section;

use std::collections::{BTreeSet, HashMap, HashSet};

use mewrk_client::model::{ApiProvider, AppDocument, ModelProfile, ProviderFamily};
use mewrk_client::prelude::*;

use crate::logic::codex_provider::{is_builtin_provider, is_claude_agent_provider, is_codex_provider};
use crate::logic::endpoint_meta::{api_address_derivation, api_address_example, chat_request_preview, no_address_note};
use crate::logic::model_caps::has_model_catalog;
use crate::logic::provider_ops::{active_after_enable_change, active_after_removal, apply_provider_update, create_model, create_provider, key_error_reason, merge_model_profiles, preferred_provider_id, provider_endpoint_fingerprint, provider_matches, reorder};
use crate::logic::store;
use crate::widgets::*;

pub use claude_agent::{format_bytes, ClaudeAgentLoginStatus, ClaudeCliStatus};
pub use codex::CodexOauthStatus;
pub use drawers::{model_id_error, optional_positive_integer};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RailFilter {
    #[default]
    All,
    Enabled,
    Disabled,
}

#[derive(Clone, Debug)]
pub struct ModelEditor {
    pub create: bool,
    pub provider_id: String,
    pub original_id: Option<String>,
    pub draft: ModelProfile,
    /// A model being added takes Mewrk's declarations as its ID is typed, until any of their switches is moved.
    pub append_touched: bool,
}

#[derive(Default)]
pub struct ProviderDrag {
    pub item: Option<String>,
    pub moving: bool,
    pub target: Option<(String, bool)>,
}

#[derive(Default)]
pub struct ProvidersState {
    /// Bumps on unmount: `mountedRef` for answers on their way.
    pub generation: u64,
    pub started: bool,
    pub selected_id: Option<String>,
    pub key_drafts: HashMap<String, String>,
    pub key_lengths: HashMap<String, usize>,
    pub saving_keys: HashSet<String>,
    pub revealing_keys: HashSet<String>,
    pub visible_keys: HashSet<String>,
    pub model_editor: Option<ModelEditor>,
    pub busy_action: Option<&'static str>,
    pub provider_query: String,
    pub rail_filter: RailFilter,
    pub adding_provider: bool,
    pub add_name: String,
    pub add_family: Option<ProviderFamily>,
    pub options_open: bool,
    pub model_manager_open: bool,
    /// Latest `GET /models` results by provider ID, used by the model manager drawer.
    pub discovered: HashMap<String, Vec<ModelProfile>>,
    /// Why the most recent `GET /models` failed: a relay that answers 401 is otherwise reported as "no matching model".
    pub discovery_errors: HashMap<String, String>,
    /// Why the most recent credential transaction failed. A refused save must not look like a successful one.
    pub key_errors: HashMap<String, String>,
    pub action_token: u64,
    pub key_length_token: u64,
    pub key_save_tokens: HashMap<String, u64>,
    pub saved_key_drafts: HashMap<String, String>,
    /// Which provider the secondary panels were opened for.
    pub panels_for: Option<String>,
    pub key_length_for: Option<String>,
    pub drag: ProviderDrag,
    pub section: model_section::SectionState,
    pub manager: drawers::ManagerState,
    pub codex: codex::CodexState,
    pub claude_login: claude_agent::LoginState,
    pub claude_cli: claude_agent::CliState,
}

pub fn state(app: &mut App) -> &mut ProvidersState {
    app.local::<ProvidersState>()
}

pub fn unmount(app: &mut App) {
    let s = state(app);
    let generation = s.generation + 1;
    *s = ProvidersState { generation, ..ProvidersState::default() };
}

/// `window.addEventListener("focus")` after a blur: the panels that read something a terminal may have changed.
pub fn window_refocused(app: &mut App) {
    let selected = selected_provider(app);
    if selected.as_ref().is_some_and(|p| is_claude_agent_provider(p.family)) {
        claude_agent::on_refocus(app);
    }
}

fn providers(app: &App) -> &[ApiProvider] {
    &app.doc.assets.api_providers
}

fn find<'a>(doc: &'a AppDocument, id: &str) -> Option<&'a ApiProvider> {
    doc.assets.api_providers.iter().find(|p| p.id == id)
}

/// `const selected = providers.find(...) ?? providers[0]`.
pub fn selected_provider(app: &mut App) -> Option<ApiProvider> {
    let id = state(app).selected_id.clone();
    let list = providers(app);
    id.and_then(|id| list.iter().find(|p| p.id == id)).or_else(|| list.first()).cloned()
}

fn stale(app: &mut App, generation: u64) -> bool {
    state(app).generation != generation
}

fn key_length_store_key(provider_id: &str) -> String {
    format!("mewrk.api-key-length.v1.{provider_id}")
}

fn stored_key_length(app: &mut App, provider_id: &str) -> Option<usize> {
    let key = key_length_store_key(provider_id);
    app.ui_store().get(&key).and_then(|s| s.parse().ok()).filter(|n| *n > 0)
}

fn remember_key_length(app: &mut App, provider_id: &str, length: Option<usize>) {
    let key = key_length_store_key(provider_id);
    app.ui_store_update(|store| match length {
        Some(n) if n > 0 => store.set(&key, n.to_string()),
        _ => store.remove(&key),
    });
}

/// Runs each frame in place of upstream's effects on provider identity.
fn effects(app: &mut App) {
    let doc_active = app.doc.global_settings.active_provider_id.clone();
    let list_has_selected = {
        let selected = state(app).selected_id.clone();
        selected.is_some_and(|id| providers(app).iter().any(|p| p.id == id))
    };
    if !state(app).started || !list_has_selected {
        let preferred = preferred_provider_id(providers(app), doc_active.as_deref());
        let s = state(app);
        s.started = true;
        s.selected_id = preferred;
    }
    let selected = selected_provider(app);
    let selected_id = selected.as_ref().map(|p| p.id.clone());
    // Close secondary panels when switching providers so their contents cannot edit the previous provider.
    if state(app).panels_for != selected_id {
        let s = state(app);
        s.panels_for = selected_id.clone();
        s.model_editor = None;
        s.options_open = false;
        s.model_manager_open = false;
        s.section = Default::default();
        s.manager = Default::default();
        // The sign-in panel unmounts with the provider: its poll stops, and coming back reads the status again.
        codex::reset(app);
    }
    // Key-length metadata belongs to provider identity; a provider edit must not reload it.
    if state(app).key_length_for != selected_id {
        state(app).key_length_for = selected_id.clone();
        if let Some(p) = selected.filter(|p| !is_builtin_provider(p.family)) {
            let length = stored_key_length(app, &p.id);
            let s = state(app);
            match length {
                Some(n) => {
                    s.key_lengths.insert(p.id, n);
                }
                None => {
                    s.key_lengths.remove(&p.id);
                }
            }
        }
    }
}

/// `replaceProvider`: applies an edit, renames the active model along, repairs it.
pub fn replace_provider(app: &mut App, provider_id: &str, update: impl FnOnce(&mut ApiProvider), rename_active: Option<(String, Option<String>)>) {
    let id = provider_id.to_owned();
    store::change(app, move |doc| {
        if let Some(p) = doc.assets.api_providers.iter_mut().find(|p| p.id == id) {
            apply_provider_update(p, update, rename_active.as_ref().map(|(a, b)| (a.as_str(), b.as_deref())));
        }
    });
}

pub fn set_provider_enabled(app: &mut App, provider_id: &str, enabled: bool) {
    let id = provider_id.to_owned();
    store::change(app, move |doc| {
        if let Some(p) = doc.assets.api_providers.iter_mut().find(|p| p.id == id) {
            p.enabled = enabled;
        }
        let active = active_after_enable_change(&doc.assets.api_providers, doc.global_settings.active_provider_id.as_deref());
        doc.global_settings.active_provider_id = active;
    });
}

pub fn add_custom_provider(app: &mut App, name: &str, family: ProviderFamily) {
    let provider = create_provider(name, family);
    let id = provider.id.clone();
    store::change(app, move |doc| {
        if doc.global_settings.active_provider_id.is_none() {
            doc.global_settings.active_provider_id = Some(provider.id.clone());
        }
        doc.assets.api_providers.push(provider);
    });
    let s = state(app);
    s.adding_provider = false;
    s.add_name.clear();
    s.add_family = None;
    select_provider(app, Some(&id));
}

fn begin_action(app: &mut App, action: &'static str) -> u64 {
    let s = state(app);
    s.action_token += 1;
    s.busy_action = Some(action);
    s.action_token
}

fn invalidate_action(app: &mut App) {
    let s = state(app);
    s.action_token += 1;
    s.busy_action = None;
}

fn finish_action(app: &mut App, token: u64) {
    let s = state(app);
    if s.action_token == token {
        s.busy_action = None;
    }
}

pub fn select_provider(app: &mut App, provider_id: Option<&str>) {
    if state(app).selected_id.as_deref() == provider_id {
        return;
    }
    invalidate_action(app);
    let s = state(app);
    s.key_length_token += 1;
    if let Some(previous) = s.selected_id.take() {
        s.saved_key_drafts.remove(&previous);
        s.visible_keys.remove(&previous);
        s.key_drafts.remove(&previous);
    }
    s.selected_id = provider_id.map(str::to_owned);
}

pub fn remove_provider(app: &mut App, provider_id: &str) {
    let Some(removed) = find(&app.doc, provider_id).cloned() else { return };
    if is_builtin_provider(removed.family) {
        return;
    }
    let remaining: Vec<ApiProvider> = providers(app).iter().filter(|p| p.id != provider_id).cloned().collect();
    let next = preferred_provider_id(&remaining, app.doc.global_settings.active_provider_id.as_deref());
    invalidate_action(app);
    let s = state(app);
    s.key_length_token += 1;
    *s.key_save_tokens.entry(provider_id.to_owned()).or_default() += 1;
    s.saved_key_drafts.remove(provider_id);
    remember_key_length(app, provider_id, None);
    let id = provider_id.to_owned();
    store::change(app, move |doc| {
        doc.assets.api_providers.retain(|p| p.id != id);
        let active = active_after_removal(&doc.assets.api_providers, doc.global_settings.active_provider_id.as_deref(), &id);
        doc.global_settings.active_provider_id = active;
    });
    let s = state(app);
    s.selected_id = next;
    s.key_drafts.remove(provider_id);
    s.key_lengths.remove(provider_id);
    s.visible_keys.remove(provider_id);
    s.saving_keys.remove(provider_id);
    s.revealing_keys.remove(provider_id);
}

fn record_key_error(app: &mut App, provider_id: &str, save_token: u64, message: String) {
    let s = state(app);
    if s.key_save_tokens.get(provider_id) == Some(&save_token) {
        s.key_errors.insert(provider_id.to_owned(), message);
    }
}

fn finish_saving(app: &mut App, provider_id: &str, save_token: u64) {
    let s = state(app);
    if s.key_save_tokens.get(provider_id) == Some(&save_token) {
        s.saving_keys.remove(provider_id);
    }
}

/// Saves the draft. Whatever happens, `done` learns whether it is stored now.
pub fn persist_key(app: &mut App, provider_id: &str, done: impl FnOnce(&mut App, bool) + Send + 'static) {
    let s = state(app);
    let secret = s.key_drafts.get(provider_id).map_or("", |d| d.trim()).to_owned();
    if secret.is_empty() || s.saved_key_drafts.get(provider_id) == Some(&secret) {
        done(app, true);
        return;
    }
    let token = {
        let t = s.key_save_tokens.entry(provider_id.to_owned()).or_default();
        *t += 1;
        *t
    };
    s.key_length_token += 1;
    s.saving_keys.insert(provider_id.to_owned());
    let generation = s.generation;
    let id = provider_id.to_owned();
    // Persist the provider before storing its key: credential names derive from its ID, so an unsaved provider would
    // create an unreadable orphan.
    store::flush(app, move |app, flushed| {
        if stale(app, generation) {
            return done(app, false);
        }
        if let Err(e) = flushed {
            record_key_error(app, &id, token, tf("保存 API Key 失败：{reason}", "Could not save the API key: {reason}", &[("reason", &key_error_reason(&e, Some(&secret)))]));
            finish_saving(app, &id, token);
            return done(app, false);
        }
        let Some(snapshot) = find(&app.doc, &id).cloned() else {
            record_key_error(app, &id, token, t("保存 API Key 失败：这个提供商已经不在设置里了。", "Could not save the API key: this provider is no longer in settings.").to_owned());
            finish_saving(app, &id, token);
            return done(app, false);
        };
        let id2 = id.clone();
        let secret2 = secret.clone();
        app.invoke::<Value>("save_api_key", json!({ "provider": snapshot, "apiKey": secret }), move |app, result| {
            if stale(app, generation) || state(app).key_save_tokens.get(&id2) != Some(&token) {
                return done(app, false);
            }
            finish_saving(app, &id2, token);
            let refused = matches!(&result, Ok(Value::Bool(false)));
            match result {
                Ok(_) if !refused => {
                    let length = secret2.chars().count();
                    remember_key_length(app, &id2, Some(length));
                    let s = state(app);
                    s.key_lengths.insert(id2.clone(), length);
                    if s.visible_keys.contains(&id2) {
                        s.saved_key_drafts.insert(id2.clone(), secret2);
                    } else {
                        s.saved_key_drafts.remove(&id2);
                        s.key_drafts.remove(&id2);
                    }
                    s.key_errors.remove(&id2);
                    done(app, true);
                }
                other => {
                    // The draft stays so the user can retry, and no success metadata is written.
                    let reason = other.err().unwrap_or_default();
                    record_key_error(app, &id2, token, tf("保存 API Key 失败：{reason}", "Could not save the API key: {reason}", &[("reason", &key_error_reason(&reason, Some(&secret2)))]));
                    done(app, false);
                }
            }
        });
    });
}

pub fn remove_stored_key(app: &mut App, provider_id: &str) {
    let s = state(app);
    let token = {
        let t = s.key_save_tokens.entry(provider_id.to_owned()).or_default();
        *t += 1;
        *t
    };
    s.saving_keys.insert(provider_id.to_owned());
    let generation = s.generation;
    let id = provider_id.to_owned();
    let refused = |e: &str| tf("删除 API Key 失败：{reason}", "Could not delete the API key: {reason}", &[("reason", &key_error_reason(e, None))]);
    store::flush(app, move |app, flushed| {
        if stale(app, generation) {
            return;
        }
        if let Err(e) = flushed {
            record_key_error(app, &id, token, refused(&e));
            state(app).key_drafts.remove(&id);
            finish_saving(app, &id, token);
            return;
        }
        let Some(snapshot) = find(&app.doc, &id).cloned() else {
            record_key_error(app, &id, token, t("删除 API Key 失败：这个提供商已经不在设置里了。", "Could not delete the API key: this provider is no longer in settings.").to_owned());
            finish_saving(app, &id, token);
            return;
        };
        let id2 = id.clone();
        app.invoke::<Value>("delete_api_key", json!({ "provider": snapshot }), move |app, result| {
            if stale(app, generation) || state(app).key_save_tokens.get(&id2) != Some(&token) {
                return;
            }
            finish_saving(app, &id2, token);
            match result {
                Ok(_) => {
                    remember_key_length(app, &id2, None);
                    let s = state(app);
                    s.saved_key_drafts.remove(&id2);
                    s.key_drafts.remove(&id2);
                    s.key_lengths.remove(&id2);
                    s.key_errors.remove(&id2);
                }
                Err(e) => {
                    record_key_error(app, &id2, token, refused(&e));
                    state(app).key_drafts.remove(&id2);
                }
            }
        });
    });
}

pub fn toggle_key_visibility(app: &mut App, provider_id: &str) {
    if state(app).visible_keys.contains(provider_id) {
        let id = provider_id.to_owned();
        persist_key(app, provider_id, move |app, saved| {
            if saved {
                let s = state(app);
                s.visible_keys.remove(&id);
                s.key_drafts.remove(&id);
                s.saved_key_drafts.remove(&id);
            }
        });
        return;
    }
    let s = state(app);
    if s.key_drafts.contains_key(provider_id) {
        s.visible_keys.insert(provider_id.to_owned());
        return;
    }
    s.revealing_keys.insert(provider_id.to_owned());
    let generation = s.generation;
    let id = provider_id.to_owned();
    store::flush(app, move |app, flushed| {
        if stale(app, generation) {
            return;
        }
        let fail = |app: &mut App, id: &str, e: String| {
            if state(app).selected_id.as_deref() == Some(id) {
                // A missing key does not make the field unusable; leave it editable and empty. Any other read error is
                // reported, because a silent one leaves the field showing a mask the user cannot explain.
                if e.contains("未配置") || e.contains("浏览器预览不会保留") || e.to_lowercase().contains("not configured") {
                    let s = state(app);
                    s.key_drafts.insert(id.to_owned(), String::new());
                    s.visible_keys.insert(id.to_owned());
                } else {
                    let message = tf("读取 API Key 失败：{reason}", "Could not read the API key: {reason}", &[("reason", &key_error_reason(&e, None))]);
                    state(app).key_errors.insert(id.to_owned(), message);
                }
            }
            state(app).revealing_keys.remove(id);
        };
        if let Err(e) = flushed {
            return fail(app, &id, e);
        }
        let Some(snapshot) = find(&app.doc, &id).cloned() else {
            state(app).revealing_keys.remove(&id);
            return;
        };
        let id2 = id.clone();
        app.invoke::<String>("reveal_api_key", json!({ "provider": snapshot }), move |app, result| {
            if stale(app, generation) {
                return;
            }
            match result {
                Ok(secret) => {
                    let length = secret.chars().count();
                    remember_key_length(app, &id2, Some(length));
                    let s = state(app);
                    s.revealing_keys.remove(&id2);
                    if s.selected_id.as_deref() != Some(id2.as_str()) {
                        return;
                    }
                    s.key_lengths.insert(id2.clone(), length);
                    s.saved_key_drafts.insert(id2.clone(), secret.trim().to_owned());
                    s.key_drafts.insert(id2.clone(), secret);
                    s.visible_keys.insert(id2.clone());
                    s.key_errors.remove(&id2);
                }
                Err(e) => fail(app, &id2, e),
            }
        });
    });
}

/// The host omits empty `name` and `group`, and an unknown field may be anything: what `normalizeModel` makes of it.
fn normalize_discovered(values: Vec<Value>, family: ProviderFamily) -> Vec<ModelProfile> {
    values
        .into_iter()
        .filter_map(|mut v| {
            let id = v.get("id")?.as_str()?.trim().to_owned();
            if id.is_empty() {
                return None;
            }
            if v.get("reasoningContent").is_none() {
                v["reasoningContent"] = serde_json::to_value(crate::logic::model_caps::default_reasoning_content(family)).ok()?;
            }
            serde_json::from_value(v).ok()
        })
        .collect()
}

pub fn discover_models(app: &mut App) {
    let Some(selected) = selected_provider(app) else { return };
    let provider_id = selected.id.clone();
    let token = begin_action(app, "models");
    let generation = state(app).generation;
    let current = move |app: &mut App, id: &str| !stale(app, generation) && state(app).action_token == token && state(app).selected_id.as_deref() == Some(id);
    store::flush(app, move |app, flushed| {
        if !current(app, &provider_id) {
            return finish_action(app, token);
        }
        if let Err(e) = flushed {
            state(app).discovery_errors.insert(provider_id.clone(), e);
            return finish_action(app, token);
        }
        let Some(snapshot) = find(&app.doc, &provider_id).cloned() else { return finish_action(app, token) };
        let fingerprint = provider_endpoint_fingerprint(&snapshot);
        let family = snapshot.family;
        let id = provider_id.clone();
        app.invoke::<Vec<Value>>("fetch_models", json!({ "provider": snapshot }), move |app, result| {
            let same_endpoint = find(&app.doc, &id).is_some_and(|p| provider_endpoint_fingerprint(p) == fingerprint);
            if current(app, &id) {
                match result {
                    // Discovery only fills the catalog. Nothing joins the provider's list until a row is clicked.
                    Ok(values) if same_endpoint => {
                        let s = state(app);
                        s.discovered.insert(id.clone(), normalize_discovered(values, family));
                        s.discovery_errors.remove(&id);
                    }
                    Ok(_) => {}
                    // Keep the reason: a relay that answers 401/404 is otherwise reported as no matching model.
                    Err(e) => {
                        state(app).discovery_errors.insert(id.clone(), e);
                    }
                }
            }
            finish_action(app, token);
        });
    });
}

/// Opens the discovery drawer and refreshes the catalog, which may have changed since it was last opened.
pub fn open_model_manager(app: &mut App) {
    state(app).model_manager_open = true;
    focus_request(("field", "manager-search"));
    let Some(selected) = selected_provider(app) else { return };
    if state(app).busy_action.is_some() || !has_model_catalog(&selected) {
        return;
    }
    discover_models(app);
}

pub fn install_models(app: &mut App, models: Vec<ModelProfile>) {
    let Some(selected) = selected_provider(app) else { return };
    replace_provider(app, &selected.id, move |p| p.models = merge_model_profiles(&p.models, &models), None);
}

pub fn uninstall_models(app: &mut App, ids: BTreeSet<String>) {
    let Some(selected) = selected_provider(app) else { return };
    replace_provider(app, &selected.id, move |p| p.models.retain(|m| !ids.contains(&m.id)), None);
}

pub fn open_new_model_editor(app: &mut App, provider: &ApiProvider) {
    let draft = create_model(provider.family);
    open_model_editor(app, ModelEditor { create: true, provider_id: provider.id.clone(), original_id: None, draft, append_touched: false });
}

/// Every secondary panel opens with its first field ready to type in, as `Drawer` and `DialogShell` focus theirs.
pub fn open_model_editor(app: &mut App, editor: ModelEditor) {
    state(app).model_editor = Some(editor);
    focus_request(("field", "model-id"));
}

fn open_add_provider(app: &mut App) {
    state(app).adding_provider = true;
    focus_request(("field", "add-provider-name"));
}

fn open_provider_options(app: &mut App) {
    state(app).options_open = true;
    if let Some(selected) = selected_provider(app) {
        focus_request(("field", ("provider-name", selected.id.as_str())));
    }
}

fn reorder_hint(searching: bool, filtering: bool) -> String {
    let modifier = if cfg!(target_os = "macos") { "Option" } else { "Alt" };
    if !searching && !filtering {
        tf("拖动整行或按 {key}+↑/↓ 排序", "Drag the row or press {key}+↑/↓ to reorder", &[("key", &modifier)])
    } else if searching && filtering {
        t("搜索和筛选期间不能排序", "Reordering is unavailable while searching and filtering").to_owned()
    } else if searching {
        t("搜索期间不能排序", "Reordering is unavailable while searching").to_owned()
    } else {
        t("只显示已启用或已停用的提供商时不能排序", "Reordering is unavailable while the list is filtered to enabled or disabled providers").to_owned()
    }
}

fn move_by_keyboard(app: &mut App, provider_id: &str, up: bool) {
    let list = providers(app);
    let Some(index) = list.iter().position(|p| p.id == provider_id) else { return };
    let target = if up { index.checked_sub(1) } else { Some(index + 1) };
    let Some(target_id) = target.and_then(|i| list.get(i)).map(|p| p.id.clone()) else { return };
    let id = provider_id.to_owned();
    store::change(app, move |doc| reorder(&mut doc.assets.api_providers, &id, &target_id, !up, |p| &p.id));
}

/// Where a drag is pointing: the visible row under `y`, and which side of its middle.
fn drop_target(ids: &[String], item: &str, y: f32) -> Option<(String, bool)> {
    for id in ids {
        if id == item {
            continue;
        }
        if let Some(r) = rect_of(("provider-row", id.as_str()))
            && y >= r.y && y <= r.bottom() {
                return Some((id.clone(), y > r.y + r.h * 0.5));
            }
    }
    None
}

fn provider_rail(app: &mut App, visible: &[ApiProvider], selected_id: Option<&str>, total: usize) -> E {
    let t_ = theme();
    let s = state(app);
    let query = s.provider_query.clone();
    let filter = s.rail_filter;
    let searching = !query.trim().is_empty();
    let filtering = filter != RailFilter::All;
    let reorderable = !searching && !filtering;
    let hint = reorder_hint(searching, filtering);
    let untitled = t("未命名提供商", "Untitled provider");
    let mutation_disabled = s.busy_action.is_some() || selected_id.is_some_and(|id| s.saving_keys.contains(id));
    let drag_target = s.drag.target.clone();
    let dragging = s.drag.moving.then(|| s.drag.item.clone()).flatten();
    let visible_ids: Rc<Vec<String>> = Rc::new(visible.iter().map(|p| p.id.clone()).collect());

    let mut rows: Vec<E> = Vec::new();
    for provider in visible {
        let label = if provider.name.trim().is_empty() { untitled.to_owned() } else { provider.name.trim().to_owned() };
        let selected = selected_id == Some(provider.id.as_str());
        let id: Arc<str> = Arc::from(provider.id.as_str());
        let (select_id, key_id, pointer_id) = (id.clone(), id.clone(), id.clone());
        let ids = visible_ids.clone();
        let target_side = drag_target.as_ref().filter(|(t, _)| *t == provider.id).map(|(_, after)| *after);
        let mut row_el = row()
            .key(("provider-row", provider.id.as_str()))
            .w_full()
            .min_h(30.0)
            .pl(5.0)
            .pr(6.0)
            .gap(7.0)
            .items_center()
            .radius(CONTROL_RADIUS)
            .tooltip(hint.clone())
            .when(selected, |e| e.bg(t_.subtle_pressed))
            .when(dragging.as_deref() == Some(provider.id.as_str()), |e| e.opacity(0.5))
            .hover_bg(t_.subtle_hover)
            // The nested delete button must not contribute to the row's name.
            .role(Role::Button)
            .aria_label(label.clone())
            .aria_keyshortcuts("Alt+ArrowUp Alt+ArrowDown")
            .when(selected, |e| e.aria_current(AriaCurrent::True))
            .on_click(move |app: &mut App| select_provider(app, Some(&select_id)))
            .on_key(move |app: &mut App, k: &KeyPress| {
                match k.key {
                    Key::Enter | Key::Char(' ') => {
                        select_provider(app, Some(&key_id));
                        true
                    }
                    Key::Up | Key::Down if k.alt && reorderable => {
                        move_by_keyboard(app, &key_id, k.key == Key::Up);
                        true
                    }
                    _ => false,
                }
            })
            .child(avatar(&label, false))
            .child(text(label.clone()).ty(if selected { CAPTION_STRONG } else { CAPTION }).ellipsis().flex1())
            .child({
                let mut trailing = row().min_w(20.0).h(20.0).shrink0().justify_center().items_center();
                if provider.enabled {
                    trailing = trailing.child(status_dot());
                }
                trailing
            });
        if !is_builtin_provider(provider.family) {
            let del_id = id.clone();
            // The enabled dot yields to the delete button on hover.
            let delete = confirm_delete(("provider-delete", provider.id.as_str()), tf("删除 {name}", "Delete {name}", &[("name", &label)]), tf("确认删除 {name}", "Confirm deleting {name}", &[("name", &label)]), mutation_disabled, move |app| remove_provider(app, &del_id));
            row_el = row_el.child(delete.absolute().top(3.0).right(6.0).bg(t_.flyout).reveal_on_parent_hover());
        }
        if reorderable {
            row_el = row_el.on_pointer(move |app: &mut App, e: PointerEvent| {
                let s = state(app);
                match e.phase {
                    Phase::Down => s.drag = ProviderDrag { item: Some(pointer_id.to_string()), moving: false, target: None },
                    Phase::Move => {
                        if s.drag.item.is_some() && (s.drag.moving || e.dy.abs() > 4.0) {
                            s.drag.moving = true;
                            s.drag.target = drop_target(&ids, &pointer_id, e.y);
                            repaint();
                        }
                    }
                    Phase::Up => {
                        let drag = std::mem::take(&mut s.drag);
                        if let (true, Some(item), Some((target, after))) = (drag.moving, drag.item, drag.target) {
                            store::change(app, move |doc| reorder(&mut doc.assets.api_providers, &item, &target, after, |p| &p.id));
                        }
                    }
                }
            });
        }
        if let Some(after) = target_side {
            // The line a dropped row would land on.
            let line = div().absolute().left(0.0).right(0.0).h(2.0).radius(1.0).bg(t_.accent);
            row_el = row_el.child(if after { line.bottom(-2.0) } else { line.top(-2.0) });
        }
        rows.push(row_el);
    }
    if total == 0 {
        rows.push(rail_empty("Cloud", t("还没有提供商", "No providers yet"), t("添加一个自定义提供商。", "Add a custom provider.")));
    } else if visible.is_empty() {
        rows.push(rail_empty("Search", t("没有匹配的提供商", "No matching provider"), t("可按提供商名或模型 ID 搜索。", "Search by provider name or model ID.")));
    }
    let add = button::<App>("").key("provider-add").w_full().min_h(30.0).child(icon("Plus", 13.0).color(t_.text_secondary)).child(text(t("添加提供商", "Add provider")).ty(CAPTION).color(t_.text_secondary).nowrap()).on_click(open_add_provider);
    settings_rail(
        "api-providers",
        Some(RailSearch {
            key: "providers",
            value: query,
            label: t("搜索提供商", "Search providers"),
            placeholder: t("搜索提供商或模型", "Search providers or models"),
            on_change: Rc::new(|app: &mut App, v| state(app).provider_query = v),
        }),
        Some(RailFilter2::build(filter)),
        rows,
        Some(add),
    )
}

/// The filter's menu in the terms the rail takes.
struct RailFilter2;

impl RailFilter2 {
    fn build(filter: RailFilter) -> crate::widgets::RailFilter {
        crate::widgets::RailFilter {
            label: t("筛选提供商", "Filter providers"),
            value: match filter {
                RailFilter::All => 0,
                RailFilter::Enabled => 1,
                RailFilter::Disabled => 2,
            },
            neutral: 0,
            options: vec![t("全部提供商", "All providers"), t("仅已启用", "Enabled only"), t("仅已停用", "Disabled only")],
            on_change: Rc::new(|app: &mut App, i| {
                state(app).rail_filter = match i {
                    1 => RailFilter::Enabled,
                    2 => RailFilter::Disabled,
                    _ => RailFilter::All,
                }
            }),
        }
    }
}

fn api_key_section(app: &mut App, selected: &ApiProvider) -> E {
    let t_ = theme();
    let id = selected.id.clone();
    let s = state(app);
    let draft = s.key_drafts.get(&id).cloned();
    let has_draft = draft.is_some();
    let mask = "\u{2022}".repeat(s.key_lengths.get(&id).copied().unwrap_or(0));
    let has_mask = !mask.is_empty();
    let visible = s.visible_keys.contains(&id);
    let revealing = s.revealing_keys.contains(&id);
    let saving = s.saving_keys.contains(&id);
    let error = s.key_errors.get(&id).cloned();
    let (change_id, blur_id, reveal_id) = (id.clone(), id.clone(), id.clone());
    let input_el = field::<App>(("provider-key", id.as_str()), draft.unwrap_or(mask))
        .placeholder(t("输入 API Key", "Enter API key"))
        .ty(MONO_BODY)
        .bare()
        .obscure(!visible)
        .controlled()
        .select_on_focus(!has_draft && has_mask)
        .on_change(move |app, v| {
            state(app).key_drafts.insert(change_id.clone(), v);
        })
        .on_blur(move |app, _| {
            let empty = state(app).key_drafts.get(&blur_id).is_some_and(|d| d.trim().is_empty());
            if empty {
                remove_stored_key(app, &blur_id);
            } else {
                persist_key(app, &blur_id, |_, _| {});
            }
        })
        .build()
        .flex1()
        .h(18.0)
        .aria_label("API Key")
        .aria_invalid(error.is_some())
        .aria_describedby(("provider-key-error", id.as_str()))
        .aria_busy(saving);
    let reveal_label = if revealing {
        t("正在读取 API Key", "Reading API key")
    } else if visible {
        t("隐藏 API Key", "Hide API key")
    } else {
        t("显示 API Key", "Show API key")
    };
    let reveal = reveal_button(("provider-key-reveal", id.as_str()), reveal_label, revealing, visible, revealing || saving, move |app: &mut App| toggle_key_visibility(app, &reveal_id));
    let group = input_group(input_el, error.is_some()).child(reveal);
    let _ = t_;
    col()
        .gap(6.0)
        .child(text("API Key").ty(CAPTION_STRONG).nowrap())
        .child(group)
        .child(error.map(|e| error_line(e).key(("provider-key-error", id.as_str()))))
        .child(help_with_docs(t("失去焦点时自动保存到系统凭据库。", "Saved to the system credential store when you leave the field."), "providers", "add-provider"))
}

fn api_address_section(selected: &ApiProvider) -> E {
    let t_ = theme();
    let id = selected.id.clone();
    let id_options = id.clone();
    let preview = chat_request_preview(&selected.base_url, selected.family);
    let address = grouped_code_input(("provider-address", selected.id.as_str()), t("API 地址", "API address"), selected.base_url.clone(), api_address_example(selected.family), move |app, base_url| replace_provider(app, &id, |p| p.base_url = base_url, None));
    col()
        .gap(6.0)
        .child(text(t("API 地址", "API address")).ty(CAPTION_STRONG).nowrap())
        .child(row().gap(6.0).items_center().child(address).child(icon_button::<App>("Settings2", t("API 格式与身份字段", "API format and identity fields")).key(("provider-address-options", id_options.as_str())).size(34.0, 34.0).radius(CONTROL_RADIUS + 3.0).bg(t_.control_fill).border(t_.control_stroke).on_click(open_provider_options)))
        .child(if preview.is_empty() { help(api_address_derivation(selected.family).map_or_else(|| no_address_note(selected.family), str::to_owned)) } else { text(tf("对话将请求 {url}", "Chat requests go to {url}", &[("url", &preview)])).ty(MONO_SMALL).color(t_.text_tertiary) })
        .child((selected.family == ProviderFamily::Azure).then(|| help_with_docs(t("地址写到 /openai 为止，不要以 /v1 结尾。", "Stop the address at /openai; don't end it with /v1."), "providers", "protocols")))
}

fn provider_pane(app: &mut App, selected: &ApiProvider) -> E {
    let t_ = theme();
    let name = if selected.name.trim().is_empty() { t("未命名提供商", "Untitled provider").to_owned() } else { selected.name.trim().to_owned() };
    let id_toggle = selected.id.clone();
    let enabled = selected.enabled;
    let enabled_toggle = labeled_switch(("provider-enabled", selected.id.as_str()), enabled, false, move |app| set_provider_enabled(app, &id_toggle, !enabled)).aria_label(tf("{name} 启用状态", "{name} enabled state", &[("name", &name)]));
    let header = row()
        .w_full()
        .max_w(676.0)
        .mx_auto()
        .min_h(48.0)
        .py(11.0)
        .pl(18.0)
        .pr(if settings_strip() { 18.0 } else { 52.0 })
        .gap(12.0)
        .items_center()
        .child(row().flex1().min_w(0.0).items_center().gap(4.0).child(text(name).ty(BODY_SMALL_STRONG).ellipsis().role(Role::Heading).aria_level(1)).child(icon_button::<App>("Settings2", t("提供商设置", "Provider settings")).key("provider-options").size(26.0, 26.0).on_click(open_provider_options)))
        .child(enabled_toggle);
    let auth: E = if is_codex_provider(selected.family) {
        codex::codex_login_panel(app, selected)
    } else if is_claude_agent_provider(selected.family) {
        claude_agent::claude_agent_panels(app, selected)
    } else {
        api_key_section(app, selected)
    };
    let stack = col()
        .w_full()
        .max_w(640.0)
        .mx_auto()
        .gap(16.0)
        .child(auth)
        // Both built-in families own their endpoint: Codex's belongs to the ChatGPT backend and Claude Agent's is picked
        // by the local CLI. Codex keeps an overridable address for test stubs, in the drawer.
        .child((!is_builtin_provider(selected.family)).then(|| api_address_section(selected)))
        .child(model_section::model_section(app, selected));
    let _ = t_;
    col().flex1().min_w(0.0).min_h(0.0).child(header).child(col().key(("provider-pane-body", selected.id.as_str())).flex1().min_h(0.0).scroll_y().pt(14.0).px(18.0).pb(24.0).child(stack))
}

/// The page.
pub fn api_provider_settings(app: &mut App) -> E {
    let t_ = theme();
    effects(app);
    let selected = selected_provider(app);
    let selected_id = selected.as_ref().map(|p| p.id.clone());
    let total = providers(app).len();
    let s = state(app);
    let needle = s.provider_query.trim().to_lowercase();
    let filter = s.rail_filter;
    let visible: Vec<ApiProvider> = providers(app)
        .iter()
        .filter(|p| match filter {
            RailFilter::Enabled => p.enabled,
            RailFilter::Disabled => !p.enabled,
            RailFilter::All => true,
        })
        .filter(|p| provider_matches(p, &needle))
        .cloned()
        .collect();
    let rail = provider_rail(app, &visible, selected_id.as_deref(), total);
    let pane = match &selected {
        Some(p) => provider_pane(app, p),
        None => col()
            .flex1()
            .center()
            .gap(8.0)
            .child(icon("Cloud", 24.0).color(t_.text_tertiary))
            .child(text(t("添加 API 提供商", "Add an API provider")).ty(CAPTION_STRONG).color(t_.text_secondary).nowrap())
            .child(button::<App>("").key("provider-add-empty").mt(5.0).child(icon("Plus", 13.0).color(t_.text_secondary)).child(text(t("添加提供商", "Add provider")).ty(CAPTION).color(t_.text_secondary).nowrap()).on_click(open_add_provider)),
    };
    let overlays = drawers::overlays(app, selected.as_ref());
    row().w_full().h_full().child(rail).child(pane).children(overlays)
}
