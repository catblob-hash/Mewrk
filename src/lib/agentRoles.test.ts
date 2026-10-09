import { describe, expect, it } from "vitest";
import type {
  AgentModelSelection,
  AgentRole,
  AgentRoleResource,
  ApiProvider,
  ModelProfile
} from "../types";
import { NATIVE_FETCH_TOOLS, NATIVE_SEARCH_TOOLS } from "../types";
import {
  MAX_AGENT_TYPE_CHARS,
  agentModelSelectionIsAvailable,
  canonicalAgentToolNames,
  cloneAgentRole,
  defaultAgentRoleWebSearch,
  hasUsableAgentRole,
  sameAgentRole,
  selectedAgentRoleCount,
  validateAgentTypeName
} from "./agentRoles";
import {
  DEFAULT_SEARCH_COMPRESSION_CUTOFF,
  DEFAULT_SEARCH_MAX_RESULTS
} from "./searchProviders";

function model(id: string): ModelProfile {
  return {
    id,
    name: "",
    group: "",
    capabilities: [],
    reasoningContent: "plaintext",
    promptCache: true
  };
}

function provider(id: string, modelIds: string[], enabled = true): ApiProvider {
  return {
    id,
    name: id,
    enabled,
    family: "openai_chat",
    baseUrl: "https://example.invalid/v1",
    familySettings: {},
    notes: "",
    models: modelIds.map(model),
    activeModelId: modelIds[0] ?? null
  };
}

function role(overrides: Partial<AgentRole> = {}): AgentRole {
  return {
    name: "reviewer",
    description: "Reads a diff and says what is wrong with it.",
    modelSelection: { kind: "inherit" },
    effort: null,
    tools: ["read_file"],
    disallowedTools: [],
    skillIds: [],
    mcpIds: [],
    hookIds: [],
    webSearch: defaultAgentRoleWebSearch(),
    templateId: null,
    toolDescriptionFileId: null,
    ...overrides
  };
}

function resource(
  id: string,
  overrides: Partial<AgentRoleResource> = {}
): AgentRoleResource {
  return {
    id,
    name: id,
    description: "",
    location: `C:/Users/u/.mewrk/agents/${id}.json`,
    source: "user",
    available: true,
    role: role({ name: id }),
    ...overrides
  };
}

