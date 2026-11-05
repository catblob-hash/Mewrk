//! `AdvancedToolsPage.tsx`, `WebSearchBehaviorSettings.tsx`, `SearchProviderField.tsx`, `FetchProviderField.tsx`, `SearchResultShapingFields.tsx`,
//! `SearchDomainFilterRow.tsx` and `SearchDomainRulesWindow.tsx`.
//!
//! One page for every surface that asks these questions, a conversation, a preset in its window and a role, so a row added here lands on all of
//! them. A surface that cannot answer a section leaves it out, and the section is not drawn: that is the only way the three differ.

use std::collections::HashMap;

use mewrk_client::model::{ConversationWebSearchSettings, FetchProviderSelection, HostMessageContainer, NativeFetchTool, NativeSearchTool, SearchDomainFilterMode, SearchProviderKind, SearchProviderSelection, WebSearchAssets};
use mewrk_client::prelude::*;
use mewrk_ui_common::popover_menu::{option_box, option_label, PopoverMenu, PopoverMenuItem, PopoverMenuSection};

use crate::lock::{lock_hint, LockTone};
use crate::web::*;
use crate::widgets::*;

/// A change to the web settings, as a function so it lands on the settings as they are when it is applied.
pub type WebPatch = Rc<dyn Fn(&mut ConversationWebSearchSettings)>;

/// What the conversation says about a web backend selector.
#[derive(Clone, Debug, PartialEq)]
pub enum BackendLock {
    /// Orange. The selector moves through the caller's cache-break warning.
    Cache { note: String },
    /// A native backend a run has already used. A fact about the transcript, so the selector is held and says why.
    Settled { note: String },
}

impl BackendLock {
    fn tone(lock: Option<&Self>) -> Option<LockTone> {
        matches!(lock, Some(Self::Cache { .. })).then_some(LockTone::Cache)
    }

    fn settled_note(lock: Option<&Self>) -> Option<String> {
        match lock {
            Some(Self::Settled { note }) => Some(note.clone()),
            _ => None,
        }
    }
}

/// Said of a native backend a run has already used. Nothing is being withheld: the choice is settled.
pub fn settled_backend_hint() -> &'static str {
    t(
        "本对话已经用原生后端跑过了；它的结果封在只有该提供商的模型才读得回的块里，所以不能中途换人。开一段新对话可以重选。",
        "This conversation has already run on the native backend. Its results are sealed in blocks only that provider's models can read back, so it cannot be swapped part-way through — start a new conversation to choose again.",
    )
}

/// Whose web settings a picker is drawn for, so its standing advice names the right model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WebSubject {
    Conversation,
    Role,
}

pub struct WebSection {
    pub scope: String,
    pub value: ConversationWebSearchSettings,
    pub on_change: Rc<dyn Fn(&mut App, WebPatch)>,
    /// The surface's web access switch and how a flip writes it with the backends in one change. A role's caller decides this, so it has none.
    pub web_access: Option<(bool, Rc<dyn Fn(&mut App, bool, WebLegPatch)>)>,
    pub web_search_assets: WebSearchAssets,
    pub subject: WebSubject,
    pub native_fetch_available: bool,
    pub native_tool_type_selectable: bool,
    pub search_lock: Option<BackendLock>,
    pub fetch_lock: Option<BackendLock>,
    pub search_tone: Option<LockTone>,
    pub fetch_tone: Option<LockTone>,
    /// Whose switches these are, so a leg turned off keeps the backend it had across leaving the page.
    pub remember_key: Option<String>,
    /// What `remembered_legs` has for that key: a leg that is off still names the backend it would come back on.
    pub remembered: RememberedWebLegs,
}

#[derive(Default)]
struct RememberedLegs(HashMap<String, RememberedWebLegs>);

/// The backends the legs under `key` were switched off from.
pub fn remembered_legs(app: &mut App, key: &str) -> RememberedWebLegs {
    app.local::<RememberedLegs>().0.get(key).cloned().unwrap_or_default()
}

impl WebSection {
    fn write_remembered(app: &mut App, key: String, next: RememberedWebLegs) {
        app.local::<RememberedLegs>().0.insert(key, next);
    }

