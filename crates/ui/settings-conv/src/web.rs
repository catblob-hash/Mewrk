//! The web-search rules the settings pages need: upstream's `lib/webSearch.ts` and `lib/searchProviders.ts`.
//!
//! TODO(mewrk_client): the client has no web-search module to publish these from yet.

use mewrk_client::model::{
    ConversationWebSearchSettings, FetchProviderSelection, ProviderFamily, SearchCapabilitySpec, SearchProviderCatalogEntry, SearchProviderKind, SearchProviderSelection, WebSearchAssets, SEARCH_PROVIDER_CATALOG,
};

pub use mewrk_client::tool_lock::{family_supports_native_fetch, WebBackendChoice, WebBackendLeg as WebLeg};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchCapability {
    SearchKeywords,
    FetchUrls,
}

/// The ceiling the compression field clamps to. Mirrors `model.rs`.
pub const SEARCH_COMPRESSION_CUTOFF_CEILING: u32 = 200_000;
pub const DEFAULT_SEARCH_MAX_RESULTS: u32 = 5;
pub const DEFAULT_SEARCH_COMPRESSION_CUTOFF: u32 = 2000;

pub fn search_providers() -> &'static [SearchProviderCatalogEntry] {
    SEARCH_PROVIDER_CATALOG
}

pub fn search_provider_capability(kind: SearchProviderKind, capability: SearchCapability) -> Option<&'static SearchCapabilitySpec> {
    let entry = SEARCH_PROVIDER_CATALOG.iter().find(|e| e.kind == kind)?;
    match capability {
        SearchCapability::SearchKeywords => entry.search.as_ref(),
        SearchCapability::FetchUrls => entry.fetch.as_ref(),
    }
}

/// Whether this family spells its native web tools the Messages way, with the version written into the tool's `type`.
pub fn family_selects_native_tool_type(family: Option<ProviderFamily>) -> bool {
    matches!(family, Some(ProviderFamily::Anthropic | ProviderFamily::Bedrock))
}

/// Whether a fetch selection is one this model can serve as drawn: only native can fail it.
pub fn fetch_selection_offered(selection: &FetchProviderSelection, native_fetch_offered: bool) -> bool {
    !matches!(selection, FetchProviderSelection::Disabled) && (!matches!(selection, FetchProviderSelection::Native) || native_fetch_offered)
}

/// Whether a web leg's switch is drawn on. Native fetch on a model without one reads as off without being rewritten:
/// the selection stays native, so moving back to a model that has it finds the leg on again.
pub fn web_leg_on(leg: WebLeg, web_access: Option<bool>, settings: &ConversationWebSearchSettings, native_fetch_offered: bool) -> bool {
    if web_access == Some(false) {
        return false;
    }
    match leg {
        WebLeg::Search => !matches!(settings.provider, SearchProviderSelection::Disabled),
        WebLeg::Fetch => fetch_selection_offered(&settings.fetch_provider, native_fetch_offered),
    }
}

/// The backends a leg switched off was carrying, for switching it back on.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RememberedWebLegs {
    pub search: Option<SearchProviderSelection>,
    pub fetch: Option<FetchProviderSelection>,
}

/// The backend the search leg has, or comes back on when switched on. Its own selection first: while that names a backend, it
/// is the one the leg was left on, and anything remembered is older.
pub fn resumed_search_backend(settings: &ConversationWebSearchSettings, remembered: &RememberedWebLegs) -> SearchProviderSelection {
    [Some(settings.provider.clone()), remembered.search.clone()].into_iter().flatten().find(|s| !matches!(s, SearchProviderSelection::Disabled)).unwrap_or(SearchProviderSelection::Native)
}

/// The same for the fetch leg, which can also have a backend this model cannot serve.
pub fn resumed_fetch_backend(settings: &ConversationWebSearchSettings, remembered: &RememberedWebLegs, native_fetch_offered: bool, fetch_providers: &[SearchProviderKind]) -> FetchProviderSelection {
    [Some(settings.fetch_provider.clone()), remembered.fetch.clone()].into_iter().flatten().find(|s| fetch_selection_offered(s, native_fetch_offered)).unwrap_or_else(|| {
        if native_fetch_offered {
            FetchProviderSelection::Native
        } else {
            FetchProviderSelection::Explicit { provider_kind: fetch_providers.first().copied().unwrap_or(SearchProviderKind::Fetch) }
        }
    })
}

