import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createTestDocument as createSeedDocument } from "../test/fixtures";
import { BUILTIN_AGENT_ROLE_IDS } from "../seed";
import type { AgentRoleResource, ApiProvider, AppDocument, ContextItem, ModelProfile, SubagentRunRecord } from "../types";

const tauriMocks = vi.hoisted(() => ({ invoke: vi.fn() }));

vi.mock("@tauri-apps/api/core", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@tauri-apps/api/core")>()),
  invoke: tauriMocks.invoke
}));

import { CLAUDE_AGENT_REGISTRY } from "./claudeAgentProvider";
import { DEFAULT_SEARCH_COMPRESSION_CUTOFF } from "./searchProviders";
import { deleteAgentRole, deleteApiKey, defaultConversationWebSearchSettings, fetchModels, flushDocumentSaves, getStoredApiKeyLength, imageAttachmentData, loadDocument, normalizeAgentRole, normalizeDocument, prepareImageAttachment, refreshCapabilities, resetDocument, revealApiKey, runModel, saveAgentRole, saveApiKey, saveDocument } from "./runtime";

const STORAGE_KEY = "mewrk.document.v1";

function persistedPresetSettings(payload: unknown) {
  const persisted = payload as {
    presets: {
      conversationPresets: Array<{ id: string; settings: { toolDescriptionFileId: string | null } }>;
      defaultConversationPresetId: string;
    };
  };
  return (persisted.presets.conversationPresets.find(
    (preset) => preset.id === persisted.presets.defaultConversationPresetId
  ) ?? persisted.presets.conversationPresets[0]).settings;
}

function defaultPresetSettings(document: AppDocument) {
  return (document.globalSettings.conversationPresets.find(
    (preset) => preset.id === document.globalSettings.defaultConversationPresetId
  ) ?? document.globalSettings.conversationPresets[0]).settings;
}

/** A role file's body as the host sends it: every key present. */
function wireRole(overrides: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    name: "reviewer",
    description: "Reads a diff.",
    modelSelection: { kind: "inherit" },
    effort: null,
    tools: ["read_file"],
    disallowedTools: [],
    skillIds: [],
    mcpIds: [],
    hookIds: [],
    webSearch: defaultConversationWebSearchSettings(),
    templateId: null,
    ...overrides
  };
}

/** One catalog row for a usable role, as the host lists it. */
function wireRoleEntry(id: string, overrides: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    id,
    name: id,
    description: "",
    location: `C:/Users/u/.mewrk/agents/${id}.json`,
    source: "user",
    available: true,
    role: wireRole({ name: id }),
    ...overrides
  };
}

/** The catalog's role rows after a document carrying `entries` is normalized. */
function normalizedAgents(entries: unknown): AgentRoleResource[] {
  const document = createSeedDocument() as unknown as { capabilities: Record<string, unknown> };
  document.capabilities.agents = entries;
  return normalizeDocument(document as unknown as AppDocument).capabilities.agents;
}