    pub fn view(self) -> E {
        let access = self.web_access.as_ref().map(|(on, _)| *on);
        let search_on = web_leg_on(WebLeg::Search, access, &self.value, self.native_fetch_available);
        let fetch_on = web_leg_on(WebLeg::Fetch, access, &self.value, self.native_fetch_available);
        let role = self.subject == WebSubject::Role;
        // A surface with no key of its own keeps set-aside backends only while the page is open.
        let remember_key = self.remember_key.clone().unwrap_or_else(|| format!("page:{}", self.scope));
        let fetch_version_selectable = self.native_tool_type_selectable && self.native_fetch_available;

        let assets = self.web_search_assets.clone();
        let flip: Rc<dyn Fn(&mut App, WebLeg, bool)> = {
            let (value, native, web_access, on_change, key) = (self.value.clone(), self.native_fetch_available, self.web_access.clone(), self.on_change.clone(), remember_key);
            let fetch_providers: Vec<SearchProviderKind> = search_providers().iter().filter(|p| p.fetch.is_some() && assets.providers.iter().any(|a| a.kind == p.kind && a.enabled)).map(|p| p.kind).collect();
            Rc::new(move |app, leg, on| {
                let remembered = remembered_legs(app, &key);
                let access = web_access.as_ref().map(|(enabled, _)| *enabled);
                let next = switch_web_leg(leg, on, access, &value, native, &fetch_providers, &remembered);
                Self::write_remembered(app, key.clone(), next.remembered);
                match (&web_access, next.web_access) {
                    (Some((_, write)), Some(enabled)) => write(app, enabled, next.patch),
                    _ => {
                        let patch = next.patch;
                        on_change(app, Rc::new(move |w| patch.apply(w)));
                    }
                }
            })
        };

        let mut out = col().gap(SETTING_ROW_GAP).min_w(0.0);
        // A native backend a run has used is settled: its switch is held with the selector beside it, since turning the leg off would move the backend too.
        let search_settled = BackendLock::settled_note(self.search_lock.as_ref());
        let flip_search = flip.clone();
        out = out.child(
            SwitchRow {
                key: format!("{}:web-search", self.scope),
                title: t("网页搜索", "Web search").to_owned(),
                description: if role { t("让这个角色搜索网页。", "Lets this role search the web.") } else { t("让模型用关键词搜索网页。", "Lets the model search the web by keyword.") }.to_owned(),
                docs: role.then_some(("web", Some("enable"))),
                checked: search_on,
                label: if search_on { t("网页搜索已开启", "Web search enabled") } else { t("网页搜索已关闭", "Web search disabled") }.to_owned(),
                tone: self.search_tone.or(BackendLock::tone(self.search_lock.as_ref())),
                disabled: search_settled.is_some(),
                disabled_note: search_settled,
                control: Some(search_backend_menu(&self, search_on)),
                on_change: Rc::new(move |app, on| flip_search(app, WebLeg::Search, on)),
            }
            .view(),
        );
        if search_on {
            out = out.children(search_leg_params(&self));
        }
        let fetch_settled = BackendLock::settled_note(self.fetch_lock.as_ref());
        let flip_fetch = flip;
        out = out.child(
            SwitchRow {
                key: format!("{}:web-fetch", self.scope),
                title: t("网页抓取", "Web fetch").to_owned(),
                description: if role { t("让这个角色读取指定网址的页面。", "Lets this role read pages at given addresses.") } else { t("让模型读取指定网址的页面。", "Lets the model read pages at given addresses.") }.to_owned(),
                docs: Some(("web", Some(if role { "enable" } else { "fetch" }))),
                checked: fetch_on,
                label: if fetch_on { t("网页抓取已开启", "Web fetch enabled") } else { t("网页抓取已关闭", "Web fetch disabled") }.to_owned(),
                tone: self.fetch_tone.or(BackendLock::tone(self.fetch_lock.as_ref())),
                disabled: fetch_settled.is_some(),
                disabled_note: fetch_settled,
                control: Some(fetch_backend_menu(&self, fetch_on, fetch_version_selectable)),
                on_change: Rc::new(move |app, on| flip_fetch(app, WebLeg::Fetch, on)),
            }
            .view(),
        );
        if fetch_on {
            out = out.children(fetch_leg_params(&self, fetch_version_selectable));
        }
        if search_on || fetch_on {
            out = out.child(domain_filter_row(&self.scope, &self.value, self.on_change.clone()));
        }
        out
    }
}

