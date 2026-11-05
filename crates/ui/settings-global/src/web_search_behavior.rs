//! `WebSearchBehaviorSettings.tsx` and `SearchResultShapingFields.tsx`: search and fetch backends for conversations,
//! presets and subagent roles, with the numbers each backend actually has.
//!
//! The pickers, the lock tones and the domain filter belong to conversation settings (`crates/ui/settings-conv`), so they
//! come in as slots. The parts here are what the two legs share: their switches, and which rows a backend draws.

use std::collections::HashMap;

use mewrk_client::model::{ConversationWebSearchSettings, FetchProviderSelection, SearchCapability, SearchProviderSelection, WebSearchAssets};
use mewrk_client::prelude::*;

use crate::logic::search_providers::{search_provider_capability, search_providers, SEARCH_COMPRESSION_CUTOFF_CEILING};
use crate::logic::web_search::{switch_web_leg, web_leg_on, RememberedWebLegs, WebLeg, WebLegPatch};
use crate::widgets::*;

/// One number in the shape the page's row needs: what shows, what is the ceiling, what a keystroke commits.
#[derive(Clone, Debug, PartialEq)]
pub struct NumberRow {
    pub shown: u32,
    pub max: u32,
    pub floor: Option<u32>,
    pub ceiling: Option<u32>,
    pub hint: String,
    pub docs: bool,
}

/// What a keystroke commits for a count: the stored number is the user's answer and the ceiling is the backend's.
pub fn normalize_count(next: i64, ceiling: u32) -> u32 {
    next.clamp(0, ceiling as i64) as u32
}

/// What a keystroke commits for a cap: 0 is "no cap" and is never raised to the floor.
pub fn normalize_cap(next: i64, floor: u32) -> u32 {
    if next <= 0 { 0 } else { next.clamp(floor as i64, SEARCH_COMPRESSION_CUTOFF_CEILING as i64) as u32 }
}

/// The count row for a selection, or `None` for a backend with no count field: no knob that does nothing.
pub fn search_count_row(selection: &SearchProviderSelection, max_results: u32) -> Option<NumberRow> {
    let SearchProviderSelection::Explicit { provider_kind } = selection else { return None };
    let ceiling = search_provider_capability(*provider_kind, SearchCapability::SearchKeywords)?.max_results?;
    let hint = if *provider_kind == mewrk_client::model::SearchProviderKind::Searxng {
        tf("读取的结果页数，最多 {max}；0 为全部。", "Result pages to read, up to {max}; 0 reads all.", &[("max", &ceiling)])
    } else {
        tf("最多 {max}；0 使用后端默认值。", "Up to {max}; 0 uses the backend's default.", &[("max", &ceiling)])
    };
    Some(NumberRow { shown: max_results.min(ceiling), max: ceiling, floor: None, ceiling: Some(ceiling), hint, docs: false })
}

fn cap_shown(value: u32, floor: u32) -> u32 {
    if value == 0 { 0 } else { value.max(floor) }
}

pub fn search_compression_row(selection: &SearchProviderSelection, cutoff: u32) -> Option<NumberRow> {
    let SearchProviderSelection::Explicit { provider_kind } = selection else { return None };
    let floor = search_provider_capability(*provider_kind, SearchCapability::SearchKeywords)?.min_content_tokens?;
    Some(NumberRow { shown: cap_shown(cutoff, floor), max: SEARCH_COMPRESSION_CUTOFF_CEILING, floor: Some(floor), ceiling: None, hint: t("每条结果最多保留多少 token；0 不设限。", "Most tokens kept per result; 0 means no limit.").to_owned(), docs: true })
}

/// A fetch never has a count. A native fetch has a cap only where it exists as a separate server tool.
pub fn fetch_compression_row(selection: &FetchProviderSelection, native_offered: bool, for_role: bool, cutoff: u32) -> Option<NumberRow> {
    let per_page = t("每个页面最多保留多少 token；0 不设限。", "Most tokens kept per page; 0 means no limit.");
    let (floor, hint, docs) = match selection {
        FetchProviderSelection::Native if native_offered => (1, if for_role { t("每页上限，仅 Anthropic 家族模型会发送。", "Per-page cap, sent only on Anthropic-family models.") } else { per_page }, false),
        FetchProviderSelection::Explicit { provider_kind } => (search_provider_capability(*provider_kind, SearchCapability::FetchUrls)?.min_content_tokens?, per_page, true),
        _ => return None,
    };
    Some(NumberRow { shown: cap_shown(cutoff, floor), max: SEARCH_COMPRESSION_CUTOFF_CEILING, floor: Some(floor), ceiling: None, hint: hint.to_owned(), docs })
}

