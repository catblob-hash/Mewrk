//! `lib/searchProviders.ts`. The catalog itself is the host's: `model::SEARCH_PROVIDER_CATALOG`, which upstream mirrors.

use mewrk_client::model::{SearchCapability, SearchCapabilitySpec, SearchProviderCatalogEntry, SearchProviderKind, SEARCH_PROVIDER_CATALOG};

pub const DEFAULT_SEARCH_MAX_RESULTS: u32 = 5;
pub const SEARCH_MAX_RESULTS_CEILING: u32 = 100;
pub const DEFAULT_SEARCH_COMPRESSION_CUTOFF: u32 = 2000;
pub const SEARCH_COMPRESSION_CUTOFF_CEILING: u32 = 200_000;

pub fn search_providers() -> &'static [SearchProviderCatalogEntry] {
    SEARCH_PROVIDER_CATALOG
}

pub fn search_provider_entry(kind: SearchProviderKind) -> &'static SearchProviderCatalogEntry {
    SEARCH_PROVIDER_CATALOG.iter().find(|e| e.kind == kind).expect("the catalog lists every kind")
}

pub fn search_provider_capability(kind: SearchProviderKind, capability: SearchCapability) -> Option<&'static SearchCapabilitySpec> {
    let entry = search_provider_entry(kind);
    match capability {
        SearchCapability::SearchKeywords => entry.search.as_ref(),
        SearchCapability::FetchUrls => entry.fetch.as_ref(),
    }
}

pub fn search_provider_supports(kind: SearchProviderKind, capability: SearchCapability) -> bool {
    search_provider_capability(kind, capability).is_some()
}

/// The wire name a kind has in documents and credential commands.
pub fn kind_wire(kind: SearchProviderKind) -> String {
    serde_json::to_value(kind).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use SearchProviderKind::*;

    // searchProviders.test.ts
    #[test]
    fn the_catalog_holds_all_ten_rows_in_order() {
        let kinds: Vec<_> = search_providers().iter().map(|e| e.kind).collect();
        assert_eq!(kinds, [Zhipu, Tavily, Searxng, Exa, ExaMcp, Bocha, Querit, Fetch, Jina, Firecrawl]);
    }

    #[test]
    fn capabilities_say_what_each_backend_has() {
        assert!(search_provider_capability(Fetch, SearchCapability::SearchKeywords).is_none());
        assert_eq!(search_provider_capability(Jina, SearchCapability::SearchKeywords).unwrap().min_content_tokens, Some(500));
        assert_eq!(search_provider_capability(Tavily, SearchCapability::SearchKeywords).unwrap().max_results, Some(20));
        assert!(search_provider_supports(Querit, SearchCapability::FetchUrls));
        assert!(!search_provider_supports(Exa, SearchCapability::FetchUrls));
        assert_eq!(kind_wire(ExaMcp), "exa-mcp");
    }
}