/// The word beside a web leg's switch that names its backend, and the menu that picks another: the reasoning effort's, under the composer.
struct BackendMenu {
    key: String,
    /// What the word is the value of.
    name: &'static str,
    value: String,
    /// The native tool version actually sent, when there is a choice of them.
    version: Option<String>,
    /// What the native choice means, or why there is none: said on its row, or on the word where it has none.
    standing: String,
    error: bool,
    /// Dark and inert: the leg is off, or its backend is settled.
    disabled: bool,
    items: Vec<PopoverMenuItem>,
}

impl BackendMenu {
    fn view(self) -> E {
        let t_ = theme();
        let Self { key, name, value, version, standing, error, disabled, mut items } = self;
        say(&value);
        say(&standing);
        if let Some(version) = &version {
            say(version);
        }
        // A closed menu still says its rows to the tests' reader, as an option list in the DOM would.
        for item in &items {
            say(&format!("option:{}", item.label));
            for child in &item.children {
                say(&format!("option:{}", child.label));
            }
        }
        let shown = version.as_ref().map_or_else(|| value.clone(), |version| format!("{value} · {version}"));
        let native = items.iter_mut().find(|item| item.id == "native");
        let named = tf("{name}：{value}", "{name}: {value}", &[("name", &name), ("value", &shown)]);
        let tip = if native.is_some() { named.clone() } else { standing.clone() };
        if let Some(native) = native {
            native.title = Some(standing);
        }
        PopoverMenu::new(("web-backend-menu", key.as_str()), named, move |open| option_box(open, disabled).child(option_label(value, disabled).when(error && !disabled, |e| e.color(t_.critical))))
            .title(tip)
            .disabled(disabled)
            .menu_label(name)
            .width(232.0)
            .align_end()
            .sections(move || vec![PopoverMenuSection::new("backends").items(items)])
            .build()
    }
}

/// Search-backend picker. It is a menu because the native choice has a second step on Messages models: which version of the server tool to send.
fn search_backend_menu(s: &WebSection, on: bool) -> E {
    let value = resumed_search_backend(&s.value, &s.remembered);
    let enabled_providers: Vec<_> = search_providers().iter().filter(|p| p.search.is_some() && s.web_search_assets.providers.iter().any(|a| a.kind == p.kind && a.enabled)).collect();
    let unavailable = selected_provider_problem(WebBackendChoice::from(&value), &s.web_search_assets, SearchCapability::SearchKeywords).is_some();
    let native_label = t("原生", "Native");
    let is_native = matches!(value, SearchProviderSelection::Native);
    let name = match &value {
        _ if unavailable && !on => t("不启用", "Off").to_owned(),
        _ if unavailable => t("请修复搜索提供商", "Repair search provider").to_owned(),
        SearchProviderSelection::Explicit { provider_kind } => enabled_providers.iter().find(|p| p.kind == *provider_kind).map_or(String::new(), |p| p.label.to_owned()),
        _ => native_label.to_owned(),
    };
    // The version only belongs on the word where it is actually sent: elsewhere saying so would claim an effect this request does not have.
    let selected_version = s.value.native_search_tool;
    let version_shown = (is_native && s.native_tool_type_selectable).then(|| selected_version.wire_type().to_owned());
    let on_change = s.on_change.clone();
    let mut native = PopoverMenuItem::new("native", native_label).checked(is_native).maybe_hint(version_shown.clone()).on({
        let on_change = on_change.clone();
        move |app: &mut App| on_change(app, Rc::new(|w| w.provider = SearchProviderSelection::Native))
    });
    if s.native_tool_type_selectable {
        native = native.children(
            NativeSearchTool::ALL
                .iter()
                .map(|version| {
                    let version = *version;
                    let on_change = on_change.clone();
                    // Checked by the version this conversation carries, not by whether native is the backend right now: it keeps its answer while another backend searches.
                    PopoverMenuItem::new(version.wire_type(), version.wire_type()).checked(selected_version == version).on(move |app: &mut App| {
                        on_change(
                            app,
                            Rc::new(move |w| {
                                w.provider = SearchProviderSelection::Native;
                                w.native_search_tool = version;
                            }),
                        );
                    })
                })
                .collect(),
        );
    }
    let mut items = vec![native];
    for p in enabled_providers {
        let (kind, on_change) = (p.kind, on_change.clone());
        items.push(PopoverMenuItem::new(p.slug, p.label).checked(matches!(value, SearchProviderSelection::Explicit { provider_kind } if provider_kind == p.kind)).on(move |app: &mut App| on_change(app, Rc::new(move |w| w.provider = SearchProviderSelection::Explicit { provider_kind: kind }))));
    }
    let standing = if s.subject == WebSubject::Role { t("原生用这个角色所跑模型自带的搜索。", "Native uses the built-in search of the model this role runs on.") } else { t("原生用模型自带的搜索。", "Native uses the model's own search.") };
    BackendMenu { key: format!("{}:search-provider", s.scope), name: t("搜索提供商", "Search provider"), value: name, version: version_shown, standing: standing.to_owned(), error: unavailable, disabled: !on || matches!(s.search_lock, Some(BackendLock::Settled { .. })), items }.view()
}