describe("document normalization", () => {
  beforeEach(() => {
    tauriMocks.invoke.mockReset();
    Reflect.deleteProperty(window, "__TAURI_INTERNALS__");
    window.localStorage.clear();
  });

  afterEach(() => {
    Reflect.deleteProperty(window, "__TAURI_INTERNALS__");
    window.localStorage.clear();
  });

  /**
   * Skills and MCP servers are no longer document records: they are files the
   * user owns, discovered by the host. Loading therefore neither normalizes them
   * nor resurrects a retired registry — a document still carrying the old
   * `assets.skills` / `assets.mcpServers` anchors simply drops them.
   */
  it("drops retired skill and MCP registries instead of normalizing them", () => {
    const document = createSeedDocument() as unknown as Record<string, unknown>;
    const globalSettings = document.globalSettings as Record<string, unknown>;
    globalSettings.mcpServers = [{ id: "blank_stdio", name: "", transport: "stdio", command: "" }];
    globalSettings.skills = [{ id: "skill_old", name: "Old", folderName: "old" }];

    const normalized = normalizeDocument(document as unknown as AppDocument).globalSettings;

    expect(normalized).not.toHaveProperty("mcpServers");
    expect(normalized).not.toHaveProperty("skills");
  });

  /**
   * A workspace widens a conversation's filesystem boundary, so a malformed
   * entry is dropped rather than repaired: the host re-checks every one against
   * its own authorization record and would refuse the save anyway.
   */
  it("keeps only well-formed, deduplicated attached workspaces", () => {
    const document = createSeedDocument();
    const conversation = document.workspaces[0].conversations[0] as unknown as Record<string, unknown>;
    conversation.attachedWorkspaces = [
      { path: "D:/shared/lib" },
      { path: "  D:/docs  " },
      { path: "D:/shared/lib" },
      { path: "" },
      { path: 42 },
      { path: "D:/".padEnd(5000, "x") },
      // The same spelling on another machine is another directory, so it stays.
      { machine: { kind: "ssh", machineId: "m1" }, path: "D:/shared/lib" },
      { machine: { kind: "nonsense" }, path: "D:/kept-as-local" }
    ];

    const normalized = normalizeDocument(document).workspaces[0].conversations[0];

    expect(normalized.attachedWorkspaces).toEqual([
      { path: "D:/shared/lib" },
      { path: "D:/docs" },
      { machine: { kind: "ssh", machineId: "m1" }, path: "D:/shared/lib" },
      { path: "D:/kept-as-local" }
    ]);
  });

  /**
   * A document written before workspaces carried a machine is the only record
   * those grants have until it is next saved, so the legacy list is read as
   * host-machine entries rather than dropped.
   */
  it("folds pre-multi-machine extra directories into host-machine workspaces", () => {
    const document = createSeedDocument();
    const conversation = document.workspaces[0].conversations[0] as unknown as Record<string, unknown>;
    delete conversation.attachedWorkspaces;
    conversation.additionalDirectories = ["D:/shared/lib", "D:/docs"];

    expect(normalizeDocument(document).workspaces[0].conversations[0].attachedWorkspaces)
      .toEqual([{ path: "D:/shared/lib" }, { path: "D:/docs" }]);
  });

  it("defaults a conversation without attached workspaces to none", () => {
    const document = createSeedDocument();
    const conversation = document.workspaces[0].conversations[0] as unknown as Record<string, unknown>;
    delete conversation.attachedWorkspaces;
    delete conversation.additionalDirectories;

    expect(normalizeDocument(document).workspaces[0].conversations[0].attachedWorkspaces)
      .toEqual([]);
  });

  /**
   * The sidebar workspace is rebuilt field by field, and `machine` once fell
   * through that list. The on-disk record and the host were both right; only
   * the loaded copy claimed the remote directory was local, and the next save
   * was refused ("Workspace path must be absolute", a POSIX path on Windows)
   * for the rest of the session. A malformed binding is dropped rather than
   * kept, which is the attached-workspace rule too.
   */
  /**
   * Variables once belonged to a machine. They now belong to each workspace, so
   * a table keyed by a bare machine is spread over every workspace on it that
   * has none of its own — the commands keep the variables they ran with — and
   * the machine key, which the host never reads, is dropped.
   */
  it("spreads a machine's variables over the workspaces on it", () => {
    const document = createSeedDocument();
    const project = document.workspaces[0];
    project.additionalWorkspaces = [
      { machine: { kind: "ssh", machineId: "m1" }, path: "/srv/api" },
      { path: "D:/shared/lib" }
    ];
    project.conversations[0].attachedWorkspaces = [{ machine: { kind: "wsl", distro: "Ubuntu" }, path: "/home/dev/x" }];
    (document as unknown as { globalSettings: { executionEnvironments: unknown } }).globalSettings.executionEnvironments = {
      sshMachines: [],
      envVars: {
        local: { PROXY: "http://proxy" },
        "ssh:m1": { K: "v" },
        "wsl:Ubuntu": { W: "1" },
        "wsl:Debian": { UNUSED: "1" },
        // A workspace that already has its own table keeps it.
        "local|D:/shared/lib": { OWN: "yes" },
        "docker:x|/srv": { BAD: "1" }
      }
    };

    expect(normalizeDocument(document).globalSettings.executionEnvironments.envVars).toEqual({
      [`local|${project.path}`]: { PROXY: "http://proxy" },
      "local|D:/shared/lib": { OWN: "yes" },
      "ssh:m1|/srv/api": { K: "v" },
      "wsl:Ubuntu|/home/dev/x": { W: "1" }
    });
  });

  it("keeps a sidebar workspace on its machine through normalization", () => {
    const document = createSeedDocument();
    const remote = { ...document.workspaces[0], id: "ws_remote", path: "/home/dev/app", conversations: [] };
    (remote as unknown as Record<string, unknown>).machine = { kind: "ssh", machineId: "m1" };
    const malformed = { ...document.workspaces[0], id: "ws_odd", path: "D:/local", conversations: [] };
    (malformed as unknown as Record<string, unknown>).machine = { kind: "nonsense" };
    document.workspaces.push(remote, malformed);

    const workspaces = normalizeDocument(document).workspaces;
    expect(workspaces.find((workspace) => workspace.id === "ws_remote")?.machine)
      .toEqual({ kind: "ssh", machineId: "m1" });
    expect(workspaces.find((workspace) => workspace.id === "ws_odd")).not.toHaveProperty("machine");
    expect(workspaces[0]).not.toHaveProperty("machine");
  });

  /**
   * A project's later workspaces are rebuilt through the same field-by-field
   * list as its first, so they need the same care: dropping them on load would
   * make the next save quietly shrink every conversation of the project.
   */
  it("keeps a project's later workspaces through normalization", () => {
    const document = createSeedDocument();
    const project = document.workspaces[0] as unknown as Record<string, unknown>;
    project.additionalWorkspaces = [
      { machine: { kind: "ssh", machineId: "m1" }, path: "/srv/api" },
      { path: "  D:/shared/lib  " },
      // The project's own first directory is not a second workspace.
      { path: document.workspaces[0].path },
      { path: "" },
      { machine: { kind: "ssh", machineId: "m1" }, path: "/srv/api" }
    ];

    const [first] = normalizeDocument(document).workspaces;
    expect(first.additionalWorkspaces).toEqual([
      { machine: { kind: "ssh", machineId: "m1" }, path: "/srv/api" },
      { path: "D:/shared/lib" }
    ]);
    expect(normalizeDocument(createSeedDocument()).workspaces[0]).not.toHaveProperty("additionalWorkspaces");
  });

  /**
   * A workspace-sourced descriptor is the only thing that tells the renderer which
   * conversations may select it, so normalization must not flatten it away.
   */
  it("keeps a catalog descriptor's workspaceKey through normalization", () => {
    const document = createSeedDocument();
    document.capabilities.skills = [
      {
        id: "skill_workspace",
        name: "workspace-notes",
        description: "",
        location: "C:/repo/.mewrk/skills/workspace-notes/SKILL.md",
        source: "workspace",
        available: true,
        workspaceKey: "local|C:/repo"
      },
      {
        id: "skill_global",
        name: "code-review",
        description: "",
        location: "C:/Users/u/.mewrk/skills/code-review/SKILL.md",
        source: "user",
        available: true
      }
    ];

    const skills = normalizeDocument(document).capabilities.skills;

    expect(skills[0].workspaceKey).toBe("local|C:/repo");
    expect(skills[1].workspaceKey).toBeUndefined();
  });

  it("keeps exactly one disabled Codex and Claude Agent provider during normalization", () => {
    const missing = createSeedDocument();
    missing.globalSettings.apiProviders = [];
    const generated = normalizeDocument(missing).globalSettings.apiProviders;
    expect(generated).toHaveLength(2);
    expect(generated[0]).toMatchObject({ name: "OpenAI Codex", family: "openai_codex", enabled: false });
    expect(generated[1]).toMatchObject({ name: "Claude Agent", family: "claude_agent", enabled: false });
    expect(generated[0].id).toMatch(/^provider_/u);
    expect(generated[1].id).toMatch(/^provider_/u);

    const existing = { ...generated[0], id: "codex-kept", enabled: true };
    const agent = { ...generated[1], id: "agent-kept", enabled: true };
    const duplicated = createSeedDocument();
    duplicated.globalSettings.apiProviders = [
      existing,
      { ...existing, id: "codex-later" },
      agent,
      { ...agent, id: "agent-later" }
    ];
    const normalized = normalizeDocument(duplicated).globalSettings.apiProviders;
    expect(normalized).toEqual([existing, agent]);
    expect(normalized[0].id).toBe("codex-kept");
    expect(normalized[1].id).toBe("agent-kept");
  });

  it("flattens a base URL stored on the Claude Agent row", () => {
    // The local CLI picks the endpoint. An address left over from the version that
    // still offered one is dropped rather than rejected: the host ignores it either
    // way, and keeping it would let the settings pane imply it still has an effect.
    const source = createSeedDocument();
    const agent = normalizeDocument(source).globalSettings.apiProviders
      .find((provider) => provider.family === "claude_agent")!;
    source.globalSettings.apiProviders = [{ ...agent, baseUrl: "https://api.anthropic.com" }];

    const normalized = normalizeDocument(source).globalSettings.apiProviders
      .find((provider) => provider.family === "claude_agent")!;
    expect(normalized.baseUrl).toBe("");
  });

  it("opens a provider written with the retired endpoint overrides, and drops them", () => {
    // Image, speech and transcription addresses were configurable but never read.
    // A document that still carries them opens, and a provider has one address.
    const source = createSeedDocument();
    const [first, ...rest] = source.globalSettings.apiProviders;
    source.globalSettings.apiProviders = [
      {
        ...first,
        endpointBaseUrls: { openai_image_generation: "https://images.example.com/v1" }
      } as unknown as ApiProvider,
      ...rest
    ];

    const normalized = normalizeDocument(source).globalSettings.apiProviders
      .find((provider) => provider.id === first.id)!;
    expect(normalized).toMatchObject({ id: first.id, baseUrl: first.baseUrl });
    expect(normalized).not.toHaveProperty("endpointBaseUrls");
  });

  it("normalizes invalid current appearance preferences to new-document defaults", () => {
    const current = createSeedDocument() as unknown as {
      globalSettings: Record<string, unknown>;
    };
    current.globalSettings.appLanguage = "fr-FR";
    current.globalSettings.resolvedAppLanguage = "auto";
    current.globalSettings.theme = "sepia";

    expect(normalizeDocument(current).globalSettings).toMatchObject({
      appLanguage: "auto",
      resolvedAppLanguage: "zh-CN",
      theme: "system"
    });
  });

  it("restores missing web-search assets with catalog defaults", () => {
    const current = createSeedDocument() as unknown as { globalSettings: Record<string, unknown> };
    delete current.globalSettings.webSearch;
    // Restore a missing asset layer from seed defaults, including the full catalog and
    // its two anonymously available enabled providers.
    expect(normalizeDocument(current).globalSettings.webSearch).toEqual(
      createSeedDocument().globalSettings.webSearch
    );
  });

  /**
   * Subagent roles are JSON files the host discovers, so the document carries
   * them only as catalog rows and a conversation selects them by id. These tests
   * pin how a row is read back: a missing section is an empty one, a row the host
   * could not read stays visible without a body, and a body is made total.
   */
  it("reads a catalog without a roles section as having no roles", () => {
    const document = createSeedDocument() as unknown as { capabilities?: Record<string, unknown> };
    delete document.capabilities!.agents;
    expect(normalizeDocument(document as unknown as AppDocument).capabilities.agents).toEqual([]);
    // Not a list is no list.
    document.capabilities!.agents = { id: "agent_a" };
    expect(normalizeDocument(document as unknown as AppDocument).capabilities.agents).toEqual([]);
    // No catalog at all falls back to the seed's, which lists none.
    delete document.capabilities;
    expect(normalizeDocument(document as unknown as AppDocument).capabilities.agents).toEqual([]);
  });

  it("drops catalog role rows that cannot be selected and keeps the first of a repeated id", () => {
    const agents = normalizedAgents([
      wireRoleEntry("agent_a", { description: "first" }),
      { ...wireRoleEntry("agent_noid"), id: undefined },
      { ...wireRoleEntry("agent_blank"), id: "" },
      { ...wireRoleEntry("agent_numeric"), id: 7 },
      null,
      42,
      "agent_string",
      ["agent_array"],
      wireRoleEntry("agent_a", { description: "second" }),
      wireRoleEntry("agent_b")
    ]);

    expect(agents.map((agent) => agent.id)).toEqual(["agent_a", "agent_b"]);
    expect(agents[0].description).toBe("first");
  });

  it("keeps an unreadable role as a row without a body, its reason in the description", () => {
    const [broken, hollow] = normalizedAgents([
      wireRoleEntry("agent_broken", {
        description: "expected value at line 3",
        available: false,
        // A body riding along with `available: false` is never trusted.
        role: wireRole({ name: "ghost" })
      }),
      // The reverse: a row that claims to be usable but carries no body is not.
      wireRoleEntry("agent_hollow", { role: null })
    ]);

    expect(broken).toEqual({
      id: "agent_broken",
      name: "agent_broken",
      description: "expected value at line 3",
      location: "C:/Users/u/.mewrk/agents/agent_broken.json",
      source: "user",
      available: false,
      role: null
    });
    expect(hollow).toMatchObject({ id: "agent_hollow", available: false, role: null });
  });

  it("passes a complete role row through unchanged, and a second normalization changes nothing", () => {
    const entry = wireRoleEntry("agent_full", {
      source: "workspace",
      workspaceKey: "local|C:/repo",
      description: "Reviews diffs.",
      role: wireRole({
        name: "Reviewer",
        // Prose is written through verbatim: no trim, no reflow.
        description: "  第一行。\n\n第三行 - 带横线。  ",
        modelSelection: { kind: "explicit", providerId: "provider_a", modelId: "gpt-4o" },
        effort: "high",
        tools: ["read_file", "search_files"],
        disallowedTools: ["run_command"],
        skillIds: ["skill_a"],
        mcpIds: ["mcp_a"],
        hookIds: ["hook_a"],
        webSearch: {
          ...defaultConversationWebSearchSettings(),
          maxSearchesPerCall: 4,
          provider: { kind: "explicit", providerKind: "tavily" },
          fetchProvider: { kind: "explicit", providerKind: "jina" },
          domainFilter: "exclude",
          excludeDomains: ["ads.example"]
        },
        templateId: "template_a"
      })
    });

    const once = normalizedAgents([entry]);
    expect(once).toEqual([entry]);
    expect(normalizedAgents(once)).toEqual(once);
  });

  it("fills a role body's missing keys with the file format's defaults", () => {
    const [sparse, unnamed] = normalizedAgents([
      { id: "agent_sparse", role: { name: "sparse" } },
      { id: "agent_unnamed", available: false }
    ]);

    expect(sparse).toEqual({
      id: "agent_sparse",
      name: "sparse",
      description: "",
      location: "",
      source: "user",
      available: true,
      role: {
        name: "sparse",
        description: "",
        modelSelection: { kind: "inherit" },
        effort: null,
        tools: [],
        disallowedTools: [],
        skillIds: [],
        mcpIds: [],
        hookIds: [],
        webSearch: defaultConversationWebSearchSettings(),
        templateId: null
      }
    });
    // A row with nothing to call itself by is listed under its id.
    expect(unnamed).toEqual({
      id: "agent_unnamed",
      name: "agent_unnamed",
      description: "",
      location: "",
      source: "user",
      available: false,
      role: null
    });
  });

  it.each([
    ["builtin", "builtin"],
    ["user", "user"],
    ["workspace", "workspace"],
    ["absent", "user"],
    ["unknown", "user"]
  ])("reads a role row whose source is %s as %s", (stored, expected) => {
    const [agent] = normalizedAgents([wireRoleEntry("agent_a", {
      source: stored === "absent" ? undefined : stored
    })]);
    expect(agent.source).toBe(expected);
  });

  it.each([
    ["kept", { workspaceKey: "local|C:/repo" }, "local|C:/repo"],
    ["absent", {}, undefined],
    ["blank", { workspaceKey: "" }, undefined],
    ["not a string", { workspaceKey: 7 }, undefined]
  ])("reads a role row's workspaceKey that is %s", (_label, override, expected) => {
    const [agent] = normalizedAgents([wireRoleEntry("agent_a", override)]);
    if (expected === undefined) {
      expect(agent).not.toHaveProperty("workspaceKey");
    } else {
      expect(agent.workspaceKey).toBe(expected);
    }
  });

  it.each([
    ["absent", undefined, { kind: "inherit" }],
    ["inherit", { kind: "inherit" }, { kind: "inherit" }],
    [
      "an explicit pair",
      { kind: "explicit", providerId: "provider:/精确", modelId: "kimi/vision:v4-模型" },
      { kind: "explicit", providerId: "provider:/精确", modelId: "kimi/vision:v4-模型" }
    ],
    // Only the shape is checked. At rest there is no telling a signed-out
    // provider whose catalog was not fetched from a model that is gone, so a
    // pair that does not currently resolve waits instead of being destroyed;
    // availability is asked at render and at call time.
    [
      "a pair whose provider does not exist",
      { kind: "explicit", providerId: "provider_gone", modelId: "gpt-4o" },
      { kind: "explicit", providerId: "provider_gone", modelId: "gpt-4o" }
    ],
    ["already unavailable", { kind: "unavailable" }, { kind: "unavailable" }],
    // A malformed binding carries no usable identifier, so it must not read as
    // a working default.
    ["an unknown kind", { kind: "surprise" }, { kind: "unavailable" }],
    ["a pair without a model", { kind: "explicit", providerId: "provider_a" }, { kind: "unavailable" }],
    ["a pair without a provider", { kind: "explicit", modelId: "gpt-4o" }, { kind: "unavailable" }],
    [
      "a pair with a blank model",
      { kind: "explicit", providerId: "provider_a", modelId: "" },
      { kind: "unavailable" }
    ],
    [
      "a pair with a padded provider id",
      { kind: "explicit", providerId: " provider_a", modelId: "gpt-4o" },
      { kind: "unavailable" }
    ],
    [
      "a pair with a padded model id",
      { kind: "explicit", providerId: "provider_a", modelId: "gpt-4o " },
      { kind: "unavailable" }
    ],
    [
      "a pair with a non-string id",
      { kind: "explicit", providerId: 7, modelId: "gpt-4o" },
      { kind: "unavailable" }
    ]
  ])("reads a role's model binding that is %s", (_label, stored, expected) => {
    const [agent] = normalizedAgents([
      wireRoleEntry("agent_a", { role: wireRole({ modelSelection: stored }) })
    ]);
    expect(agent.role?.modelSelection).toEqual(expected);
  });

  it.each([
    ["absent", undefined, null],
    ["null", null, null],
    ["a known level", "high", "high"],
    // The same spellings a conversation's effort accepts.
    ["a retired spelling", "xhigh", "extra"],
    // A malformed effort rides the caller's, the restrictive reading.
    ["an unknown level", "extreme", null],
    ["not a string", 3, null]
  ])("reads a role's reasoning effort that is %s", (_label, stored, expected) => {
    const [agent] = normalizedAgents([
      wireRoleEntry("agent_a", { role: wireRole({ effort: stored }) })
    ]);
    expect(agent.role?.effort).toBe(expected);
  });

  it.each(["tools", "disallowedTools", "skillIds", "mcpIds", "hookIds"] as const)(
    "reads a role's %s as a de-duplicated list of non-blank strings",
    (field) => {
      const read = (stored: unknown) => normalizedAgents([
        wireRoleEntry("agent_a", { role: wireRole({ [field]: stored }) })
      ])[0].role?.[field];

      // Order is kept, and a malformed entry is dropped rather than repaired.
      expect(read(["b", "a", "b", "", "   ", 7, null, "c"])).toEqual(["b", "a", "c"]);
      // A list that is not a list narrows the role to nothing; it never widens it.
      expect(read("read_file")).toEqual([]);
      expect(read({ 0: "a" })).toEqual([]);
      expect(read(undefined)).toEqual([]);
    }
  );

  it.each([
    ["absent", undefined, null],
    ["null", null, null],
    ["blank", "", null],
    ["not a string", 7, null],
    // A trace to the template store, so a dangling id is the host's to resolve.
    ["an id", "template_a", "template_a"]
  ])("reads a role's template that is %s", (_label, stored, expected) => {
    const [agent] = normalizedAgents([
      wireRoleEntry("agent_a", { role: wireRole({ templateId: stored }) })
    ]);
    expect(agent.role?.templateId).toBe(expected);
  });

  it("reads a role's web configuration the way a conversation's is read", () => {
    const read = (stored: unknown) => normalizedAgents([
      wireRoleEntry("agent_a", { role: wireRole({ webSearch: stored }) })
    ])[0].role?.webSearch;

    // Missing or malformed: the conversation default, not a half-built object.
    expect(read(undefined)).toEqual(defaultConversationWebSearchSettings());
    expect(read("nope")).toEqual(defaultConversationWebSearchSettings());
    expect(read({
      maxSearchesPerCall: 2_000_000,
      provider: { kind: "explicit", providerKind: "tavily" },
      domainFilter: "include",
      includeDomains: [" a.example ", "", 7]
    })).toEqual({
      ...defaultConversationWebSearchSettings(),
      maxSearchesPerCall: 99_999,
      provider: { kind: "explicit", providerKind: "tavily" },
      domainFilter: "include",
      includeDomains: ["a.example"]
    });
    // A provider this build cannot resolve is unavailable, not a silent default.
    expect(read({ provider: { kind: "bad" }, maxSearchesPerCall: -5 })).toEqual({
      ...defaultConversationWebSearchSettings(),
      maxSearchesPerCall: 0,
      provider: { kind: "unavailable" }
    });
  });

  it("makes the role rows total when the host's catalog arrives, and reads a host without roles as having none", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", { configurable: true, value: {} });
    tauriMocks.invoke
      .mockResolvedValueOnce({
        hooks: [],
        skills: [],
        mcps: [],
        toolDescriptionFiles: [],
        agents: [{ id: "agent_a", role: { name: "A" } }, { name: "no id" }]
      })
      // A host from before roles were files sends no section at all.
      .mockResolvedValueOnce({ hooks: [], skills: [], mcps: [], toolDescriptionFiles: [] });

    const catalog = await refreshCapabilities();
    expect(tauriMocks.invoke).toHaveBeenNthCalledWith(1, "discover_capabilities");
    expect(catalog.agents).toEqual([expect.objectContaining({
      id: "agent_a",
      available: true,
      role: expect.objectContaining({ name: "A", tools: [], modelSelection: { kind: "inherit" } })
    })]);
    expect((await refreshCapabilities()).agents).toEqual([]);
  });

  it("cannot save or delete a role in browser preview, where there is no file to write", async () => {
    const role = normalizeAgentRole({ name: "reviewer" })!;

    await expect(saveAgentRole({}, role)).rejects.toThrow("浏览器预览无法保存角色");
    await expect(saveAgentRole({ id: "agent_a" }, role)).rejects.toThrow("浏览器预览无法保存角色");
    await expect(deleteAgentRole("agent_a")).rejects.toThrow("浏览器预览无法删除角色");
    expect(tauriMocks.invoke).not.toHaveBeenCalled();
  });

  it("sends a role to the host by catalog id or by level, never by path", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", { configurable: true, value: {} });
    const role = normalizeAgentRole({ name: "reviewer", tools: ["read_file"] })!;
    tauriMocks.invoke
      .mockResolvedValueOnce("agent_user_a")
      .mockResolvedValueOnce("agent_user_b")
      .mockResolvedValueOnce(undefined);

    // An existing file is named by its id and overwritten in place.
    await expect(saveAgentRole({ id: "agent_user_a" }, role)).resolves.toBe("agent_user_a");
    // A new file is created at the level the workspace key names.
    await expect(saveAgentRole({ workspaceKey: "local|C:/repo" }, role)).resolves.toBe("agent_user_b");
    await expect(deleteAgentRole("agent_user_a")).resolves.toBeUndefined();

    expect(tauriMocks.invoke).toHaveBeenNthCalledWith(1, "save_agent_role", {
      target: { id: "agent_user_a", workspaceKey: null },
      role
    });
    expect(tauriMocks.invoke).toHaveBeenNthCalledWith(2, "save_agent_role", {
      target: { workspaceKey: "local|C:/repo" },
      role
    });
    expect(tauriMocks.invoke).toHaveBeenNthCalledWith(3, "delete_agent_role", {
      roleId: "agent_user_a"
    });
  });

  /**
   * A conversation names its roles by id, like its skills: no role record is
   * carried in the document, and a dangling id stays because the file may come
   * back. All three places a conversation's settings live read them one way.
   */
  it.each([
    [
      "a list with duplicates, blanks and non-strings",
      ["agent_a", "agent_a", "", "   ", 7, null, "agent_gone", "agent_b"],
      ["agent_a", "agent_gone", "agent_b"]
    ],
    ["an empty list", [], []],
    ["absent", undefined, []],
    ["not a list", "agent_a", []]
  ])("reads the role selection of a conversation, a workspace's remembered settings and the draft alike: %s", (
    _label,
    stored,
    expected
  ) => {
    const document = createSeedDocument();
    const workspace = document.workspaces[0];
    const base = structuredClone(workspace.conversations[0].settings) as unknown as Record<string, unknown>;
    const withStored = () => {
      const copy = structuredClone(base);
      if (stored === undefined) delete copy.agentIds;
      else copy.agentIds = stored;
      return copy;
    };
    (workspace.conversations[0] as unknown as Record<string, unknown>).settings = withStored();
    (workspace as unknown as Record<string, unknown>).lastConversationSettings = withStored();
    (workspace as unknown as Record<string, unknown>).draftConversation = {
      settings: withStored(),
      presetId: ""
    };

    const normalized = normalizeDocument(document);

    expect(normalized.workspaces[0].conversations[0].settings.agentIds).toEqual(expected);
    expect(normalized.workspaces[0].lastConversationSettings?.agentIds).toEqual(expected);
    expect(normalized.workspaces[0].draftConversation?.settings.agentIds).toEqual(expected);
  });

  it("reads a preset's roles like its other resource ids, and a silent preset as the built-in roles", () => {
    const read = (agentIds: unknown) => {
      const current = createSeedDocument();
      const settings = defaultPresetSettings(current) as unknown as Record<string, unknown>;
      if (agentIds === undefined) delete settings.agentIds;
      else settings.agentIds = agentIds;
      return normalizeDocument(current).globalSettings.conversationPresets[0].settings.agentIds;
    };

    expect(read(["agent_a", "agent_a", "", "   ", 7, "agent_b"])).toEqual(["agent_a", "agent_b"]);
    // An empty selection is an answer: a preset that chose no role keeps choosing none.
    expect(read([])).toEqual([]);
    // Silence is not: the preset falls back to the roles the product ships.
    expect(read(undefined)).toEqual([...BUILTIN_AGENT_ROLE_IDS]);
    expect(read("agent_a")).toEqual([...BUILTIN_AGENT_ROLE_IDS]);
  });

  it("drops the legacy agentDefinitions list from every place a settings object lives", () => {
    // Roles used to be records carried in settings. The host exports that list to
    // files on load and puts their ids in `agentIds`, so a renderer that spread it
    // back would write the retired key into the next save.
    const legacy = [{ enabled: true, deleted: false, name: "old", description: "", revision: 1 }];
    const document = createSeedDocument();
    const workspace = document.workspaces[0];
    const withLegacy = () => ({
      ...structuredClone(workspace.conversations[0].settings),
      agentIds: ["agent_a"],
      agentDefinitions: legacy
    });
    (workspace.conversations[0] as unknown as Record<string, unknown>).settings = withLegacy();
    (workspace as unknown as Record<string, unknown>).lastConversationSettings = withLegacy();
    (workspace as unknown as Record<string, unknown>).draftConversation = {
      settings: withLegacy(),
      presetId: ""
    };
    (defaultPresetSettings(document) as unknown as Record<string, unknown>).agentDefinitions = legacy;

    const normalized = normalizeDocument(document);

    for (const settings of [
      normalized.workspaces[0].conversations[0].settings,
      normalized.workspaces[0].lastConversationSettings!,
      normalized.workspaces[0].draftConversation!.settings,
      normalized.globalSettings.conversationPresets[0].settings
    ]) {
      expect(settings).not.toHaveProperty("agentDefinitions");
    }
    expect(normalized.workspaces[0].conversations[0].settings.agentIds).toEqual(["agent_a"]);
    expect(JSON.stringify(normalized)).not.toContain("agentDefinitions");
  });

  it("keeps catalog providers, deduplicated and in catalog order", () => {
    const current = createSeedDocument() as unknown as { globalSettings: Record<string, unknown> };
    current.globalSettings.webSearch = {
      providers: [
        { kind: "searxng", enabled: 1, searchApiHost: " http://searx.local:8080 ", engines: [" google ", "", 5], basicAuthUsername: " searx " },
        { kind: "tavily", enabled: true, searchApiHost: " https://tavily.example " },
        { kind: "tavily", enabled: false, searchApiHost: "ignored" },
        { kind: "unknown", enabled: true },
        42
      ],
      // Keys a build before the split wrote here. They describe how a search
      // behaves, which is the conversation's question now, so nothing reads
      // them off the asset layer and nothing carries them back out of it.
      fetchProvider: "tavily",
      maxResults: 999,
      excludeDomains: [" *://ads.example/* ", "", 7],
      compression: { method: "nonsense", cutoffLimit: 0 }
    };
    const normalized = normalizeDocument(current).globalSettings.webSearch;
    // Catalog order is canonical: retain the first duplicate and drop non-catalog or
    // non-object entries.
    expect(normalized.providers.map((provider) => provider.kind)).toEqual(
      createSeedDocument().globalSettings.webSearch.providers.map((provider) => provider.kind)
    );
    const tavily = normalized.providers.find((provider) => provider.kind === "tavily");
    expect(tavily).toEqual({
      kind: "tavily",
      enabled: true,
      searchApiHost: "https://tavily.example",
      fetchApiHost: "",
      engines: [],
      basicAuthUsername: ""
    });
    const searxng = normalized.providers.find((provider) => provider.kind === "searxng");
    expect(searxng?.enabled).toBe(true);
    expect(searxng?.engines).toEqual(["google"]);
    expect(searxng?.basicAuthUsername).toBe("searx");
    // The catalog is the whole of this layer; a stale key is dropped rather
    // than carried forward as a setting nothing consults.
    expect(Object.keys(normalized)).toEqual(["providers"]);
  });

  it("bounds per-conversation search limits and provider selections", () => {
    const current = createSeedDocument() as unknown as { workspaces: { conversations: { settings: Record<string, unknown> }[] }[] };
    const conversations = current.workspaces[0].conversations;
    conversations.push(structuredClone(conversations[0]));
    conversations[0].settings.webSearch = { maxSearchesPerCall: 2_000_000, provider: { kind: "explicit", providerKind: "tavily" } };
    conversations[1].settings.webSearch = { maxSearchesPerCall: -5, provider: { kind: "explicit", providerKind: "ghost" } };
    const normalized = normalizeDocument(current).workspaces[0].conversations;
    expect(normalized[0].settings.webSearch).toEqual({
      ...defaultConversationWebSearchSettings(),
      maxSearchesPerCall: 99_999,
      provider: { kind: "explicit", providerKind: "tavily" }
    });
    expect(normalized[1].settings.webSearch).toEqual({
      ...defaultConversationWebSearchSettings(),
      maxSearchesPerCall: 0,
      provider: { kind: "unavailable" }
    });
  });

  /* The single knob that used to cover both legs is carried onto the fetch leg
     when a stored document has no cap of its own for it, so a user who chose 0
     (no cap) keeps that on fetch. Rust's hand-written `Deserialize` answers the
     same, and the two have to agree on the JSON. */
  it("gives a stored conversation without a fetch compression the value of its compression", () => {
    const current = createSeedDocument() as unknown as { workspaces: { conversations: { settings: Record<string, unknown> }[] }[] };
    const conversations = current.workspaces[0].conversations;
    conversations.push(structuredClone(conversations[0]), structuredClone(conversations[0]), structuredClone(conversations[0]));
    conversations[0].settings.webSearch = { compressionCutoff: 0 };
    conversations[1].settings.webSearch = { compressionCutoff: 750 };
    // Nothing stored at all reads as the default on both.
    conversations[2].settings.webSearch = { maxSearchesPerCall: 1 };
    // One that is stored is its own answer, not the other leg's.
    conversations[3].settings.webSearch = { compressionCutoff: 750, fetchCompressionCutoff: 0 };
    const webSearch = normalizeDocument(current).workspaces[0].conversations.map((conversation) => ({
      compressionCutoff: conversation.settings.webSearch.compressionCutoff,
      fetchCompressionCutoff: conversation.settings.webSearch.fetchCompressionCutoff
    }));
    expect(webSearch.slice(0, 4)).toEqual([
      { compressionCutoff: 0, fetchCompressionCutoff: 0 },
      { compressionCutoff: 750, fetchCompressionCutoff: 750 },
      { compressionCutoff: DEFAULT_SEARCH_COMPRESSION_CUTOFF, fetchCompressionCutoff: DEFAULT_SEARCH_COMPRESSION_CUTOFF },
      { compressionCutoff: 750, fetchCompressionCutoff: 0 }
    ]);
  });

  it("clamps the stored result-shaping numbers to this build's ceilings and falls back on junk", () => {
    const current = createSeedDocument() as unknown as { workspaces: { conversations: { settings: Record<string, unknown> }[] }[] };
    const conversations = current.workspaces[0].conversations;
    conversations.push(structuredClone(conversations[0]));
    conversations[0].settings.webSearch = {
      maxResults: 5000,
      compressionCutoff: 9_000_000,
      fetchCompressionCutoff: 9_000_000
    };
    conversations[1].settings.webSearch = { compressionCutoff: 640, fetchCompressionCutoff: "lots" };
    const [clamped, junk] = normalizeDocument(current).workspaces[0].conversations
      .map((conversation) => conversation.settings.webSearch);
    // 100 is the widest backend's own ceiling; 200 000 bounds both caps.
    expect(clamped).toMatchObject({ maxResults: 100, compressionCutoff: 200_000, fetchCompressionCutoff: 200_000 });
    expect(junk).toMatchObject({ compressionCutoff: 640, fetchCompressionCutoff: 640 });
  });

  it("rewrites malformed provider selections to unavailable", () => {
    const current = createSeedDocument() as unknown as { workspaces: { conversations: { settings: Record<string, unknown> }[] }[] };
    current.workspaces[0].conversations[0].settings.webSearch = { maxSearchesPerCall: 0, provider: { kind: "bad" } };
    expect(normalizeDocument(current).workspaces[0].conversations[0].settings.webSearch).toEqual({
      ...defaultConversationWebSearchSettings(),
      provider: { kind: "unavailable" }
    });
  });

  /* Naming no backend is an answer the user gave, not a binding that broke, so
     it survives a load as itself. Rewriting it to `unavailable` would put a
     repairable error in front of a conversation that is working as asked. */
  it("keeps a disabled search selection as itself", () => {
    const current = createSeedDocument() as unknown as { workspaces: { conversations: { settings: Record<string, unknown> }[] }[] };
    current.workspaces[0].conversations[0].settings.webSearch = { provider: { kind: "disabled" } };
    expect(normalizeDocument(current).workspaces[0].conversations[0].settings.webSearch.provider)
      .toEqual({ kind: "disabled" });
  });

  // Absence is not unavailability. Rust gives `provider` a `#[serde(default)]` Auto
  // value; interpreting an omitted field as unavailable would silently remove search
  // capability from valid documents and make the two sides disagree about the JSON.
  it("treats an absent provider selection as auto, not unavailable", () => {
    const current = createSeedDocument() as unknown as { workspaces: { conversations: { settings: Record<string, unknown> }[] }[] };
    current.workspaces[0].conversations[0].settings.webSearch = { maxSearchesPerCall: 3 };
    expect(normalizeDocument(current).workspaces[0].conversations[0].settings.webSearch)
      .toEqual({ ...defaultConversationWebSearchSettings(), maxSearchesPerCall: 3 });
  });

  // `native` is a legal answer for fetching on every family, which is exactly
  // why it must survive a load: a family that keeps retrieval inside its search
  // tool grants no second tool, and rewriting the choice to `disabled` would
  // look identical while meaning something the user did not pick.
  it("keeps a native fetch selection and keeps the tool lock's pins", () => {
    const current = createSeedDocument() as unknown as {
      workspaces: { conversations: { settings: Record<string, unknown> }[] }[]
    };
    const settings = current.workspaces[0].conversations[0].settings;
    settings.webSearch = { maxSearchesPerCall: 0, fetchProvider: { kind: "native" } };
    settings.toolLock = {
      tools: [],
      mcpIds: [],
      webSearch: true,
      skillIds: ["skill_gone_from_disk"],
      promptSkillIds: [],
      searchProvider: { kind: "explicit", providerKind: "tavily" },
      fetchProvider: { kind: "native" }
    };
    const loaded = normalizeDocument(current).workspaces[0].conversations[0].settings;
    expect(loaded.webSearch.fetchProvider).toEqual({ kind: "native" });
    expect(loaded.toolLock?.searchProvider).toEqual({ kind: "explicit", providerKind: "tavily" });
    expect(loaded.toolLock?.fetchProvider).toEqual({ kind: "native" });
    // A skill id is kept even when discovery no longer finds it: its body is in
    // the transcript regardless of what is on disk now.
    expect(loaded.toolLock?.skillIds).toEqual(["skill_gone_from_disk"]);
    expect(loaded.toolLock?.promptSkillIds).toEqual([]);
  });

  /* A fetch provider this build cannot resolve is a broken binding, as on the
     search leg: it reads "Repair fetch provider" and keeps `web_fetch`, rather
     than passing for the user's own "off". */
  it("loads an unknown or non-fetching fetch provider as unavailable, not off", () => {
    const loadedFetch = (fetchProvider: unknown) => {
      const current = createSeedDocument() as unknown as {
        workspaces: { conversations: { settings: Record<string, unknown> }[] }[]
      };
      current.workspaces[0].conversations[0].settings.webSearch = { fetchProvider };
      return normalizeDocument(current).workspaces[0].conversations[0].settings.webSearch.fetchProvider;
    };
    expect(loadedFetch({ kind: "explicit", providerKind: "brave" })).toEqual({ kind: "unavailable" });
    expect(loadedFetch({ kind: "explicit", providerKind: "tavily" })).toEqual({ kind: "unavailable" });
    expect(loadedFetch({ kind: "unavailable" })).toEqual({ kind: "unavailable" });
    expect(loadedFetch({ kind: "disabled" })).toEqual({ kind: "disabled" });
    expect(loadedFetch({ kind: "explicit", providerKind: "jina" })).toEqual({ kind: "explicit", providerKind: "jina" });
  });

  it("restores only the reserved temporary workspace when it is missing", () => {
    const missing = createSeedDocument();
    missing.workspaces = missing.workspaces.filter((workspace) => workspace.id !== "__temporary__");
    const restored = normalizeDocument(missing);
    expect(restored.workspaces.find((workspace) => workspace.id === "__temporary__"))
      .toMatchObject({ name: "临时工作区", kind: "temporary", path: "", conversations: [] });
  });

  it("stays a fixed point on capability selections when normalized twice", () => {
    const document = createSeedDocument();
    const conversation = document.workspaces[0].conversations[0];
    const capabilityIds = {
      hookIds: ["hook_format"],
      // The catalog has no `skill_missing`; normalization must retain dangling IDs.
      skillIds: ["skill_installer", "skill_missing"],
      mcpIds: ["mcp_workspace"]
    };
    const toolDescriptionFileId = "tooldesc_user_main_0f0f0f0f";
    const webSearch = {
      maxSearchesPerCall: 42,
      provider: { kind: "native" as const }
    };
    Object.assign(conversation.settings, capabilityIds, { toolDescriptionFileId, webSearch });
    Object.assign(defaultPresetSettings(document), capabilityIds, { toolDescriptionFileId });

    const once = normalizeDocument(document);
    const twice = normalizeDocument(once);

    // A second normalization pass must equal the first: normalization is idempotent
    // and must retain dangling IDs.
    for (const normalized of [once, twice]) {
      expect(normalized.workspaces[0].conversations[0].settings).toMatchObject({
        ...capabilityIds,
        toolDescriptionFileId,
        webSearch
      });
      expect(defaultPresetSettings(normalized)).toMatchObject({ ...capabilityIds, toolDescriptionFileId });
    }
    expect(twice).toEqual(once);
  });

  it("preserves a blank conversation preset name during normalization", () => {
    const document = createSeedDocument();
    document.globalSettings.conversationPresets[0].name = "";

    const normalized = normalizeDocument(document);

    expect(normalized.globalSettings.conversationPresets[0].name).toBe("");
  });

  it("preserves dangling capability IDs in presets and in live conversation settings", () => {
    // Resources may be referenced before installation or remain in a preset after
    // removal, so catalog filtering would silently change behavior when they return.
    const document = createSeedDocument();
    const globalPreset = defaultPresetSettings(document);
    globalPreset.skillIds = ["skills_missing"];
    globalPreset.mcpIds = ["mcp_missing"];
    const conversation = document.workspaces[0].conversations[0];
    conversation.settings.hookIds = ["hooks_local_missing"];
    conversation.settings.skillIds = ["skills_local_missing"];
    conversation.settings.mcpIds = ["mcp_local_missing"];

    const normalized = normalizeDocument(document);
    expect(defaultPresetSettings(normalized)).toMatchObject({
      skillIds: ["skills_missing"],
      mcpIds: ["mcp_missing"]
    });
    const normalizedConversation = normalized.workspaces[0].conversations[0];
    expect(normalizedConversation.settings.hookIds).toEqual(["hooks_local_missing"]);
    expect(normalizedConversation.settings.skillIds).toEqual(["skills_local_missing"]);
    expect(normalizedConversation.settings.mcpIds).toEqual(["mcp_local_missing"]);
  });

  it("preserves branch slots and recursively normalizes their suffix contexts", () => {
    const document = createSeedDocument();
    const conversation = document.workspaces[0].conversations[0];
    conversation.contexts = [
      { id: "fork-user", kind: "user", content: "fork", createdAt: "2026-07-20T00:00:00Z" },
      { id: "new-answer", kind: "assistant", content: "new", createdAt: "2026-07-20T00:00:01Z" }
    ];
    conversation.branches = [
      {
        id: "old-branch",
        forkContextId: "fork-user",
        active: false,
        contexts: [{ id: "old-answer", kind: "assistant", content: "old", createdAt: "2026-07-20T00:00:02Z" }],
        createdAt: "2026-07-20T00:00:00Z",
        updatedAt: "2026-07-20T00:00:02Z"
      },
      {
        id: "new-branch",
        forkContextId: "fork-user",
        active: true,
        contexts: [],
        createdAt: "2026-07-20T00:00:03Z",
        updatedAt: "2026-07-20T00:00:03Z"
      }
    ];

    expect(normalizeDocument(document).workspaces[0].conversations[0].branches).toEqual(conversation.branches);
  });

  it("keeps preview image bytes outside the document and resolves them after a reload boundary", async () => {
    const encoded = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=";
    const bytes = Uint8Array.from(window.atob(encoded), (character) => character.charCodeAt(0));

    const attachment = await prepareImageAttachment("pixel.png", bytes);

    expect(attachment).toMatchObject({
      name: "pixel.png",
      mime: "image/png",
      width: 1,
      height: 1,
      bytes: bytes.byteLength
    });
    expect(JSON.stringify(createSeedDocument())).not.toContain(encoded);
    expect(await imageAttachmentData(attachment.id)).toBe(`data:image/png;base64,${encoded}`);
  });

  it("rejects invalid or oversized browser-preview image dimensions before storing bytes", async () => {
    const pngWithDimensions = (width: number, height: number) => {
      const bytes = new Uint8Array(24);
      bytes.set([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
      const view = new DataView(bytes.buffer);
      view.setUint32(16, width);
      view.setUint32(20, height);
      return bytes;
    };

    await expect(prepareImageAttachment("unknown.png", pngWithDimensions(0, 100)))
      .rejects.toThrow(/1–8000/);
    await expect(prepareImageAttachment("too-wide.png", pngWithDimensions(8001, 1)))
      .rejects.toThrow(/1–8000/);
    await expect(prepareImageAttachment("too-many-pixels.png", pngWithDimensions(4097, 4097)))
      .rejects.toThrow(/16 MP/);
    expect(window.localStorage.length).toBe(0);
  });

  it("uses the native filename contract in browser preview", async () => {
    const encoded = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=";
    const bytes = Uint8Array.from(window.atob(encoded), (character) => character.charCodeAt(0));

    await expect(prepareImageAttachment("\u0000unsafe.png", bytes))
      .rejects.toThrow(/控制字符/);
    await expect(prepareImageAttachment("界".repeat(86), bytes))
      .rejects.toThrow(/256 字节/);
    expect(window.localStorage.length).toBe(0);
  });

  it("accepts a static GIF but rejects an animated GIF before storing it", async () => {
    const staticGif = new Uint8Array([
      71, 73, 70, 56, 57, 97, 1, 0, 1, 0, 128, 0, 0, 0, 0, 0, 255, 255, 255,
      44, 0, 0, 0, 0, 1, 0, 1, 0, 0, 2, 2, 68, 1, 0, 59
    ]);
    const animatedGif = new Uint8Array([
      ...staticGif.slice(0, -1),
      ...staticGif.slice(19, -1),
      59
    ]);

    await expect(prepareImageAttachment("static.gif", staticGif)).resolves.toMatchObject({
      mime: "image/gif",
      width: 1,
      height: 1
    });
    const storedKeys = window.localStorage.length;
    await expect(prepareImageAttachment("animated.gif", animatedGif))
      .rejects.toThrow(/动画 GIF/);
    expect(window.localStorage.length).toBe(storedKeys);
  });

  it("revalidates browser-preview sidecar bytes and their SHA-256 before rendering", async () => {
    const encoded = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=";
    const bytes = Uint8Array.from(window.atob(encoded), (character) => character.charCodeAt(0));
    const attachment = await prepareImageAttachment("safe.png", bytes);
    const storageKey = `mewrk.image-attachment.v1.${attachment.id}`;
    const stored = JSON.parse(window.localStorage.getItem(storageKey)!);
    const tampered = Uint8Array.from(bytes);
    tampered[tampered.length - 1] ^= 1;
    stored.dataUrl = `data:image/png;base64,${window.btoa(
      String.fromCharCode(...tampered)
    )}`;
    window.localStorage.setItem(storageKey, JSON.stringify(stored));

    await expect(imageAttachmentData(attachment.id)).rejects.toThrow(/已损坏/);
  });

  it("revalidates the single-frame policy after browser-preview sidecar tampering", async () => {
    const staticGif = new Uint8Array([
      71, 73, 70, 56, 57, 97, 1, 0, 1, 0, 128, 0, 0, 0, 0, 0, 255, 255, 255,
      44, 0, 0, 0, 0, 1, 0, 1, 0, 0, 2, 2, 68, 1, 0, 59
    ]);
    const animatedGif = new Uint8Array([
      ...staticGif.slice(0, -1),
      ...staticGif.slice(19, -1),
      59
    ]);
    const attachment = await prepareImageAttachment("safe.gif", staticGif);
    const storageKey = `mewrk.image-attachment.v1.${attachment.id}`;
    const stored = JSON.parse(window.localStorage.getItem(storageKey)!);
    stored.attachment.bytes = animatedGif.byteLength;
    stored.dataUrl = `data:image/gif;base64,${window.btoa(
      String.fromCharCode(...animatedGif)
    )}`;
    window.localStorage.setItem(storageKey, JSON.stringify(stored));

    await expect(imageAttachmentData(attachment.id)).rejects.toThrow(/已损坏/);
  });

  it("delays browser-preview orphan cleanup and never classifies an unsent draft during an unrelated save", async () => {
    const encoded = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=";
    const bytes = Uint8Array.from(window.atob(encoded), (character) => character.charCodeAt(0));
    const attachment = await prepareImageAttachment("draft.png", bytes);
    const indexKey = "mewrk.image-attachment-index.v1";
    const storageKey = `mewrk.image-attachment.v1.${attachment.id}`;

    await saveDocument(createSeedDocument());
    expect(JSON.parse(window.localStorage.getItem(indexKey)!)[0]).not.toHaveProperty("orphanedAt");
    expect(window.localStorage.getItem(storageKey)).not.toBeNull();

    const referenced = createSeedDocument();
    referenced.workspaces[0].conversations[0].contexts = [{
      id: "image-user",
      kind: "user",
      content: "",
      images: [attachment],
      createdAt: "2026-07-24T00:00:00Z"
    }];
    await saveDocument(referenced);
    const forgedOnly = createSeedDocument();
    forgedOnly.workspaces[0].conversations[0].contexts = [{
      id: "tool-input-image-shaped-json",
      kind: "tool",
      toolName: "custom_tool",
      input: { images: [{ id: attachment.id }] },
      result: {
        success: true,
        output: "ordinary JSON",
        executedAt: "2026-07-24T00:00:00Z",
        durationMs: 1
      },
      createdAt: "2026-07-24T00:00:00Z"
    }];
    await saveDocument(forgedOnly);
    const orphaned = JSON.parse(window.localStorage.getItem(indexKey)!);
    expect(orphaned[0].orphanedAt).toEqual(expect.any(Number));

    orphaned[0].orphanedAt = Date.now() - (60 * 60 * 1000) - 1;
    window.localStorage.setItem(indexKey, JSON.stringify(orphaned));
    window.localStorage.setItem("unrelated.user.storage", "keep");
    await loadDocument();
    expect(window.localStorage.getItem(storageKey)).toBeNull();
    expect(window.localStorage.getItem("unrelated.user.storage")).toBe("keep");
  });

  it("browser-preview reset removes only Mewrk image attachment storage", async () => {
    const encoded = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=";
    const bytes = Uint8Array.from(window.atob(encoded), (character) => character.charCodeAt(0));
    const attachment = await prepareImageAttachment("reset.png", bytes);
    const storageKey = `mewrk.image-attachment.v1.${attachment.id}`;
    window.localStorage.setItem("unrelated.user.storage", "keep");

    await resetDocument();

    expect(window.localStorage.getItem(storageKey)).toBeNull();
    expect(window.localStorage.getItem("mewrk.image-attachment-index.v1")).toBeNull();
    expect(window.localStorage.getItem("unrelated.user.storage")).toBe("keep");
  });

  it("browser-preview reset forgets the key markers of every provider", async () => {
    // Key-status metadata is fingerprinted by provider ID. Reset it so a recreated
    // provider with the same ID does not show stale configured state or length.
    const provider: ApiProvider = {
      id: "provider_reset_probe",
      name: "重置探针",
      enabled: true,
      family: "openai_responses",
      baseUrl: "https://api.openai.com/v1",
      familySettings: {},
      notes: "",
      models: [],
      activeModelId: null
    };
    await saveApiKey(provider, "preview-secret-key");
    expect(await getStoredApiKeyLength(provider.id)).toBe(18);
    window.localStorage.setItem("unrelated.session.storage", "keep");

    await resetDocument();

    expect(await getStoredApiKeyLength(provider.id)).toBeUndefined();
    expect(window.localStorage.getItem("unrelated.session.storage")).toBe("keep");
  });

  it("rejects a forged browser-preview sidecar MIME instead of rendering active image content", async () => {
    const encoded = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=";
    const bytes = Uint8Array.from(window.atob(encoded), (character) => character.charCodeAt(0));
    const attachment = await prepareImageAttachment("safe.png", bytes);
    const storageKey = `mewrk.image-attachment.v1.${attachment.id}`;
    const stored = JSON.parse(window.localStorage.getItem(storageKey)!);
    stored.dataUrl = "data:image/svg+xml;base64,PHN2Zy8+";
    window.localStorage.setItem(storageKey, JSON.stringify(stored));

    await expect(imageAttachmentData(attachment.id)).rejects.toThrow(/已损坏/);
  });

  it("uses the final native image attachment command shapes", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", { configurable: true, value: {} });
    const attachment = {
      id: "image-native",
      name: "native.png",
      mime: "image/png",
      width: 20,
      height: 10,
      bytes: 4
    };
    tauriMocks.invoke
      .mockResolvedValueOnce(attachment)
      .mockResolvedValueOnce("data:image/png;base64,AAAA");

    await expect(prepareImageAttachment("native.png", new Uint8Array([1, 2, 3, 4]))).resolves.toEqual(attachment);
    await expect(imageAttachmentData("image-native")).resolves.toBe("data:image/png;base64,AAAA");
    expect(tauriMocks.invoke).toHaveBeenNthCalledWith(1, "image_attachment_upload", {
      name: "native.png",
      data: "AQIDBA=="
    });
    expect(tauriMocks.invoke).toHaveBeenNthCalledWith(2, "image_attachment_data", {
      imageId: "image-native"
    });
  });

  it("skips a disabled provider when resolving the active selection", () => {
    const document = createSeedDocument() as unknown as {
      globalSettings: Record<string, unknown>;
    };
    document.globalSettings.apiProviders = [
      {
        id: "provider-disabled",
        name: "Disabled",
        enabled: false,
        presetProviderId: null,
        notes: "",
        family: "openai_responses",
        baseUrl: "https://example.invalid/v1",
        models: [],
        activeModelId: null
      },
      {
        id: "provider-enabled",
        name: "Enabled",
        enabled: true,
        presetProviderId: null,
        notes: "",
        family: "openai_responses",
        baseUrl: "https://example.invalid/v1",
        models: [],
        activeModelId: null
      }
    ];
    document.globalSettings.activeProviderId = null;

    expect(normalizeDocument(document).globalSettings.activeProviderId).toBe("provider-enabled");

    // An explicit disabled provider is also skipped in favor of the first available
    // provider.
    document.globalSettings.activeProviderId = "provider-disabled";
    expect(normalizeDocument(document).globalSettings.activeProviderId).toBe("provider-enabled");
  });

  it("validates persisted effort values and falls back to the global effort for invalid ones", () => {
    const persisted = createSeedDocument() as unknown as Record<string, unknown>;
    (persisted.globalSettings as Record<string, unknown>).lastReasoningEffort = "high";
    const persistedWorkspaces = persisted.workspaces as Array<{ conversations: Array<{ settings: Record<string, unknown> }> }>;
    persistedWorkspaces[0].conversations[0].settings.reasoningEffort = "max";
    // An unknown level is rejected and falls back to the most recent global level.
    persistedWorkspaces[0].conversations[1].settings.reasoningEffort = "ultra";

    const normalized = normalizeDocument(persisted);
    expect(normalized.globalSettings.lastReasoningEffort).toBe("high");
    expect(normalized.workspaces[0].conversations[0].settings.reasoningEffort).toBe("max");
    expect(normalized.workspaces[0].conversations[1].settings.reasoningEffort).toBe("high");
  });

  it("reads the retired effort spellings as the host does", () => {
    // Mirrors the serde aliases on Rust `ReasoningEffort`: thinking-off and
    // `minimal` are the lowest level now, `xhigh` is `extra`.
    for (const [legacy, level] of [["disabled", "low"], ["minimal", "low"], ["xhigh", "extra"]] as const) {
      const persisted = createSeedDocument() as unknown as Record<string, unknown>;
      (persisted.globalSettings as Record<string, unknown>).lastReasoningEffort = legacy;
      const persistedWorkspaces = persisted.workspaces as Array<{ conversations: Array<{ settings: Record<string, unknown> }> }>;
      persistedWorkspaces[0].conversations[0].settings.reasoningEffort = legacy;

      const normalized = normalizeDocument(persisted);
      expect(normalized.globalSettings.lastReasoningEffort).toBe(level);
      expect(normalized.workspaces[0].conversations[0].settings.reasoningEffort).toBe(level);
    }
  });

  it("uses the approval-first policy when security data is missing or invalid", () => {
    const current = createSeedDocument() as unknown as Record<string, unknown>;
    const currentWorkspaces = current.workspaces as Array<{ conversations: Array<{ settings: Record<string, unknown> }> }>;
    currentWorkspaces[0].conversations[0].settings.securityLevel = "unsupported";
    // An absent conversation security level must use the most cautious default, not
    // a mutable global default.
    delete currentWorkspaces[0].conversations[1].settings.securityLevel;

    const normalized = normalizeDocument(current);
    expect(normalized.workspaces[0].conversations[0].settings.securityLevel).toBe("request_approval");
    expect(normalized.workspaces[0].conversations[1].settings.securityLevel).toBe("request_approval");
  });

  it("normalizes a workspace's remembered conversation settings and keeps a dangling default preset id", () => {
    const current = createSeedDocument() as unknown as Record<string, unknown>;
    const currentWorkspaces = current.workspaces as Array<Record<string, unknown>>;
    currentWorkspaces[0].defaultConversationPresetId = "preset-that-was-deleted";
    currentWorkspaces[0].lastConversationSettings = {
      enabledTools: ["read", "read", "no-such-tool"],
      hookIds: [],
      skillIds: [],
      mcpIds: [],
      toolDescriptionFileId: null,
      agentIds: ["agent_a", "agent_a", "agent_b"],
      reasoningEffort: "high",
      securityLevel: "unsupported",
      globalMemoryEnabled: true,
      projectMemoryEnabled: false
    };

    const workspace = normalizeDocument(current).workspaces[0];
    // Retain dangling IDs so deleting a preset neither rejects the document nor
    // silently rewrites the user's selection.
    expect(workspace.defaultConversationPresetId).toBe("preset-that-was-deleted");
    expect(workspace.lastConversationSettings?.enabledTools).toEqual(["read"]);
    expect(workspace.lastConversationSettings?.agentIds).toEqual(["agent_a", "agent_b"]);
    expect(workspace.lastConversationSettings?.securityLevel).toBe("request_approval");
    expect(workspace.lastConversationSettings?.reasoningEffort).toBe("high");
    expect(workspace.lastConversationSettings?.globalMemoryEnabled).toBe(true);
  });

  it("never retains API Key plaintext in browser preview storage", async () => {
    const provider = {
      ...createSeedDocument().globalSettings.apiProviders[0],
      id: "preview-provider"
    };
    await saveApiKey(provider, "super-secret-preview-key");
    const storedValues = Array.from({ length: window.localStorage.length }, (_, index) => {
      const key = window.localStorage.key(index)!;
      return window.localStorage.getItem(key);
    });
    const storedKeys = Array.from({ length: window.localStorage.length }, (_, index) => window.localStorage.key(index));
    // Persist only non-secret length metadata; its fingerprint contains neither ID nor
    // endpoint.
    expect(JSON.stringify(storedValues)).not.toContain("super-secret-preview-key");
    expect(JSON.stringify(storedKeys)).not.toContain(provider.id);
    expect(JSON.stringify(storedKeys)).not.toContain(provider.baseUrl);
    expect(await getStoredApiKeyLength(provider.id)).toBe(24);
    await expect(revealApiKey(provider)).rejects.toThrow(/浏览器预览不会保留/);
  });

  it("keeps key status outside the persisted provider document", async () => {
    const document = createSeedDocument();
    const provider = document.globalSettings.apiProviders[0];
    await saveDocument(document);
    await saveApiKey(provider, "discard-me");

    const loadedProvider = (await loadDocument()).globalSettings.apiProviders[0];
    expect(loadedProvider).not.toHaveProperty("hasApiKey");
    expect(loadedProvider).not.toHaveProperty("apiKeys");
    expect(JSON.parse(window.localStorage.getItem(STORAGE_KEY)!).assets.apiProviders[0]).not.toHaveProperty("hasApiKey");
    expect(await getStoredApiKeyLength(loadedProvider.id)).toBe(10);
  });

  it("persists the schema-60 assets and presets containers with no retired sub-preset layer", async () => {
    const document = createSeedDocument();
    document.workspaces[0].conversations[0].settings.skillIds = ["skill_installer"];
    document.globalSettings.webSearch = {
      ...document.globalSettings.webSearch,
      providers: [{
        kind: "tavily",
        enabled: true,
        searchApiHost: "",
        fetchApiHost: "",
        engines: [],
        basicAuthUsername: ""
      }]
    };

    await saveDocument(document);
    const persisted = JSON.parse(window.localStorage.getItem(STORAGE_KEY)!);

    expect(persisted.schemaVersion).toBe(createSeedDocument().schemaVersion);
    expect(Object.keys(persisted.presets).sort()).toEqual([
      "conversationPresets",
      "defaultConversationPresetId"
    ]);
    // The persisted asset-key set is a contract: skills and MCP servers are files
    // the user owns, so they must NOT appear here, and any missing or extra key
    // must be visible to this assertion.
    expect(Object.keys(persisted.assets).sort()).toEqual([
      "apiProviders",
      "executionEnvironments",
      "webSearch"
    ]);
    // The asset layer is the catalog and nothing else: how a search behaves is
    // the conversation's question, so a key describing it here would be a
    // second, installation-wide answer nothing reads.
    expect(Object.keys(persisted.assets.webSearch).sort()).toEqual(["providers"]);
    // Core settings have an exact key set: appearance, shortcuts, and environment
    // settings belong here, while assets and presets do not.
    expect(Object.keys(persisted.globalSettings).sort()).toEqual([
      "activeProviderId",
      "appLanguage",
      "appearance",
      "autoCompact",
      "environmentTools",
      "lastReasoningEffort",
      "resolvedAppLanguage",
      "shortcuts",
      "theme"
    ]);
    for (const provider of persisted.assets.apiProviders) {
      expect(provider).not.toHaveProperty("chatEnabled");
    }
    // Retired sub-preset layers and their reference keys must never reach disk.
    const serialized = JSON.stringify(persisted);
    for (const retired of [
      "hookPresets",
      "skillPresets",
      "mcpPresets",
      "toolDescriptionSets",
      "securityPolicies",
      "modelPresets",
      "toolDescriptionSetId",
      "toolDescriptions",
      "modelPresetId",
      "securityPolicyId",
      "chatEnabled",
      "localPreset",
      // Roles are files now: a document holds only the ids a conversation selected.
      "agentDefinitions"
    ]) {
      expect(serialized).not.toContain(retired);
    }
    const settings = persisted.workspaces[0].conversations[0].settings;
    // A conversation's settings carry no `modelSelection`. A role does, but roles
    // are files the host discovers, so a conversation holds only the ids it chose.
    expect(settings).not.toHaveProperty("modelSelection");
    expect(settings).not.toHaveProperty("agentDefinitions");
    expect(settings.skillIds).toEqual(["skill_installer"]);
    expect(Object.keys(settings)).toEqual(expect.arrayContaining([
      "hookIds",
      "skillIds",
      "mcpIds",
      "agentIds",
      "toolDescriptionFileId",
      "webSearch"
    ]));
    expect(Object.keys(settings.webSearch).sort()).toEqual([
      "compressionCutoff",
      "domainFilter",
      "excludeDomains",
      "fetchCompressionCutoff",
      "fetchProvider",
      "includeDomains",
      "maxResults",
      "maxSearchesPerCall",
      "nativeFetchTool",
      "nativeSearchTool",
      "provider"
    ]);
  });

  it("persists a validated structured result through a real save/load round-trip", async () => {
    const document = createSeedDocument();
    const conversation = document.workspaces[0].conversations[0];
    const timestamp = "2026-07-28T00:00:00Z";
    const structuredOutput = { verdict: "ok", findings: [{ file: "a.ts", line: 3 }] };
    const outputSchema = {
      type: "object",
      properties: { verdict: { type: "string" } },
      required: ["verdict"]
    };
    const usage = { inputTokens: 120, cachedInputTokens: 40, outputTokens: 8, totalTokens: 128 };
    conversation.contexts = [{
      id: "schema-bound-agent",
      kind: "tool",
      toolName: "agent_spawn",
      input: {},
      result: { success: true, output: "spawned a1", executedAt: timestamp, durationMs: 1 },
      subagent: {
        kind: "general",
        name: "a1",
        task: "review",
        status: "completed",
        contexts: [],
        updates: [],
        structuredOutput,
        outputSchema,
        usage
      },
      createdAt: timestamp
    }];

    await saveDocument(document);
    const stored = JSON.parse(window.localStorage.getItem(STORAGE_KEY)!);
    expect(stored.workspaces[0].conversations[0].contexts[0].subagent.structuredOutput)
      .toEqual(structuredOutput);
    // Dropping the schema on save silently unbinds every later continuation of
    // this agent (the host re-compiles it at rehydration), so the STORED bytes
    // are the assertion that matters.
    expect(stored.workspaces[0].conversations[0].contexts[0].subagent.outputSchema)
      .toEqual(outputSchema);
    // The host fingerprints the ENTIRE serialized record for the tool receipt,
    // so a field that survives the run but not the save does not merely lose a
    // number — it makes the document unsaveable with "结果或附属记录不是后端执行返回值".
    // That is exactly how `usage` was lost; this builder is an allowlist, so
    // every field of SubagentRunRecord needs an assertion like this one.
    expect(stored.workspaces[0].conversations[0].contexts[0].subagent.usage)
      .toEqual(usage);

    const loaded = (await loadDocument()).workspaces[0].conversations[0];
    expect((loaded.contexts[0] as Extract<ContextItem, { kind: "tool" }>).subagent?.structuredOutput)
      .toEqual(structuredOutput);
    expect((loaded.contexts[0] as Extract<ContextItem, { kind: "tool" }>).subagent?.outputSchema)
      .toEqual(outputSchema);
    expect((loaded.contexts[0] as Extract<ContextItem, { kind: "tool" }>).subagent?.usage)
      .toEqual(usage);
  });

  it("carries the host attestation through a save/load round-trip", async () => {
    const document = createSeedDocument();
    const conversation = document.workspaces[0].conversations[0];
    const timestamp = "2026-08-09T00:00:00Z";
    const attestation = "a".repeat(64);
    conversation.contexts = [{
      id: "attested-card",
      kind: "tool",
      toolName: "read",
      input: { path: "README.md" },
      result: { success: true, output: "contents", executedAt: timestamp, durationMs: 1 },
      attestation,
      createdAt: timestamp
    }];

    await saveDocument(document);
    const stored = JSON.parse(window.localStorage.getItem(STORAGE_KEY)!);

    // This projection is an allowlist, so an unnamed field is dropped with no
    // type error. Dropping this one strips the card's only durable proof that
    // its result came from the backend, and the host quarantines the card on
    // the very next save — the same class of failure that losing `usage` caused.
    expect(stored.workspaces[0].conversations[0].contexts[0].attestation).toBe(attestation);

    const loaded = (await loadDocument()).workspaces[0].conversations[0];
    expect((loaded.contexts[0] as Extract<ContextItem, { kind: "tool" }>).attestation)
      .toBe(attestation);
  });

  it("carries the provider call id through a save/load round-trip", async () => {
    const document = createSeedDocument();
    const conversation = document.workspaces[0].conversations[0];
    const timestamp = "2026-08-09T00:00:00Z";
    const providerCallId = "toolu_01DijKBKyyWKCXcHTjEoAuJz";
    conversation.contexts = [{
      id: "ctx_tool_abc",
      kind: "tool",
      toolName: "read",
      input: { path: "README.md" },
      result: { success: true, output: "contents", executedAt: timestamp, durationMs: 1 },
      providerCallId,
      createdAt: timestamp
    }];

    await saveDocument(document);
    const stored = JSON.parse(window.localStorage.getItem(STORAGE_KEY)!);

    // Same allowlist hazard as the attestation above, but it fails quietly:
    // dropping this field does not break a save, it makes replay mint a digest
    // instead, so every tool call in the turn changes id on the next turn and
    // the prompt cache for that turn is forfeited. Nothing goes red.
    expect(stored.workspaces[0].conversations[0].contexts[0].providerCallId).toBe(providerCallId);

    const loaded = (await loadDocument()).workspaces[0].conversations[0];
    expect((loaded.contexts[0] as Extract<ContextItem, { kind: "tool" }>).providerCallId)
      .toBe(providerCallId);
  });

  it("leaves a pre-versioning binding unstamped instead of inventing a receipt version", async () => {
    const document = createSeedDocument();
    const conversation = document.workspaces[0].conversations[0];
    const timestamp = "2026-07-24T01:00:00Z";
    const toolContext = (id: string, subagent: SubagentRunRecord): ContextItem => ({
      id,
      kind: "tool",
      toolName: "agent_spawn",
      input: {},
      result: {
        success: true,
        output: `spawned ${subagent.name}`,
        executedAt: timestamp,
        durationMs: 1
      },
      subagent,
      createdAt: timestamp
    });
    const baseRecord = {
      kind: "general" as const,
      task: "continue safely",
      status: "completed" as const,
      contexts: [],
      updates: []
    };
    // Neither binding carries `receiptVersion`, exactly as every record written
    // before the payload version existed looks on disk.
    conversation.contexts = [
      toolContext("legacy-fork-agent", {
        ...baseRecord,
        name: "legacy-fork-a1",
        inheritsModelMemory: true,
        forkModelBinding: {
          providerId: "provider-a",
          modelId: "kimi-k3",
          memoryLanguage: "zh-CN",
          memoryToolNames: ["memory_read"],
          systemPromptSnapshot: "trusted fork prompt",
          systemPromptReceipt: "2".repeat(64),
          bindingReceipt: "4".repeat(64)
        },
        executionModeReceipt: "5".repeat(64)
      }),
      toolContext("legacy-named-agent", {
        ...baseRecord,
        name: "legacy-named-a1",
        agentDefinition: {
          source: "user",
          sourceKey: "",
          name: "reviewer",
          revision: 7,
          memoryEpoch: 3,
          providerId: "provider-a",
          modelId: "deepseek-v4-flash",
          memory: "project",
          scopeKey: "workspace-a",
          configurationReceipt: "6".repeat(64)
        },
        executionModeReceipt: "7".repeat(64)
      })
    ];

    await saveDocument(document);
    const storedContexts = JSON.parse(window.localStorage.getItem(STORAGE_KEY)!)
      .workspaces[0].conversations[0].contexts;
    // Absence is the version marker on the host side, so the projection must
    // preserve absence: stamping an explicit 1 here would make a genuine
    // pre-versioning record indistinguishable from a deliberately stamped v1.
    expect(Object.keys(storedContexts[0].subagent.forkModelBinding))
      .not.toContain("receiptVersion");
    expect(Object.keys(storedContexts[1].subagent.agentDefinition))
      .not.toContain("receiptVersion");
    // The rest of the binding still round-trips, so the absence above is the
    // projection preserving the record rather than losing the whole binding.
    expect(storedContexts[0].subagent.forkModelBinding.bindingReceipt).toBe("4".repeat(64));
    expect(storedContexts[1].subagent.agentDefinition.configurationReceipt).toBe("6".repeat(64));

    const loadedContexts = (await loadDocument()).workspaces[0].conversations[0].contexts;
    expect(loadedContexts).toEqual(storedContexts);
    const loadedFork = loadedContexts[0] as Extract<ContextItem, { kind: "tool" }>;
    const loadedNamed = loadedContexts[1] as Extract<ContextItem, { kind: "tool" }>;
    expect(loadedFork.subagent?.forkModelBinding?.receiptVersion).toBeUndefined();
    expect(loadedNamed.subagent?.agentDefinition?.receiptVersion).toBeUndefined();
  });

  it("keeps the browser marker when API format or endpoint changes", async () => {
    const provider = createSeedDocument().globalSettings.apiProviders[0];
    await saveApiKey(provider, "discard-me");
    // Key-state identity is provider ID only, so endpoint and protocol changes retain
    // the same marker.
    expect(await getStoredApiKeyLength(provider.id)).toBe(10);
    await saveApiKey({ ...provider, baseUrl: `${provider.baseUrl}/` }, "discard-me");
    expect(await getStoredApiKeyLength(provider.id)).toBe(10);
    await saveApiKey({ ...provider, family: "openai_chat" }, "discard-me");
    expect(await getStoredApiKeyLength(provider.id)).toBe(10);
  });

  it("deletes browser key state and its persisted length", async () => {
    const provider = createSeedDocument().globalSettings.apiProviders[0];
    await saveApiKey(provider, "delete-this-key");
    expect(await getStoredApiKeyLength(provider.id)).toBe(15);

    await expect(deleteApiKey(provider)).resolves.toEqual({ configured: false });
    expect(await getStoredApiKeyLength(provider.id)).toBeUndefined();
  });

  it("preserves malformed browser JSON instead of silently replacing it with defaults", async () => {
    const malformed = "{not-json";
    window.localStorage.setItem(STORAGE_KEY, malformed);

    await expect(loadDocument()).rejects.toThrow(/原始数据已保留/);
    expect(window.localStorage.getItem(STORAGE_KEY)).toBe(malformed);
  });

  it("preserves documents written by a future schema instead of downgrading them", async () => {
    const future = createSeedDocument();
    future.schemaVersion += 1;
    const serialized = JSON.stringify(future);
    window.localStorage.setItem(STORAGE_KEY, serialized);

    await expect(loadDocument()).rejects.toThrow(/更新版本|schema/);
    expect(window.localStorage.getItem(STORAGE_KEY)).toBe(serialized);
  });

  it("rejects documents written by an outdated schema now that migrations are deleted", async () => {
    const outdated = createSeedDocument();
    outdated.schemaVersion -= 1;
    const serialized = JSON.stringify(outdated);
    window.localStorage.setItem(STORAGE_KEY, serialized);

    await expect(loadDocument()).rejects.toThrow(/旧版 schema/);
    expect(window.localStorage.getItem(STORAGE_KEY)).toBe(serialized);
  });

  it("trims and deduplicates provider and model IDs before resolving active selections", () => {
    const source = createSeedDocument() as unknown as Record<string, unknown>;
    const settings = { ...(source.globalSettings as Record<string, unknown>) };
    const providers = settings.apiProviders as Array<Record<string, unknown>>;
    settings.apiProviders = [
      {
        ...providers[0],
        id: "  openai_responses  ",
        activeModelId: "  model-a  ",
        models: [
          // The first two collide once the id is trimmed. Their capability sets
          // differ so the assertion below proves the earlier entry wins rather
          // than the later duplicate.
          { id: "  model-a  ", name: "", group: "", capabilities: ["image_recognition"] },
          { id: "model-a", name: "", group: "", capabilities: [] },
          { id: "model-b", name: "", group: "", capabilities: [] },
          { id: "   ", name: "", group: "", capabilities: [] }
        ]
      },
      { ...providers[0], id: "openai_responses", name: "duplicate provider" },
      { ...providers[1], id: "  openai_chat  " }
    ];
    settings.activeProviderId = "  openai_responses  ";
    source.globalSettings = settings;

    const normalized = normalizeDocument(source);
    expect(normalized.globalSettings.apiProviders.slice(0, 2).map((provider) => provider.id))
      .toEqual(["openai_responses", "openai_chat"]);
    expect(normalized.globalSettings.apiProviders[2]).toMatchObject({ family: "openai_codex", enabled: false });
    expect(normalized.globalSettings.activeProviderId).toBe("openai_responses");
    expect(normalized.globalSettings.apiProviders[0].models.map((model) => model.id)).toEqual(["model-a", "model-b"]);
    expect(normalized.globalSettings.apiProviders[0].models[0].capabilities).toEqual(["image_recognition"]);
    expect(normalized.globalSettings.apiProviders[0].activeModelId).toBe("model-a");
    // The rows above predate `promptCache`; a document from before the key
    // existed loads with Claude Code's default rather than an undefined hole.
    expect(normalized.globalSettings.apiProviders[0].models.map((model) => model.promptCache)).toEqual([true, true]);
  });

  it("keeps a curated promptCache: false through document normalization", () => {
    const source = createSeedDocument() as unknown as Record<string, unknown>;
    const settings = { ...(source.globalSettings as Record<string, unknown>) };
    const providers = settings.apiProviders as Array<Record<string, unknown>>;
    settings.apiProviders = [
      {
        ...providers[0],
        models: [
          { id: "off", name: "", group: "", capabilities: [], promptCache: false },
          { id: "malformed", name: "", group: "", capabilities: [], promptCache: "off" }
        ],
        activeModelId: "off"
      }
    ];
    source.globalSettings = settings;
    const normalized = normalizeDocument(source);
    expect(normalized.globalSettings.apiProviders[0].models.map((model) => [model.id, model.promptCache]))
      .toEqual([["off", false], ["malformed", true]]);
  });

  it("adds only the built-in providers to document-carried user entries", () => {
    // Provider normalization preserves user rows and appends its fixed built-in rows.
    const source = createSeedDocument() as unknown as Record<string, unknown>;
    const settings = { ...(source.globalSettings as Record<string, unknown>) };
    settings.apiProviders = [{
      id: "provider_mine",
      name: "我的中转站",
      enabled: true,
      family: "openai_chat",
      baseUrl: "https://relay.example.com/v1",
      familySettings: {},
      notes: "",
      models: [{ id: "gpt-5", name: "", group: "", capabilities: [] }],
      activeModelId: "gpt-5"
    }];
    settings.activeProviderId = "provider_mine";
    source.globalSettings = settings;

    const normalized = normalizeDocument(source);
    expect(normalized.globalSettings.apiProviders[0].id).toBe("provider_mine");
    expect(normalized.globalSettings.apiProviders).toHaveLength(3);
    expect(normalized.globalSettings.apiProviders[0]).toMatchObject({
      enabled: true,
      name: "我的中转站",
      baseUrl: "https://relay.example.com/v1",
      activeModelId: "gpt-5"
    });
    expect(normalized.globalSettings.apiProviders[1]).toMatchObject({ family: "openai_codex", enabled: false });
    expect(normalized.globalSettings.apiProviders[2]).toMatchObject({ family: "claude_agent", enabled: false });
    expect(normalized.globalSettings.activeProviderId).toBe("provider_mine");
    // Catalog-like IDs receive no special treatment; they are ordinary user entries.
    expect(normalized.globalSettings.apiProviders.some((provider) => provider.id === "openai"))
      .toBe(false);
  });

  it("sends every level as thinking effort, with no thinking-off mode", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: { transformCallback: vi.fn(() => 1) }
    });
    const document = createSeedDocument();
    const provider = document.globalSettings.apiProviders[0];
    const model: ModelProfile = {
      id: "thinking-wire-model",
      name: "",
      group: "",
      capabilities: [],
      reasoningContent: "encrypted",
      promptCache: true
    };
    const response = {
      contexts: [],
      usage: {},
      model: model.id,
      providerName: provider.name,
      durationMs: 1
    };
    tauriMocks.invoke.mockResolvedValue(response);
    const request = {
      provider,
      model,
      reasoningEffort: "low" as const,
      conversationId: document.workspaces[0].conversations[0].id,
      workspacePath: document.workspaces[0].path,
      systemPrompt: "",
      enabledTools: [],
      contexts: [],
      tools: document.tools
    };

    await runModel(request, vi.fn(), "low-thinking");
    await runModel({ ...request, reasoningEffort: "max" }, vi.fn(), "max-thinking");

    const lowRequest = (tauriMocks.invoke.mock.calls[0][1] as {
      request: Record<string, unknown>;
    }).request;
    expect(lowRequest).toMatchObject({ thinkingEffort: "low" });
    expect(lowRequest).not.toHaveProperty("thinkingMode");
    expect(lowRequest).not.toHaveProperty("reasoningEffort");

    const maxRequest = (tauriMocks.invoke.mock.calls[1][1] as {
      request: Record<string, unknown>;
    }).request;
    expect(maxRequest).toMatchObject({ thinkingEffort: "max" });
    expect(maxRequest).not.toHaveProperty("thinkingMode");
    expect(maxRequest).not.toHaveProperty("reasoningEffort");
  });

  it("fetches the model catalog for a provider that is not enabled yet", async () => {
    // Fetching a catalog is configuration-time work: a provider must be reachable and
    // its models discovered before it can be enabled. An enabled-only gate would make
    // provider setup impossible and fail silently.
    Object.defineProperty(window, "__TAURI_INTERNALS__", { configurable: true, value: {} });
    const provider = { ...createSeedDocument().globalSettings.apiProviders[0], enabled: false };
    tauriMocks.invoke.mockResolvedValue([{ id: "catalog-model" }]);

    const models = await fetchModels(provider);

    expect(models.map((model) => model.id)).toEqual(["catalog-model"]);
    // Discovery resolves the protocol default rather than leaving it deferred:
    // this seed provider is the Responses family.
    expect(models[0].reasoningContent).toBe("encrypted");
    // A discovered model starts with prompt caching on, like Claude Code.
    expect(models[0].promptCache).toBe(true);
    expect(tauriMocks.invoke).toHaveBeenCalledWith("fetch_models", { provider });
  });

  it("serves the built-in Claude Agent catalog in browser preview", async () => {
    // There is no `GET /models` behind a local CLI, so the preview mirrors the
    // host's built-in registry row for row instead of inventing placeholder ids.
    const provider = {
      ...createSeedDocument().globalSettings.apiProviders[0],
      family: "claude_agent" as const,
      baseUrl: ""
    };

    const models = await fetchModels(provider);

    expect(models.map((model) => model.id)).toEqual(CLAUDE_AGENT_REGISTRY.map((entry) => entry.id));
    expect(models.map((model) => model.contextWindow))
      .toEqual(CLAUDE_AGENT_REGISTRY.map((entry) => entry.contextWindow));
    expect(models.map((model) => model.maxOutputTokens))
      .toEqual(CLAUDE_AGENT_REGISTRY.map((entry) => entry.maxOutputTokens));
    expect(tauriMocks.invoke).not.toHaveBeenCalled();
  });

  it("replaces stale tool descriptors and removes unsupported tool references", () => {
    const source = createSeedDocument();
    source.tools.push({
      name: "extinct-tool",
      label: "已下线工具",
      description: "旧占位工具",
      category: "orchestration",
      dangerous: false,
      parameters: []
    });
    defaultPresetSettings(source).enabledTools.push("extinct-tool", "missing-tool");
    source.workspaces[0].conversations[0].settings.enabledTools.push("extinct-tool", "missing-tool");

    const normalized = normalizeDocument(source);
    expect(normalized.tools.map((tool) => tool.name)).toEqual(
      createSeedDocument().tools.map((tool) => tool.name)
    );
    expect(defaultPresetSettings(normalized).enabledTools).not.toContain("extinct-tool");
    expect(normalized.workspaces[0].conversations[0].settings.enabledTools).not.toContain("missing-tool");
  });

  it("keeps a deliberate tool disable through normalization", () => {
    const document = createSeedDocument();
    defaultPresetSettings(document).enabledTools = defaultPresetSettings(document)
      .enabledTools.filter((name) => name !== "web_search");
    const conversation = document.workspaces[0].conversations[0];
    conversation.settings.enabledTools = conversation.settings.enabledTools
      .filter((name) => name !== "preview_start");

    const normalized = normalizeDocument(document);
    expect(defaultPresetSettings(normalized).enabledTools).not.toContain("web_search");
    expect(normalized.workspaces[0].conversations[0].settings.enabledTools).not.toContain("preview_start");
  });

  it("preserves dynamically discovered MCP descriptors and their enablement", () => {
    // Rust persists MCP-discovered tools, including `inputSchema`, in `tools`.
    // Replacing them unconditionally with the seed catalog would silently lose both
    // descriptors and enablement.
    const source = createSeedDocument();
    source.tools.push({
      name: "mcp__probe__search",
      label: "MCP 搜索",
      description: "动态发现的 MCP 工具",
      category: "mcp",
      dangerous: false,
      parameters: [],
      inputSchema: { type: "object", properties: { query: { type: "string" } } }
    });
    source.workspaces[0].conversations[0].settings.enabledTools.push("mcp__probe__search");

    const normalized = normalizeDocument(source);
    const preserved = normalized.tools.find((tool) => tool.name === "mcp__probe__search");
    expect(preserved).toBeDefined();
    expect(preserved!.inputSchema).toEqual({
      type: "object",
      properties: { query: { type: "string" } }
    });
    expect(normalized.workspaces[0].conversations[0].settings.enabledTools)
      .toContain("mcp__probe__search");
  });

  it("keeps valid queued messages and rejects malformed queue records while normalizing", () => {
    const source = createSeedDocument() as unknown as AppDocument & {
      workspaces: Array<AppDocument["workspaces"][number] & {
        conversations: Array<Record<string, unknown>>
      }>
    };
    const image = {
      id: "a".repeat(64),
      name: "screen.png",
      mime: "image/png",
      width: 1280,
      height: 720,
      bytes: 4096
    };
    source.workspaces[0].conversations[0].queuedMessages = [
      {
        id: "queue-valid",
        content: "稍后执行",
        images: [image],
        createdAt: "2026-07-24T00:00:00Z"
      },
      {
        id: "queue-image-only",
        content: "",
        images: [image],
        createdAt: "2026-07-24T00:00:01Z"
      },
      {
        id: "",
        content: "无效",
        createdAt: "2026-07-24T00:00:00Z"
      },
      {
        id: "queue-invalid-time",
        content: "无效",
        createdAt: "not-a-date"
      }
    ];

    expect(normalizeDocument(source).workspaces[0].conversations[0].queuedMessages).toEqual([
      {
        id: "queue-valid",
        content: "稍后执行",
        images: [image],
        createdAt: "2026-07-24T00:00:00Z"
      },
      {
        id: "queue-image-only",
        content: "",
        images: [image],
        createdAt: "2026-07-24T00:00:01Z"
      }
    ]);
  });

  it("serializes Tauri document saves and snapshots each queued value", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", { configurable: true, value: {} });
    let resolveFirst!: () => void;
    tauriMocks.invoke
      .mockImplementationOnce(() => new Promise<void>((resolve) => {
        resolveFirst = resolve;
      }))
      .mockResolvedValueOnce(undefined);
    const first = createSeedDocument();
    defaultPresetSettings(first).toolDescriptionFileId = "first snapshot";
    const second = createSeedDocument();
    defaultPresetSettings(second).toolDescriptionFileId = "second snapshot";

    const firstSave = saveDocument(first);
    defaultPresetSettings(first).toolDescriptionFileId = "mutated after queueing";
    const secondSave = saveDocument(second);

    await vi.waitFor(() => expect(tauriMocks.invoke).toHaveBeenCalledTimes(1));
    expect(tauriMocks.invoke.mock.calls[0][0]).toBe("save_document");
    expect(persistedPresetSettings(
      (tauriMocks.invoke.mock.calls[0][1] as { document: unknown }).document
    ).toolDescriptionFileId).toBe("first snapshot");
    resolveFirst();
    await firstSave;
    await secondSave;

    expect(tauriMocks.invoke).toHaveBeenCalledTimes(2);
    expect(persistedPresetSettings(
      (tauriMocks.invoke.mock.calls[1][1] as { document: unknown }).document
    ).toolDescriptionFileId).toBe("second snapshot");
  });

  it("waits for the native durability barrier after queued document saves", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", { configurable: true, value: {} });
    tauriMocks.invoke.mockResolvedValue(undefined);

    await saveDocument(createSeedDocument());
    await flushDocumentSaves();

    expect(tauriMocks.invoke).toHaveBeenNthCalledWith(
      tauriMocks.invoke.mock.calls.length - 1,
      "save_document",
      expect.any(Object)
    );
    expect(tauriMocks.invoke).toHaveBeenLastCalledWith("flush_document_saves");
  });

  it("marks workflow-critical document saves as durable in the native command", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", { configurable: true, value: {} });
    tauriMocks.invoke.mockResolvedValue(undefined);
    const document = createSeedDocument();

    await saveDocument(document, { immutableSnapshot: true, durable: true });

    expect(tauriMocks.invoke).toHaveBeenLastCalledWith("save_document", {
      document: expect.any(Object),
      durable: true
    });
  });

  it("cleans only the removed provider's browser key marker after the deletion is persisted", async () => {
    const document = createSeedDocument();
    const removed = document.globalSettings.apiProviders[0];
    const retained = document.globalSettings.apiProviders[1];
    await saveDocument(document);
    await saveApiKey(removed, "removed-key");
    await saveApiKey(retained, "retained-key");

    const next = {
      ...document,
      globalSettings: {
        ...document.globalSettings,
        apiProviders: document.globalSettings.apiProviders.filter((provider) => provider.id !== removed.id),
        activeProviderId: retained.id
      }
    };
    await saveDocument(next);

    expect(await getStoredApiKeyLength(removed.id)).toBeUndefined();
    expect(await getStoredApiKeyLength(retained.id)).toBe(12);
  });
});
