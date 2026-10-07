import type { SearchCapability, SearchProviderKind } from "../types";

/**
 * The catalog declaration for one capability, mirroring Rust's `SearchCapabilitySpec`.
 *
 * An empty `defaultApiHost` means the capability needs no endpoint. Only `fetch` does this because it retrieves the target URL locally.
 *
 * The last two fields say which of the web legs' numbers this backend actually
 * has, which is what decides whether the settings page draws a row for it:
 * a number is offered only where it is a real request parameter of the selected
 * backend (or of mewrk's own reading of it).
 */
export interface SearchCapabilitySpec {
  defaultApiHost: string;
  requiresApiKey: boolean;
  /**
   * Most results this capability's own count field takes, or `null` when it has
   * none. SearXNG's is mewrk's own: its API has no count, so this is how many
   * result pages mewrk reads. A fetch capability never has one.
   */
  maxResults: number | null;
  /**
   * Smallest non-zero per-result token cap this capability takes, or `null` when
   * it has no content cap. The cap is sent upstream (Exa, Jina) except for
   * SearXNG and the local `fetch`, where mewrk truncates the page itself.
   */
  minContentTokens: number | null;
}

export interface SearchProviderCatalogEntry {
  kind: SearchProviderKind;
  label: string;
  search: SearchCapabilitySpec | null;
  fetch: SearchCapabilitySpec | null;
}

/**
 * Mirrors Rust's `model.rs::SEARCH_PROVIDER_CATALOG`; the cross-language parity test requires both catalogs to change together.
 */
export const SEARCH_PROVIDERS: readonly SearchProviderCatalogEntry[] = [
  {
    kind: "zhipu",
    label: "Zhipu",
    search: { defaultApiHost: "https://open.bigmodel.cn/api/paas/v4/web_search", requiresApiKey: true, maxResults: 50, minContentTokens: null },
    fetch: null
  },
  {
    kind: "tavily",
    label: "Tavily",
    search: { defaultApiHost: "https://api.tavily.com", requiresApiKey: true, maxResults: 20, minContentTokens: null },
    fetch: null
  },
  {
    kind: "searxng",
    label: "Searxng",
    search: { defaultApiHost: "http://localhost:8080", requiresApiKey: false, maxResults: 50, minContentTokens: 1 },
    fetch: null
  },
  {
    kind: "exa",
    label: "Exa",
    search: { defaultApiHost: "https://api.exa.ai", requiresApiKey: true, maxResults: 100, minContentTokens: 1 },
    fetch: null
  },
  {
    kind: "exa-mcp",
    label: "ExaMCP",
    search: { defaultApiHost: "https://mcp.exa.ai/mcp", requiresApiKey: false, maxResults: 100, minContentTokens: null },
    fetch: null
  },
  {
    kind: "bocha",
    label: "Bocha",
    search: { defaultApiHost: "https://api.bochaai.com", requiresApiKey: true, maxResults: 50, minContentTokens: null },
    fetch: null
  },
  {
    kind: "querit",
    label: "Querit",
    search: { defaultApiHost: "https://api.querit.ai", requiresApiKey: true, maxResults: 100, minContentTokens: null },
    fetch: { defaultApiHost: "https://api.querit.ai", requiresApiKey: true, maxResults: null, minContentTokens: null }
  },
  {
    kind: "fetch",
    label: "fetch",
    search: null,
    fetch: { defaultApiHost: "", requiresApiKey: false, maxResults: null, minContentTokens: 1 }
  },
  {
    kind: "jina",
    label: "Jina",
    search: { defaultApiHost: "https://s.jina.ai", requiresApiKey: true, maxResults: 20, minContentTokens: 500 },
    fetch: { defaultApiHost: "https://r.jina.ai", requiresApiKey: false, maxResults: null, minContentTokens: 500 }
  },
  {
    kind: "firecrawl",
    label: "Firecrawl",
    search: { defaultApiHost: "https://api.firecrawl.dev", requiresApiKey: false, maxResults: 100, minContentTokens: null },
    fetch: { defaultApiHost: "https://api.firecrawl.dev", requiresApiKey: false, maxResults: null, minContentTokens: null }
  }
];

export function isKnownSearchProvider(kind: string): kind is SearchProviderKind {
  return SEARCH_PROVIDERS.some((provider) => provider.kind === kind);
}

/**
 * Result-shaping bounds, mirroring Rust `model.rs`.
 *
 * Every one of these numbers reads 0 as "send nothing / no cap", so 0 is a legal
 * value rather than the bottom of a range. The ceilings here are the widest any
 * backend takes; a backend that takes fewer says so in its own catalog row
 * (`maxResults`, `minContentTokens`), and the settings rows clamp to that.
 *
 * The compression ceiling bounds both the search leg's per-result cap and the
 * fetch leg's per-page cap.
 */
export const DEFAULT_SEARCH_MAX_RESULTS = 5;
export const SEARCH_MAX_RESULTS_CEILING = 100;
export const DEFAULT_SEARCH_COMPRESSION_CUTOFF = 2000;
export const SEARCH_COMPRESSION_CUTOFF_CEILING = 200_000;

export function searchProviderEntry(kind: SearchProviderKind): SearchProviderCatalogEntry {
  const entry = SEARCH_PROVIDERS.find((provider) => provider.kind === kind);
  if (!entry) {
    throw new Error(`Unknown search provider: ${kind}`);
  }
  return entry;
}

export function searchProviderCapability(
  kind: SearchProviderKind,
  capability: SearchCapability
): SearchCapabilitySpec | null {
  const entry = searchProviderEntry(kind);
  return capability === "searchKeywords" ? entry.search : entry.fetch;
}

export function searchProviderSupports(kind: SearchProviderKind, capability: SearchCapability): boolean {
  return searchProviderCapability(kind, capability) !== null;
}
