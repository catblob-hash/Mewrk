//! `lib/webSearch.ts`: which web tools a conversation is offered, the two legs' switches, and credential slots.

use mewrk_client::model::{ConversationWebSearchSettings, FetchProviderSelection, ProviderFamily, SearchCapability, SearchProviderKind, SearchProviderSelection, WebSearchAssets};
use serde_json::Value;

use super::search_providers::search_provider_supports;

/// Whether a provider family has a server-side page-fetch tool the host can read as text. Mirrors Rust.
pub fn family_supports_native_fetch(family: Option<ProviderFamily>) -> bool {
    matches!(family, Some(ProviderFamily::Anthropic | ProviderFamily::Bedrock | ProviderFamily::ClaudeAgent))
}

/// Whether this family spells its native web tools the Messages way, with the version in the tool's own `type`.
pub fn family_selects_native_tool_type(family: Option<ProviderFamily>) -> bool {
    matches!(family, Some(ProviderFamily::Anthropic | ProviderFamily::Bedrock))
}

/// Whether these settings grant `web_fetch` at all. A broken provider keeps the tool and fails each call with a fix.
pub fn grants_web_fetch(web_search_enabled: bool, conversation: &ConversationWebSearchSettings, family: Option<ProviderFamily>) -> bool {
    if !web_search_enabled {
        return false;
    }
    match conversation.fetch_provider {
        FetchProviderSelection::Disabled => false,
        FetchProviderSelection::Native => family_supports_native_fetch(family),
        FetchProviderSelection::Explicit { .. } | FetchProviderSelection::Unavailable => true,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WebLeg {
    Search,
    Fetch,
}

/// Whether a fetch selection is one this model can serve as drawn.
pub fn fetch_selection_offered(selection: &FetchProviderSelection, native_fetch_offered: bool) -> bool {
    !matches!(selection, FetchProviderSelection::Disabled) && (!matches!(selection, FetchProviderSelection::Native) || native_fetch_offered)
}

/// Whether a leg's switch is drawn on. Native fetch on a model without one reads as off without being rewritten.
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

#[derive(Clone, Debug, Default, PartialEq)]
pub struct WebLegPatch {
    pub provider: Option<SearchProviderSelection>,
    pub fetch_provider: Option<FetchProviderSelection>,
}

impl WebLegPatch {
    pub fn apply(&self, settings: &mut ConversationWebSearchSettings) {
        if let Some(p) = &self.provider {
            settings.provider = p.clone();
        }
        if let Some(p) = &self.fetch_provider {
            settings.fetch_provider = p.clone();
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

/// Flips one web leg's switch. Off is the backend reading `disabled`, so what it had is kept and comes back with the
/// switch. Web access is the union of the two legs: it goes off with the last leg that names a backend, and on with the
/// first one switched on, which also stops the other leg's backend from quietly coming back with it.
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
    // The leg's own selection first: while it still names a backend, that is the one the leg was left on.
    match leg {
        WebLeg::Search => {
            let chosen = [Some(settings.provider.clone()), remembered.search.clone()].into_iter().flatten().find(|s| !matches!(s, SearchProviderSelection::Disabled));
            patch.provider = Some(chosen.unwrap_or(SearchProviderSelection::Native));
            remembered.search = None;
        }
        WebLeg::Fetch => {
            let chosen = [Some(settings.fetch_provider.clone()), remembered.fetch.clone()].into_iter().flatten().find(|s| fetch_selection_offered(s, native_fetch_offered));
            patch.fetch_provider = Some(chosen.unwrap_or(if native_fetch_offered { FetchProviderSelection::Native } else { FetchProviderSelection::Explicit { provider_kind: fetch_providers.first().copied().unwrap_or(SearchProviderKind::Fetch) } }));
            remembered.fetch = None;
        }
    }
    if web_access == Some(false) && other_named {
        set_aside(if leg == WebLeg::Search { WebLeg::Fetch } else { WebLeg::Search }, &mut remembered, &mut patch);
    }
    WebLegSwitch { web_access: web_access.map(|_| true), patch, remembered }
}

/// Whether the transcript holds a report a native search produced. Takes the tool results' `(tool name, streaming,
/// success, output)`, since only the native envelope `{findings, sources}` has a string `findings`.
pub fn native_search_ran<'a>(results: impl IntoIterator<Item = (&'a str, bool, bool, &'a str)>) -> bool {
    results.into_iter().any(|(name, streaming, success, output)| {
        name == "web_search" && !streaming && success && serde_json::from_str::<Value>(output).ok().is_some_and(|v| v.get("findings").is_some_and(Value::is_string))
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderProblem {
    Unavailable,
    Disabled,
}

/// Why a selected catalog provider cannot serve `capability`. A missing API key is deliberately not one of these: the
/// call fails naming the setting to change, as the key is a fact about the credential store at call time.
pub fn selected_provider_problem(unavailable: bool, explicit: Option<SearchProviderKind>, assets: &WebSearchAssets, capability: SearchCapability) -> Option<ProviderProblem> {
    if unavailable {
        return Some(ProviderProblem::Unavailable);
    }
    let kind = explicit?;
    if !search_provider_supports(kind, capability) {
        return Some(ProviderProblem::Unavailable);
    }
    if !assets.providers.iter().any(|p| p.kind == kind && p.enabled) {
        return Some(ProviderProblem::Disabled);
    }
    None
}

/// Secrets a search provider may hold, mirroring Rust's `web_search::CredentialSlot`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SearchCredentialSlot {
    ApiKey,
    BasicAuthPassword,
}

impl SearchCredentialSlot {
    pub fn wire(self) -> &'static str {
        match self {
            Self::ApiKey => "apiKey",
            Self::BasicAuthPassword => "basicAuthPassword",
        }
    }
}

/// Where the mask's length is remembered: never the secret, only how many bullets to draw.
pub fn search_key_length_store_key(provider_kind: &str, slot: SearchCredentialSlot) -> String {
    format!("mewrk.search-api-key-length.v2.{provider_kind}.{}", slot.wire())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mewrk_client::model::{NativeFetchTool, NativeSearchTool, SearchDomainFilterMode};
    use FetchProviderSelection as F;
    use SearchProviderKind::*;
    use SearchProviderSelection as S;

    fn settings(provider: S, fetch_provider: F) -> ConversationWebSearchSettings {
        ConversationWebSearchSettings { max_searches_per_call: 0, provider, fetch_provider, native_search_tool: NativeSearchTool::default(), native_fetch_tool: NativeFetchTool::default(), max_results: 5, compression_cutoff: 2000, fetch_compression_cutoff: 2000, domain_filter: SearchDomainFilterMode::Off, include_domains: vec![], exclude_domains: vec![] }
    }

    fn off() -> RememberedWebLegs {
        RememberedWebLegs::default()
    }

    // webSearch.test.ts
    #[test]
    fn native_fetch_is_a_family_fact_and_explicit_always_grants() {
        assert!(family_supports_native_fetch(Some(ProviderFamily::Anthropic)));
        assert!(family_supports_native_fetch(Some(ProviderFamily::ClaudeAgent)));
        assert!(!family_supports_native_fetch(Some(ProviderFamily::OpenaiResponses)));
        assert!(!family_supports_native_fetch(None));
        assert!(family_selects_native_tool_type(Some(ProviderFamily::Bedrock)));
        assert!(!family_selects_native_tool_type(Some(ProviderFamily::ClaudeAgent)));
        let s = settings(S::Native, F::Native);
        assert!(!grants_web_fetch(false, &s, Some(ProviderFamily::Anthropic)));
        assert!(grants_web_fetch(true, &s, Some(ProviderFamily::Anthropic)));
        assert!(!grants_web_fetch(true, &s, Some(ProviderFamily::OpenaiChat)));
        assert!(grants_web_fetch(true, &settings(S::Native, F::Unavailable), None));
        assert!(!grants_web_fetch(true, &settings(S::Native, F::Disabled), Some(ProviderFamily::Anthropic)));
    }

    #[test]
    fn a_native_fetch_without_one_reads_off_and_is_not_rewritten() {
        let s = settings(S::Native, F::Native);
        assert!(!web_leg_on(WebLeg::Fetch, None, &s, false));
        assert!(web_leg_on(WebLeg::Fetch, None, &s, true));
        assert!(web_leg_on(WebLeg::Search, None, &s, false));
        assert!(!web_leg_on(WebLeg::Search, Some(false), &s, true));
    }

    #[test]
    fn switching_a_leg_off_remembers_its_backend_and_on_restores_it() {
        let tavily = S::Explicit { provider_kind: Tavily };
        let s = settings(tavily.clone(), F::Native);
        let off_flip = switch_web_leg(WebLeg::Search, false, None, &s, true, &[], &off());
        assert_eq!(off_flip.patch.provider, Some(S::Disabled));
        assert_eq!(off_flip.remembered.search, Some(tavily.clone()));
        let s2 = settings(S::Disabled, F::Native);
        let on_flip = switch_web_leg(WebLeg::Search, true, None, &s2, true, &[], &off_flip.remembered);
        assert_eq!(on_flip.patch.provider, Some(tavily));
        assert_eq!(on_flip.remembered.search, None);
    }

    #[test]
    fn access_goes_off_with_the_last_named_leg_keeping_the_backend() {
        let s = settings(S::Native, F::Disabled);
        let flip = switch_web_leg(WebLeg::Search, false, Some(true), &s, true, &[], &off());
        assert_eq!(flip.web_access, Some(false));
        assert_eq!(flip.patch, WebLegPatch::default());
    }

    #[test]
    fn access_on_with_one_leg_sets_the_other_aside() {
        let s = settings(S::Native, F::Native);
        let flip = switch_web_leg(WebLeg::Search, true, Some(false), &s, true, &[], &off());
        assert_eq!(flip.web_access, Some(true));
        assert_eq!(flip.patch.provider, Some(S::Native));
        assert_eq!(flip.patch.fetch_provider, Some(F::Disabled));
        assert_eq!(flip.remembered.fetch, Some(F::Native));
    }

    #[test]
    fn a_fetch_leg_with_no_native_takes_the_first_provider_switched_on() {
        let s = settings(S::Native, F::Disabled);
        let flip = switch_web_leg(WebLeg::Fetch, true, None, &s, false, &[Jina, Fetch], &off());
        assert_eq!(flip.patch.fetch_provider, Some(F::Explicit { provider_kind: Jina }));
        let flip = switch_web_leg(WebLeg::Fetch, true, None, &s, false, &[], &off());
        assert_eq!(flip.patch.fetch_provider, Some(F::Explicit { provider_kind: Fetch }));
        let flip = switch_web_leg(WebLeg::Fetch, true, None, &s, true, &[Jina], &off());
        assert_eq!(flip.patch.fetch_provider, Some(F::Native));
    }

    #[test]
    fn only_the_native_envelope_counts_as_a_native_search() {
        assert!(native_search_ran([("web_search", false, true, r#"{"findings":"x","sources":[]}"#)]));
        assert!(!native_search_ran([("web_search", false, true, r#"{"results":[]}"#)]));
        assert!(!native_search_ran([("web_search", true, true, r#"{"findings":"x"}"#)]));
        assert!(!native_search_ran([("web_search", false, false, r#"{"findings":"x"}"#)]));
        assert!(!native_search_ran([("web_fetch", false, true, r#"{"findings":"x"}"#)]));
        assert!(!native_search_ran([("web_search", false, true, "not json")]));
    }

    #[test]
    fn a_problem_is_a_lost_or_switched_off_provider_never_a_missing_key() {
        let mut assets = WebSearchAssets::default();
        assert_eq!(selected_provider_problem(true, None, &assets, SearchCapability::SearchKeywords), Some(ProviderProblem::Unavailable));
        assert_eq!(selected_provider_problem(false, None, &assets, SearchCapability::SearchKeywords), None);
        assert_eq!(selected_provider_problem(false, Some(Fetch), &assets, SearchCapability::SearchKeywords), Some(ProviderProblem::Unavailable));
        assert_eq!(selected_provider_problem(false, Some(Tavily), &assets, SearchCapability::SearchKeywords), Some(ProviderProblem::Disabled));
        let mut row = mewrk_client::model::SearchProviderConfig::new(Tavily);
        row.enabled = true;
        assets.providers.push(row);
        assert_eq!(selected_provider_problem(false, Some(Tavily), &assets, SearchCapability::SearchKeywords), None);
    }
}