/// Fetch-backend picker: the twin of the search one, and its own function because a role picks its fetch backend on the same terms.
fn fetch_backend_menu(s: &WebSection, on: bool, version_selectable: bool) -> E {
    let enabled_providers: Vec<_> = search_providers().iter().filter(|p| p.fetch.is_some() && s.web_search_assets.providers.iter().any(|a| a.kind == p.kind && a.enabled)).collect();
    let fetch_kinds: Vec<SearchProviderKind> = enabled_providers.iter().map(|p| p.kind).collect();
    let value = resumed_fetch_backend(&s.value, &s.remembered, s.native_fetch_available, &fetch_kinds);
    let native_label = t("原生", "Native");
    let is_native = matches!(value, FetchProviderSelection::Native);
    let unavailable = selected_provider_problem(WebBackendChoice::from(&value), &s.web_search_assets, SearchCapability::FetchUrls).is_some();
    let name = match &value {
        _ if unavailable && !on => t("不启用", "Off").to_owned(),
        _ if unavailable => t("请修复抓取提供商", "Repair fetch provider").to_owned(),
        FetchProviderSelection::Explicit { provider_kind } => enabled_providers.iter().find(|p| p.kind == *provider_kind).map_or(String::new(), |p| p.label.to_owned()),
        _ => native_label.to_owned(),
    };
    let selected_version = s.value.native_fetch_tool;
    let version_shown = (is_native && version_selectable).then(|| selected_version.wire_type().to_owned());
    let on_change = s.on_change.clone();
    let mut items = Vec::new();
    // Without a fetch tool of its own there is no native row: a family that reads pages inside its search tool grants no `web_fetch` for native.
    if s.native_fetch_available {
        let mut native = PopoverMenuItem::new("native", native_label).checked(is_native).maybe_hint(version_shown.clone()).on({
            let on_change = on_change.clone();
            move |app: &mut App| on_change(app, Rc::new(|w| w.fetch_provider = FetchProviderSelection::Native))
        });
        if version_selectable {
            native = native.children(
                NativeFetchTool::ALL
                    .iter()
                    .map(|version| {
                        let version = *version;
                        let on_change = on_change.clone();
                        PopoverMenuItem::new(version.wire_type(), version.wire_type()).checked(selected_version == version).on(move |app: &mut App| {
                            on_change(
                                app,
                                Rc::new(move |w| {
                                    w.fetch_provider = FetchProviderSelection::Native;
                                    w.native_fetch_tool = version;
                                }),
                            );
                        })
                    })
                    .collect(),
            );
        }
        items.push(native);
    }
    for p in enabled_providers {
        let (kind, on_change) = (p.kind, on_change.clone());
        items.push(PopoverMenuItem::new(p.slug, p.label).checked(matches!(value, FetchProviderSelection::Explicit { provider_kind } if provider_kind == p.kind)).on(move |app: &mut App| on_change(app, Rc::new(move |w| w.fetch_provider = FetchProviderSelection::Explicit { provider_kind: kind }))));
    }
    let role = s.subject == WebSubject::Role;
    let standing = if !s.native_fetch_available {
        if role { t("这个角色所跑的模型没有自带的抓取，由所选提供商读取。", "This role's model has no fetch of its own; the provider reads pages.") } else { t("这个模型没有自带的抓取，由所选提供商读取。", "This model has no fetch of its own; the provider reads pages.") }
    } else if role {
        t("原生用这个角色所跑模型自带的抓取。", "Native uses the built-in fetch of the model this role runs on.")
    } else {
        t("原生用模型自带的抓取。", "Native uses the model's own fetch.")
    };
    BackendMenu { key: format!("{}:fetch-provider", s.scope), name: t("抓取提供商", "Fetch provider"), value: name, version: version_shown, standing: standing.to_owned(), error: unavailable, disabled: !on || matches!(s.fetch_lock, Some(BackendLock::Settled { .. })), items }.view()
}

