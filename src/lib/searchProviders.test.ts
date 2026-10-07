import { describe, expect, it } from "vitest";
import providerSource from "../../src-tauri/src/model.rs?raw";
import {
  isKnownSearchProvider,
  SEARCH_MAX_RESULTS_CEILING,
  SEARCH_PROVIDERS,
  searchProviderCapability,
  searchProviderSupports
} from "./searchProviders";

/**
 * The Rust table stores one row per line; this regex enforces that shape. Each
 * capability slot is `Some(SearchCapabilitySpec { … })` or `None`, and the two
 * trailing numbers of a spec are `Some(<n>)` or `None`.
 */
const SPEC_BODY = String.raw`default_api_host: "([^"]*)", requires_api_key: (true|false), max_results: (Some\(\d+\)|None), min_content_tokens: (Some\(\d+\)|None)`;
const CATALOG_ROW = new RegExp(
  String.raw`SearchProviderCatalogEntry \{ kind: SearchProviderKind::\w+, slug: "([^"]+)", label: "([^"]+)", `
    + String.raw`search: (None|Some\(SearchCapabilitySpec \{ ${SPEC_BODY} \}\)), `
    + String.raw`fetch: (None|Some\(SearchCapabilitySpec \{ ${SPEC_BODY} \}\)) \}`,
  "g"
);

/** `Some(20)` → 20, `None` → null. */
function optionalNumber(literal: string | undefined): number | null {
  const match = /^Some\((\d+)\)$/.exec(literal ?? "");
  return match ? Number(match[1]) : null;
}

/** Capture groups of one slot, starting at `at`: the literal, host, key flag, count, content cap. */
function capability(match: RegExpMatchArray, at: number) {
  return match[at] === "None"
    ? null
    : {
      defaultApiHost: match[at + 1] ?? "",
      requiresApiKey: match[at + 2] === "true",
      maxResults: optionalNumber(match[at + 3]),
      minContentTokens: optionalNumber(match[at + 4])
    };
}

describe("search provider catalog", () => {
  it("mirrors the Rust catalog exactly, including order", () => {
    const rustProviders = Array.from(providerSource.matchAll(CATALOG_ROW), (match) => ({
      kind: match[1],
      label: match[2],
      search: capability(match, 3),
      fetch: capability(match, 8)
    }));
    expect(rustProviders).toHaveLength(10);
    expect(SEARCH_PROVIDERS).toEqual(rustProviders);
  });

  it("answers membership for catalog providers only", () => {
    for (const provider of SEARCH_PROVIDERS) {
      expect(isKnownSearchProvider(provider.kind)).toBe(true);
    }
    for (const retired of ["openai", "anthropic", "deepseek"]) {
      expect(isKnownSearchProvider(retired)).toBe(false);
    }
    expect(isKnownSearchProvider("not-a-provider")).toBe(false);
    expect(isKnownSearchProvider("Tavily")).toBe(false);
    expect(isKnownSearchProvider("")).toBe(false);
  });

  it("keeps every row usable: at least one capability, and only fetch is hostless", () => {
    for (const provider of SEARCH_PROVIDERS) {
      expect(provider.search ?? provider.fetch).not.toBeNull();
    }
    const hostless = SEARCH_PROVIDERS.filter((provider) =>
      [provider.search, provider.fetch].some((spec) => spec && spec.defaultApiHost.length === 0)
    ).map((provider) => provider.kind);
    // Only fetch runs locally and has no third-party endpoint.
    expect(hostless).toEqual(["fetch"]);
  });

  it("routes each capability to its own endpoint", () => {
    // Jina's capabilities use separate hosts, so endpoints are resolved by
    // capability rather than a shared base URL.
    expect(searchProviderCapability("jina", "searchKeywords")?.defaultApiHost).toBe("https://s.jina.ai");
    expect(searchProviderCapability("jina", "fetchUrls")?.defaultApiHost).toBe("https://r.jina.ai");
    expect(searchProviderSupports("tavily", "fetchUrls")).toBe(false);
    expect(searchProviderSupports("fetch", "searchKeywords")).toBe(false);
    expect(searchProviderSupports("fetch", "fetchUrls")).toBe(true);
  });

  /* The per-backend table is what decides which rows the settings page draws
     and where each number is clamped, so the numbers are pinned here against the
     table the product decision wrote down. */
  it("carries each backend's own result count ceiling and content-cap floor", () => {
    const numbers = (kind: Parameters<typeof searchProviderCapability>[0], leg: "searchKeywords" | "fetchUrls") => {
      const spec = searchProviderCapability(kind, leg);
      return spec && [spec.maxResults, spec.minContentTokens];
    };
    expect(numbers("zhipu", "searchKeywords")).toEqual([50, null]);
    expect(numbers("tavily", "searchKeywords")).toEqual([20, null]);
    expect(numbers("searxng", "searchKeywords")).toEqual([50, 1]);
    expect(numbers("exa", "searchKeywords")).toEqual([100, 1]);
    expect(numbers("exa-mcp", "searchKeywords")).toEqual([100, null]);
    expect(numbers("bocha", "searchKeywords")).toEqual([50, null]);
    expect(numbers("querit", "searchKeywords")).toEqual([100, null]);
    expect(numbers("jina", "searchKeywords")).toEqual([20, 500]);
    expect(numbers("firecrawl", "searchKeywords")).toEqual([100, null]);

    expect(numbers("querit", "fetchUrls")).toEqual([null, null]);
    expect(numbers("fetch", "fetchUrls")).toEqual([null, 1]);
    expect(numbers("jina", "fetchUrls")).toEqual([null, 500]);
    expect(numbers("firecrawl", "fetchUrls")).toEqual([null, null]);
  });

  it("never gives a fetch capability a result count, and the generic ceiling is the widest one", () => {
    for (const provider of SEARCH_PROVIDERS) {
      expect(provider.fetch?.maxResults ?? null).toBeNull();
    }
    const widest = Math.max(...SEARCH_PROVIDERS.map((provider) => provider.search?.maxResults ?? 0));
    expect(SEARCH_MAX_RESULTS_CEILING).toBe(widest);
  });
});
