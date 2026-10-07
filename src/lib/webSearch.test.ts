import { describe, expect, it } from "vitest";
import type {
  ContextItem,
  ConversationWebSearchSettings,
  SearchProviderKind,
  WebSearchAssets
} from "../types";
import { familySelectsNativeToolType, grantsWebFetch, nativeSearchRan, selectedProviderProblem } from "./webSearch";

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