/// A whole-number row edited in place. It cannot render the committed number back while being typed: clearing it to type another would put the
/// old value straight back under the cursor. So a keystroke that reads as a number commits, and one that does not leaves the last value alone.
fn number_row(key: String, name: &str, hint: Hint, value: u32, normalize: impl Fn(u32) -> u32 + 'static, on_change: impl Fn(&mut App, u32) + 'static) -> E {
    say(&value.to_string());
    let control = field(("number", key.as_str()), value.to_string())
        .on_change(move |app: &mut App, text| {
            let Ok(next) = text.trim().parse::<u64>() else { return };
            let next = u32::try_from(next).unwrap_or(u32::MAX);
            on_change(app, normalize(next));
        })
        .build()
        .tooltip(name.to_owned())
        .w(72.0);
    setting_row(("number-row", key.as_str()), setting_lead(name.to_owned(), hint), control)
}

/// The search leg's parameters: only the rows the selected backend actually has, because these numbers are that backend's request parameters.
fn search_leg_params(s: &WebSection) -> Vec<E> {
    let SearchProviderSelection::Explicit { provider_kind: kind } = s.value.provider else { return Vec::new() };
    let Some(spec) = search_provider_capability(kind, SearchCapability::SearchKeywords) else { return Vec::new() };
    let mut rows: Vec<E> = Vec::new();
    if let Some(ceiling) = spec.max_results {
        let hint = if kind == SearchProviderKind::Searxng {
            tf("读取的结果页数，最多 {max}；0 为全部。", "Result pages to read, up to {max}; 0 reads all.", &[("max", &ceiling)])
        } else {
            tf("最多 {max}；0 使用后端默认值。", "Up to {max}; 0 uses the backend's default.", &[("max", &ceiling)])
        };
        let on_change = s.on_change.clone();
        // The stored number is the user's answer and the ceiling the backend's: a stored 30 on a backend that takes 20 is shown as 20 without rewriting the 30.
        rows.push(number_row(format!("{}:max-results", s.scope), t("搜索结果数", "Search result count"), Hint::new(hint), s.value.max_results.min(ceiling), move |n| n.min(ceiling), move |app, n| on_change(app, Rc::new(move |w| w.max_results = n))));
    }
    if let Some(floor) = spec.min_content_tokens {
        let on_change = s.on_change.clone();
        let value = s.value.compression_cutoff;
        let hint = Hint::new(t("每条结果最多保留多少 token；0 不设限。", "Most tokens kept per result; 0 means no limit.")).docs("web", "results");
        rows.push(compression_row(format!("{}:search-compression", s.scope), t("搜索结果压缩", "Search result compression"), value, floor, hint, move |app, n| on_change(app, Rc::new(move |w| w.compression_cutoff = n))));
    }
    rows
}

/// How many tokens of one result's or page's text are kept. 0 is "no cap" and is never raised to the floor.
fn compression_row(key: String, name: &str, value: u32, floor: u32, hint: Hint, on_change: impl Fn(&mut App, u32) + 'static) -> E {
    let shown = if value == 0 { 0 } else { value.max(floor) };
    number_row(key, name, hint, shown, move |n| if n == 0 { 0 } else { n.max(floor).min(SEARCH_COMPRESSION_CUTOFF_CEILING) }, on_change)
}

