//! `WebSearchSettings.tsx`: the search provider catalog. Every setting here belongs to one provider row; how a search
//! behaves is asked of the conversation that runs it.

use mewrk_client::model::{SearchProviderConfig, SearchProviderKind};
use mewrk_client::prelude::*;

use crate::logic::search_providers::{search_provider_entry, search_providers};
use crate::logic::store;
use crate::logic::web_search::SearchCredentialSlot;
use crate::secret_field::{self, secret_field, Identity};
use crate::widgets::*;

#[derive(Default)]
pub struct WebSearchPageState {
    pub selected: Option<SearchProviderKind>,
    /// A field parsed while typing needs its own draft: rendering the parse would eat a trailing comma.
    pub engines_draft: Option<String>,
}

fn page_state(app: &mut App) -> &mut WebSearchPageState {
    app.local::<WebSearchPageState>()
}

pub fn unmount(app: &mut App) {
    *page_state(app) = WebSearchPageState::default();
    secret_field::unmount(app);
}

fn provider_config(app: &App, kind: SearchProviderKind) -> SearchProviderConfig {
    app.doc.assets.web_search.providers.iter().find(|p| p.kind == kind).cloned().unwrap_or_else(|| SearchProviderConfig::new(kind))
}

/// Writes one row, filling in the whole catalog as `updateProvider` does.
pub fn update_provider(app: &mut App, kind: SearchProviderKind, patch: impl FnOnce(&mut SearchProviderConfig)) {
    store::change(app, move |doc| {
        let mut rows: Vec<SearchProviderConfig> = search_providers().iter().map(|entry| doc.assets.web_search.providers.iter().find(|p| p.kind == entry.kind).cloned().unwrap_or_else(|| SearchProviderConfig::new(entry.kind))).collect();
        if let Some(row) = rows.iter_mut().find(|r| r.kind == kind) {
            patch(row);
        }
        doc.assets.web_search.providers = rows;
    });
}

/// `engines.split(",").map(trim).filter(Boolean)`.
pub fn parse_engines(text: &str) -> Vec<String> {
    text.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect()
}

fn titled(title: &str, control: E, under: E) -> E {
    col().gap(6.0).child(text(title.to_owned()).ty(CAPTION_STRONG).nowrap()).child(control).child(under)
}

fn endpoint_rule() -> E {
    help_with_docs(t("留空使用默认端点，须为 https。", "Leave empty for the default; must be https."), "web", "provider-settings")
}