/// A whole-number row edited in place. It cannot render the committed number back while being typed, or clearing it to
/// type another would put the old value under the caret: the draft holds what was typed until the field is left.
fn number_field(key: (&'static str, u64), label: &str, row_data: NumberRow, on_commit: impl Fn(&mut App, u32) + 'static) -> E {
    let draft = use_state(("number-draft", key.0, key.1), || None::<String>);
    let value = draft.get().unwrap_or_else(|| row_data.shown.to_string());
    let change_draft = draft.clone();
    let (floor, ceiling) = (row_data.floor, row_data.ceiling);
    let input_el = field::<App>(key, value)
        .on_change(move |app, v| {
            change_draft.set(Some(v.clone()));
            // A keystroke that doesn't read as a number, an empty field mid-edit, leaves the committed value alone.
            let Ok(next) = v.trim().parse::<i64>() else { return };
            let committed = match (floor, ceiling) {
                (Some(floor), _) => normalize_cap(next, floor),
                (None, Some(ceiling)) => normalize_count(next, ceiling),
                (None, None) => next.max(0) as u32,
            };
            on_commit(app, committed);
        })
        .on_blur(move |_, _| draft.set(None))
        .build()
        .aria_label(label);
    let hint = if row_data.docs { help_with_docs(row_data.hint, "web", "results") } else { help(row_data.hint) };
    field_row(label.to_owned(), Some(hint), input_el)
}

pub type Patch = Rc<dyn Fn(&mut App, Box<dyn FnOnce(&mut ConversationWebSearchSettings)>)>;

/// `SearchLegParams`: drawn only with the rows the selected backend has. Empty for a native search on any family.
pub fn search_leg_params(scope: u64, selection: &SearchProviderSelection, max_results: u32, cutoff: u32, patch: Patch) -> Option<E> {
    let count = search_count_row(selection, max_results);
    let compression = search_compression_row(selection, cutoff);
    if count.is_none() && compression.is_none() {
        return None;
    }
    let count_patch = patch.clone();
    Some(col().pl(12.0).child(count.map(|r| number_field(("search-count", scope), t("结果数", "Result count"), r, move |app, v| count_patch(app, Box::new(move |s| s.max_results = v))))).child(compression.map(|r| number_field(("search-compression", scope), t("结果压缩", "Result compression"), r, move |app, v| patch(app, Box::new(move |s| s.compression_cutoff = v))))))
}

/// `FetchLegParams`.
pub fn fetch_leg_params(scope: u64, selection: &FetchProviderSelection, native_offered: bool, for_role: bool, cutoff: u32, patch: Patch) -> Option<E> {
    let row_data = fetch_compression_row(selection, native_offered, for_role, cutoff)?;
    Some(col().pl(12.0).child(number_field(("fetch-compression", scope), t("结果压缩", "Result compression"), row_data, move |app, v| patch(app, Box::new(move |s| s.fetch_compression_cutoff = v)))))
}

#[derive(Default)]
struct RememberedLegs(HashMap<String, RememberedWebLegs>);

/// What the page needs handed in.
pub struct WebBehaviorProps {
    pub remember_key: String,
    pub value: ConversationWebSearchSettings,
    pub assets: WebSearchAssets,
    pub native_fetch_available: bool,
    pub native_tool_type_selectable: bool,
    pub for_role: bool,
    /// The surface's web access switch; `None` where that decision is someone else's.
    pub web_access: Option<bool>,
    pub patch: Patch,
    /// Writes the access switch together with the backends, in one change.
    pub on_web_access: Option<Rc<dyn Fn(&mut App, bool, WebLegPatch)>>,
    /// The pickers and the domain row, from conversation settings.
    pub search_field: E,
    pub fetch_field: E,
    pub domain_row: E,
}

/// The legs' switches over their pickers and numbers.
pub fn web_search_behavior(app: &mut App, props: WebBehaviorProps) -> E {
    let t_ = theme();
    let WebBehaviorProps { remember_key, value, assets, native_fetch_available, native_tool_type_selectable, for_role, web_access, patch, on_web_access, search_field, fetch_field, domain_row } = props;
    let search_on = web_leg_on(WebLeg::Search, web_access, &value, native_fetch_available);
    let fetch_on = web_leg_on(WebLeg::Fetch, web_access, &value, native_fetch_available);
    let scope = hash_key(&remember_key);
    let flip = {
        let (value, assets, patch, on_web_access, remember_key) = (value.clone(), assets.clone(), patch.clone(), on_web_access.clone(), remember_key.clone());
        Rc::new(move |app: &mut App, leg: WebLeg, on: bool| {
            let fetch_providers: Vec<_> = search_providers().iter().filter(|p| p.fetch.is_some() && assets.providers.iter().any(|a| a.kind == p.kind && a.enabled)).map(|p| p.kind).collect();
            let remembered = app.local::<RememberedLegs>().0.get(&remember_key).cloned().unwrap_or_default();
            let next = switch_web_leg(leg, on, web_access, &value, native_fetch_available, &fetch_providers, &remembered);
            app.local::<RememberedLegs>().0.insert(remember_key.clone(), next.remembered);
            match (&on_web_access, next.web_access) {
                (Some(f), Some(access)) => f(app, access, next.patch),
                _ => {
                    let p = next.patch;
                    patch(app, Box::new(move |s| p.apply(s)));
                }
            }
        })
    };
    let leg_row = |title: &str, label: &str, description: E, on: bool, leg: WebLeg, flip: Rc<dyn Fn(&mut App, WebLeg, bool)>| row().min_h(44.0).py(8.0).gap(12.0).items_center().justify_between().border_top(t_.divider).child(col().flex1().min_w(0.0).gap(2.0).child(text(title.to_owned()).ty(BODY_SMALL)).child(description)).child(labeled_switch(("web-leg", scope, leg == WebLeg::Search), on, false, move |app| flip(app, leg, !on)).aria_label(label));
    let search_description = if for_role { help_with_docs(t("让这个角色搜索网页。", "Lets this role search the web."), "web", "enable") } else { help(t("让模型用关键词搜索网页。", "Lets the model search the web by keyword.")) };
    let fetch_description = help_with_docs(if for_role { t("让这个角色读取指定网址的页面。", "Lets this role read pages at given addresses.") } else { t("让模型读取指定网址的页面。", "Lets the model read pages at given addresses.") }, "web", if for_role { "enable" } else { "fetch" });
    // The fetch version is a Messages spelling, so it is offered only where it is sent.
    let fetch_version_selectable = native_tool_type_selectable && native_fetch_available;
    let _ = app;
    col()
        .child(leg_row(t("网页搜索", "Web search"), if search_on { t("网页搜索已开启", "Web search enabled") } else { t("网页搜索已关闭", "Web search disabled") }, search_description, search_on, WebLeg::Search, flip.clone()))
        .child(search_on.then(|| col().gap(8.0).pb(8.0).child(search_field).child(search_leg_params(scope, &value.provider, value.max_results, value.compression_cutoff, patch.clone()))))
        .child(leg_row(t("网页抓取", "Web fetch"), if fetch_on { t("网页抓取已开启", "Web fetch enabled") } else { t("网页抓取已关闭", "Web fetch disabled") }, fetch_description, fetch_on, WebLeg::Fetch, flip))
        .child(fetch_on.then(|| col().gap(8.0).pb(8.0).child(fetch_field).child(fetch_leg_params(scope, &value.fetch_provider, fetch_version_selectable, for_role, value.fetch_compression_cutoff, patch))))
        .child((search_on || fetch_on).then(|| col().pb(8.0).child(domain_row)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mewrk_client::model::SearchProviderKind::*;

    fn explicit(kind: mewrk_client::model::SearchProviderKind) -> SearchProviderSelection {
        SearchProviderSelection::Explicit { provider_kind: kind }
    }

    // WebSearchBehaviorSettings.test.tsx
    #[test]
    fn a_native_search_draws_no_numbers() {
        assert!(search_count_row(&SearchProviderSelection::Native, 5).is_none());
        assert!(search_compression_row(&SearchProviderSelection::Native, 2000).is_none());
    }

    #[test]
    fn a_stored_number_shows_through_the_backends_ceiling_without_being_rewritten() {
        let r = search_count_row(&explicit(Tavily), 30).unwrap();
        assert_eq!((r.shown, r.max), (20, 20));
        assert_eq!(normalize_count(-3, 20), 0);
        assert_eq!(normalize_count(999, 20), 20);
    }

    #[test]
    fn each_backend_draws_only_the_rows_it_has() {
        assert!(search_compression_row(&explicit(Tavily), 2000).is_none());
        assert!(search_compression_row(&explicit(Exa), 2000).is_some());
        assert!(search_count_row(&explicit(Exa), 5).is_some());
        assert!(fetch_compression_row(&FetchProviderSelection::Explicit { provider_kind: Firecrawl }, false, false, 100).is_none());
        assert!(fetch_compression_row(&FetchProviderSelection::Explicit { provider_kind: Fetch }, false, false, 100).is_some());
    }

    #[test]
    fn a_zero_cap_is_no_cap_and_a_floor_only_raises_the_rest() {
        let r = search_compression_row(&explicit(Jina), 100).unwrap();
        assert_eq!(r.shown, 500);
        assert_eq!(search_compression_row(&explicit(Jina), 0).unwrap().shown, 0);
        assert_eq!(normalize_cap(0, 500), 0);
        assert_eq!(normalize_cap(-1, 500), 0);
        assert_eq!(normalize_cap(100, 500), 500);
        assert_eq!(normalize_cap(i64::MAX, 500), SEARCH_COMPRESSION_CUTOFF_CEILING);
    }

    #[test]
    fn a_native_fetch_has_a_cap_only_where_it_is_a_tool_of_its_own() {
        assert!(fetch_compression_row(&FetchProviderSelection::Native, false, false, 100).is_none());
        let r = fetch_compression_row(&FetchProviderSelection::Native, true, false, 0).unwrap();
        assert_eq!((r.shown, r.floor), (0, Some(1)));
        let _language = crate::language_for_test(true);
        let role = fetch_compression_row(&FetchProviderSelection::Native, true, true, 5).unwrap();
        assert_eq!(role.hint, "Per-page cap, sent only on Anthropic-family models.");
    }
}