/// What flipping one leg's switch writes. `provider` and `fetch_provider` are `None` where the flip leaves them alone.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WebLegPatch {
    pub provider: Option<SearchProviderSelection>,
    pub fetch_provider: Option<FetchProviderSelection>,
}

impl WebLegPatch {
    pub fn apply(&self, web: &mut ConversationWebSearchSettings) {
        if let Some(p) = &self.provider {
            web.provider = p.clone();
        }
        if let Some(p) = &self.fetch_provider {
            web.fetch_provider = p.clone();
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct WebLegSwitch {
    /// The web access switch after the flip; `None` where the surface has none.
    pub web_access: Option<bool>,
    pub patch: WebLegPatch,
    pub remembered: RememberedWebLegs,
}

/// Flips one web leg's switch.
///
/// Off is the leg's backend reading `disabled`, so the backend it had is kept in `remembered` and comes back when the
/// switch does. Web access is the union of the two legs: it goes off with the last leg that names a backend, leaving that
/// backend in place, and on with the first one switched on, which also stops the other leg's backend from quietly coming
/// back with it. A leg drawn off only because this model lacks native fetch still names a backend, so switching the search
/// leg off keeps access on for it.
pub fn switch_web_leg(leg: WebLeg, on: bool, web_access: Option<bool>, settings: &ConversationWebSearchSettings, native_fetch_offered: bool, fetch_providers: &[SearchProviderKind], remembered: &RememberedWebLegs) -> WebLegSwitch {
    let mut remembered = remembered.clone();
    let mut patch = WebLegPatch::default();
    let other_named = match leg {
        WebLeg::Search => !matches!(settings.fetch_provider, FetchProviderSelection::Disabled),
        WebLeg::Fetch => !matches!(settings.provider, SearchProviderSelection::Disabled),
    };
    let set_aside = |which: WebLeg, remembered: &mut RememberedWebLegs, patch: &mut WebLegPatch| match which {
        WebLeg::Search => {
            remembered.search = Some(settings.provider.clone());
            patch.provider = Some(SearchProviderSelection::Disabled);
        }
        WebLeg::Fetch => {
            remembered.fetch = Some(settings.fetch_provider.clone());
            patch.fetch_provider = Some(FetchProviderSelection::Disabled);
        }
    };
    if !on {
        if web_access.is_some() && !other_named {
            return WebLegSwitch { web_access: Some(false), patch, remembered };
        }
        set_aside(leg, &mut remembered, &mut patch);
        return WebLegSwitch { web_access, patch, remembered };
    }
    match leg {
        WebLeg::Search => {
            patch.provider = Some(resumed_search_backend(settings, &remembered));
            remembered.search = None;
        }
        WebLeg::Fetch => {
            patch.fetch_provider = Some(resumed_fetch_backend(settings, &remembered, native_fetch_offered, fetch_providers));
            remembered.fetch = None;
        }
    }
    if web_access == Some(false) && other_named {
        set_aside(if leg == WebLeg::Search { WebLeg::Fetch } else { WebLeg::Search }, &mut remembered, &mut patch);
    }
    WebLegSwitch { web_access: web_access.map(|_| true), patch, remembered }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderProblem {
    Unavailable,
    Disabled,
}

/// Why a selected catalog provider cannot serve `capability` at all. A missing API key is deliberately not one of these:
/// the provider stays a plain choice, and the call itself fails naming the setting to change.
pub fn selected_provider_problem(choice: WebBackendChoice, assets: &WebSearchAssets, capability: SearchCapability) -> Option<ProviderProblem> {
    match choice {
        WebBackendChoice::Unavailable => Some(ProviderProblem::Unavailable),
        WebBackendChoice::Explicit(kind) => {
            if search_provider_capability(kind, capability).is_none() {
                return Some(ProviderProblem::Unavailable);
            }
            if !assets.providers.iter().any(|p| p.kind == kind && p.enabled) {
                return Some(ProviderProblem::Disabled);
            }
            None
        }
        _ => None,
    }
}