fn provider_pane(app: &mut App, kind: SearchProviderKind) -> E {
    let entry = search_provider_entry(kind);
    let config = provider_config(app, kind);
    let is_searxng = kind == SearchProviderKind::Searxng;
    let is_local_fetch = kind == SearchProviderKind::Fetch;
    // Each capability says for itself whether it needs a key: Jina searches only with one but reads pages without, so the
    // field is marked required only when nothing this provider does works anonymously.
    let capabilities: Vec<_> = [entry.search.as_ref(), entry.fetch.as_ref()].into_iter().flatten().collect();
    let key_required = capabilities.iter().all(|s| s.requires_api_key);
    let search_needs_key_only = !key_required && entry.search.as_ref().is_some_and(|s| s.requires_api_key);
    let key_note = if key_required {
        None
    } else if search_needs_key_only {
        Some(t("仅搜索需要 API Key。", "Only search needs an API key."))
    } else {
        Some(t("API Key 可选，用于提高配额。", "The API key is optional; it raises the quota."))
    };
    let secret_help = t("失去焦点时自动保存到系统凭据库。", "Saved to the system credential store when you leave the field.");
    let wire = entry.slug.to_owned();
    let enabled = config.enabled;
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
        .child(text(entry.label).ty(BODY_SMALL_STRONG).ellipsis().flex1().role(Role::Heading).aria_level(1))
        // The list-row dot is state; this switch is the only control.
        .child(labeled_switch(("search-enabled", entry.slug), enabled, false, move |app| update_provider(app, kind, |p| p.enabled = !enabled)).aria_label(tf("启用搜索提供商 {name}", "Enable search provider {name}", &[("name", &entry.label)])));
    let mut stack = col().w_full().max_w(640.0).mx_auto().gap(16.0);
    if is_local_fetch {
        stack = stack.child(col().gap(6.0).child(text(t("本机抓取", "Local fetch")).ty(CAPTION_STRONG).nowrap()).child(help_with_docs(t("由应用自己读取网页，无需配置。", "The app reads pages itself; nothing to configure."), "web", "local-fetch")));
    }
    if !is_local_fetch && !is_searxng {
        let help_text = match key_note {
            Some(note) => tf("{note}{storage}", "{note} {storage}", &[("note", &note), ("storage", &secret_help)]),
            None => secret_help.to_owned(),
        };
        stack = stack.child(secret_field(app, Identity { provider_kind: wire.clone(), slot: SearchCredentialSlot::ApiKey }, "API Key", help_text, key_required));
    }
    if is_searxng {
        let engines_value = page_state(app).engines_draft.clone().unwrap_or_else(|| config.engines.join(", "));
        let engines = field::<App>(("search-engines", entry.slug), engines_value)
            .bare()
            .placeholder("google, duckduckgo, brave")
            .ty(MONO_BODY)
            .on_change(move |app, v| {
                let parsed = parse_engines(&v);
                page_state(app).engines_draft = Some(v);
                update_provider(app, kind, |p| p.engines = parsed);
            })
            .on_blur(|app, _| page_state(app).engines_draft = None)
            .build()
            .flex1()
            .h(18.0)
            .aria_label(t("搜索引擎", "Search engines"));
        let engines = input_group(engines, false);
        stack = stack.child(titled(t("搜索引擎", "Search engines"), engines, help(t("逗号分隔；留空则用实例已启用的引擎。", "Comma separated; empty uses the instance's enabled engines."))));
        let user = grouped_code_input(("search-basic-user", entry.slug), t("Basic Auth 用户名", "Basic auth username"), config.basic_auth_username.clone(), "", move |app, v| update_provider(app, kind, |p| p.basic_auth_username = v));
        stack = stack.child(titled(t("Basic Auth 用户名", "Basic auth username"), user, help(t("实例有 Basic Auth 时填写。", "For instances behind basic auth."))));
        stack = stack.child(secret_field(app, Identity { provider_kind: wire.clone(), slot: SearchCredentialSlot::BasicAuthPassword }, t("Basic Auth 密码", "Basic auth password"), secret_help.to_owned(), false));
    }
    if let Some(spec) = entry.search.as_ref().filter(|s| !s.default_api_host.is_empty()) {
        let host = grouped_code_input(("search-host", entry.slug), t("搜索端点", "Search endpoint"), config.search_api_host.clone(), spec.default_api_host, move |app, v| update_provider(app, kind, |p| p.search_api_host = v));
        stack = stack.child(titled(t("搜索端点", "Search endpoint"), host, endpoint_rule()));
    }
    if let Some(spec) = entry.fetch.as_ref().filter(|s| !s.default_api_host.is_empty()) {
        let host = grouped_code_input(("fetch-host", entry.slug), t("抓取端点", "Fetch endpoint"), config.fetch_api_host.clone(), spec.default_api_host, move |app, v| update_provider(app, kind, |p| p.fetch_api_host = v));
        stack = stack.child(titled(t("抓取端点", "Fetch endpoint"), host, endpoint_rule()));
    }
    col().flex1().min_w(0.0).min_h(0.0).child(header).child(col().key(("search-pane-body", entry.slug)).flex1().min_h(0.0).scroll_y().pt(14.0).px(18.0).pb(24.0).child(stack))
}

/// The page.
pub fn web_search_settings(app: &mut App) -> E {
    let t_ = theme();
    let selected = page_state(app).selected.unwrap_or(search_providers()[0].kind);
    let mut rows: Vec<E> = Vec::new();
    for entry in search_providers() {
        let kind = entry.kind;
        let active = provider_config(app, kind).enabled;
        rows.push(rail_row(("search-row", entry.slug), entry.label, kind == selected, active, move |app| {
            let page = page_state(app);
            if page.selected != Some(kind) {
                page.selected = Some(kind);
                page.engines_draft = None;
                secret_field::identity_changed(app);
            }
        }));
    }
    let add = button::<App>("")
        .w_full()
        .min_h(30.0)
        .disabled(true)
        .disabled_reason(t("搜索提供商目录随应用发布，暂不支持自定义条目。", "The search-provider catalog ships with the app; custom entries are not supported yet."))
        .child(icon("Plus", 13.0).color(t_.text_secondary))
        .child(text(t("添加提供商", "Add provider")).ty(CAPTION).color(t_.text_secondary).nowrap());
    row().w_full().h_full().child(settings_rail("search-providers", None, None, rows, Some(add))).child(provider_pane(app, selected))
}