/// The fetch leg's one parameter. A native fetch has it only where it exists as a separate server tool. A role riding the caller's model meets
/// whichever family calls it, so its hint says only an Anthropic-family model sends it.
fn fetch_leg_params(s: &WebSection, native_offered: bool) -> Option<E> {
    let role = s.subject == WebSubject::Role;
    let (floor, hint) = match s.value.fetch_provider {
        FetchProviderSelection::Native if native_offered => (1, if role { Hint::new(t("每页上限，仅 Anthropic 家族模型会发送。", "Per-page cap, sent only on Anthropic-family models.")) } else { Hint::new(t("每个页面最多保留多少 token；0 不设限。", "Most tokens kept per page; 0 means no limit.")) }),
        FetchProviderSelection::Explicit { provider_kind } => (search_provider_capability(provider_kind, SearchCapability::FetchUrls)?.min_content_tokens?, Hint::new(t("每个页面最多保留多少 token；0 不设限。", "Most tokens kept per page; 0 means no limit.")).docs("web", "results")),
        _ => return None,
    };
    let on_change = s.on_change.clone();
    Some(compression_row(format!("{}:fetch-compression", s.scope), t("抓取结果压缩", "Fetch result compression"), s.value.fetch_compression_cutoff, floor, hint, move |app, n| on_change(app, Rc::new(move |w| w.fetch_compression_cutoff = n))))
}

