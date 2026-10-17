import { describe, expect, it } from "vitest";
import type {
  ContextItem,
  ConversationWebSearchSettings,
  SearchProviderKind,
  WebSearchAssets
} from "../types";
import {
  familySelectsNativeToolType,
  familySupportsNativeFetch,
  fetchSelectionOffered,
  grantsWebFetch,
  nativeSearchRan,
  selectedProviderProblem,
  switchWebLeg,
  webLegOn,
  type WebLeg
} from "./webSearch";

/**
 * `grantsWebFetch` mirrors Rust `WebSearchSettings::fetch_withheld`. These
 * cases are the same ones `model.rs` asserts, so a one-sided change shows up
 * as a disagreement rather than as a quietly wrong tool lock.
 */
function assets(patch: Partial<WebSearchAssets> = {}): WebSearchAssets {
  const enabled: SearchProviderKind[] = ["tavily", "jina", "firecrawl", "exa", "querit"];
  return {
    providers: enabled.map((kind) => ({
      kind,
      enabled: true,
      searchApiHost: "",
      fetchApiHost: "",
      engines: [],
      basicAuthUsername: ""
    })),
    ...patch
  };
}

function conversation(
  patch: Partial<ConversationWebSearchSettings> = {}
): ConversationWebSearchSettings {
  return {
    maxSearchesPerCall: 0,
    provider: { kind: "native" },
    fetchProvider: { kind: "native" },
    nativeSearchTool: "web_search_20250305",
    nativeFetchTool: "web_fetch_20250910",
    maxResults: 5,
    compressionCutoff: 2000,
    fetchCompressionCutoff: 2000,
    domainFilter: "off",
    includeDomains: [],
    excludeDomains: [],
    ...patch
  };
}

function withProvider(kind: SearchProviderKind, enabled: boolean): WebSearchAssets {
  return assets({
    providers: assets().providers.map((provider) => (
      provider.kind === kind ? { ...provider, enabled } : provider
    ))
  });
}

describe("grantsWebFetch", () => {
  it("withholds fetching from a conversation with no web access at all", () => {
    expect(grantsWebFetch(false, conversation(), "anthropic")).toBe(false);
  });

  it("reads a native selection off the family", () => {
    expect(grantsWebFetch(true, conversation(), "anthropic")).toBe(true);
    expect(grantsWebFetch(true, conversation(), "bedrock")).toBe(true);
    // A family that fetches inside its one search tool grants no second tool.
    expect(grantsWebFetch(true, conversation(), "openai_responses")).toBe(false);
    expect(grantsWebFetch(true, conversation(), "openai_chat")).toBe(false);
    expect(grantsWebFetch(true, conversation(), "google")).toBe(false);
  });

  /* The agent family borrows the CLI's own `WebFetch`, so a native fetch is a
     real tool there even though the host writes no Messages definition for it. */
  it("grants a native fetch to the agent family, which borrows the CLI's own", () => {
    expect(grantsWebFetch(true, conversation(), "claude_agent")).toBe(true);
    expect(grantsWebFetch(false, conversation(), "claude_agent")).toBe(false);
    expect(grantsWebFetch(true, conversation({ fetchProvider: { kind: "disabled" } }), "claude_agent")).toBe(false);
  });

  /* A provider that cannot serve the call keeps the tool: each call fails with
     the setting to change, and global settings never add or remove a tool. */
  it("keeps the tool for any named provider, usable or not", () => {
    const jina = conversation({ fetchProvider: { kind: "explicit", providerKind: "jina" } });
    expect(grantsWebFetch(true, jina, "openai_responses")).toBe(true);
    // Tavily searches only; the binding is broken, not absent.
    const tavily = conversation({ fetchProvider: { kind: "explicit", providerKind: "tavily" } });
    expect(grantsWebFetch(true, tavily, "anthropic")).toBe(true);
    expect(grantsWebFetch(true, conversation({ fetchProvider: { kind: "unavailable" } }), "anthropic")).toBe(true);
  });

  it("grants nothing at all when the conversation switched fetching off", () => {
    // Off holds even when the search backend fetches for itself, because what
    // searches has no say in what fetches.
    const off = conversation({
      provider: { kind: "explicit", providerKind: "firecrawl" },
      fetchProvider: { kind: "disabled" }
    });
    expect(grantsWebFetch(true, off, "anthropic")).toBe(false);
  });
});