describe("agentRoles", () => {
  it("takes a role name as free text and refuses only what cannot be echoed back", () => {
    expect(validateAgentTypeName("reviewer")).toBeNull();
    expect(validateAgentTypeName("code-reviewer_2")).toBeNull();
    // The shape rule is gone: a role name is prose the user writes and the model
    // reads back, so case, spaces, punctuation and non-ASCII are all ordinary.
    expect(validateAgentTypeName("Code Reviewer")).toBeNull();
    expect(validateAgentTypeName("对抗式审查")).toBeNull();
    expect(validateAgentTypeName("2reviewer")).toBeNull();
    expect(validateAgentTypeName("code/reviewer")).toBeNull();
    // What is left is not about shape: nothing to name, more than the host
    // stores, and characters that cannot survive a listing or an error message.
    expect(validateAgentTypeName("")).toBe("required");
    expect(validateAgentTypeName(`a${"x".repeat(64)}`)).toBe("too_long");
    expect(validateAgentTypeName("review\ner")).toBe("characters");
  });

  it("counts a role name's length in characters, not UTF-16 units", () => {
    expect(MAX_AGENT_TYPE_CHARS).toBe(64);
    expect(validateAgentTypeName("x".repeat(MAX_AGENT_TYPE_CHARS))).toBeNull();
    expect(validateAgentTypeName("x".repeat(MAX_AGENT_TYPE_CHARS + 1))).toBe("too_long");
    // An astral character is one character to the user and two UTF-16 units: a
    // name of exactly the limit must pass, or the editor would refuse what the
    // host stores.
    expect(validateAgentTypeName("😀".repeat(MAX_AGENT_TYPE_CHARS))).toBeNull();
    expect(validateAgentTypeName("😀".repeat(MAX_AGENT_TYPE_CHARS + 1))).toBe("too_long");
    // Control characters are refused wherever they sit, DEL included.
    expect(validateAgentTypeName("\u0000reviewer")).toBe("characters");
    expect(validateAgentTypeName("review\u007fer")).toBe("characters");
    expect(validateAgentTypeName("review\ter")).toBe("characters");
  });

  describe("agentModelSelectionIsAvailable", () => {
    const providers = [
      provider("provider_a", ["gpt-4o", "gpt-5"]),
      provider("provider_b", ["claude-x"]),
      provider("provider_off", ["gpt-4o"], false)
    ];
    const explicit = (providerId: string, modelId: string): AgentModelSelection => ({
      kind: "explicit",
      providerId,
      modelId
    });

    it("always resolves an inherited model, whatever providers exist", () => {
      expect(agentModelSelectionIsAvailable({ kind: "inherit" }, providers)).toBe(true);
      expect(agentModelSelectionIsAvailable({ kind: "inherit" }, [])).toBe(true);
    });

    it("never resolves an unavailable binding: there is no pair left to re-check", () => {
      expect(agentModelSelectionIsAvailable({ kind: "unavailable" }, providers)).toBe(false);
      expect(agentModelSelectionIsAvailable({ kind: "unavailable" }, [])).toBe(false);
    });

    it("resolves an explicit pair only when an enabled provider lists the model", () => {
      expect(agentModelSelectionIsAvailable(explicit("provider_a", "gpt-4o"), providers)).toBe(true);
      expect(agentModelSelectionIsAvailable(explicit("provider_b", "claude-x"), providers)).toBe(true);
      // Provider missing.
      expect(agentModelSelectionIsAvailable(explicit("provider_gone", "gpt-4o"), providers)).toBe(false);
      expect(agentModelSelectionIsAvailable(explicit("provider_a", "gpt-4o"), [])).toBe(false);
      // Provider disabled: the model is listed, but nothing may call it.
      expect(agentModelSelectionIsAvailable(explicit("provider_off", "gpt-4o"), providers)).toBe(false);
      // Model missing under an enabled provider.
      expect(agentModelSelectionIsAvailable(explicit("provider_a", "gpt-6"), providers)).toBe(false);
    });

    it("matches the (provider, model) pair, never the bare model id", () => {
      // `gpt-4o` exists under provider_a, but provider_b does not carry it: a
      // role bound to provider_b must not silently rebind to the other one.
      expect(agentModelSelectionIsAvailable(explicit("provider_b", "gpt-4o"), providers)).toBe(false);
      expect(agentModelSelectionIsAvailable(explicit("provider_a", "claude-x"), providers)).toBe(false);
    });
  });

  describe("hasUsableAgentRole", () => {
    const providers = [provider("provider_a", ["gpt-4o"])];
    const bound = (providerId: string, modelId: string) => role({
      modelSelection: { kind: "explicit", providerId, modelId }
    });

    it("counts only the roles the conversation selected", () => {
      const agents = [resource("agent_a"), resource("agent_b")];
      expect(hasUsableAgentRole(agents, ["agent_a"], providers)).toBe(true);
      expect(hasUsableAgentRole(agents, ["agent_b"], providers)).toBe(true);
      expect(hasUsableAgentRole(agents, [], providers)).toBe(false);
      // A dangling id selects nothing the catalog lists.
      expect(hasUsableAgentRole(agents, ["agent_gone"], providers)).toBe(false);
      expect(hasUsableAgentRole([], ["agent_a"], providers)).toBe(false);
    });

    it("ignores a selected role whose file could not be used", () => {
      const agents = [
        resource("agent_broken", { available: false, role: null, description: "invalid JSON" }),
        // `available` and `role` are separate fields on the wire: either one
        // alone is enough to make the row uncallable.
        resource("agent_no_body", { role: null }),
        resource("agent_flagged", { available: false })
      ];
      expect(hasUsableAgentRole(
        agents,
        ["agent_broken", "agent_no_body", "agent_flagged"],
        providers
      )).toBe(false);
      // One usable role among them is enough.
      expect(hasUsableAgentRole(
        [...agents, resource("agent_ok")],
        ["agent_broken", "agent_ok"],
        providers
      )).toBe(true);
    });

    it("requires the role's model to resolve", () => {
      const agents = [
        resource("agent_explicit", { role: bound("provider_a", "gpt-4o") }),
        resource("agent_dead_model", { role: bound("provider_a", "gpt-gone") }),
        resource("agent_dead_provider", { role: bound("provider_gone", "gpt-4o") }),
        resource("agent_unavailable", { role: role({ modelSelection: { kind: "unavailable" } }) })
      ];
      expect(hasUsableAgentRole(agents, ["agent_explicit"], providers)).toBe(true);
      expect(hasUsableAgentRole(agents, ["agent_dead_model"], providers)).toBe(false);
      expect(hasUsableAgentRole(agents, ["agent_dead_provider"], providers)).toBe(false);
      expect(hasUsableAgentRole(agents, ["agent_unavailable"], providers)).toBe(false);
      // The same role becomes usable the moment its provider is enabled.
      expect(hasUsableAgentRole(agents, ["agent_dead_provider"], [
        ...providers,
        provider("provider_gone", ["gpt-4o"])
      ])).toBe(true);
      expect(hasUsableAgentRole(agents, ["agent_explicit"], [
        provider("provider_a", ["gpt-4o"], false)
      ])).toBe(false);
    });

    it("narrows to the conversation's own workspaces, as the count does", () => {
      const agents = [
        resource("agent_global"),
        resource("agent_here", { source: "workspace", workspaceKey: "local|/work/a" }),
        resource("agent_elsewhere", { source: "workspace", workspaceKey: "local|/work/c" })
      ];
      const keys = ["local|/work/a"];
      // The host never offers a role of a workspace the conversation does not
      // have, so selecting only that one leaves nothing to call.
      expect(hasUsableAgentRole(agents, ["agent_elsewhere"], providers, keys)).toBe(false);
      expect(hasUsableAgentRole(agents, ["agent_elsewhere"], providers, [])).toBe(false);
      // Global roles are always reachable, and so is a workspace's own.
      expect(hasUsableAgentRole(agents, ["agent_global"], providers, [])).toBe(true);
      expect(hasUsableAgentRole(agents, ["agent_here"], providers, keys)).toBe(true);
      expect(hasUsableAgentRole(agents, ["agent_here"], providers, [])).toBe(false);
      expect(hasUsableAgentRole(agents, ["agent_elsewhere", "agent_here"], providers, keys)).toBe(true);
      // A preset points at no workspace in particular, so nothing is narrowed.
      expect(hasUsableAgentRole(agents, ["agent_elsewhere"], providers)).toBe(true);
    });
  });

  describe("selectedAgentRoleCount", () => {
    const agents = [resource("agent_a"), resource("agent_b"), resource("agent_c")];

    it("counts the selected ids the catalog lists", () => {
      expect(selectedAgentRoleCount(agents, [])).toBe(0);
      expect(selectedAgentRoleCount(agents, ["agent_a"])).toBe(1);
      expect(selectedAgentRoleCount(agents, ["agent_a", "agent_c"])).toBe(2);
      expect(selectedAgentRoleCount(agents, ["agent_c", "agent_b", "agent_a"])).toBe(3);
    });

    it("does not count a dangling id: nothing can call that role", () => {
      expect(selectedAgentRoleCount(agents, ["agent_a", "agent_gone"])).toBe(1);
      expect(selectedAgentRoleCount(agents, ["agent_gone"])).toBe(0);
      expect(selectedAgentRoleCount([], ["agent_a"])).toBe(0);
    });

    it("counts a duplicated id once", () => {
      expect(selectedAgentRoleCount(agents, ["agent_a", "agent_a", "agent_b"])).toBe(2);
    });

    it("counts a listed role whose file is unusable: it is still a row the user can untick", () => {
      const withBroken = [
        ...agents,
        resource("agent_broken", { available: false, role: null })
      ];
      expect(selectedAgentRoleCount(withBroken, ["agent_a", "agent_broken"])).toBe(2);
    });

    it("narrows to the conversation's own workspaces the way the roles page does", () => {
      const scoped = [
        resource("agent_global"),
        resource("agent_here", { source: "workspace", workspaceKey: "local|/work/a" }),
        resource("agent_elsewhere", { source: "workspace", workspaceKey: "local|/work/c" })
      ];
      const ids = ["agent_global", "agent_here", "agent_elsewhere"];
      // A role of a workspace this conversation does not have is dangling here.
      expect(selectedAgentRoleCount(scoped, ids, ["local|/work/a"])).toBe(2);
      // A draft with no workspace yet reaches the global level alone.
      expect(selectedAgentRoleCount(scoped, ids, [])).toBe(1);
      // A preset points at no workspace in particular, so nothing is narrowed.
      expect(selectedAgentRoleCount(scoped, ids)).toBe(3);
    });
  });

  it("canonicalizes tool names to a sorted set so a reorder is not mistaken for an edit", () => {
    expect(canonicalAgentToolNames(["b_tool", "a_tool", "b_tool", "c_tool"]))
      .toEqual(["a_tool", "b_tool", "c_tool"]);
    expect(canonicalAgentToolNames([])).toEqual([]);
    // The result is a new array, so sorting it never reorders the caller's list.
    const input = ["z_tool", "a_tool"];
    const canonical = canonicalAgentToolNames(input);
    expect(canonical).not.toBe(input);
    expect(input).toEqual(["z_tool", "a_tool"]);
    expect(canonicalAgentToolNames(["b_tool", "a_tool"]))
      .toEqual(canonicalAgentToolNames(["a_tool", "b_tool", "a_tool"]));
  });

  describe("defaultAgentRoleWebSearch", () => {
    it("opens a new role on native search and fetch with the shared result shaping", () => {
      expect(defaultAgentRoleWebSearch()).toEqual({
        maxSearchesPerCall: 0,
        provider: { kind: "native" },
        fetchProvider: { kind: "native" },
        nativeSearchTool: NATIVE_SEARCH_TOOLS[0],
        nativeFetchTool: NATIVE_FETCH_TOOLS[0],
        maxResults: DEFAULT_SEARCH_MAX_RESULTS,
        compressionCutoff: DEFAULT_SEARCH_COMPRESSION_CUTOFF,
        fetchCompressionCutoff: DEFAULT_SEARCH_COMPRESSION_CUTOFF,
        domainFilter: "off",
        includeDomains: [],
        excludeDomains: []
      });
    });

    it("returns a fresh object every call, so an edit never leaks into the next new role", () => {
      const first = defaultAgentRoleWebSearch();
      const second = defaultAgentRoleWebSearch();
      expect(second).not.toBe(first);
      expect(second.provider).not.toBe(first.provider);
      expect(second.fetchProvider).not.toBe(first.fetchProvider);
      expect(second.includeDomains).not.toBe(first.includeDomains);
      expect(second.excludeDomains).not.toBe(first.excludeDomains);
      first.includeDomains.push("example.com");
      first.provider = { kind: "unavailable" };
      first.maxSearchesPerCall = 9;
      expect(defaultAgentRoleWebSearch()).toEqual(second);
    });
  });

  describe("sameAgentRole", () => {
    it("treats identical bodies, and a deep copy, as the same", () => {
      expect(sameAgentRole(role(), role())).toBe(true);
      const original = role({
        modelSelection: { kind: "explicit", providerId: "provider_a", modelId: "gpt-4o" },
        effort: "high",
        skillIds: ["skill_a", "skill_b"]
      });
      expect(sameAgentRole(original, cloneAgentRole(original))).toBe(true);
    });

    it("ignores object key order at every depth", () => {
      const original = role({
        modelSelection: { kind: "explicit", providerId: "provider_a", modelId: "gpt-4o" }
      });
      const reordered = {
        toolDescriptionFileId: original.toolDescriptionFileId,
        templateId: original.templateId,
        webSearch: Object.fromEntries(Object.entries(original.webSearch).reverse()),
        hookIds: original.hookIds,
        mcpIds: original.mcpIds,
        skillIds: original.skillIds,
        disallowedTools: original.disallowedTools,
        tools: original.tools,
        effort: original.effort,
        modelSelection: { modelId: "gpt-4o", providerId: "provider_a", kind: "explicit" },
        description: original.description,
        name: original.name
      } as unknown as AgentRole;
      expect(Object.keys(reordered)).not.toEqual(Object.keys(original));
      expect(sameAgentRole(original, reordered)).toBe(true);
    });

    it("keeps list order significant: order is meaning in every list", () => {
      expect(sameAgentRole(
        role({ skillIds: ["skill_a", "skill_b"] }),
        role({ skillIds: ["skill_b", "skill_a"] })
      )).toBe(false);
      expect(sameAgentRole(
        role({ webSearch: { ...defaultAgentRoleWebSearch(), includeDomains: ["a.com", "b.com"] } }),
        role({ webSearch: { ...defaultAgentRoleWebSearch(), includeDomains: ["b.com", "a.com"] } })
      )).toBe(false);
    });

    it.each<[string, Partial<AgentRole>]>([
      ["name", { name: "auditor" }],
      ["description", { description: "something else" }],
      ["model selection kind", { modelSelection: { kind: "unavailable" } }],
      ["model selection pair", {
        modelSelection: { kind: "explicit", providerId: "provider_a", modelId: "gpt-5" }
      }],
      ["effort", { effort: "low" }],
      ["tool allowlist", { tools: ["read_file", "write_file"] }],
      ["tool deny list", { disallowedTools: ["run_command"] }],
      ["skills", { skillIds: ["skill_a"] }],
      ["MCP servers", { mcpIds: ["mcp_a"] }],
      ["hooks", { hookIds: ["hook_a"] }],
      ["search backend", { webSearch: { ...defaultAgentRoleWebSearch(), provider: { kind: "unavailable" } } }],
      ["search call limit", { webSearch: { ...defaultAgentRoleWebSearch(), maxSearchesPerCall: 3 } }],
      ["domain filter", { webSearch: { ...defaultAgentRoleWebSearch(), domainFilter: "include" } }],
      ["domain list", { webSearch: { ...defaultAgentRoleWebSearch(), excludeDomains: ["ads.example"] } }],
      ["template", { templateId: "template_a" }],
      ["tool-description file", { toolDescriptionFileId: "tooldesc_builtin_concise_en_us" }]
    ])("detects a change to the %s", (_label, change) => {
      const base = role({
        modelSelection: { kind: "explicit", providerId: "provider_a", modelId: "gpt-4o" }
      });
      expect(sameAgentRole(base, { ...base, ...change })).toBe(false);
      expect(sameAgentRole({ ...base, ...change }, base)).toBe(false);
    });

    it("tells an absent effort from a set one, and an empty list from a populated one", () => {
      expect(sameAgentRole(role({ effort: null }), role({ effort: "medium" }))).toBe(false);
      expect(sameAgentRole(role({ tools: [] }), role({ tools: ["read_file"] }))).toBe(false);
    });

    it("tells one tool-description file from another, and following the caller from naming the guided built-in", () => {
      const named = (toolDescriptionFileId: string | null) => role({ toolDescriptionFileId });
      expect(sameAgentRole(named("tooldesc_builtin_concise_en_us"), named("tooldesc_builtin_concise_en_us"))).toBe(true);
      expect(sameAgentRole(named("tooldesc_builtin_concise_en_us"), named("tooldesc_user_main_0f0f0f0f"))).toBe(false);
      // For a role these differ: the guided id pins the default under a caller on another file.
      expect(sameAgentRole(named(null), named("tooldesc_builtin_en_us"))).toBe(false);
    });
  });

  describe("cloneAgentRole", () => {
    it("copies every nested value, so editing the copy never touches the catalog", () => {
      const original = role({
        modelSelection: { kind: "explicit", providerId: "provider_a", modelId: "gpt-4o" },
        tools: ["read_file"],
        disallowedTools: ["run_command"],
        skillIds: ["skill_a"],
        mcpIds: ["mcp_a"],
        hookIds: ["hook_a"],
        effort: "high",
        templateId: "template_a",
        toolDescriptionFileId: "tooldesc_builtin_concise_en_us"
      });
      original.webSearch.includeDomains = ["a.example"];
      const snapshot = structuredClone(original);

      const copy = cloneAgentRole(original);

      expect(copy).toEqual(original);
      expect(copy.toolDescriptionFileId).toBe("tooldesc_builtin_concise_en_us");
      expect(copy).not.toBe(original);
      expect(copy.modelSelection).not.toBe(original.modelSelection);
      expect(copy.webSearch).not.toBe(original.webSearch);
      expect(copy.webSearch.provider).not.toBe(original.webSearch.provider);
      for (const key of ["tools", "disallowedTools", "skillIds", "mcpIds", "hookIds"] as const) {
        expect(copy[key]).not.toBe(original[key]);
      }
      copy.tools.push("write_file");
      copy.disallowedTools.length = 0;
      copy.skillIds.push("skill_b");
      copy.mcpIds.push("mcp_b");
      copy.hookIds.push("hook_b");
      copy.modelSelection = { kind: "inherit" };
      copy.webSearch.includeDomains.push("b.example");
      copy.webSearch.provider = { kind: "unavailable" };
      copy.name = "renamed";
      expect(original).toEqual(snapshot);
    });
  });
});