/// The domain-filter row: the way in to writing both lists and how long each is, then which of them is in effect. Shared by all surfaces so a role filters by its own lists.
fn domain_filter_row(scope: &str, value: &ConversationWebSearchSettings, on_change: Rc<dyn Fn(&mut App, WebPatch)>) -> E {
    let window = use_state(("domain-window", scope), || false);
    let mode = value.domain_filter;
    let open = window.clone();
    let counts = tf("过滤 {blocked} / 允许 {allowed}", "Block {blocked} / allow {allowed}", &[("blocked", &value.exclude_domains.len()), ("allowed", &value.include_domains.len())]);
    let options = vec![
        Segment { id: "exclude", value: SearchDomainFilterMode::Exclude, label: t("黑名单", "Blocklist"), tip: t("丢掉命中黑名单的结果。", "Drops results that match the blocklist.").into(), disabled: false },
        Segment { id: "include", value: SearchDomainFilterMode::Include, label: t("白名单", "Allowlist"), tip: t("只保留命中白名单的结果。", "Keeps only results that match the allowlist.").into(), disabled: false },
        Segment { id: "off", value: SearchDomainFilterMode::Off, label: t("不启用", "Off"), tip: t("不按域名筛选。", "Does not filter by domain.").into(), disabled: false },
    ];
    let select = on_change.clone();
    let control = row()
        .wrap()
        .items_center()
        .justify_end()
        .gap(6.0)
        .child(btn(("domain-edit", scope), Look::Subtle, t("编辑名单", "Edit lists")).on_click(move |_| open.set(true)))
        .child(segmented(("domain-filter", scope), options, mode, None, Rc::new(move |app, m| select(app, Rc::new(move |w| w.domain_filter = m)))).aria_label(t("域名过滤", "Domain filter")));
    let lead = setting_lead(t("域名过滤", "Domain filter"), Hint::new(t("按域名筛选检索结果。", "Filters results by domain.")).docs("web", "domains")).child(small(counts).key(("domain-counts", scope)));
    let mut out = col().min_w(0.0).child(setting_row(("domain-row", scope), lead, control));
    if window.get() {
        // The window opens on the list the selector has in effect; with filtering off, on the blocklist a person reaching for this row almost always means.
        let initial = if mode == SearchDomainFilterMode::Include { DomainList::Include } else { DomainList::Exclude };
        let close = window.clone();
        out = out.child(domain_rules_window(scope, value, initial, on_change, Rc::new(move |_| close.set(false))));
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DomainList {
    Exclude,
    Include,
}

/// One rule per line, in the order they were written. Nothing is sorted or de-duplicated: a list that rearranges itself under an edit is one a user
/// cannot keep. Both lists are always editable here, so the one that is off can be written before it is switched on.
pub fn parse_domain_rules(text: &str) -> Vec<String> {
    text.split('\n').map(str::trim).filter(|r| !r.is_empty()).map(str::to_owned).collect()
}

fn domain_rules_window(scope: &str, value: &ConversationWebSearchSettings, initial: DomainList, on_change: Rc<dyn Fn(&mut App, WebPatch)>, on_close: Rc<dyn Fn(&mut App)>) -> E {
    let t_ = theme();
    let page = use_state(("domain-rules-page", scope), || initial);
    let list = page.get();
    let names = |l: DomainList| if l == DomainList::Exclude { t("黑名单", "Blocklist") } else { t("白名单", "Allowlist") };
    let nav_items = vec![
        NavItem { id: DomainList::Exclude, icon: "Ban", label: names(DomainList::Exclude).to_owned(), count: Some(value.exclude_domains.len()) },
        NavItem { id: DomainList::Include, icon: "ShieldCheck", label: names(DomainList::Include).to_owned(), count: Some(value.include_domains.len()) },
    ];
    let page_set = page.clone();
    let nav = settings_nav(&format!("{scope}:domain-rules"), t("域名名单", "Domain lists"), nav_items, list, NavStyle::Domains, Some(t("域名名单", "Domain lists").to_owned()), Rc::new(move |_, v| page_set.set(v)), None);
    let rules = if list == DomainList::Include { &value.include_domains } else { &value.exclude_domains };
    let help = if list == DomainList::Exclude { t("命中的结果会被丢掉，一行一条规则。", "Matching results are dropped; one rule per line.") } else { t("只保留命中的结果，空名单什么都不留。", "Only matching results are kept; an empty list keeps nothing.") };
    // The field keeps exactly what was typed while focused, so a blank line typed between two rules is not swallowed the moment it appears.
    let editor = field(("domain-rules", scope, list), rules.join("\n"))
        .placeholder("*://ads.example/*\n*://*.example.com/*\n/\\/(login|signup)$/")
        .ty(MONO_BODY)
        .multiline(300.0, 620.0)
        .on_change(move |app: &mut App, text| {
            let parsed = parse_domain_rules(&text);
            on_change(
                app,
                Rc::new(move |w| match list {
                    DomainList::Exclude => w.exclude_domains = parsed.clone(),
                    DomainList::Include => w.include_domains = parsed.clone(),
                }),
            );
        })
        .build()
        .tooltip(names(list));
    let body = col().flex1().min_w(0.0).min_h(0.0).gap(9.0).p(14.0).child(row().min_h(20.0).pr(38.0).items_center().child(strong(names(list)).ty(BODY_STRONG))).child(Hint::new(help).docs("web", "domains").view()).child(editor).bg(t_.flyout);
    settings_window(("domain-rules-window", scope), t("域名名单", "Domain lists"), 860.0, true, false, on_close, nav, body)
}

/// A switch this page draws: its value, tone and writer.
pub struct Flag {
    pub enabled: bool,
    pub tone: Option<LockTone>,
    pub on_change: Rc<dyn Fn(&mut App, bool)>,
}

pub struct AdvancedPage {
    pub scope: String,
    pub web: WebSection,
    /// (global, project). Omitted where memory is not a switch of this surface.
    pub memory: Option<(Flag, Flag)>,
    pub file_write_guards: Option<Flag>,
    pub subagent_messaging: Option<Flag>,
    pub environment_prompt: Option<Flag>,
    pub host_messages: Option<(HostMessageContainer, Option<LockTone>, Rc<dyn Fn(&mut App, HostMessageContainer)>)>,
}

fn flag_row(scope: &str, id: &str, title: &str, description: &str, docs: (&'static str, &'static str), on_label: &str, off_label: &str, flag: Flag) -> E {
    SwitchRow {
        key: format!("{scope}:{id}"),
        title: title.to_owned(),
        description: description.to_owned(),
        docs: Some((docs.0, Some(docs.1))),
        checked: flag.enabled,
        label: if flag.enabled { on_label } else { off_label }.to_owned(),
        tone: flag.tone,
        disabled: false,
        disabled_note: None,
        control: None,
        on_change: flag.on_change,
    }
    .view()
}

impl AdvancedPage {
    pub fn view(self) -> E {
        let scope = self.scope;
        let mut out = col().gap(SETTING_ROW_GAP).min_w(0.0).child(self.web.view());
        if let Some((global, project)) = self.memory {
            out = out
                .child(flag_row(&scope, "global-memory", t("启用全局记忆", "Enable global memory"), t("接入全局记忆及其工具。", "Brings in global memory and its tools."), ("memory", "memory-switches"), t("全局记忆已开启", "Global memory enabled"), t("全局记忆已关闭", "Global memory disabled"), global))
                .child(flag_row(&scope, "project-memory", t("启用项目记忆", "Enable project memory"), t("接入项目记忆及其工具。", "Brings in project memory and its tools."), ("memory", "memory-switches"), t("项目记忆已开启", "Project memory enabled"), t("项目记忆已关闭", "Project memory disabled"), project));
        }
        if let Some(flag) = self.file_write_guards {
            out = out.child(flag_row(&scope, "file-write-guards", t("启用文件防误写保护", "Enable file write guards"), t("写入前须先读过，并拦下过期写入。", "Requires a read before a write and refuses stale writes."), ("working", "advanced-tools"), t("文件防误写保护已开启", "File write guards enabled"), t("文件防误写保护已关闭", "File write guards disabled"), flag));
        }
        if let Some(flag) = self.subagent_messaging {
            out = out.child(flag_row(&scope, "subagent-messaging", t("子代理通信", "Subagent communication"), t("让主代理与子代理互相发消息。", "Lets the main agent and subagents message each other."), ("agents", "subagents"), t("子代理通信已开启", "Subagent communication enabled"), t("子代理通信已关闭", "Subagent communication disabled"), flag));
        }
        if let Some(flag) = self.environment_prompt {
            out = out.child(flag_row(&scope, "environment-prompt", t("拼接环境信息", "Include environment info"), t("提示词开头附上工作目录、系统与日期。", "Adds the working directory, OS and date to the prompt."), ("memory", "injected"), t("环境信息已拼接", "Environment info included"), t("环境信息未拼接", "Environment info left out"), flag));
        }
        if let Some((value, tone, on_change)) = self.host_messages {
            out = out.child(host_message_row(&scope, value, tone, on_change));
        }
        out
    }
}

/// The host-message container: one of two, so a pair of buttons rather than a switch: neither is the "off" of the other. What the chosen one costs
/// is said under the row, and the other's on its button.
fn host_message_row(scope: &str, value: HostMessageContainer, tone: Option<LockTone>, on_change: Rc<dyn Fn(&mut App, HostMessageContainer)>) -> E {
    let cost = |container: HostMessageContainer| match container {
        HostMessageContainer::User => t("Claude Code 的做法；可能被当成你说的话。", "Claude Code's way; may be mistaken for something you said."),
        HostMessageContainer::Box => t("作为 box 调用的结果；模型可能当成自己调的。", "Arrives as a box call result; the model may think it called it."),
    };
    let options = vec![
        Segment { id: "user", value: HostMessageContainer::User, label: "user", tip: cost(HostMessageContainer::User).into(), disabled: false },
        Segment { id: "box", value: HostMessageContainer::Box, label: "box", tip: cost(HostMessageContainer::Box).into(), disabled: false },
    ];
    let lead = col()
        .gap(1.0)
        .child(strong(t("宿主消息容器", "Host message container")))
        .child(row().wrap().items_center().gap(4.0).child(small(t("Mewrk 交给模型的消息用什么送达。", "What Mewrk's messages to the model arrive in."))).child(docs_inline("context", Some("host-messages"))))
        .child(small(cost(value)));
    let control = segmented(("host-message", scope), options, value, tone, on_change).aria_label(t("宿主消息容器", "Host message container"));
    let control = match tone {
        Some(tone) => control.tip(lock_hint(tone)),
        None => control,
    };
    setting_row(("host-message-row", scope), lead, control)
}