describe("selectedProviderProblem", () => {
  it("names what keeps a selected provider from working, for the Repair label", () => {
    const jina = { kind: "explicit", providerKind: "jina" } as const;
    expect(selectedProviderProblem(jina, assets(), "fetchUrls")).toBeNull();
    expect(selectedProviderProblem(jina, withProvider("jina", false), "fetchUrls")).toBe("disabled");
    expect(selectedProviderProblem({ kind: "unavailable" }, assets(), "fetchUrls")).toBe("unavailable");
    // Tavily cannot fetch at all.
    expect(selectedProviderProblem({ kind: "explicit", providerKind: "tavily" }, assets(), "fetchUrls"))
      .toBe("unavailable");
    // A provider that needs a key is a plain choice either way: the call fails, not the picker.
    expect(selectedProviderProblem(jina, assets(), "searchKeywords")).toBeNull();
    expect(selectedProviderProblem({ kind: "explicit", providerKind: "querit" }, assets(), "fetchUrls")).toBeNull();
    // Native and Off name no provider to repair.
    expect(selectedProviderProblem({ kind: "native" }, assets(), "searchKeywords")).toBeNull();
    expect(selectedProviderProblem({ kind: "disabled" }, assets(), "searchKeywords")).toBeNull();
  });
});

describe("nativeSearchRan", () => {
  const search = (success: boolean, output: unknown): ContextItem => ({
    id: `ctx_${Math.random()}`,
    kind: "tool",
    toolName: "web_search",
    createdAt: "2026-10-03T00:00:00Z",
    input: { query: "q" },
    result: {
      success,
      output: typeof output === "string" ? output : JSON.stringify(output),
      executedAt: "2026-10-03T00:00:00Z",
      durationMs: 1
    }
  });

  it("is true only once a native report is in the transcript", () => {
    expect(nativeSearchRan([])).toBe(false);
    // A catalog provider's results are ordinary output, not a native report.
    expect(nativeSearchRan([search(true, { results: [] })])).toBe(false);
    // A native call on a family without native search fails before it is sent.
    expect(nativeSearchRan([search(false, "Provider X belongs to the openai_chat family…")])).toBe(false);
    expect(nativeSearchRan([search(true, { findings: "report", sources: [] })])).toBe(true);
  });
});

/* The version of a native web tool is a Messages spelling. Every family that
   has native web tools has exactly one shape for them; only Messages writes the
   version into the request, so only there is there a choice to offer. */
describe("familySelectsNativeToolType", () => {
  it("is true only of the families that speak Messages", () => {
    expect(familySelectsNativeToolType("anthropic")).toBe(true);
    expect(familySelectsNativeToolType("bedrock")).toBe(true);
    // Responses searches natively and still has no version to name, which is
    // why this is its own table rather than the native-search one.
    expect(familySelectsNativeToolType("openai_responses")).toBe(false);
    expect(familySelectsNativeToolType("google")).toBe(false);
    expect(familySelectsNativeToolType("xai")).toBe(false);
    // The agent family makes its own Messages calls inside the CLI, so the host
    // never writes a tool definition for a version to ride on.
    expect(familySelectsNativeToolType("claude_agent")).toBe(false);
    expect(familySelectsNativeToolType(undefined)).toBe(false);
  });
});

/* Fetching natively needs a server-side fetch tool whose result the host can
   read. Anthropic has one; the agent family borrows the CLI's own; every other
   family reads pages inside its search tool or not at all. */
describe("familySupportsNativeFetch", () => {
  it("is true only of the families with a fetch tool of their own", () => {
    expect(familySupportsNativeFetch("anthropic")).toBe(true);
    expect(familySupportsNativeFetch("bedrock")).toBe(true);
    expect(familySupportsNativeFetch("claude_agent")).toBe(true);
    expect(familySupportsNativeFetch("openai_responses")).toBe(false);
    expect(familySupportsNativeFetch("openai_chat")).toBe(false);
    expect(familySupportsNativeFetch("google")).toBe(false);
    expect(familySupportsNativeFetch("xai")).toBe(false);
    expect(familySupportsNativeFetch("openai_compatible")).toBe(false);
    expect(familySupportsNativeFetch(undefined)).toBe(false);
  });

  it("is not the same table as the one that decides whether a tool version is written", () => {
    // The agent family fetches natively but never writes a Messages tool.
    expect(familySupportsNativeFetch("claude_agent")).toBe(true);
    expect(familySelectsNativeToolType("claude_agent")).toBe(false);
    for (const family of ["anthropic", "bedrock"] as const) {
      expect(familySupportsNativeFetch(family)).toBe(familySelectsNativeToolType(family));
    }
  });
});

const NATIVE = { kind: "native" } as const;
const DISABLED = { kind: "disabled" } as const;
const TAVILY = { kind: "explicit", providerKind: "tavily" } as const;
const EXA = { kind: "explicit", providerKind: "exa" } as const;
const JINA = { kind: "explicit", providerKind: "jina" } as const;
const FIRECRAWL = { kind: "explicit", providerKind: "firecrawl" } as const;

describe("fetchSelectionOffered", () => {
  it("rejects Off, and native only where the model has a fetch tool of its own", () => {
    expect(fetchSelectionOffered(DISABLED, true)).toBe(false);
    expect(fetchSelectionOffered(DISABLED, false)).toBe(false);
    expect(fetchSelectionOffered(NATIVE, true)).toBe(true);
    expect(fetchSelectionOffered(NATIVE, false)).toBe(false);
  });

  it("accepts a named provider on any model, usable or not", () => {
    expect(fetchSelectionOffered(JINA, true)).toBe(true);
    expect(fetchSelectionOffered(JINA, false)).toBe(true);
    // A lost binding keeps `web_fetch` offered and reads "Repair", so it is on.
    expect(fetchSelectionOffered({ kind: "unavailable" }, false)).toBe(true);
  });
});

describe("webLegOn", () => {
  it("reads both legs off while web access is off, whatever they still name", () => {
    const named = conversation({ provider: TAVILY, fetchProvider: JINA });
    expect(webLegOn("search", false, named, true)).toBe(false);
    expect(webLegOn("fetch", false, named, true)).toBe(false);
  });

  it("reads a leg off when it names no backend, and on when it names one", () => {
    const searchOnly = conversation({ provider: TAVILY, fetchProvider: DISABLED });
    expect(webLegOn("search", true, searchOnly, true)).toBe(true);
    expect(webLegOn("fetch", true, searchOnly, true)).toBe(false);

    const fetchOnly = conversation({ provider: DISABLED, fetchProvider: JINA });
    expect(webLegOn("search", true, fetchOnly, true)).toBe(false);
    expect(webLegOn("fetch", true, fetchOnly, true)).toBe(true);

    // A backend that has stopped working is still a named one.
    const broken = conversation({ provider: { kind: "unavailable" }, fetchProvider: { kind: "unavailable" } });
    expect(webLegOn("search", true, broken, false)).toBe(true);
    expect(webLegOn("fetch", true, broken, false)).toBe(true);
  });

  /* Native fetch is stored as the user left it: on a model without one it is
     not rewritten, the leg only reads off. */
  it("reads a native fetch off on a model without one, and on again on a model that has one", () => {
    const native = conversation({ provider: NATIVE, fetchProvider: NATIVE });
    expect(webLegOn("fetch", true, native, false)).toBe(false);
    expect(webLegOn("fetch", true, native, true)).toBe(true);
    // Native search is a backend on every family.
    expect(webLegOn("search", true, native, false)).toBe(true);
    // The model's fetch says nothing about a provider that fetches for it.
    expect(webLegOn("fetch", true, conversation({ fetchProvider: JINA }), false)).toBe(true);
  });

  it("leaves it to the backends alone where the surface has no web access switch of its own", () => {
    const named = conversation({ provider: TAVILY, fetchProvider: NATIVE });
    expect(webLegOn("search", null, named, false)).toBe(true);
    expect(webLegOn("fetch", null, named, false)).toBe(false);
    expect(webLegOn("fetch", null, named, true)).toBe(true);
    const none = conversation({ provider: DISABLED, fetchProvider: DISABLED });
    expect(webLegOn("search", null, none, true)).toBe(false);
    expect(webLegOn("fetch", null, none, true)).toBe(false);
    // The same settings under an access switch that is on read the same.
    expect(webLegOn("search", true, named, false)).toBe(webLegOn("search", null, named, false));
    expect(webLegOn("fetch", true, named, false)).toBe(webLegOn("fetch", null, named, false));
  });
});

describe("switchWebLeg", () => {
  type Current = Parameters<typeof switchWebLeg>[2];
  const flip = (leg: WebLeg, on: boolean, current: Partial<Current> = {}) => switchWebLeg(leg, on, {
    webAccess: true,
    settings: conversation({ provider: TAVILY, fetchProvider: JINA }),
    nativeFetchOffered: true,
    fetchProviders: ["jina", "firecrawl"],
    remembered: {},
    ...current
  });

  describe("off", () => {
    it("sets the leg's backend to disabled and remembers the one it had, while the other leg still names one", () => {
      const search = flip("search", false);
      expect(search).toEqual({
        webAccess: true,
        patch: { provider: DISABLED },
        remembered: { search: TAVILY }
      });

      const fetch = flip("fetch", false);
      expect(fetch).toEqual({
        webAccess: true,
        patch: { fetchProvider: DISABLED },
        remembered: { fetch: JINA }
      });
    });

    it("keeps what the other leg was remembering", () => {
      const next = flip("search", false, { remembered: { fetch: FIRECRAWL } });
      expect(next.remembered).toEqual({ search: TAVILY, fetch: FIRECRAWL });
    });

    /* Web access is the union of the legs: it goes off with the last leg that
       names a backend, and that backend stays where it is — access off already
       withholds it, and it is what comes back with the switch. */
    it("turns web access off with the last leg that names a backend, and leaves that backend alone", () => {
      const search = flip("search", false, { settings: conversation({ provider: TAVILY, fetchProvider: DISABLED }) });
      expect(search.webAccess).toBe(false);
      expect(search.patch).toEqual({});
      expect(search.remembered).toEqual({});

      const fetch = flip("fetch", false, { settings: conversation({ provider: DISABLED, fetchProvider: JINA }) });
      expect(fetch.webAccess).toBe(false);
      expect(fetch.patch).toEqual({});
      expect(fetch.remembered).toEqual({});
    });

    it("counts a leg as naming a backend while it merely reads off for lack of a native fetch", () => {
      const next = flip("search", false, {
        settings: conversation({ provider: TAVILY, fetchProvider: NATIVE }),
        nativeFetchOffered: false
      });
      // Access stays on for the stored native fetch ...
      expect(next.webAccess).toBe(true);
      expect(next.patch).toEqual({ provider: DISABLED });

      // ... so a model that has native fetch finds fetching on again.
      const restored = conversation({ provider: TAVILY, fetchProvider: NATIVE, ...next.patch });
      expect(webLegOn("fetch", next.webAccess, restored, true)).toBe(true);
      expect(webLegOn("fetch", next.webAccess, restored, false)).toBe(false);
    });

    it("writes only the leg where the surface has no web access switch", () => {
      const last = flip("fetch", false, {
        webAccess: null,
        settings: conversation({ provider: DISABLED, fetchProvider: JINA })
      });
      expect(last).toEqual({ webAccess: null, patch: { fetchProvider: DISABLED }, remembered: { fetch: JINA } });
      expect(flip("search", false, { webAccess: null }).webAccess).toBeNull();
    });
  });

  describe("on", () => {
    it("brings back the backend the leg was carrying when it was switched off", () => {
      const search = flip("search", true, {
        settings: conversation({ provider: DISABLED, fetchProvider: JINA }),
        remembered: { search: EXA, fetch: FIRECRAWL }
      });
      expect(search.webAccess).toBe(true);
      expect(search.patch).toEqual({ provider: EXA });
      // What it brought back is no longer remembered; the other leg's stays.
      expect(search.remembered).toEqual({ fetch: FIRECRAWL });

      const fetch = flip("fetch", true, {
        settings: conversation({ provider: TAVILY, fetchProvider: DISABLED }),
        remembered: { fetch: FIRECRAWL }
      });
      expect(fetch.patch).toEqual({ fetchProvider: FIRECRAWL });
      expect(fetch.remembered).toEqual({});
    });

    it("falls back to the stored backend when it can be served, and to a default when nothing can", () => {
      // Nothing remembered, but the leg still names a usable backend (web access was off).
      const stored = flip("search", true, {
        webAccess: false,
        settings: conversation({ provider: EXA, fetchProvider: DISABLED })
      });
      expect(stored.patch).toEqual({ provider: EXA });

      // Off and unremembered: native search is the default on every family.
      const search = flip("search", true, { settings: conversation({ provider: DISABLED, fetchProvider: JINA }) });
      expect(search.patch).toEqual({ provider: NATIVE });
    });

    it("picks native fetch where the model has one, else the first catalog provider that can fetch", () => {
      const off = conversation({ provider: TAVILY, fetchProvider: DISABLED });
      expect(flip("fetch", true, { settings: off, nativeFetchOffered: true }).patch)
        .toEqual({ fetchProvider: NATIVE });
      expect(flip("fetch", true, { settings: off, nativeFetchOffered: false }).patch)
        .toEqual({ fetchProvider: JINA });
      expect(flip("fetch", true, { settings: off, nativeFetchOffered: false, fetchProviders: ["firecrawl", "jina"] }).patch)
        .toEqual({ fetchProvider: FIRECRAWL });
    });

    it("names the local fetch when no catalog provider is switched on to fetch", () => {
      const off = conversation({ provider: TAVILY, fetchProvider: DISABLED });
      expect(flip("fetch", true, { settings: off, nativeFetchOffered: false, fetchProviders: [] }).patch)
        .toEqual({ fetchProvider: { kind: "explicit", providerKind: "fetch" } });
    });

    it("does not bring back a native fetch the model cannot serve", () => {
      // Remembered from a model that had one, and stored as native: neither serves here.
      const next = flip("fetch", true, {
        settings: conversation({ provider: TAVILY, fetchProvider: NATIVE }),
        remembered: { fetch: NATIVE },
        nativeFetchOffered: false
      });
      expect(next.patch).toEqual({ fetchProvider: JINA });
      // A remembered provider still wins over the stored native one.
      const remembered = flip("fetch", true, {
        settings: conversation({ provider: TAVILY, fetchProvider: NATIVE }),
        remembered: { fetch: FIRECRAWL },
        nativeFetchOffered: false
      });
      expect(remembered.patch).toEqual({ fetchProvider: FIRECRAWL });
    });

    it("turns a stored native fetch on as it is where the model has one", () => {
      const next = flip("fetch", true, {
        webAccess: false,
        settings: conversation({ provider: DISABLED, fetchProvider: NATIVE }),
        nativeFetchOffered: true
      });
      expect(next.webAccess).toBe(true);
      expect(next.patch).toEqual({ fetchProvider: NATIVE });
    });

    /* Access off reads both legs off, though each still names its backend.
       Switching one on must not bring the other back with it. */
    it("sets the other leg aside to disabled, and remembers it, when it turns web access on", () => {
      const fetch = flip("fetch", true, { webAccess: false });
      expect(fetch.webAccess).toBe(true);
      expect(fetch.patch).toEqual({ fetchProvider: JINA, provider: DISABLED });
      expect(fetch.remembered).toEqual({ search: TAVILY });

      const search = flip("search", true, { webAccess: false });
      expect(search.webAccess).toBe(true);
      expect(search.patch).toEqual({ provider: TAVILY, fetchProvider: DISABLED });
      expect(search.remembered).toEqual({ fetch: JINA });

      // And switching the other back on later restores exactly what was set aside.
      const later = flip("search", true, {
        settings: conversation({ provider: DISABLED, fetchProvider: JINA }),
        remembered: fetch.remembered
      });
      expect(later.patch).toEqual({ provider: TAVILY });
    });

    it("sets nothing aside when web access is already on, or the other leg names nothing", () => {
      const accessOn = flip("search", true, { settings: conversation({ provider: DISABLED, fetchProvider: JINA }) });
      expect(accessOn.patch).toEqual({ provider: NATIVE });
      expect(accessOn.remembered).toEqual({});

      const bare = flip("search", true, {
        webAccess: false,
        settings: conversation({ provider: TAVILY, fetchProvider: DISABLED })
      });
      expect(bare.webAccess).toBe(true);
      expect(bare.patch).toEqual({ provider: TAVILY });
      expect(bare.remembered).toEqual({});
    });

    it("writes only the leg where the surface has no web access switch, and sets nothing aside", () => {
      const next = flip("fetch", true, {
        webAccess: null,
        settings: conversation({ provider: TAVILY, fetchProvider: DISABLED }),
        nativeFetchOffered: true
      });
      expect(next).toEqual({ webAccess: null, patch: { fetchProvider: NATIVE }, remembered: {} });
    });
  });

  it("does not touch the memory it was handed, and a leg that is switched off and on again ends where it started", () => {
    const remembered = { fetch: FIRECRAWL };
    const settings = conversation({ provider: TAVILY, fetchProvider: JINA });
    const off = flip("search", false, { settings, remembered });
    expect(remembered).toEqual({ fetch: FIRECRAWL });

    const afterOff = { ...settings, ...off.patch };
    const on = flip("search", true, { settings: afterOff, remembered: off.remembered });
    expect({ ...afterOff, ...on.patch }).toEqual(settings);
    expect(on.webAccess).toBe(true);
    expect(on.remembered).toEqual({ fetch: FIRECRAWL });
  });
});
