import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { configureI18n } from "../i18n";
import { resetCacheBreakWarnings } from "../lib/cacheBreakWarning";
import { modelChoiceOf } from "../lib/documentUpdates";
import { knownProtocolCapabilities } from "../lib/modelCapabilities";
import { defaultConversationWebSearchSettings, type SaveAgentRoleTarget } from "../lib/runtime";
import { isHostDerivedToolName, isPreviewLifecycleToolName } from "../lib/taskTools";
import { EMPTY_TOOL_LOCK } from "../lib/toolLock";
import type { CapabilityWorkspace } from "../lib/workspaces";
import { createTestDocument as createSeedDocument } from "../test/fixtures";
import { BUILTIN_PRESET_ID } from "../seed";
import type {
  AgentRole,
  AgentRoleResource,
  CapabilityCatalog,
  CapabilityResourceKind,
  ContextItem,
  Conversation,
  ConversationPreset,
  ConversationPresetSettings,
  ConversationSettings as ConversationSettingsType,
  ConversationTemplateSummary,
  ConversationToolLock,
  GlobalSettings,
  McpProbeReport,
  ResourceDescriptor,
  ToolDescriptor
} from "../types";

import { ConversationSettings } from "./ConversationSettings";

/* Template bodies live in the host's store, never in the document, so the pane
   only ever reaches them through these two. Module-level so a rerender does not
   hand the pane a new identity and re-read the body under the editor. */
const readNothing = async (): Promise<ContextItem[]> => [];
/** A conversation with one workspace on this computer, as `capabilityWorkspaces` lists it. */
const WORKSPACE_A: CapabilityWorkspace[] = [{ number: 1, key: "local|/work/a", machine: null, path: "/work/a" }];
const writeSomewhere = async (templateId: string): Promise<string> => templateId || "template_minted";

/* The test document selects a provider with no models, so a test about the
   lock selects one: Opus 5.5 takes a tool mid-conversation on the Messages
   API, Haiku 4.5 does not. Each declares what a fetch from that endpoint
   would declare for it. */
function withSelectedModel(globalSettings: GlobalSettings, modelId: string): GlobalSettings {
  return {
    ...globalSettings,
    activeProviderId: "anthropic_messages",
    apiProviders: globalSettings.apiProviders.map((provider) => (
      provider.id === "anthropic_messages"
        ? {
          ...provider,
          activeModelId: modelId,
          models: [{
            id: modelId,
            name: modelId,
            group: "claude",
            capabilities: knownProtocolCapabilities(provider, modelId),
            reasoningContent: "plaintext",
            promptCache: true
          }]
        }
        : provider
    ))
  };
}
const withWarmModel = (globalSettings: GlobalSettings) => withSelectedModel(globalSettings, "claude-opus-5-5");
const withNonAppendModel = (globalSettings: GlobalSettings) => withSelectedModel(globalSettings, "claude-haiku-4-5");

/* A lock as a request by the selected model would have left it — just now,
   unless `at` says otherwise — so a test states only what it is about. */
function lockFor(
  globalSettings: GlobalSettings,
  patch: Partial<ConversationToolLock> = {},
  at = new Date().toISOString()
): ConversationToolLock {
  const { provider, model } = modelChoiceOf(globalSettings);
  return {
    ...EMPTY_TOOL_LOCK,
    promptSkillIds: [],
    lastRequest: { providerId: provider!.id, modelId: model!.id, at },
    ...patch
  };
}

beforeEach(() => resetCacheBreakWarnings());

/* The two web-backend pickers are menus rather than `<select>`s, because the
   native row opens a second step naming the wire tool version and an option
   list cannot hold one. Drive them the way a user does: open, then click. */
function backendTrigger(field: string): HTMLElement {
  return screen.getByRole("button", { name: new RegExp(`^${field}：`) });
}

async function chooseBackend(
  user: ReturnType<typeof userEvent.setup>,
  field: string,
  row: string | RegExp
): Promise<void> {
  await user.click(backendTrigger(field));
  await user.click(
    within(screen.getByRole("menu", { name: field })).getByRole("menuitemradio", { name: row })
  );
}

function SettingsHarness({
  initialConversation,
  globalSettings,
  tools,
  roleTools,
  capabilities,
  onSettingsChange,
  onConversationOnlyChange = vi.fn(),
  onApplyPreset = vi.fn(),
  onRenamePreset = vi.fn(),
  onDeletePreset = vi.fn(),
  onSavePreset = vi.fn(),
  onSavePresetCopy = vi.fn(),
  onCreatePreset,
  onBindPresetTemplate = vi.fn(),
  templates = [],
  onReadTemplate = readNothing,
  onWriteTemplate = writeSomewhere,
  onDeleteCapability,
  onSaveAgentRole,
  workspaces,
  onRescanCapabilities,
  onRevealCapabilityLocation,
  onProbeMcpServer,
  onCapabilityFingerprint
}: {
  initialConversation: Conversation;
  globalSettings: GlobalSettings;
  tools: ToolDescriptor[];
  /** The whole catalogue a role's window draws from, where it differs from the conversation's. */
  roleTools?: ToolDescriptor[];
  capabilities: CapabilityCatalog;
  onSettingsChange: (settings: ConversationSettingsType) => void;
  onConversationOnlyChange?: (patch: Partial<ConversationSettingsType>) => void;
  onApplyPreset?: (presetId: string) => void;
  onRenamePreset?: (presetId: string, name: string) => void;
  onDeletePreset?: (presetId: string) => void;
  onSavePreset?: (presetId: string, settings: ConversationPresetSettings) => void;
  onSavePresetCopy?: (presetId: string, settings: ConversationPresetSettings, templateBody: ContextItem[] | null) => void;
  /** Resolves with the preset it made; the harness puts it in the document, as App does. */
  onCreatePreset?: (request: { name: string; captureTemplate: boolean }) => Promise<ConversationPreset>;
  onBindPresetTemplate?: (presetId: string, templateId: string) => void;
  templates?: ConversationTemplateSummary[];
  onReadTemplate?: (templateId: string) => Promise<ContextItem[]>;
  onWriteTemplate?: (templateId: string, contexts: ContextItem[]) => Promise<string>;
  onDeleteCapability?: (
    kind: CapabilityResourceKind,
    resource: ResourceDescriptor
  ) => Promise<boolean> | undefined;
  /** Writes a role file from the role window and resolves with the id it landed under. */
  onSaveAgentRole?: (target: SaveAgentRoleTarget, role: AgentRole) => Promise<string>;
  /** The conversation's workspaces. Undefined is "no prop": no narrowing at all. */
  workspaces?: CapabilityWorkspace[];
  onRescanCapabilities?: () => void | Promise<void>;
  onRevealCapabilityLocation?: (
    kind: CapabilityResourceKind | "toolDescriptions",
    workspaceKey: string | null
  ) => void;
  onProbeMcpServer?: (resource: ResourceDescriptor) => Promise<McpProbeReport>;
  onCapabilityFingerprint?: () => Promise<string>;
}) {
  const [conversation, setConversation] = useState(initialConversation);
  const [presets, setPresets] = useState(globalSettings.conversationPresets);
  return (
    <ConversationSettings
      conversation={conversation}
      globalSettings={{ ...globalSettings, conversationPresets: presets }}
      tools={tools}
      roleTools={roleTools}
      capabilities={capabilities}
      onChange={(settings) => {
        onSettingsChange(settings);
        setConversation((current) => ({ ...current, settings }));
      }}
      onChangeConversationOnly={(patch) => {
        onConversationOnlyChange(patch);
        setConversation((current) => ({
          ...current,
          settings: { ...current.settings, ...patch }
        }));
      }}
      onApplyPreset={onApplyPreset}
      onRenamePreset={onRenamePreset}
      onDeletePreset={onDeletePreset}
      onSavePreset={onSavePreset}
      onSavePresetCopy={onSavePresetCopy}
      onCreatePreset={onCreatePreset && (async (request) => {
        const preset = await onCreatePreset(request);
        setPresets((current) => [...current, preset]);
        return preset;
      })}
      onBindPresetTemplate={onBindPresetTemplate}
      templates={templates}
      onReadTemplate={onReadTemplate}
      onWriteTemplate={onWriteTemplate}
      onDeleteCapability={onDeleteCapability}
      onSaveAgentRole={onSaveAgentRole}
      workspaces={workspaces}
      onRescanCapabilities={onRescanCapabilities}
      onRevealCapabilityLocation={onRevealCapabilityLocation}
      onProbeMcpServer={onProbeMcpServer}
      onCapabilityFingerprint={onCapabilityFingerprint}
    />
  );
}

/** Every page is reached from the list on the left, which is the pane's only navigation. */
function navigation(): HTMLElement {
  // Matched by either label so the helper serves the English tests too.
  return screen.getByRole("navigation", {
    name: /对话设置分类|Conversation settings categories/
  });
}

async function openPage(user: ReturnType<typeof userEvent.setup>, name: RegExp): Promise<void> {
  await user.click(within(navigation()).getByRole("button", { name }));
}

/* The tool list's own rail entry. A bare /^工具/ also matches 「工具描述」, the
   page that follows the hooks, so the tool list is named by what it is not. */
const TOOLS_PAGE = /^工具(?!描述)/;
const TOOL_DESCRIPTIONS_PAGE = /^工具描述/;
/** The prompt profile every fixture conversation's lock is worded with unless it says otherwise. */
const BUILTIN_PROFILE = "tooldesc_builtin_en_us";
const CONCISE_PROFILE = "tooldesc_builtin_concise_en_us";
const MAIN_PROFILE = "tooldesc_user_main_0f0f0f0f";
const ALT_PROFILE = "tooldesc_user_alt_1a1a1a1a";
/** An id a conversation still names though the catalog has lost the file. */
const GONE_PROFILE = "tooldesc_user_gone_deadbeef";
/** A file that is in the catalog but has nothing usable in it. */
const BROKEN_PROFILE = "tooldesc_user_broken_2b2b2b2b";
const CACHE_BREAK_TITLE = "这样改会让缓存失效";

/** The fixture's one file of the user's, plus a second to replace it with. */
function withAltToolDescription(capabilities: CapabilityCatalog): void {
  capabilities.toolDescriptionFiles.push({
    id: ALT_PROFILE,
    name: "alt",
    description: "1 个工具描述 · 0 条提示词覆盖",
    location: "test://tool-descriptions/alt.json",
    source: "user",
    available: true
  });
}

/** A stored template, as the pane sees it: a name, and how long its body is. */
function template(
  id: string,
  name: string,
  messageCount: number
): ConversationTemplateSummary {
  return {
    id,
    name,
    messageCount,
    createdAt: "2026-01-02T03:04:05.000Z",
    updatedAt: "2026-01-03T03:04:05.000Z"
  };
}

/* A role body that follows its caller on every answer it may, so a test states
   only what it is about. */
function roleBody(name: string, patch: Partial<AgentRole> = {}): AgentRole {
  return {
    name,
    description: "",
    modelSelection: { kind: "inherit" },
    effort: null,
    tools: [],
    disallowedTools: [],
    skillIds: [],
    mcpIds: [],
    hookIds: [],
    webSearch: defaultConversationWebSearchSettings(),
    templateId: null,
    toolDescriptionFileId: null,
    ...patch
  };
}

/** One role as the catalog lists it: a file the host read, at a level of its own. */
function roleResource(
  id: string,
  name: string,
  patch: Partial<AgentRoleResource> = {}
): AgentRoleResource {
  return {
    id,
    name,
    description: "",
    location: `/home/me/.mewrk/agents/${id}.json`,
    source: "user",
    available: true,
    role: roleBody(name),
    ...patch
  };
}

/* The roles a catalog lists, one of each kind a page has to draw: two global
   files, a file in `WORKSPACE_A`, and one whose body could not be read.
   Fresh on every call, so no test sees another's edits. */
function listedRoles(): AgentRoleResource[] {
  return [
    roleResource("agent_user_opus", "Opus", { location: "/home/me/.mewrk/agents/opus.json" }),
    roleResource("agent_user_reviewer", "reviewer", { location: "/home/me/.mewrk/agents/reviewer.json" }),
    roleResource("agent_ws_planner", "planner", {
      source: "workspace",
      location: "/work/a/.mewrk/agents/planner.json",
      workspaceKey: WORKSPACE_A[0].key
    }),
    roleResource("agent_user_broken", "broken", {
      available: false,
      description: "角色文件不是合法的 JSON",
      role: null
    })
  ];
}

/** The seed's catalog with `agents` in place of the roles the browser preview has no host to list. */
function withRoles(catalog: CapabilityCatalog, agents: AgentRoleResource[] = listedRoles()): CapabilityCatalog {
  return { ...catalog, agents };
}

/** The row a role's toggle sits in, for what is drawn beside it. */
function roleRow(name: string, root: HTMLElement = document.body): HTMLElement {
  return within(root).getByRole("button", { name }).closest(".catalog-row") as HTMLElement;
}

/** The pane over the role catalog, opened on its roles page. */
async function openRolesPage(options: {
  /** The ids the conversation has selected. */
  selected?: string[];
  agents?: AgentRoleResource[];
  workspaces?: CapabilityWorkspace[];
  globalSettings?: (settings: GlobalSettings) => GlobalSettings;
  /** What the conversation can run, where it is narrower than the catalogue. */
  tools?: (catalogue: ToolDescriptor[]) => ToolDescriptor[];
  /** The pane's navigation entry for the roles page, where it is not read in Chinese. */
  navigation?: RegExp;
  onDeleteCapability?: (kind: CapabilityResourceKind, resource: ResourceDescriptor) => undefined;
  onSaveAgentRole?: (target: SaveAgentRoleTarget, role: AgentRole) => Promise<string>;
} = {}) {
  const seed = createSeedDocument();
  const conversation = seed.workspaces[0].conversations[0];
  conversation.settings.agentIds = options.selected ?? [];
  const onSettingsChange = vi.fn();
  const user = userEvent.setup();
  const { unmount } = render(
    <SettingsHarness
      initialConversation={conversation}
      globalSettings={options.globalSettings?.(seed.globalSettings) ?? seed.globalSettings}
      tools={options.tools?.(seed.tools) ?? seed.tools}
      roleTools={seed.tools}
      capabilities={withRoles(seed.capabilities, options.agents)}
      onSettingsChange={onSettingsChange}
      workspaces={options.workspaces}
      onDeleteCapability={options.onDeleteCapability}
      onSaveAgentRole={options.onSaveAgentRole}
    />
  );
  await openPage(user, options.navigation ?? /^代理角色/);
  return { seed, user, onSettingsChange, unmount };
}

/** A save that lands the file under `id`, as the host does once the catalog has been rescanned. */
const savesRoleAs = (id: string) => (
  vi.fn<(target: SaveAgentRoleTarget, role: AgentRole) => Promise<string>>().mockResolvedValue(id)
);

describe("ConversationSettings", () => {
  afterEach(() => configureI18n("zh-CN"));

  it("lists its pages and opens on the tools page", () => {
    const seed = createSeedDocument();
    render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
      />
    );

    // The order is fixed and not data-sorted: the tool list first, then what
    // switches derive, then one page per catalog of named things the
    // conversation composes with (the tool-description files, which a
    // conversation picks one of, closing the catalogs), with the save-as-preset
    // entry point closing the list.
    const pages = within(navigation()).getAllByRole("button")
      .map((button) => (button.textContent ?? "").replace(/\d+$/, ""));
    expect(pages).toEqual(["工具", "高级工具", "技能", "MCP", "钩子", "工具描述", "代理角色", "对话预设", "另存为预设"]);
    // A live conversation's own timeline IS its message queue, editable in
    // place, so the template page belongs to a preset and to a role's window.
    expect(within(navigation()).queryByRole("button", { name: /^对话模板/ })).toBeNull();
    const active = within(navigation()).getByRole("button", { name: TOOLS_PAGE });
    expect(active).toHaveAttribute("aria-current", "true");
    // A row is `[category icon] [label] [optional count]`. Selection is the
    // active class alone, so the trailing chevron is gone and the row's one svg
    // is its leading icon — a stray second svg here would mean a stylesheet rule
    // targeting `svg:last-child` had found something to hide instead.
    expect(active.querySelectorAll("svg")).toHaveLength(1);
    // Nothing is a disclosure any more, so the catalogs are not rendered until visited.
    expect(screen.queryByRole("switch", { name: /代码审查/ })).toBeNull();
  });

  it("has no sandbox page: the sandbox belongs to each workspace", () => {
    const seed = createSeedDocument();
    render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
      />
    );

    expect(within(navigation()).queryByRole("button", { name: /^沙箱/ })).toBeNull();
  });

  it("keeps the save-as-preset entry point as the last thing in the nav", async () => {
    const seed = createSeedDocument();
    const user = userEvent.setup();
    render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
      />
    );

    const nav = navigation();
    const saveAs = within(nav).getByRole("button", { name: "另存为预设" });
    // Saving is the one preset action that belongs to the whole page rather than
    // to a row, so it is the sole thing under the list — and the picker that used
    // to sit there is gone.
    expect(saveAs.parentElement).toHaveClass("conversation-settings__preset-actions");
    expect(nav.lastElementChild).toHaveClass("conversation-settings__nav-footer");
    expect(saveAs.parentElement?.parentElement).toBe(nav.lastElementChild);
    expect(within(nav).getAllByRole("button").at(-1)).toBe(saveAs);
    expect(within(nav).queryByRole("combobox", { name: "套用预设" })).toBeNull();

    await user.click(saveAs);
    expect(screen.getByRole("dialog", { name: "另存为预设" })).toBeInTheDocument();
  });

  it("saves a preset under a name, with the timeline as its template when asked, and opens it at that page", async () => {
    const seed = createSeedDocument();
    const conversation = {
      ...seed.workspaces[0].conversations[0],
      contexts: [{ id: "ctx_here", kind: "user" as const, content: "先读一下 README", createdAt: "2026-01-02T03:04:05.000Z" }]
    };
    const onCreatePreset = vi.fn(async ({ name, captureTemplate }: { name: string; captureTemplate: boolean }) => ({
      ...seed.globalSettings.conversationPresets[0],
      id: "preset_review",
      name,
      description: "",
      templateId: captureTemplate ? "template_review" : ""
    }));
    const onReadTemplate = vi.fn(async (): Promise<ContextItem[]> => conversation.contexts);
    const user = userEvent.setup();
    render(
      <SettingsHarness
        initialConversation={conversation}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
        onCreatePreset={onCreatePreset}
        onReadTemplate={onReadTemplate}
      />
    );

    await user.click(within(navigation()).getByRole("button", { name: "另存为预设" }));
    const dialog = screen.getByRole("dialog", { name: "另存为预设" });
    // A name and one choice: no description to write, and no paragraph about it.
    expect(within(dialog).getAllByRole("textbox")).toHaveLength(1);
    expect(within(dialog).queryByText(/说明|可复用模板/)).toBeNull();
    const capture = within(dialog).getByRole("checkbox", { name: "将当前上下文作为对话模板" });
    expect(capture).not.toBeChecked();
    expect(within(dialog).getByRole("button", { name: "保存为预设" })).toBeDisabled();

    await user.type(within(dialog).getByRole("textbox", { name: "预设名称" }), "代码评审");
    await user.click(capture);
    await user.click(within(dialog).getByRole("button", { name: "保存为预设" }));

    expect(onCreatePreset).toHaveBeenCalledWith({ name: "代码评审", captureTemplate: true });
    // The new preset opens in its window, on the page holding what it opens with.
    const window = await screen.findByRole("dialog", { name: "代码评审" });
    const nestedNav = within(window).getByRole("navigation", { name: "对话设置分类" });
    expect(within(nestedNav).getByRole("button", { name: /^对话模板/ })).toHaveAttribute("aria-current", "true");
    expect(onReadTemplate).toHaveBeenCalledWith("template_review");
    expect(await within(window).findByText("先读一下 README")).toBeInTheDocument();
    expect(screen.queryByRole("dialog", { name: "另存为预设" })).toBeNull();
  });

  it("opens a preset saved without the timeline at its empty template page too", async () => {
    const seed = createSeedDocument();
    const onCreatePreset = vi.fn(async ({ name }: { name: string; captureTemplate: boolean }) => ({
      ...seed.globalSettings.conversationPresets[0],
      id: "preset_plain",
      name,
      description: "",
      templateId: ""
    }));
    const onReadTemplate = vi.fn(readNothing);
    const user = userEvent.setup();
    render(
      <SettingsHarness
        initialConversation={{ ...seed.workspaces[0].conversations[0], contexts: [] }}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
        onCreatePreset={onCreatePreset}
        onReadTemplate={onReadTemplate}
      />
    );

    await user.click(within(navigation()).getByRole("button", { name: "另存为预设" }));
    const dialog = screen.getByRole("dialog", { name: "另存为预设" });
    // An empty timeline has nothing to open with.
    expect(within(dialog).getByRole("checkbox", { name: "将当前上下文作为对话模板" })).toBeDisabled();
    await user.type(within(dialog).getByRole("textbox", { name: "预设名称" }), "空白{Enter}");

    expect(onCreatePreset).toHaveBeenCalledWith({ name: "空白", captureTemplate: false });
    const window = await screen.findByRole("dialog", { name: "空白" });
    const nestedNav = within(window).getByRole("navigation", { name: "对话设置分类" });
    expect(within(nestedNav).getByRole("button", { name: /^对话模板/ })).toHaveAttribute("aria-current", "true");
    expect(onReadTemplate).not.toHaveBeenCalled();
  });

  it("keeps the save-as dialog open with the reason when saving fails", async () => {
    const seed = createSeedDocument();
    const onCreatePreset = vi.fn(async (): Promise<ConversationPreset> => {
      throw new Error("对话模板写入失败");
    });
    const user = userEvent.setup();
    render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
        onCreatePreset={onCreatePreset}
      />
    );

    await user.click(within(navigation()).getByRole("button", { name: "另存为预设" }));
    const dialog = screen.getByRole("dialog", { name: "另存为预设" });
    await user.type(within(dialog).getByRole("textbox", { name: "预设名称" }), "评审");
    await user.click(within(dialog).getByRole("button", { name: "保存为预设" }));

    expect(await within(dialog).findByRole("alert")).toHaveTextContent("对话模板写入失败");
    expect(within(dialog).getByRole("textbox", { name: "预设名称" })).toHaveValue("评审");
  });

  it("applies a preset, names the one a conversation carries, and forgets a deleted one", async () => {
    const seed = createSeedDocument();
    seed.globalSettings.conversationPresets.push({
      ...seed.globalSettings.conversationPresets[0],
      id: "conversation_second",
      name: "第二预设"
    });
    const conversation = seed.workspaces[0].conversations[0];
    const onApplyPreset = vi.fn();
    const user = userEvent.setup();
    const first = render(
      <SettingsHarness
        initialConversation={conversation}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
        onApplyPreset={onApplyPreset}
      />
    );

    await openPage(user, /^对话预设/);
    // Applying stays an action: no "following" banner and nothing to detach from.
    expect(screen.queryByText(/Following:/)).toBeNull();
    expect(screen.queryByRole("button", { name: "停止跟随" })).toBeNull();

    const second = screen.getByRole("button", { name: "打开预设 第二预设" })
      .closest(".catalog-row") as HTMLElement;
    await user.click(within(second).getByRole("button", { name: "套用" }));
    expect(onApplyPreset).toHaveBeenLastCalledWith("conversation_second");
    first.unmount();

    const carried = render(
      <SettingsHarness
        initialConversation={{ ...conversation, presetId: "conversation_second", templateId: "" }}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
      />
    );
    await openPage(user, /^对话预设/);
    expect(screen.getByRole("button", { name: "打开预设 第二预设" })
      .closest(".catalog-row")).toHaveClass("catalog-row--on");
    expect(screen.getByRole("button", { name: "打开预设 默认" })
      .closest(".catalog-row")).not.toHaveClass("catalog-row--on");
    carried.unmount();

    render(
      <SettingsHarness
        initialConversation={{ ...conversation, presetId: "conversation_deleted", templateId: "" }}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
      />
    );
    await openPage(user, /^对话预设/);
    // A preset the conversation still names but that is gone leaves no row marked.
    expect(document.querySelectorAll(".catalog-row--on")).toHaveLength(0);
  });

  it("shows a localized fallback for a blank persisted preset name", async () => {
    configureI18n("en-US");
    const seed = createSeedDocument();
    seed.globalSettings.conversationPresets[0].name = "";
    const user = userEvent.setup();

    render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
      />
    );

    await openPage(user, /^Conversation presets/);
    // The fallback is display-only: the blank name stays blank on disk.
    expect(screen.getByText("Untitled preset")).toBeInTheDocument();
    expect(seed.globalSettings.conversationPresets[0].name).toBe("");
  });

  it("renders English controls and offers no rename or delete for the built-in preset", async () => {
    configureI18n("en-US");
    const seed = createSeedDocument();
    seed.globalSettings.conversationPresets.unshift({
      ...seed.globalSettings.conversationPresets[0],
      id: BUILTIN_PRESET_ID,
      name: "mewrk"
    });
    const user = userEvent.setup();

    render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
      />
    );

    expect(screen.getByRole("link", { name: "Configuration docs" })).toBeInTheDocument();
    expect(screen.getByRole("navigation", { name: "Conversation settings categories" }))
      .toBeInTheDocument();

    await openPage(user, /^Conversation presets/);
    // The built-in ships with the build, so renaming or deleting it is offered
    // but spent; a preset of the user's own keeps both.
    const builtin = screen.getByRole("button", { name: /^Open preset mewrk$/ })
      .closest(".catalog-row") as HTMLElement;
    expect(within(builtin).getByRole("button", { name: "Rename" })).toBeDisabled();
    expect(within(builtin).getByRole("button", { name: "Delete preset mewrk" })).toBeDisabled();
    const own = screen.getByRole("button", { name: /^Open preset 默认$/ })
      .closest(".catalog-row") as HTMLElement;
    expect(within(own).getByRole("button", { name: "Rename" })).toBeEnabled();
  });

  it("saves an edited built-in preset as a new preset, never in place", async () => {
    const seed = createSeedDocument();
    seed.globalSettings.conversationPresets.unshift({
      ...seed.globalSettings.conversationPresets[0],
      id: BUILTIN_PRESET_ID,
      name: "mewrk",
      templateId: "template_preset_mewrk"
    });
    const onSavePreset = vi.fn();
    const onSavePresetCopy = vi.fn();
    const user = userEvent.setup();
    render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
        onSavePreset={onSavePreset}
        onSavePresetCopy={onSavePresetCopy}
      />
    );

    await openPage(user, /^对话预设/);
    await user.click(screen.getByRole("button", { name: "打开预设 mewrk" }));
    const dialog = screen.getByRole("dialog");
    expect(within(dialog).getByText(/内置预设随 Mewrk 版本更新/)).toBeInTheDocument();
    const nestedNav = within(dialog).getByRole("navigation", { name: "对话设置分类" });
    expect(within(nestedNav).queryByRole("button", { name: "保存预设" })).toBeNull();
    await user.click(within(nestedNav).getByRole("button", { name: "另存为新预设" }));
    // Its template was never opened, so the copy takes the built-in's own.
    expect(onSavePresetCopy).toHaveBeenCalledWith(BUILTIN_PRESET_ID, expect.objectContaining({
      enabledTools: seed.globalSettings.conversationPresets[0].settings.enabledTools
    }), null);
    expect(onSavePreset).not.toHaveBeenCalled();
  });

  it("edits the built-in preset's template in its window and carries the edit into the copy", async () => {
    const seed = createSeedDocument();
    seed.globalSettings.conversationPresets.unshift({
      ...seed.globalSettings.conversationPresets[0],
      id: BUILTIN_PRESET_ID,
      name: "mewrk",
      templateId: "template_preset_mewrk"
    });
    const prompt: ContextItem = {
      id: "ctx_builtin_prompt",
      kind: "system",
      content: "You are a software engineer.",
      createdAt: "2026-01-02T03:04:05.000Z"
    };
    const onReadTemplate = vi.fn(async (): Promise<ContextItem[]> => [prompt]);
    const onWriteTemplate = vi.fn(writeSomewhere);
    const onSavePresetCopy = vi.fn();
    const user = userEvent.setup();
    render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
        onReadTemplate={onReadTemplate}
        onWriteTemplate={onWriteTemplate}
        onSavePresetCopy={onSavePresetCopy}
      />
    );

    await openPage(user, /^对话预设/);
    await user.click(screen.getByRole("button", { name: "打开预设 mewrk" }));
    const dialog = screen.getByRole("dialog", { name: "mewrk" });
    const nestedNav = within(dialog).getByRole("navigation", { name: "对话设置分类" });
    await user.click(within(nestedNav).getByRole("button", { name: /^对话模板/ }));
    expect(await within(dialog).findByText("You are a software engineer.")).toBeInTheDocument();

    // The page is as editable as any preset's: saving was never going to write it in place.
    await user.click(within(dialog).getByRole("button", { name: "编辑上下文" }));
    const field = within(dialog).getByRole("textbox", { name: "系统提示词上下文" });
    await user.clear(field);
    await user.type(field, "You review code.");
    await user.click(within(dialog.querySelector(".inline-text-editor") as HTMLElement).getByRole("button", { name: "保存" }));

    // A trip to another page keeps the edit: there is nowhere it was written to read it back from.
    await user.click(within(nestedNav).getByRole("button", { name: TOOLS_PAGE }));
    await user.click(within(nestedNav).getByRole("button", { name: /^对话模板/ }));
    expect((await within(dialog).findAllByText("You review code.")).length).toBeGreaterThan(0);
    expect(within(dialog).queryByText("You are a software engineer.")).toBeNull();
    expect(onWriteTemplate).not.toHaveBeenCalled();

    await user.click(within(nestedNav).getByRole("button", { name: "另存为新预设" }));
    expect(onSavePresetCopy).toHaveBeenCalledWith(BUILTIN_PRESET_ID, expect.anything(), [
      { ...prompt, content: "You review code." }
    ]);
  });

  it("is a pane, not a dialog, and offers no preset-managed or conversation-only zone", () => {
    const seed = createSeedDocument();
    const onSettingsChange = vi.fn();
    render(
      <SettingsHarness
        initialConversation={{ ...seed.workspaces[0].conversations[0] }}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={onSettingsChange}
      />
    );

    // The pane's own chrome is the side pane's title bar, so the component draws
    // no modal of its own and nothing closes it from the inside.
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(screen.queryByRole("button", { name: "关闭" })).toBeNull();
    // Nor does it name itself or its page, as a window does.
    expect(document.querySelector(".dialog__sidebar-title, .settings-page-heading")).toBeNull();
    // A preset is not conversation state, so no preset-managed or conversation-only sections appear.
    expect(screen.queryByText("由预设管理")).toBeNull();
    expect(screen.queryByText("仅此对话")).toBeNull();
    // The system prompt is a timeline card now, not a setting.
    expect(screen.queryByRole("textbox", { name: "系统提示词" })).toBeNull();
    expect(onSettingsChange).not.toHaveBeenCalled();
  });

  it("counts only catalog tools, links the docs, and exposes group expansion state", async () => {
    const seed = createSeedDocument();
    // Memory tools derive from memory-tier switches and are excluded from the
    // count, as are the preview lifecycle tools, which follow the other preview tools.
    const toolNames = Array.from(new Set(
      seed.tools
        .filter((tool) => tool.category !== "memory"
          && !isHostDerivedToolName(tool.name)
          && !isPreviewLifecycleToolName(tool.name))
        .map((tool) => tool.name)
    ));
    const conversation = {
      ...seed.workspaces[0].conversations[0],
      settings: {
        ...seed.workspaces[0].conversations[0].settings,
        enabledTools: [toolNames[0], "missing-tool"]
      }
    };
    const onSettingsChange = vi.fn();
    const user = userEvent.setup();
    render(
      <SettingsHarness
        initialConversation={conversation}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={onSettingsChange}
      />
    );

    // Bulk enable/disable is gone, and so is the heading row over the list: the
    // documentation link ends the line that says what the page is for, and the
    // count is the one the rail trails, as every other page's is.
    expect(screen.queryByRole("button", { name: "全部启用" })).toBeNull();
    expect(screen.queryByRole("button", { name: "全部关闭" })).toBeNull();
    expect(screen.queryByText("启用工具")).toBeNull();
    expect(screen.queryByText(/个已选/)).toBeNull();
    const docs = screen.getByRole("link", { name: "配置说明文档" });
    expect(docs).toHaveAttribute("href", "https://mewrk.dev/zh-CN/working.html");
    expect(docs).toHaveAttribute("target", "_blank");
    expect(docs.parentElement).toHaveClass("conversation-settings__page-header-line");
    expect(docs.previousElementSibling).toHaveTextContent("本对话交给模型的工具。");
    expect(within(navigation()).getByRole("button", { name: TOOLS_PAGE })).toHaveTextContent(/^工具1$/);
    expect(toolNames.length).toBeGreaterThan(1);

    // Toggling a tool must not collapse groups; disclosure state changes only on user action.
    const filesystemGroup = screen.getByRole("button", { name: "文件与搜索" });
    expect(filesystemGroup).toHaveAttribute("aria-expanded", "true");
    await user.click(filesystemGroup);
    expect(filesystemGroup).toHaveAttribute("aria-expanded", "false");
  });

  it("keeps the web, memory and host-message controls on the advanced tools page, and no tool-description picker", async () => {
    const seed = createSeedDocument();
    const onSettingsChange = vi.fn();
    const onConversationOnlyChange = vi.fn();
    const user = userEvent.setup();
    render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={onSettingsChange}
        onConversationOnlyChange={onConversationOnlyChange}
      />
    );

    // The tools page is the list alone.
    expect(screen.queryByRole("combobox", { name: "工具描述" })).toBeNull();
    expect(screen.queryByRole("switch", { name: /全局记忆/ })).toBeNull();
    // Memory tools leave the picker along with the tier switches that derive them.
    // A picker row is a button keyed by `data-tool-name` — its accessible name is
    // "{label}已启用/已关闭" and it carries `aria-pressed`, not the switch role — so
    // a role query would stay green no matter what the picker drew. The positive
    // half is what keeps that true: if picker rows ever stop carrying the
    // attribute, this goes red instead of the guard quietly becoming a no-op.
    expect(document.querySelector('[data-tool-name="read"]')).not.toBeNull();
    expect(document.querySelector('[data-tool-name="read_global_memory"]')).toBeNull();

    await openPage(user, /^高级工具/);
    // And the advanced page is everything the list is not, under the same
    // documentation link.
    expect(document.querySelector('[data-tool-name="read"]')).toBeNull();
    const blurb = screen.getByRole("link", { name: "配置说明文档" }).previousElementSibling;
    expect(blurb).toHaveTextContent(/由开关派生/);
    // The tool-description files have a page of their own now, so neither the
    // page's blurb nor its controls mention them.
    expect(blurb).not.toHaveTextContent(/工具描述/);
    expect(backendTrigger("搜索提供商")).toBeEnabled();
    expect(screen.getByRole("radiogroup", { name: "宿主消息容器" })).toBeInTheDocument();
    // The five write guards are one switch, on when the settings say nothing,
    // and the page's blurb names it.
    expect(blurb).toHaveTextContent(/文件防误写保护/);
    expect(screen.getByRole("switch", { name: "文件防误写保护已开启" })).toBeChecked();
    expect(screen.getByText("启用文件防误写保护")).toBeInTheDocument();
    expect(screen.queryByRole("combobox", { name: "工具描述" })).toBeNull();
    expect(screen.queryByRole("option", { name: "Mewrk 内置" })).toBeNull();
    // Executor limits and the web-search heading stay gone; the identically named tool row remains.
    expect(screen.queryByRole("spinbutton", { name: /搜索次数/ })).toBeNull();
    // Security policy is configured in the composer, and memory tools derive from the tier switches.
    expect(screen.queryByText("安全")).toBeNull();
    // The credential channels are gone, not merely off by default.
    expect(screen.queryByText("允许联网搜索使用我的登录凭证")).toBeNull();
    expect(screen.queryByText("允许 Agent 使用内置浏览器的 Cookie")).toBeNull();

    // Tier switches are preset components and use `onChange`, not conversation-only patches.
    await user.click(screen.getByRole("switch", { name: "全局记忆已关闭" }));
    expect(onSettingsChange).toHaveBeenLastCalledWith(
      expect.objectContaining({ globalMemoryEnabled: true, projectMemoryEnabled: false })
    );
    expect(onConversationOnlyChange).not.toHaveBeenCalled();
    await user.click(screen.getByRole("switch", { name: "项目记忆已关闭" }));
    expect(onSettingsChange).toHaveBeenLastCalledWith(
      expect.objectContaining({ globalMemoryEnabled: true, projectMemoryEnabled: true })
    );
  });

  it("chooses what host messages come in on the advanced tools page, each choice saying what it costs", async () => {
    const seed = createSeedDocument();
    const onSettingsChange = vi.fn();
    const user = userEvent.setup();
    render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={onSettingsChange}
        onConversationOnlyChange={vi.fn()}
      />
    );
    await openPage(user, /^高级工具/);

    const group = screen.getByRole("radiogroup", { name: "宿主消息容器" });
    const userMessage = within(group).getByRole("radio", { name: /^user 消息/ });
    const box = within(group).getByRole("radio", { name: /^box 工具结果/ });
    // Absent is Claude Code's way.
    expect(userMessage).toBeChecked();
    expect(box).not.toBeChecked();
    expect(userMessage.closest("label")).toHaveTextContent(/缺点：与你说的话同在 user 角色里/);
    expect(box.closest("label")).toHaveTextContent(/缺点：这是模型从没发起过的伪造调用/);

    // A preset component, like the switches beside it.
    await user.click(box);
    expect(onSettingsChange).toHaveBeenLastCalledWith(expect.objectContaining({ hostMessageContainer: "box" }));
    expect(box).toBeChecked();
  });

  it("switches the file write guards on the advanced tools page, on until it is told otherwise", async () => {
    const seed = createSeedDocument();
    const onSettingsChange = vi.fn();
    const onConversationOnlyChange = vi.fn();
    const user = userEvent.setup();
    const conversation = seed.workspaces[0].conversations[0];
    // A conversation from before the switch says nothing about the guards.
    expect(conversation.settings.fileWriteGuardsEnabled).toBeUndefined();
    render(
      <SettingsHarness
        initialConversation={conversation}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={onSettingsChange}
        onConversationOnlyChange={onConversationOnlyChange}
      />
    );
    await openPage(user, /^高级工具/);

    const guards = screen.getByRole("switch", { name: "文件防误写保护已开启" });
    expect(guards).toBeChecked();
    const row = guards.closest(".tool-toggle-row") as HTMLElement;
    expect(row).toHaveTextContent("启用文件防误写保护");
    expect(row).toHaveTextContent(/修改或覆盖已有文件前必须先读过它/);
    expect(row).toHaveTextContent(/子代理与工作流跟随本对话/);
    // Its own section, apart from the memory tiers and the host-message container.
    expect(row.closest("section")).not.toBe(
      screen.getByRole("switch", { name: "全局记忆已关闭" }).closest("section")
    );
    expect(row.closest("section")).not.toBe(
      screen.getByRole("radiogroup", { name: "宿主消息容器" }).closest("section")
    );

    // A preset component, so it goes through `onChange` rather than a conversation-only patch.
    await user.click(guards);
    expect(onSettingsChange).toHaveBeenLastCalledWith(expect.objectContaining({ fileWriteGuardsEnabled: false }));
    expect(onConversationOnlyChange).not.toHaveBeenCalled();
    const off = screen.getByRole("switch", { name: "文件防误写保护已关闭" });
    expect(off).not.toBeChecked();
    await user.click(off);
    expect(onSettingsChange).toHaveBeenLastCalledWith(expect.objectContaining({ fileWriteGuardsEnabled: true }));
    expect(screen.getByRole("switch", { name: "文件防误写保护已开启" })).toBeChecked();
  });

  it("reads a conversation that saved the guards off as off", async () => {
    const seed = createSeedDocument();
    const conversation = seed.workspaces[0].conversations[0];
    const user = userEvent.setup();
    render(
      <SettingsHarness
        initialConversation={{ ...conversation, settings: { ...conversation.settings, fileWriteGuardsEnabled: false } }}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
      />
    );
    await openPage(user, /^高级工具/);
    expect(screen.getByRole("switch", { name: "文件防误写保护已关闭" })).not.toBeChecked();
    expect(screen.queryByRole("switch", { name: "文件防误写保护已开启" })).toBeNull();
  });

  it("draws the file write guards orange while the cache is warm, and asks before either way of switching them", async () => {
    const seed = createSeedDocument();
    const warm = withWarmModel(seed.globalSettings);
    const conversation = seed.workspaces[0].conversations[0];
    const onSettingsChange = vi.fn();
    const user = userEvent.setup();
    const { unmount } = render(
      <SettingsHarness
        initialConversation={{
          ...conversation,
          settings: { ...conversation.settings, toolLock: lockFor(warm, { fileWriteGuards: true }) }
        }}
        globalSettings={warm}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={onSettingsChange}
      />
    );
    await openPage(user, /^高级工具/);
    // The `edit` and `write` descriptions carry the guard rules, so the switch
    // rewrites the cached prefix whichever way it goes.
    const guards = screen.getByRole("switch", { name: "文件防误写保护已开启" });
    const row = guards.closest(".tool-toggle-row") as HTMLElement;
    expect(row).toHaveClass("tool-toggle-row--cache");
    expect(row.querySelector(".lock-mark--cache")).not.toBeNull();
    expect(row).toHaveTextContent("缓存还热");

    await user.click(guards);
    const warning = screen.getByRole("dialog", { name: "这样改会让缓存失效" });
    expect(onSettingsChange).not.toHaveBeenCalled();
    await user.click(within(warning).getByRole("button", { name: "仍然更改" }));
    expect(onSettingsChange).toHaveBeenLastCalledWith(expect.objectContaining({ fileWriteGuardsEnabled: false }));
    // Moved away from what the last request had, the cache is already lost for it.
    const moved = screen.getByRole("switch", { name: "文件防误写保护已关闭" });
    expect(moved.closest(".tool-toggle-row")).not.toHaveClass("tool-toggle-row--cache");
    unmount();

    // A lock from before the switch knows nothing of it: plain, and no question.
    resetCacheBreakWarnings();
    const onOlderChange = vi.fn();
    render(
      <SettingsHarness
        initialConversation={{
          ...conversation,
          settings: { ...conversation.settings, toolLock: lockFor(warm) }
        }}
        globalSettings={warm}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={onOlderChange}
      />
    );
    await openPage(user, /^高级工具/);
    const older = screen.getByRole("switch", { name: "文件防误写保护已开启" });
    expect(older.closest(".tool-toggle-row")).not.toHaveClass("tool-toggle-row--cache");
    await user.click(older);
    expect(screen.queryByRole("dialog", { name: "这样改会让缓存失效" })).toBeNull();
    expect(onOlderChange).toHaveBeenLastCalledWith(expect.objectContaining({ fileWriteGuardsEnabled: false }));
  });

  it("draws the host-message container orange while the cache is warm on a model that cannot append tools, and asks before it switches", async () => {
    const seed = createSeedDocument();
    const noAppend = withNonAppendModel(seed.globalSettings);
    const conversation = seed.workspaces[0].conversations[0];
    const onSettingsChange = vi.fn();
    const user = userEvent.setup();
    const settings = (at?: string) => ({
      ...conversation.settings,
      toolLock: {
        ...lockFor(noAppend, {}, at),
        hostMessageContainer: "user" as const
      }
    });
    const { unmount } = render(
      <SettingsHarness
        initialConversation={{ ...conversation, settings: settings() }}
        globalSettings={noAppend}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={onSettingsChange}
      />
    );
    await openPage(user, /^高级工具/);
    // Orange like every other row the warm cache holds, and not refused: the
    // radios stay the user's to move, since `box` comes and goes with them.
    const group = screen.getByRole("radiogroup", { name: "宿主消息容器" });
    const heading = group.previousElementSibling as HTMLElement;
    expect(heading).toHaveClass("tool-toggle-row--cache");
    expect(heading.querySelector(".lock-mark--cache")).not.toBeNull();
    expect(heading).toHaveTextContent("缓存还热");
    for (const choice of within(group).getAllByRole("radio")) expect(choice).toBeEnabled();

    // Switching asks first, and nothing moves until the answer.
    const box = within(group).getByRole("radio", { name: /^box 工具结果/ });
    await user.click(box);
    const warning = screen.getByRole("dialog", { name: "这样改会让缓存失效" });
    expect(onSettingsChange).not.toHaveBeenCalled();
    expect(box).not.toBeChecked();
    await user.click(within(warning).getByRole("button", { name: "仍然更改" }));
    expect(onSettingsChange).toHaveBeenLastCalledWith(expect.objectContaining({ hostMessageContainer: "box" }));
    expect(box).toBeChecked();
    unmount();

    // Long cold, the same model's container is plain and moves without asking.
    resetCacheBreakWarnings();
    const onColdChange = vi.fn();
    render(
      <SettingsHarness
        initialConversation={{
          ...conversation,
          settings: settings(new Date(Date.now() - 3 * 3_600_000).toISOString())
        }}
        globalSettings={noAppend}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={onColdChange}
      />
    );
    await openPage(user, /^高级工具/);
    const coldGroup = screen.getByRole("radiogroup", { name: "宿主消息容器" });
    expect(coldGroup.previousElementSibling).not.toHaveClass("tool-toggle-row--cache");
    expect(coldGroup.previousElementSibling?.querySelector(".lock-mark")).toBeNull();
    await user.click(within(coldGroup).getByRole("radio", { name: /^box 工具结果/ }));
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(onColdChange).toHaveBeenLastCalledWith(expect.objectContaining({ hostMessageContainer: "box" }));
  });

  it("edits search provider behavior as a conversation-only patch", async () => {
    const seed = createSeedDocument();
    const defaults = defaultConversationWebSearchSettings();
    const onSettingsChange = vi.fn();
    const onConversationOnlyChange = vi.fn();
    const user = userEvent.setup();
    render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={{
          ...seed.globalSettings,
          webSearch: {
            ...seed.globalSettings.webSearch,
            providers: seed.globalSettings.webSearch.providers.map((provider) => (
              provider.kind === "tavily" ? { ...provider, enabled: true } : provider
            ))
          }
        }}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={onSettingsChange}
        onConversationOnlyChange={onConversationOnlyChange}
      />
    );
    await openPage(user, /^高级工具/);

    await chooseBackend(user, "搜索提供商", /^Tavily/);
    expect(onConversationOnlyChange).toHaveBeenLastCalledWith({
      webSearch: { ...defaults, provider: { kind: "explicit", providerKind: "tavily" } }
    });
    expect(onSettingsChange).not.toHaveBeenCalled();
  });

  /* The native row's second step: which version of the Messages server-side
     tool this conversation sends. It is a step under the row rather than a
     sibling of it because it refines a choice already made — you cannot pick a
     `web_search_*` spelling without having picked native first. */
  it("opens the native rows into the Messages tool versions on a Messages model", async () => {
    const seed = createSeedDocument();
    const defaults = defaultConversationWebSearchSettings();
    const onConversationOnlyChange = vi.fn();
    const user = userEvent.setup();
    render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={{ ...seed.globalSettings, activeProviderId: "anthropic_messages" }}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
        onConversationOnlyChange={onConversationOnlyChange}
      />
    );
    await openPage(user, /^高级工具/);

    await user.click(backendTrigger("搜索提供商"));
    const searchMenu = screen.getByRole("menu", { name: "搜索提供商" });
    const nativeSearch = within(searchMenu).getByRole("menuitemradio", { name: /^原生/ });
    expect(nativeSearch).toHaveAttribute("aria-haspopup", "menu");
    // Clicking the row only opens the step; the versions are the choices.
    await user.click(nativeSearch);
    expect(onConversationOnlyChange).not.toHaveBeenCalled();
    // The step is a panel of its own beside the menu.
    const versions = screen.getByRole("menu", { name: /^原生/ });
    // The list is the wire `type` verbatim — the request carries no other name
    // for these, so neither should the menu.
    expect(within(versions).getAllByRole("menuitemradio").map((row) => row.textContent))
      .toEqual(["web_search_20250305", "web_search_20260209"]);
    await user.click(within(versions).getByRole("menuitemradio", { name: "web_search_20260209" }));
    expect(onConversationOnlyChange).toHaveBeenLastCalledWith({
      webSearch: {
        ...defaults,
        provider: { kind: "native" },
        nativeSearchTool: "web_search_20260209"
      }
    });

    // The fetch leg has its own versions, because it is its own server tool.
    await user.click(backendTrigger("抓取提供商"));
    const fetchMenu = screen.getByRole("menu", { name: "抓取提供商" });
    await user.click(within(fetchMenu).getByRole("menuitemradio", { name: /^原生/ }));
    const fetchVersions = screen.getByRole("menu", { name: /^原生/ });
    expect(within(fetchVersions).getAllByRole("menuitemradio").map((row) => row.textContent))
      .toEqual(["web_fetch_20250910", "web_fetch_20260209"]);
    await user.click(
      within(fetchVersions).getByRole("menuitemradio", { name: "web_fetch_20260209" })
    );
    expect(onConversationOnlyChange).toHaveBeenLastCalledWith({
      webSearch: {
        ...defaults,
        provider: { kind: "native" },
        nativeSearchTool: "web_search_20260209",
        fetchProvider: { kind: "native" },
        nativeFetchTool: "web_fetch_20260209"
      }
    });
  });

  /* Moving to a model on another protocol is a silent fall back to plain
     native: the row selects immediately, nothing warns, and the versions the
     conversation is carrying are neither shown nor rewritten — so the next
     Messages model it runs on finds them exactly as they were left. */
  it("falls back to plain native off Messages and keeps the chosen versions", async () => {
    const seed = createSeedDocument();
    const onConversationOnlyChange = vi.fn();
    const user = userEvent.setup();
    const conversation = seed.workspaces[0].conversations[0];
    const carried = {
      ...defaultConversationWebSearchSettings(),
      provider: { kind: "explicit" as const, providerKind: "tavily" as const },
      nativeSearchTool: "web_search_20260209" as const,
      nativeFetchTool: "web_fetch_20260209" as const
    };
    const { unmount } = render(
      <SettingsHarness
        initialConversation={{
          ...conversation,
          settings: { ...conversation.settings, webSearch: carried }
        }}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
        onConversationOnlyChange={onConversationOnlyChange}
      />
    );
    await openPage(user, /^高级工具/);

    await user.click(backendTrigger("搜索提供商"));
    const menu = screen.getByRole("menu", { name: "搜索提供商" });
    const native = within(menu).getByRole("menuitemradio", { name: /^原生/ });
    expect(native).not.toHaveAttribute("aria-haspopup");
    await user.click(native);
    // The patch moves the backend and leaves both versions where they were.
    expect(onConversationOnlyChange).toHaveBeenLastCalledWith({
      webSearch: { ...carried, provider: { kind: "native" } }
    });
    // Nor are they claimed on the trigger, which would promise an effect this
    // model's requests do not have.
    expect(backendTrigger("搜索提供商")).not.toHaveTextContent("web_search_20260209");

    unmount();
    render(
      <SettingsHarness
        initialConversation={{
          ...conversation,
          settings: {
            ...conversation.settings,
            webSearch: { ...carried, provider: { kind: "native" } }
          }
        }}
        globalSettings={{ ...seed.globalSettings, activeProviderId: "anthropic_messages" }}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
        onConversationOnlyChange={vi.fn()}
      />
    );
    await openPage(user, /^高级工具/);

    // Back on a Messages model, the version it left with is the one it has.
    expect(backendTrigger("搜索提供商")).toHaveTextContent("web_search_20260209");
  });

  it("offers the model's own provider as a fetch backend and pins both native backends once they have been used", async () => {    const seed = createSeedDocument();
    const defaults = defaultConversationWebSearchSettings();
    const onConversationOnlyChange = vi.fn();
    const user = userEvent.setup();
    const conversation = seed.workspaces[0].conversations[0];
    const { unmount } = render(
      <SettingsHarness
        initialConversation={conversation}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
        onConversationOnlyChange={onConversationOnlyChange}
      />
    );
    await openPage(user, /^高级工具/);

    /* Drawn whatever the search backend is: fetching and searching are separate
       capabilities, so "the model's own provider" is an answer here too — one
       that grants a second web tool on Anthropic and none on OpenAI. */
    await chooseBackend(user, "抓取提供商", /^原生/);
    expect(onConversationOnlyChange).toHaveBeenLastCalledWith({
      webSearch: { ...defaults, fetchProvider: { kind: "native" } }
    });

    unmount();
    render(
      <SettingsHarness
        initialConversation={{
          ...conversation,
          settings: {
            ...conversation.settings,
            toolLock: {
              tools: [],
              mcpIds: [],
              globalMemory: false,
              projectMemory: false,
              skillTool: false,
              mcpToolDiscovery: false,
              webSearch: true,
              planMode: false,
              skillIds: [],
              promptSkillIds: [],
              searchBackend: { kind: "native" },
              fetchBackend: { kind: "native" },
              webFetch: true,
              searchProvider: { kind: "native" },
              fetchProvider: { kind: "native" },
              lastRequest: null,
              modelRequests: [],
              hookIds: null,
              promptProfile: null,
              hostMessageContainer: null,
              fileWriteGuards: null
            }
          }
        }}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
        onConversationOnlyChange={vi.fn()}
      />
    );
    await openPage(user, /^高级工具/);

    // A native backend seals its results into the transcript, so neither
    // selector moves once its tool has gone out — whatever model is selected.
    expect(backendTrigger("搜索提供商")).toBeDisabled();
    expect(backendTrigger("抓取提供商")).toBeDisabled();
    expect(screen.getAllByText(/已经用原生后端跑过了/)).toHaveLength(2);
  });

  it("draws a host-run backend orange while its cache is warm, and warns once before it moves", async () => {
    const seed = createSeedDocument();
    const globalSettings = withWarmModel({
      ...seed.globalSettings,
      webSearch: {
        ...seed.globalSettings.webSearch,
        providers: seed.globalSettings.webSearch.providers.map((provider) => (
          provider.kind === "tavily" || provider.kind === "jina" ? { ...provider, enabled: true } : provider
        ))
      }
    });
    const tavily = { kind: "explicit" as const, providerKind: "tavily" as const };
    const jina = { kind: "explicit" as const, providerKind: "jina" as const };
    const webSearch = { ...defaultConversationWebSearchSettings(), provider: tavily, fetchProvider: jina };
    const onConversationOnlyChange = vi.fn();
    const user = userEvent.setup();
    const conversation = seed.workspaces[0].conversations[0];
    render(
      <SettingsHarness
        initialConversation={{
          ...conversation,
          settings: {
            ...conversation.settings,
            webSearchEnabled: true,
            webSearch,
            toolLock: lockFor(globalSettings, {
              webSearch: true,
              searchBackend: tavily,
              fetchBackend: jina,
              webFetch: true
            })
          }
        }}
        globalSettings={globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
        onConversationOnlyChange={onConversationOnlyChange}
      />
    );
    await openPage(user, /^高级工具/);

    // Orange, not settled: the choice is still the user's.
    expect(backendTrigger("搜索提供商")).toBeEnabled();
    expect(backendTrigger("搜索提供商")).toHaveClass("popover-select__trigger--cache");
    expect(backendTrigger("抓取提供商")).toHaveClass("popover-select__trigger--cache");

    // Moving it asks first, and nothing moves until the answer.
    await chooseBackend(user, "搜索提供商", /^不启用/);
    const warning = screen.getByRole("dialog", { name: "这样改会让缓存失效" });
    expect(onConversationOnlyChange).not.toHaveBeenCalled();
    await user.click(within(warning).getByRole("button", { name: "仍然更改" }));
    expect(onConversationOnlyChange).toHaveBeenLastCalledWith({
      webSearch: { ...webSearch, provider: { kind: "disabled" } }
    });
    // Moved away, it is plain again: the cache is already lost for it.
    expect(backendTrigger("搜索提供商")).not.toHaveClass("popover-select__trigger--cache");
  });

  it("picks one discovered tool-description file rather than editing entries", async () => {
    const user = userEvent.setup();
    const seed = createSeedDocument();
    const onSettingsChange = vi.fn();
    render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={onSettingsChange}
      />
    );
    await openPage(user, TOOL_DESCRIPTIONS_PAGE);

    // Sniffed from disk and selected, exactly like skills and MCP: the app
    // shows what it found and never offers a place to author entries.
    const global = screen.getByRole("region", { name: "全局" });
    expect(within(global).queryByRole("textbox")).toBeNull();
    expect(screen.queryByRole("combobox")).toBeNull();
    // The conversation selects nothing, which the host renders with Mewrk
    // guided. That one is no row of the list — selecting none IS it — so the
    // rows there are the concise built-in and the user's file, neither on.
    expect(screen.queryByRole("button", { name: "Mewrk guided" })).toBeNull();
    const main = within(global).getByRole("button", { name: "main" });
    expect(main).toHaveAttribute("aria-pressed", "false");
    expect(within(global).getByRole("button", { name: "Mewrk concise" })).toHaveAttribute("aria-pressed", "false");
    expect(global.querySelectorAll(".catalog-row")).toHaveLength(2);
    expect(screen.getByText("0 / 2 个已选")).toBeInTheDocument();

    await user.click(main);
    expect(onSettingsChange).toHaveBeenLastCalledWith(expect.objectContaining({
      toolDescriptionFileId: MAIN_PROFILE
    }));
    expect(screen.getByRole("button", { name: "main" })).toHaveAttribute("aria-pressed", "true");
  });

  it("offers Mewrk concise as a selectable built-in row, and leaves Mewrk guided to selecting none", async () => {
    const user = userEvent.setup();
    const seed = createSeedDocument();
    const onSettingsChange = vi.fn();
    render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={onSettingsChange}
      />
    );
    await openPage(user, TOOL_DESCRIPTIONS_PAGE);

    const global = screen.getByRole("region", { name: "全局" });
    // In the host's order, the built-in before the user's files; the guided one is not drawn.
    expect([...global.querySelectorAll(".catalog-row__toggle")].map((row) => row.getAttribute("aria-label")))
      .toEqual(["Mewrk concise", "main"]);
    expect(screen.queryByRole("button", { name: "Mewrk guided" })).toBeNull();
    // It says it is built in, and the user's file does not.
    const concise = screen.getByRole("button", { name: "Mewrk concise" }).closest(".catalog-row") as HTMLElement;
    expect(within(concise).getByText("内置")).toBeInTheDocument();
    expect(concise).toHaveAttribute("title", expect.stringContaining("builtin:en-US/concise"));
    const main = screen.getByRole("button", { name: "main" }).closest(".catalog-row") as HTMLElement;
    expect(within(main).queryByText("内置")).toBeNull();

    await user.click(screen.getByRole("button", { name: "Mewrk concise" }));
    expect(onSettingsChange).toHaveBeenLastCalledWith(expect.objectContaining({
      toolDescriptionFileId: CONCISE_PROFILE
    }));
    expect(screen.getByRole("button", { name: "Mewrk concise" })).toHaveAttribute("aria-pressed", "true");
    expect(concise).toHaveClass("catalog-row--on");
    expect(screen.getByText("1 / 2 个已选")).toBeInTheDocument();
    expect(within(navigation()).getByRole("button", { name: TOOL_DESCRIPTIONS_PAGE })).toHaveTextContent(/^工具描述1$/);

    // Another row replaces it, and unticking it goes back to selecting none: null, not the guided id.
    await user.click(screen.getByRole("button", { name: "main" }));
    expect(onSettingsChange).toHaveBeenLastCalledWith(expect.objectContaining({ toolDescriptionFileId: MAIN_PROFILE }));
    expect(screen.getByRole("button", { name: "Mewrk concise" })).toHaveAttribute("aria-pressed", "false");
    await user.click(screen.getByRole("button", { name: "main" }));
    expect(onSettingsChange).toHaveBeenLastCalledWith(expect.objectContaining({ toolDescriptionFileId: null }));
    expect(screen.getByText("0 / 2 个已选")).toBeInTheDocument();
  });

  it("reads a conversation naming Mewrk concise as having picked its row", async () => {
    const user = userEvent.setup();
    const seed = createSeedDocument();
    const conversation = seed.workspaces[0].conversations[0];
    render(
      <SettingsHarness
        initialConversation={{ ...conversation, settings: { ...conversation.settings, toolDescriptionFileId: CONCISE_PROFILE } }}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
      />
    );
    expect(within(navigation()).getByRole("button", { name: TOOL_DESCRIPTIONS_PAGE })).toHaveTextContent(/^工具描述1$/);
    await openPage(user, TOOL_DESCRIPTIONS_PAGE);
    const concise = screen.getByRole("button", { name: "Mewrk concise" });
    expect(concise).toHaveAttribute("aria-pressed", "true");
    expect(concise.closest(".catalog-row")).toHaveClass("catalog-row--on");
    // A listed built-in is no dangling id.
    expect(screen.queryByText("悬空")).toBeNull();
    expect(screen.getByText("1 / 2 个已选")).toBeInTheDocument();
  });

  it("puts the tool-descriptions page right after the hooks and counts the one file a conversation uses", async () => {
    const seed = createSeedDocument();
    const conversation = seed.workspaces[0].conversations[0];
    const user = userEvent.setup();
    const mount = (toolDescriptionFileId: string | null) => render(
      <SettingsHarness
        initialConversation={{ ...conversation, settings: { ...conversation.settings, toolDescriptionFileId } }}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
      />
    );

    const first = mount(null);
    const nav = navigation();
    const hooks = within(nav).getByRole("button", { name: /^钩子/ });
    const descriptions = within(nav).getByRole("button", { name: TOOL_DESCRIPTIONS_PAGE });
    // Between the hooks and the roles, not at either end of the catalogs.
    expect(hooks.nextElementSibling).toBe(descriptions);
    expect(descriptions.nextElementSibling).toBe(within(nav).getByRole("button", { name: /^代理角色/ }));
    // Selecting nothing is the built-in; the nav says so with a zero.
    expect(descriptions).toHaveTextContent(/^工具描述0$/);
    expect(descriptions.querySelectorAll("svg")).toHaveLength(1);
    expect(descriptions.querySelector("svg")).toHaveClass("lucide-scroll-text");
    expect(descriptions).not.toHaveAttribute("aria-current");
    await user.click(descriptions);
    expect(descriptions).toHaveAttribute("aria-current", "true");
    expect(within(nav).getByRole("button", { name: TOOLS_PAGE })).not.toHaveAttribute("aria-current");
    first.unmount();

    // A file picked counts one; Mewrk guided's own id is no pick at all.
    const picked = mount(MAIN_PROFILE);
    expect(within(navigation()).getByRole("button", { name: TOOL_DESCRIPTIONS_PAGE })).toHaveTextContent(/^工具描述1$/);
    picked.unmount();
    mount(BUILTIN_PROFILE);
    expect(within(navigation()).getByRole("button", { name: TOOL_DESCRIPTIONS_PAGE })).toHaveTextContent(/^工具描述0$/);
  });

  it("draws only the global section on the tool-descriptions page, whatever workspaces the conversation has", async () => {
    const seed = createSeedDocument();
    // The files are read from ~/.mewrk alone; one that claims a workspace is
    // not a level this page has a section for.
    seed.capabilities.toolDescriptionFiles.push({
      id: "tooldesc_workspace_stray",
      name: "stray",
      description: "放在项目里的文件",
      location: "/work/a/.mewrk/tool-descriptions/stray.json",
      source: "workspace",
      available: true,
      workspaceKey: WORKSPACE_A[0].key
    });
    const onRevealCapabilityLocation = vi.fn();
    const user = userEvent.setup();
    render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
        workspaces={[
          ...WORKSPACE_A,
          { number: 2, key: "ssh:m1|/srv/b", machine: { kind: "ssh", machineId: "m1" }, path: "/srv/b" }
        ]}
        onRevealCapabilityLocation={onRevealCapabilityLocation}
      />
    );

    // The skills page of the same conversation does draw a section per workspace.
    await openPage(user, /^技能/);
    expect(screen.getByRole("region", { name: "/work/a" })).toBeInTheDocument();

    await openPage(user, TOOL_DESCRIPTIONS_PAGE);
    expect(screen.getAllByRole("region").map((section) => section.getAttribute("aria-label"))).toEqual(["全局"]);
    expect(screen.queryByRole("region", { name: "/work/a" })).toBeNull();
    expect(screen.queryByRole("region", { name: /^\/srv\/b/ })).toBeNull();
    expect(screen.queryByRole("button", { name: /的配置目录$/ })).toBeNull();
    expect(screen.getByRole("button", { name: "打开全局配置目录" })).toBeInTheDocument();
    const global = screen.getByRole("region", { name: "全局" });
    expect(within(global).getByText("~/.mewrk")).toBeInTheDocument();
    // The user's file and the concise built-in are drawn; Mewrk guided and the
    // workspace's stray are not.
    expect(within(global).getByRole("button", { name: "main" })).toBeInTheDocument();
    expect(within(global).getByRole("button", { name: "Mewrk concise" })).toBeInTheDocument();
    expect(screen.queryByText("Mewrk guided")).toBeNull();
    expect(screen.queryByRole("button", { name: "Mewrk guided" })).toBeNull();
    expect(screen.queryByRole("button", { name: "stray" })).toBeNull();
    expect(global.querySelectorAll(".catalog-row")).toHaveLength(2);
    // The counter divides what is drawn, so it too leaves both out.
    expect(screen.getByText("0 / 2 个已选")).toBeInTheDocument();
  });

  it("picks one tool-description file at a time: another row replaces it, and unticking goes back to Mewrk guided", async () => {
    const seed = createSeedDocument();
    withAltToolDescription(seed.capabilities);
    const conversation = seed.workspaces[0].conversations[0];
    const onSettingsChange = vi.fn();
    const user = userEvent.setup();
    const { unmount } = render(
      <SettingsHarness
        initialConversation={{ ...conversation, settings: { ...conversation.settings, toolDescriptionFileId: MAIN_PROFILE } }}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={onSettingsChange}
      />
    );
    const count = () => within(navigation()).getByRole("button", { name: TOOL_DESCRIPTIONS_PAGE });
    const row = (name: string) => screen.getByRole("button", { name }).closest(".catalog-row") as HTMLElement;
    expect(count()).toHaveTextContent(/^工具描述1$/);

    await openPage(user, TOOL_DESCRIPTIONS_PAGE);
    expect(screen.getByRole("button", { name: "main" })).toHaveAttribute("aria-pressed", "true");
    expect(row("main")).toHaveClass("catalog-row--pick", "catalog-row--on");
    expect(screen.getByRole("button", { name: "alt" })).toHaveAttribute("aria-pressed", "false");
    expect(row("alt")).not.toHaveClass("catalog-row--on");
    expect(screen.getByText("1 / 3 个已选")).toBeInTheDocument();
    // Plain rows: the conversation has no lock, so nothing here is orange.
    expect(document.querySelectorAll(".catalog-row--cache")).toHaveLength(0);

    // Another row replaces the pick rather than joining it.
    await user.click(screen.getByRole("button", { name: "alt" }));
    expect(onSettingsChange).toHaveBeenLastCalledWith(expect.objectContaining({ toolDescriptionFileId: ALT_PROFILE }));
    expect(screen.getByRole("button", { name: "alt" })).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByRole("button", { name: "main" })).toHaveAttribute("aria-pressed", "false");
    expect(row("alt")).toHaveClass("catalog-row--on");
    expect(row("main")).not.toHaveClass("catalog-row--on");
    expect(screen.getByText("1 / 3 个已选")).toBeInTheDocument();
    expect(count()).toHaveTextContent(/^工具描述1$/);

    // Unticking the one picked selects nothing, which is Mewrk guided: null, not "".
    await user.click(screen.getByRole("button", { name: "alt" }));
    expect(onSettingsChange).toHaveBeenLastCalledWith(expect.objectContaining({ toolDescriptionFileId: null }));
    expect(screen.getByRole("button", { name: "alt" })).toHaveAttribute("aria-pressed", "false");
    expect(screen.getByRole("button", { name: "main" })).toHaveAttribute("aria-pressed", "false");
    expect(screen.getByText("0 / 3 个已选")).toBeInTheDocument();
    expect(count()).toHaveTextContent(/^工具描述0$/);
    expect(onSettingsChange).toHaveBeenCalledTimes(2);
    unmount();

    // A conversation that names Mewrk guided reads as selecting nothing: no row
    // is on, no dangling row appears for an id the list deliberately leaves out,
    // and the first file clicked is a plain pick.
    resetCacheBreakWarnings();
    const onBuiltinChange = vi.fn();
    render(
      <SettingsHarness
        initialConversation={{ ...conversation, settings: { ...conversation.settings, toolDescriptionFileId: BUILTIN_PROFILE } }}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={onBuiltinChange}
      />
    );
    expect(count()).toHaveTextContent(/^工具描述0$/);
    await openPage(user, TOOL_DESCRIPTIONS_PAGE);
    expect(document.querySelectorAll(".catalog-row--on")).toHaveLength(0);
    expect(screen.queryByText("悬空")).toBeNull();
    expect(screen.queryByRole("button", { name: BUILTIN_PROFILE })).toBeNull();
    expect(screen.getByText("0 / 3 个已选")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "main" }));
    expect(onBuiltinChange).toHaveBeenLastCalledWith(expect.objectContaining({ toolDescriptionFileId: MAIN_PROFILE }));
  });

  it("draws every tool-description row orange while the warm cache was worded with the current file, and asks before changing it", async () => {
    const seed = createSeedDocument();
    seed.globalSettings = withWarmModel(seed.globalSettings);
    withAltToolDescription(seed.capabilities);
    const conversation = seed.workspaces[0].conversations[0];
    const settings = (toolDescriptionFileId: string | null, lock: Partial<ConversationToolLock>, at?: string) => ({
      ...conversation,
      settings: {
        ...conversation.settings,
        toolDescriptionFileId,
        toolLock: lockFor(seed.globalSettings, lock, at)
      }
    });
    const user = userEvent.setup();
    const onSettingsChange = vi.fn();
    const { unmount } = render(
      <SettingsHarness
        initialConversation={settings(MAIN_PROFILE, { promptProfile: MAIN_PROFILE })}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={onSettingsChange}
      />
    );
    await openPage(user, TOOL_DESCRIPTIONS_PAGE);

    // Whichever row is clicked, the words the cached prefix was written in
    // change, so the picked row and the ones beside it, the concise built-in
    // included, are orange alike.
    for (const name of ["main", "alt", "Mewrk concise"]) {
      const box = screen.getByRole("button", { name }).closest(".catalog-row") as HTMLElement;
      expect(box).toHaveClass("catalog-row--cache");
      expect(box.querySelector(".catalog-row__sign.lock-mark--cache")).not.toBeNull();
      expect(box).toHaveAttribute("title", expect.stringContaining("缓存还热"));
    }
    expect(screen.getByRole("button", { name: "main" })).toBeEnabled();

    // Replacing the file asks first, and nothing moves until the answer.
    await user.click(screen.getByRole("button", { name: "alt" }));
    const warning = screen.getByRole("dialog", { name: CACHE_BREAK_TITLE });
    expect(onSettingsChange).not.toHaveBeenCalled();
    expect(screen.getByRole("button", { name: "main" })).toHaveAttribute("aria-pressed", "true");
    await user.click(within(warning).getByRole("button", { name: "取消" }));
    expect(onSettingsChange).not.toHaveBeenCalled();
    expect(screen.queryByRole("dialog", { name: CACHE_BREAK_TITLE })).toBeNull();
    expect(screen.getByRole("button", { name: "main" })).toHaveAttribute("aria-pressed", "true");

    await user.click(screen.getByRole("button", { name: "alt" }));
    await user.click(within(screen.getByRole("dialog", { name: CACHE_BREAK_TITLE }))
      .getByRole("button", { name: "仍然更改" }));
    expect(onSettingsChange).toHaveBeenCalledTimes(1);
    expect(onSettingsChange).toHaveBeenLastCalledWith(expect.objectContaining({ toolDescriptionFileId: ALT_PROFILE }));
    expect(screen.getByRole("button", { name: "alt" })).toHaveAttribute("aria-pressed", "true");
    // Moved off the profile the cache was written in, it is plain: the cache is already lost for it.
    expect(document.querySelectorAll(".catalog-row--cache")).toHaveLength(0);
    unmount();

    // Going back to Mewrk guided by unticking asks as well: it moves the same profile.
    resetCacheBreakWarnings();
    const onUntick = vi.fn();
    const untick = render(
      <SettingsHarness
        initialConversation={settings(MAIN_PROFILE, { promptProfile: MAIN_PROFILE })}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={onUntick}
      />
    );
    await openPage(user, TOOL_DESCRIPTIONS_PAGE);
    await user.click(screen.getByRole("button", { name: "main" }));
    expect(onUntick).not.toHaveBeenCalled();
    await user.click(within(screen.getByRole("dialog", { name: CACHE_BREAK_TITLE }))
      .getByRole("button", { name: "仍然更改" }));
    expect(onUntick).toHaveBeenLastCalledWith(expect.objectContaining({ toolDescriptionFileId: null }));
    untick.unmount();

    // Nothing is orange, and nothing asks, when the cache is long cold, when the
    // selection already moved off the profile the lock recorded, or when the
    // lock predates profiles being recorded.
    const plain: Array<[string, ReturnType<typeof settings>]> = [
      ["cold", settings(MAIN_PROFILE, { promptProfile: MAIN_PROFILE }, new Date(Date.now() - 3 * 3_600_000).toISOString())],
      ["moved off", settings(MAIN_PROFILE, { promptProfile: BUILTIN_PROFILE })],
      ["unrecorded", settings(MAIN_PROFILE, {})]
    ];
    for (const [, initialConversation] of plain) {
      resetCacheBreakWarnings();
      const onPlainChange = vi.fn();
      const view = render(
        <SettingsHarness
          initialConversation={initialConversation}
          globalSettings={seed.globalSettings}
          tools={seed.tools}
          capabilities={seed.capabilities}
          onSettingsChange={onPlainChange}
        />
      );
      await openPage(user, TOOL_DESCRIPTIONS_PAGE);
      expect(document.querySelectorAll(".catalog-row--cache")).toHaveLength(0);
      expect(document.querySelector(".lock-mark")).toBeNull();
      await user.click(screen.getByRole("button", { name: "alt" }));
      expect(screen.queryByRole("dialog")).toBeNull();
      expect(onPlainChange).toHaveBeenLastCalledWith(expect.objectContaining({ toolDescriptionFileId: ALT_PROFILE }));
      view.unmount();
    }
  });

  it("keeps a tool-description id the catalog lost as a dangling row that unticks to the built-in without asking", async () => {
    const seed = createSeedDocument();
    seed.globalSettings = withWarmModel(seed.globalSettings);
    const conversation = seed.workspaces[0].conversations[0];
    const onSettingsChange = vi.fn();
    const user = userEvent.setup();
    render(
      <SettingsHarness
        initialConversation={{
          ...conversation,
          settings: {
            ...conversation.settings,
            toolDescriptionFileId: GONE_PROFILE,
            // Warm, and worded with the very id that is gone: were the untick a
            // change like the others it would be orange and would ask.
            toolLock: lockFor(seed.globalSettings, { promptProfile: GONE_PROFILE })
          }
        }}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={onSettingsChange}
      />
    );
    expect(within(navigation()).getByRole("button", { name: TOOL_DESCRIPTIONS_PAGE })).toHaveTextContent(/^工具描述1$/);

    await openPage(user, TOOL_DESCRIPTIONS_PAGE);
    const dangling = screen.getByRole("button", { name: GONE_PROFILE });
    const box = dangling.closest(".catalog-row") as HTMLElement;
    expect(dangling).toHaveAttribute("aria-pressed", "true");
    expect(box).toHaveClass("catalog-row--on");
    expect(within(box).getByText("悬空")).toBeInTheDocument();
    // The host words the run with the built-in instead of failing it, and says so.
    expect(box).toHaveAttribute("title", expect.stringContaining("目录中已不存在；运行时用的是 Mewrk guided（内置引导版）"));
    expect(box).not.toHaveTextContent("每次运行都会失败");
    expect(box).not.toHaveClass("catalog-row--cache");
    expect(box.querySelector(".lock-mark")).toBeNull();
    // The counter is over the catalog, which the dangling row is no part of.
    expect(screen.getByText("0 / 2 个已选")).toBeInTheDocument();

    await user.click(dangling);
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(onSettingsChange).toHaveBeenCalledTimes(1);
    expect(onSettingsChange).toHaveBeenLastCalledWith(expect.objectContaining({ toolDescriptionFileId: null }));
    expect(screen.queryByRole("button", { name: GONE_PROFILE })).toBeNull();
    expect(screen.queryByText("悬空")).toBeNull();
    expect(within(navigation()).getByRole("button", { name: TOOL_DESCRIPTIONS_PAGE })).toHaveTextContent(/^工具描述0$/);
  });

  it("says a selected tool-description file with nothing usable in it falls back to the built-in", async () => {
    const seed = createSeedDocument();
    seed.capabilities.toolDescriptionFiles.push({
      id: BROKEN_PROFILE,
      name: "broken",
      description: "0 个工具描述 · 0 条提示词覆盖",
      location: "test://tool-descriptions/broken.json",
      source: "user",
      available: false
    });
    const user = userEvent.setup();
    render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
      />
    );
    await openPage(user, TOOL_DESCRIPTIONS_PAGE);

    const box = () => screen.getByRole("button", { name: "broken" }).closest(".catalog-row") as HTMLElement;
    const fallback = "已选择，但文件里没有可用条目；运行时用的是 Mewrk guided（内置引导版）";
    // Not picked, it says only that it is unavailable.
    expect(within(box()).getByText("不可用")).toBeInTheDocument();
    expect(box()).toHaveAttribute("title", expect.stringContaining("当前不可用"));
    expect(box()).toHaveAttribute("title", expect.not.stringContaining("已选择"));

    // Picked, it says what the run does about it — not that every run fails.
    await user.click(screen.getByRole("button", { name: "broken" }));
    expect(box()).toHaveClass("catalog-row--on");
    expect(box()).toHaveAttribute("title", expect.stringContaining(fallback));
    expect(box()).toHaveAttribute("title", expect.not.stringContaining("每次运行都会失败"));
  });

  it("opens ~/.mewrk, rescans, searches and states its one-at-a-time rule from the tool-descriptions page", async () => {
    const seed = createSeedDocument();
    withAltToolDescription(seed.capabilities);
    const onRevealCapabilityLocation = vi.fn();
    const onRescanCapabilities = vi.fn();
    const user = userEvent.setup();
    render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
        workspaces={WORKSPACE_A}
        onRevealCapabilityLocation={onRevealCapabilityLocation}
        onRescanCapabilities={onRescanCapabilities}
      />
    );
    await openPage(user, TOOL_DESCRIPTIONS_PAGE);

    // The one folder this page has is the global one, and the kind travels with the click.
    await user.click(screen.getByRole("button", { name: "打开全局配置目录" }));
    expect(onRevealCapabilityLocation).toHaveBeenCalledTimes(1);
    expect(onRevealCapabilityLocation).toHaveBeenLastCalledWith("toolDescriptions", null);

    // Opening the pane rescanned once; the toolbar button reads the disk again.
    expect(onRescanCapabilities).toHaveBeenCalledTimes(1);
    await user.click(screen.getByRole("button", { name: "重新扫描" }));
    expect(onRescanCapabilities).toHaveBeenCalledTimes(2);

    expect(screen.getByRole("link", { name: "配置说明文档" }))
      .toHaveAttribute("href", "https://mewrk.dev/zh-CN/prompt-profiles.html");
    expect(screen.getByText("一次只用一份：选另一份会替换当前这份。都不选时，用 Mewrk guided（内置引导版）。"))
      .toBeInTheDocument();

    const search = screen.getByRole("textbox", { name: "搜索工具描述" });
    await user.type(search, "alt");
    expect(screen.queryByRole("button", { name: "main" })).toBeNull();
    expect(screen.getByRole("button", { name: "alt" })).toBeInTheDocument();
    await user.clear(search);
    await user.type(search, "没有这个文件");
    expect(screen.getByText("没有匹配的条目")).toBeInTheDocument();
  });

  it("says no tool-description file was found when only Mewrk guided exists, and still states the rule", async () => {
    const seed = createSeedDocument();
    seed.capabilities.toolDescriptionFiles = seed.capabilities.toolDescriptionFiles
      .filter((resource) => resource.id === BUILTIN_PROFILE);
    const user = userEvent.setup();
    render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
      />
    );
    await openPage(user, TOOL_DESCRIPTIONS_PAGE);

    // Mewrk guided is not a row, so a catalog of nothing but it is an empty one.
    expect(screen.getByText("尚未发现工具描述文件")).toBeInTheDocument();
    expect(screen.getByText("把 JSON 文件放在 ~/.mewrk/tool-descriptions/ 下，列表很快会自动刷新。"))
      .toBeInTheDocument();
    expect(screen.getByText("0 / 0 个已选")).toBeInTheDocument();
    expect(screen.queryByText("Mewrk guided")).toBeNull();
    expect(document.querySelectorAll(".catalog-row")).toHaveLength(0);
    // The rule is drawn with or without rows: how nothing selected behaves is
    // worth knowing before the first file is written.
    expect(screen.getByText(/都不选时，用 Mewrk guided（内置引导版）/)).toBeInTheDocument();
  });

  it("words the tool-descriptions page in English", async () => {
    configureI18n("en-US");
    const seed = createSeedDocument();
    const user = userEvent.setup();
    render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
      />
    );

    await openPage(user, /^Tool descriptions/);
    expect(screen.getByRole("textbox", { name: "Search tool descriptions" })).toBeInTheDocument();
    expect(screen.getByRole("region", { name: "Global" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "main" })).toBeInTheDocument();
    expect(screen.getByText(
      "One at a time: picking another replaces the current one. With none picked, Mewrk guided (built-in) is used."
    )).toBeInTheDocument();
  });

  it("never shows host-derived tools or a skill tool in the tool picker", () => {
    const seed = createSeedDocument();
    render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
      />
    );

    // `skill` is cataloged for timeline rendering but derives from its switch, so
    // the picker must not draw a row for it. A row is a button keyed by
    // `data-tool-name` whose accessible name is "{label}已启用/已关闭" — neither a
    // switch nor the bare name — so only the attribute can catch a regression.
    expect(seed.tools.some((tool) => tool.name === "skill")).toBe(true);
    expect(document.querySelector('[data-tool-name="read"]')).not.toBeNull();
    expect(document.querySelector('[data-tool-name="skill"]')).toBeNull();
    expect(screen.queryByRole("button", { name: "长期记忆" })).toBeNull();
  });

  it("selects skills, MCP servers, and hooks on their own pages, one catalog per page", async () => {
    const seed = createSeedDocument();
    seed.capabilities.hooks.push({
      id: "hook_lint",
      name: "Lint 钩子",
      description: "测试用钩子。",
      location: "test://hooks/lint.json#/hooks/PreToolUse/0/hooks/0",
      source: "user",
      available: true
    });
    const onSettingsChange = vi.fn();
    const user = userEvent.setup();
    render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={onSettingsChange}
      />
    );

    await openPage(user, /^技能/);
    // A row is picked like a tool row: its toggle button carries the resource's
    // name and `aria-pressed`, and the whole row fills in green while it is on.
    const codeReview = screen.getByRole("button", { name: "代码审查" });
    expect(codeReview).toHaveAttribute("aria-pressed", "true");
    expect(codeReview.closest(".catalog-row")).toHaveClass("catalog-row--pick", "catalog-row--on");
    // A row names where the resource came from, which is the only way to tell
    // two same-named resources apart — now in the row's tooltip rather than a
    // second line.
    expect(codeReview.closest(".catalog-row")).toHaveAttribute(
      "title", expect.stringContaining("test://skills/code-review/SKILL.md")
    );
    await user.click(codeReview);
    expect(onSettingsChange).toHaveBeenLastCalledWith(expect.objectContaining({
      skillIds: [],
      mcpIds: ["mcp_workspace"],
      hookIds: []
    }));
    // Off again, the row is plain and its toggle says so.
    expect(screen.getByRole("button", { name: "代码审查" })).toHaveAttribute("aria-pressed", "false");
    expect(screen.getByRole("button", { name: "代码审查" }).closest(".catalog-row"))
      .not.toHaveClass("catalog-row--on");

    await openPage(user, /^MCP/);
    expect(screen.getByRole("button", { name: "Workspace Files" })).toHaveAttribute("aria-pressed", "true");
    await user.click(screen.getByRole("button", { name: "Workspace Files" }));
    expect(onSettingsChange).toHaveBeenLastCalledWith(expect.objectContaining({ mcpIds: [] }));

    await openPage(user, /^钩子/);
    // A hook addresses one handler inside a file; the file is what the row's tooltip shows.
    const lintHook = screen.getByRole("button", { name: "Lint 钩子" });
    expect(lintHook).toHaveAttribute("aria-pressed", "false");
    expect(lintHook.closest(".catalog-row"))
      .toHaveAttribute("title", expect.stringContaining("test://hooks/lint.json"));
    await user.click(lintHook);
    expect(onSettingsChange).toHaveBeenLastCalledWith(expect.objectContaining({
      hookIds: ["hook_lint"]
    }));

    // Scanning is automatic: no list offers a manual trigger, and the old
    // "no hook preset" group layer is gone.
    expect(screen.queryByRole("button", { name: "重新扫描" })).toBeNull();
    expect(screen.queryByText("不使用钩子预设")).toBeNull();
  });

  it("searches a catalog by name, description and path, and toggles what the search shows", async () => {
    const seed = createSeedDocument();
    seed.capabilities.skills.push({
      id: "skill_release",
      name: "发布流程",
      description: "测试用技能。",
      location: "test://skills/release/SKILL.md",
      source: "workspace",
      available: true
    });
    const onSettingsChange = vi.fn();
    const user = userEvent.setup();
    render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={onSettingsChange}
      />
    );

    await openPage(user, /^技能/);
    expect(screen.getByText("1 / 2 个已选")).toBeInTheDocument();

    await user.type(screen.getByRole("textbox", { name: "搜索技能" }), "release");
    expect(screen.queryByRole("button", { name: "代码审查" })).toBeNull();
    // A row the search hides keeps its selection; only the visible one is edited.
    await user.click(screen.getByRole("button", { name: "发布流程" }));
    expect(onSettingsChange).toHaveBeenLastCalledWith(expect.objectContaining({
      skillIds: ["skill_code_review", "skill_release"]
    }));

    await user.clear(screen.getByRole("textbox", { name: "搜索技能" }));
    await user.type(screen.getByRole("textbox", { name: "搜索技能" }), "没有这个");
    expect(screen.getByText("没有匹配的条目")).toBeInTheDocument();
  });

  it("keeps a selected capability that has vanished from the catalog instead of dropping it", async () => {
    const seed = createSeedDocument();
    const conversation = {
      ...seed.workspaces[0].conversations[0],
      settings: {
        ...seed.workspaces[0].conversations[0].settings,
        skillIds: ["skill_code_review", "skill_uninstalled"]
      }
    };
    const onSettingsChange = vi.fn();
    const user = userEvent.setup();
    render(
      <SettingsHarness
        initialConversation={conversation}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={onSettingsChange}
      />
    );

    // A resource may just be temporarily unscanned, so the selection survives
    // as a checked dangling row the user can clear on purpose.
    await openPage(user, /^技能/);
    const dangling = screen.getByRole("button", { name: "skill_uninstalled" });
    expect(dangling).toHaveAttribute("aria-pressed", "true");
    expect(dangling.closest(".catalog-row")).toHaveClass("catalog-row--on");
    expect(screen.getByText("悬空")).toBeInTheDocument();
    // The counter is over the catalog, which the dangling row is no longer part of:
    // counting it would read "2 / 1".
    expect(screen.getByText("1 / 1 个已选")).toBeInTheDocument();
    await user.click(dangling);
    expect(onSettingsChange).toHaveBeenLastCalledWith(expect.objectContaining({
      skillIds: ["skill_code_review"]
    }));
  });

  it("draws a skill the warm cache holds orange, and warns once before it goes", async () => {
    const seed = createSeedDocument();
    seed.globalSettings = withWarmModel(seed.globalSettings);
    const conversation = seed.workspaces[0].conversations[0];
    const onSettingsChange = vi.fn();
    const user = userEvent.setup();
    render(
      <SettingsHarness
        initialConversation={{
          ...conversation,
          settings: {
            ...conversation.settings,
            skillIds: ["skill_code_review"],
            toolLock: lockFor(seed.globalSettings, {
              skillIds: ["skill_code_review"],
              promptSkillIds: ["skill_code_review"]
            })
          }
        }}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={onSettingsChange}
      />
    );

    await openPage(user, /^技能/);
    // Orange, not spent: the user may still take it off. The whole row fills
    // orange, and its trailing sign is the lock rather than a minus.
    const row = screen.getByRole("button", { name: "代码审查" });
    const box = row.closest(".catalog-row") as HTMLElement;
    expect(row).toHaveAttribute("aria-pressed", "true");
    expect(row).toBeEnabled();
    expect(box).toHaveClass("catalog-row--on", "catalog-row--cache");
    expect(box.querySelector(".catalog-row__sign.lock-mark--cache")).not.toBeNull();
    expect(box).toHaveAttribute("title", expect.stringContaining("缓存还热"));
    // How skills arrive rewrites the prompt they went out in, either way.
    expect(screen.getByRole("switch", { name: "拼进提示词" })).toHaveClass("switch--cache");

    // Taking it off asks first, and nothing moves until the answer.
    await user.click(row);
    const warning = screen.getByRole("dialog", { name: "这样改会让缓存失效" });
    expect(onSettingsChange).not.toHaveBeenCalled();
    await user.click(within(warning).getByRole("button", { name: "取消" }));
    expect(onSettingsChange).not.toHaveBeenCalled();
    expect(screen.getByRole("button", { name: "代码审查" })).toHaveAttribute("aria-pressed", "true");

    await user.click(screen.getByRole("button", { name: "代码审查" }));
    await user.click(within(screen.getByRole("dialog", { name: "这样改会让缓存失效" }))
      .getByRole("button", { name: "仍然更改" }));
    expect(onSettingsChange).toHaveBeenLastCalledWith(expect.objectContaining({ skillIds: [] }));
    // Moved away, it is plain again: the cache is already lost for it.
    const moved = screen.getByRole("button", { name: "代码审查" });
    expect(moved).toHaveAttribute("aria-pressed", "false");
    expect(moved.closest(".catalog-row")).not.toHaveClass("catalog-row--cache");
    expect(moved.closest(".catalog-row")?.querySelector(".lock-mark")).toBeNull();

    // Once per conversation: the next orange change goes straight through.
    await user.click(screen.getByRole("switch", { name: "拼进提示词" }));
    expect(screen.queryByRole("dialog", { name: "这样改会让缓存失效" })).toBeNull();
    expect(onSettingsChange).toHaveBeenLastCalledWith(expect.objectContaining({ skillToolEnabled: true }));
  });

  it("stops warning anywhere once told not to show it again", async () => {
    const seed = createSeedDocument();
    seed.globalSettings = withWarmModel(seed.globalSettings);
    const conversation = seed.workspaces[0].conversations[0];
    const user = userEvent.setup();
    const settingsWithLock = {
      ...conversation.settings,
      mcpIds: ["mcp_workspace"],
      toolLock: lockFor(seed.globalSettings, { mcpIds: ["mcp_workspace"] })
    };
    const { unmount } = render(
      <SettingsHarness
        initialConversation={{ ...conversation, settings: settingsWithLock }}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
      />
    );
    await openPage(user, /^MCP/);
    await user.click(screen.getByRole("button", { name: "Workspace Files" }));
    const warning = screen.getByRole("dialog", { name: "这样改会让缓存失效" });
    await user.click(within(warning).getByRole("checkbox", { name: "不再显示" }));
    await user.click(within(warning).getByRole("button", { name: "仍然更改" }));
    unmount();

    // Another conversation, another orange row: no warning.
    const onSettingsChange = vi.fn();
    render(
      <SettingsHarness
        initialConversation={{ ...conversation, id: "conversation_other", settings: settingsWithLock }}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={onSettingsChange}
      />
    );
    await openPage(user, /^MCP/);
    await user.click(screen.getByRole("button", { name: "Workspace Files" }));
    expect(screen.queryByRole("dialog", { name: "这样改会让缓存失效" })).toBeNull();
    expect(onSettingsChange).toHaveBeenLastCalledWith(expect.objectContaining({ mcpIds: [] }));
  });

  it("draws an MCP selection the warm cache holds orange rather than spent", async () => {
    const seed = createSeedDocument();
    seed.globalSettings = withWarmModel(seed.globalSettings);
    const conversation = seed.workspaces[0].conversations[0];
    const user = userEvent.setup();
    render(
      <SettingsHarness
        initialConversation={{
          ...conversation,
          settings: {
            ...conversation.settings,
            mcpIds: ["mcp_workspace"],
            toolLock: lockFor(seed.globalSettings, { mcpIds: ["mcp_workspace"] })
          }
        }}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
      />
    );

    await openPage(user, /^MCP/);
    const row = screen.getByRole("button", { name: "Workspace Files" });
    expect(row).toHaveAttribute("aria-pressed", "true");
    expect(row).toBeEnabled();
    expect(row.closest(".catalog-row")).toHaveClass("catalog-row--on", "catalog-row--cache");
    expect(row.closest(".catalog-row")?.querySelector(".catalog-row__sign.lock-mark--cache")).not.toBeNull();
    // An entry the last request carried is not deleted out from under it.
    expect(screen.queryByRole("button", { name: "删除 Workspace Files" })).toBeNull();
  });

  it("drops the orange once the cache has gone cold, or for another model", async () => {
    const seed = createSeedDocument();
    seed.globalSettings = withWarmModel(seed.globalSettings);
    const conversation = seed.workspaces[0].conversations[0];
    const user = userEvent.setup();
    const coldLock = lockFor(
      seed.globalSettings,
      { mcpIds: ["mcp_workspace"] },
      new Date(Date.now() - 31 * 60_000).toISOString()
    );
    const { unmount } = render(
      <SettingsHarness
        initialConversation={{
          ...conversation,
          settings: { ...conversation.settings, mcpIds: ["mcp_workspace"], toolLock: coldLock }
        }}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
      />
    );
    await openPage(user, /^MCP/);
    const cold = screen.getByRole("button", { name: "Workspace Files" });
    expect(cold).toHaveAttribute("aria-pressed", "true");
    expect(cold.closest(".catalog-row")).toHaveClass("catalog-row--on");
    expect(cold.closest(".catalog-row")).not.toHaveClass("catalog-row--cache");
    expect(cold.closest(".catalog-row")?.querySelector(".lock-mark")).toBeNull();
    unmount();

    // Warm, but sent by a model other than the one selected now.
    const other = withSelectedModel(seed.globalSettings, "claude-sonnet-5-5");
    render(
      <SettingsHarness
        initialConversation={{
          ...conversation,
          settings: {
            ...conversation.settings,
            mcpIds: ["mcp_workspace"],
            toolLock: lockFor(seed.globalSettings, { mcpIds: ["mcp_workspace"] })
          }
        }}
        globalSettings={other}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
      />
    );
    await openPage(user, /^MCP/);
    const elsewhere = screen.getByRole("button", { name: "Workspace Files" });
    expect(elsewhere.closest(".catalog-row")).toHaveClass("catalog-row--on");
    expect(elsewhere.closest(".catalog-row")).not.toHaveClass("catalog-row--cache");
    expect(elsewhere.closest(".catalog-row")?.querySelector(".lock-mark")).toBeNull();
  });

  it("offers the skill delivery switch below the list whether or not a skill is selected", async () => {
    const seed = createSeedDocument();
    const onSettingsChange = vi.fn();
    const onConversationOnlyChange = vi.fn();
    const user = userEvent.setup();
    const withoutSkills = {
      ...seed.workspaces[0].conversations[0],
      settings: { ...seed.workspaces[0].conversations[0].settings, skillIds: [] }
    };
    const { unmount } = render(
      <SettingsHarness
        initialConversation={withoutSkills}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={onSettingsChange}
        onConversationOnlyChange={onConversationOnlyChange}
      />
    );

    await openPage(user, /^技能/);
    /* How a skill would arrive is worth knowing before installing one, so the
       policy is drawn with an empty selection too — and it reads after the list
       it qualifies rather than above it. */
    const policy = screen.getByRole("switch", { name: /拼进提示词|按需加载/ });
    const list = policy.closest(".conversation-settings__page-stack")
      ?.querySelector(".catalog-list");
    expect(list).not.toBeNull();
    expect(list!.compareDocumentPosition(policy) & Node.DOCUMENT_POSITION_FOLLOWING)
      .toBeTruthy();

    // Remount rather than rerender because the harness retains the conversation in local state.
    unmount();
    render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={onSettingsChange}
        onConversationOnlyChange={onConversationOnlyChange}
      />
    );

    await openPage(user, /^技能/);
    await user.click(screen.getByRole("switch", { name: "拼进提示词" }));
    // This is a preset component and must use `onChange`.
    expect(onSettingsChange).toHaveBeenLastCalledWith(
      expect.objectContaining({ skillToolEnabled: true })
    );
    expect(onConversationOnlyChange).not.toHaveBeenCalled();
  });

  it("offers the MCP tool-discovery switch below the list and draws it orange once a request has gone out with it", async () => {
    const seed = createSeedDocument();
    const onSettingsChange = vi.fn();
    const onConversationOnlyChange = vi.fn();
    const user = userEvent.setup();
    const withoutServers = {
      ...seed.workspaces[0].conversations[0],
      settings: { ...seed.workspaces[0].conversations[0].settings, mcpIds: [] }
    };
    const { unmount } = render(
      <SettingsHarness
        initialConversation={withoutServers}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={onSettingsChange}
        onConversationOnlyChange={onConversationOnlyChange}
      />
    );

    await openPage(user, /^MCP/);
    /* What a server's tools would cost per request is worth knowing before
       adding the first one, so the policy is drawn with an empty selection too
       — and it reads after the list it qualifies. */
    const policy = screen.getByRole("switch", { name: /全部声明|按需取回/ });
    const list = policy.closest(".conversation-settings__page-stack")
      ?.querySelector(".catalog-list");
    expect(list).not.toBeNull();
    expect(list!.compareDocumentPosition(policy) & Node.DOCUMENT_POSITION_FOLLOWING)
      .toBeTruthy();
    await user.click(policy);
    // A preset component, so it goes through `onChange` like skill delivery.
    expect(onSettingsChange).toHaveBeenLastCalledWith(
      expect.objectContaining({ mcpToolDiscoveryEnabled: true })
    );
    expect(onConversationOnlyChange).not.toHaveBeenCalled();

    // Once a request has gone out with it, moving it rewrites the prompt the
    // cache holds: orange, and still the user's to move.
    unmount();
    render(
      <SettingsHarness
        initialConversation={{
          ...seed.workspaces[0].conversations[0],
          settings: {
            ...seed.workspaces[0].conversations[0].settings,
            mcpToolDiscoveryEnabled: true,
            toolLock: lockFor(withWarmModel(seed.globalSettings), { mcpIds: ["mcp_workspace"], mcpToolDiscovery: true })
          }
        }}
        globalSettings={withWarmModel(seed.globalSettings)}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
      />
    );
    await openPage(user, /^MCP/);
    const settled = screen.getByRole("switch", { name: "按需取回" });
    expect(settled).toBeChecked();
    expect(settled).toBeEnabled();
    expect(settled).toHaveClass("switch--cache");
  });

  it("does not offer tool discovery on a model that cannot take a tool mid-conversation", async () => {
    const seed = createSeedDocument();
    const noAppend = withNonAppendModel(seed.globalSettings);
    const user = userEvent.setup();
    render(
      <SettingsHarness
        initialConversation={{
          ...seed.workspaces[0].conversations[0],
          settings: {
            ...seed.workspaces[0].conversations[0].settings,
            mcpToolDiscoveryEnabled: true,
            // Warm, and it names discovery as the last request had it: were the
            // row not held for a reason of its own it would be orange.
            toolLock: lockFor(noAppend, { mcpIds: ["mcp_workspace"], mcpToolDiscovery: true })
          }
        }}
        globalSettings={noAppend}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
      />
    );
    await openPage(user, /^MCP/);
    const discovery = screen.getByRole("switch", { name: "按需取回" });
    expect(discovery).toBeDisabled();
    expect(discovery).not.toBeChecked();
    expect(screen.getByText(/当前模型不支持中途追加工具/)).toBeInTheDocument();
    // This is a capability the model lacks, not a lock on the cache: the row says
    // so in its own words and wears no tone and no lock.
    const row = discovery.closest(".tool-toggle-row") as HTMLElement;
    expect(row).toHaveClass("tool-toggle-row--disabled");
    expect(row).not.toHaveClass("tool-toggle-row--cache");
    expect(row.querySelector(".lock-mark")).toBeNull();
    expect(discovery).not.toHaveClass("switch--cache");
  });

  it("draws the whole tool surface orange on a model that cannot append tools, warns before moving it, and leaves skills free", async () => {
    const seed = createSeedDocument();
    const noAppend = withNonAppendModel(seed.globalSettings);
    const conversation = seed.workspaces[0].conversations[0];
    const settings = (at?: string) => ({
      ...conversation.settings,
      enabledTools: ["read"],
      skillIds: [],
      mcpIds: [],
      // Off, and the last request had it off: only a model that cannot append
      // tools draws an off switch orange.
      webSearchEnabled: false,
      toolLock: lockFor(noAppend, { tools: ["read"] }, at)
    });
    const onSettingsChange = vi.fn();
    const user = userEvent.setup();
    const { unmount } = render(
      <SettingsHarness
        initialConversation={{ ...conversation, settings: settings() }}
        globalSettings={noAppend}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={onSettingsChange}
      />
    );

    // In place, not folded away: on and off rows alike are orange with their
    // lock and still enabled. Without a tool-append interface adding a tool
    // rewrites the declared list as surely as taking one away does.
    const read = document.querySelector<HTMLButtonElement>('[data-tool-name="read"]')!;
    const write = document.querySelector<HTMLButtonElement>('[data-tool-name="write"]')!;
    for (const row of [read, write]) {
      expect(row).toBeEnabled();
      expect(row).toHaveAttribute("data-lock-tone", "cache");
      expect(row).toHaveClass("tool-toggle-row--cache");
      expect(row.querySelector(".lock-mark--cache")).not.toBeNull();
    }
    expect(read).toHaveAttribute("aria-pressed", "true");
    expect(write).toHaveAttribute("aria-pressed", "false");

    // The switches on the advanced page are orange with the list — web access
    // and each memory tier, off as they are — and none is held.
    await openPage(user, /^高级工具/);
    for (const name of [/^联网搜索已/, "全局记忆已关闭", "项目记忆已关闭"]) {
      const control = screen.getByRole("switch", { name });
      expect(control).toBeEnabled();
      expect(control).toHaveClass("switch--cache");
      expect(control.closest(".tool-toggle-row")).toHaveClass("tool-toggle-row--cache");
      expect(control.closest(".tool-toggle-row")?.querySelector(".lock-mark--cache")).not.toBeNull();
    }
    // Plan mode is the composer's switch, not a row here.
    expect(screen.queryByRole("switch", { name: /^计划模式已/ })).toBeNull();
    expect(screen.queryByText("已生效的工具")).toBeNull();

    // An MCP server, selected or not, is part of the surface too: off, it is orange.
    await openPage(user, /^MCP/);
    const server = screen.getByRole("button", { name: "Workspace Files" });
    expect(server).toBeEnabled();
    expect(server).toHaveAttribute("aria-pressed", "false");
    expect(server.closest(".catalog-row")).toHaveClass("catalog-row--cache");
    expect(server.closest(".catalog-row")?.querySelector(".catalog-row__sign.lock-mark--cache")).not.toBeNull();

    // Skills are not tools: one not yet selected stays plain, and joins without
    // asking. (Nobody has been warned yet, so the missing dialog is the
    // lock's doing.)
    await openPage(user, /^技能/);
    const skill = screen.getByRole("button", { name: "代码审查" });
    expect(skill).toHaveAttribute("aria-pressed", "false");
    expect(skill.closest(".catalog-row")).not.toHaveClass("catalog-row--cache");
    expect(skill.closest(".catalog-row")?.querySelector(".lock-mark")).toBeNull();
    await user.click(skill);
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(onSettingsChange).toHaveBeenLastCalledWith(expect.objectContaining({ skillIds: ["skill_code_review"] }));
    expect(onSettingsChange).toHaveBeenCalledTimes(1);
    // How skills arrive rewrites the prompt they went out in, on every model.
    expect(screen.getByRole("switch", { name: "拼进提示词" })).toHaveClass("switch--cache");

    // Adding a tool is not refused: it asks, and nothing moves until the answer.
    await openPage(user, TOOLS_PAGE);
    await user.click(document.querySelector<HTMLButtonElement>('[data-tool-name="write"]')!);
    const warning = screen.getByRole("dialog", { name: "这样改会让缓存失效" });
    expect(onSettingsChange).toHaveBeenCalledTimes(1);
    await user.click(within(warning).getByRole("button", { name: "取消" }));
    expect(onSettingsChange).toHaveBeenCalledTimes(1);
    expect(document.querySelector('[data-tool-name="write"]')).toHaveAttribute("aria-pressed", "false");

    await user.click(document.querySelector<HTMLButtonElement>('[data-tool-name="write"]')!);
    await user.click(within(screen.getByRole("dialog", { name: "这样改会让缓存失效" }))
      .getByRole("button", { name: "仍然更改" }));
    expect(onSettingsChange).toHaveBeenLastCalledWith(expect.objectContaining({ enabledTools: ["read", "write"] }));
    unmount();

    // Long cold, the same model has nothing cached to lose: nothing is toned,
    // nothing asks, whichever way a row moves.
    resetCacheBreakWarnings();
    const onColdChange = vi.fn();
    render(
      <SettingsHarness
        initialConversation={{ ...conversation, settings: settings(new Date(Date.now() - 3 * 3_600_000).toISOString()) }}
        globalSettings={noAppend}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={onColdChange}
      />
    );
    const coldRead = document.querySelector<HTMLButtonElement>('[data-tool-name="read"]')!;
    const coldWrite = document.querySelector<HTMLButtonElement>('[data-tool-name="write"]')!;
    for (const row of [coldRead, coldWrite]) {
      expect(row).toBeEnabled();
      expect(row).not.toHaveAttribute("data-lock-tone");
      expect(row).not.toHaveClass("tool-toggle-row--cache");
      expect(row.querySelector(".lock-mark")).toBeNull();
    }
    await openPage(user, /^高级工具/);
    expect(screen.getByRole("switch", { name: /^联网搜索已/ })).not.toHaveClass("switch--cache");
    expect(screen.getByRole("switch", { name: "全局记忆已关闭" })).not.toHaveClass("switch--cache");
    await openPage(user, /^MCP/);
    expect(screen.getByRole("button", { name: "Workspace Files" }).closest(".catalog-row"))
      .not.toHaveClass("catalog-row--cache");
    await openPage(user, TOOLS_PAGE);
    await user.click(document.querySelector<HTMLButtonElement>('[data-tool-name="write"]')!);
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(onColdChange).toHaveBeenLastCalledWith(expect.objectContaining({ enabledTools: ["read", "write"] }));
  });

  it("draws the tools the warm cache holds orange where they stand", async () => {
    const seed = createSeedDocument();
    seed.globalSettings = withWarmModel(seed.globalSettings);
    const conversation = seed.workspaces[0].conversations[0];
    const onSettingsChange = vi.fn();
    const user = userEvent.setup();
    render(
      <SettingsHarness
        initialConversation={{
          ...conversation,
          settings: {
            ...conversation.settings,
            enabledTools: ["read"],
            toolLock: lockFor(seed.globalSettings, { tools: ["read"] })
          }
        }}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={onSettingsChange}
      />
    );

    const read = document.querySelector<HTMLButtonElement>('[data-tool-name="read"]')!;
    expect(read).toBeEnabled();
    expect(read).toHaveAttribute("data-lock-tone", "cache");
    // Adding a tool appends it at the end, so nothing cached is at stake.
    const write = document.querySelector<HTMLButtonElement>('[data-tool-name="write"]')!;
    expect(write).not.toHaveAttribute("data-lock-tone");
    await user.click(write);
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(onSettingsChange).toHaveBeenLastCalledWith(expect.objectContaining({ enabledTools: ["read", "write"] }));

    await user.click(document.querySelector<HTMLButtonElement>('[data-tool-name="read"]')!);
    expect(screen.getByRole("dialog", { name: "这样改会让缓存失效" })).toBeInTheDocument();
  });

  it("tones the hooks and the prompt profile the warm cache was built from", async () => {
    const seed = createSeedDocument();
    seed.globalSettings = withWarmModel(seed.globalSettings);
    seed.capabilities.hooks.push({
      id: "hook_lint",
      name: "Lint 钩子",
      description: "测试用钩子。",
      location: "test://hooks/lint.json#/hooks/PreToolUse/0/hooks/0",
      source: "user",
      available: true
    });
    const conversation = seed.workspaces[0].conversations[0];
    const onSettingsChange = vi.fn();
    const onConversationOnlyChange = vi.fn();
    const user = userEvent.setup();
    render(
      <SettingsHarness
        initialConversation={{
          ...conversation,
          settings: {
            ...conversation.settings,
            hookIds: ["hook_lint"],
            toolLock: lockFor(seed.globalSettings, {
              hookIds: ["hook_lint"],
              promptProfile: BUILTIN_PROFILE
            })
          }
        }}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={onSettingsChange}
        onConversationOnlyChange={onConversationOnlyChange}
      />
    );

    await openPage(user, /^钩子/);
    const lintHook = screen.getByRole("button", { name: "Lint 钩子" });
    expect(lintHook.closest(".catalog-row")).toHaveClass("catalog-row--on", "catalog-row--cache");
    await user.click(lintHook);
    expect(screen.getByRole("dialog", { name: "这样改会让缓存失效" })).toBeInTheDocument();
    expect(onSettingsChange).not.toHaveBeenCalled();
    await user.click(screen.getByRole("button", { name: "取消" }));

    // The prompt profile is the tool-description page's pick now: the row that
    // would replace the built-in is orange, as is every row of that page, since
    // whichever is clicked rewrites the words the cached prefix was written in.
    await openPage(user, TOOL_DESCRIPTIONS_PAGE);
    const main = screen.getByRole("button", { name: "main" });
    expect(main.closest(".catalog-row")).toHaveClass("catalog-row--cache");
    expect(main.closest(".catalog-row")?.querySelector(".catalog-row__sign.lock-mark--cache")).not.toBeNull();
    expect(main.closest(".catalog-row")).toHaveAttribute("title", expect.stringContaining("缓存还热"));
    // And the advanced page no longer carries the profile at all.
    await openPage(user, /^高级工具/);
    expect(screen.queryByRole("combobox", { name: "工具描述" })).toBeNull();
  });

  it("rescans by itself while it is open, when the capability files change", async () => {
    vi.useFakeTimers();
    try {
      const seed = createSeedDocument();
      const answers = ["a", "a", "b", "b"];
      const onCapabilityFingerprint = vi.fn(async () => answers.shift() ?? "b");
      const onRescanCapabilities = vi.fn(async () => undefined);
      render(
        <SettingsHarness
          initialConversation={seed.workspaces[0].conversations[0]}
          globalSettings={seed.globalSettings}
          tools={seed.tools}
          capabilities={seed.capabilities}
          onSettingsChange={vi.fn()}
          onRescanCapabilities={onRescanCapabilities}
          onCapabilityFingerprint={onCapabilityFingerprint}
        />
      );
      // Opening the pane rescans once.
      expect(onRescanCapabilities).toHaveBeenCalledTimes(1);
      await vi.advanceTimersByTimeAsync(4_100);
      // "a" then "a": nothing moved.
      expect(onRescanCapabilities).toHaveBeenCalledTimes(1);
      await vi.advanceTimersByTimeAsync(2_000);
      // "b": a file changed, so the lists are read again.
      expect(onRescanCapabilities).toHaveBeenCalledTimes(2);
      await vi.advanceTimersByTimeAsync(2_000);
      expect(onRescanCapabilities).toHaveBeenCalledTimes(2);
    } finally {
      vi.useRealTimers();
    }
  });

  it("ends every capability page with its own documentation link and nothing else", async () => {
    const seed = createSeedDocument();
    const user = userEvent.setup();
    render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
      />
    );

    // The jump to a global settings page went away with the pages themselves;
    // each catalog now points at the page documenting how it is configured.
    await openPage(user, /^MCP/);
    expect(screen.queryByRole("button", { name: "管理 MCP" })).toBeNull();
    expect(screen.getByRole("link", { name: "配置说明文档" }))
      .toHaveAttribute("href", "https://mewrk.dev/zh-CN/mcp.html");

    await openPage(user, /^技能/);
    expect(screen.queryByRole("button", { name: "管理技能" })).toBeNull();
    expect(screen.getByRole("link", { name: "配置说明文档" }))
      .toHaveAttribute("href", "https://mewrk.dev/zh-CN/skills.html");

    await openPage(user, /^钩子/);
    const hooksDocs = screen.getByRole("link", { name: "配置说明文档" });
    expect(hooksDocs).toHaveAttribute("href", "https://mewrk.dev/zh-CN/hooks.html");
    expect(hooksDocs.parentElement).toHaveClass("capability-page__toolbar");
  });

  it("lists the catalog's roles under the level each was read from, and selects one by its id", async () => {
    // A role is a file the host discovers, and a conversation selects it by id
    // exactly as it does a skill: nothing of the role itself is stored on the
    // conversation, so an edit made here must not flow back into the preset the
    // conversation came from.
    const { seed, user, onSettingsChange } = await openRolesPage({ workspaces: WORKSPACE_A });

    const global = screen.getByRole("region", { name: "全局" });
    expect(within(global).getByRole("button", { name: "Opus" })).toHaveAttribute("aria-pressed", "false");
    expect(within(global).getByRole("button", { name: "reviewer" })).toHaveAttribute("aria-pressed", "false");
    expect(within(global).queryByRole("button", { name: "planner" })).toBeNull();
    const workspace = screen.getByRole("region", { name: "/work/a" });
    expect(within(workspace).getByRole("button", { name: "planner" })).toHaveAttribute("aria-pressed", "false");
    // The owner routed no delete to the host, so no row offers one.
    expect(screen.queryByRole("button", { name: /^删除 / })).toBeNull();

    await user.click(within(global).getByRole("button", { name: "reviewer" }));
    expect(onSettingsChange).toHaveBeenLastCalledWith(expect.objectContaining({
      agentIds: ["agent_user_reviewer"]
    }));
    await user.click(within(workspace).getByRole("button", { name: "planner" }));
    expect(onSettingsChange).toHaveBeenLastCalledWith(expect.objectContaining({
      agentIds: ["agent_user_reviewer", "agent_ws_planner"]
    }));
    expect(within(global).getByRole("button", { name: "reviewer" })).toHaveAttribute("aria-pressed", "true");

    await user.click(within(global).getByRole("button", { name: "reviewer" }));
    expect(onSettingsChange).toHaveBeenLastCalledWith(expect.objectContaining({
      agentIds: ["agent_ws_planner"]
    }));
    expect(within(global).getByRole("button", { name: "reviewer" })).toHaveAttribute("aria-pressed", "false");
    expect(seed.globalSettings.conversationPresets[0].settings.agentIds).toEqual([]);
  });

  it("offers a draft with no project yet the global roles alone", async () => {
    await openRolesPage({ workspaces: [] });

    expect(screen.getByRole("button", { name: "reviewer" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "planner" })).toBeNull();
  });

  it("counts only the selected roles the catalog lists, and draws a dangling one to untick", async () => {
    const { user, onSettingsChange } = await openRolesPage({
      selected: ["agent_user_reviewer", "agent_ws_planner", "agent_gone"],
      workspaces: WORKSPACE_A
    });

    // The rail announces the roles something can call. `agent_gone` is still a
    // real row below, so the user can clear it, but counting it would announce a
    // role nobody can name.
    expect(within(navigation()).getByRole("button", { name: /^代理角色/ })).toHaveTextContent(/^代理角色2$/);
    // The page divides the same population: four listed, two of them selected.
    expect(screen.getByText("2 / 4 个已选")).toBeInTheDocument();

    const dangling = screen.getByText("agent_gone").closest(".catalog-row") as HTMLElement;
    expect(within(dangling).getByText("悬空")).toBeInTheDocument();
    expect(within(dangling).getByRole("button", { name: "agent_gone" })).toHaveAttribute("aria-pressed", "true");
    await user.click(within(dangling).getByRole("button", { name: "agent_gone" }));

    expect(onSettingsChange).toHaveBeenLastCalledWith(expect.objectContaining({
      agentIds: ["agent_user_reviewer", "agent_ws_planner"]
    }));
    expect(screen.queryByText("agent_gone")).toBeNull();
    expect(within(navigation()).getByRole("button", { name: /^代理角色/ })).toHaveTextContent(/^代理角色2$/);
  });

  it("does not count a selected role of a workspace this conversation does not have", async () => {
    await openRolesPage({
      selected: ["agent_c_auditor"],
      workspaces: WORKSPACE_A,
      agents: [...listedRoles(), roleResource("agent_c_auditor", "auditor", {
        source: "workspace",
        location: "/work/c/.mewrk/agents/auditor.json",
        workspaceKey: "local|/work/c"
      })]
    });

    // No run of this conversation can reach that workspace, so the page draws
    // the selection as dangling — and the rail must not announce it either.
    expect(within(navigation()).getByRole("button", { name: /^代理角色/ })).toHaveTextContent(/^代理角色0$/);
    expect(screen.getByText("0 / 4 个已选")).toBeInTheDocument();
    expect(within(screen.getByText("agent_c_auditor").closest(".catalog-row") as HTMLElement)
      .getByText("悬空")).toBeInTheDocument();
  });

  it("says in each row's tooltip which model a role runs on, and flags one that cannot resolve", async () => {
    const bound = (name: string, modelId: string) => roleBody(name, {
      modelSelection: { kind: "explicit", providerId: "anthropic_messages", modelId }
    });
    await openRolesPage({
      globalSettings: withWarmModel,
      agents: [
        // What an older build wrote in place of a binding it had found dead.
        roleResource("agent_user_opus", "Opus", {
          role: roleBody("Opus", { modelSelection: { kind: "unavailable" } })
        }),
        roleResource("agent_user_pinned", "pinned", { role: bound("pinned", "claude-opus-5-5") }),
        roleResource("agent_user_stale", "stale", { role: bound("stale", "claude-gone") }),
        roleResource("agent_user_follower", "follower")
      ]
    });

    // A row is one line, so the model rides the tooltip.
    expect(roleRow("pinned")).toHaveAttribute("title", expect.stringContaining("Anthropic Messages · claude-opus-5-5"));
    expect(within(roleRow("pinned")).queryByText("模型不可用")).toBeNull();
    expect(roleRow("follower")).toHaveAttribute("title", expect.stringContaining("跟随对话模型"));
    expect(within(roleRow("follower")).queryByText("模型不可用")).toBeNull();

    // The host hides a role whose model does not resolve from the model, so the
    // list says so instead of reading as a working row.
    expect(roleRow("stale")).toHaveAttribute("title", expect.stringContaining("claude-gone"));
    expect(roleRow("stale")).toHaveAttribute("title", expect.stringContaining("模型看不到这个角色"));
    expect(within(roleRow("stale")).getByText("模型不可用")).toBeInTheDocument();
    expect(roleRow("Opus")).toHaveAttribute("title", expect.stringContaining("没有可用的模型"));
    expect(within(roleRow("Opus")).getByText("模型不可用")).toBeInTheDocument();
    // No role is built in, so the warning is the only badge a row can carry.
    expect(within(roleRow("Opus")).queryByText("内置")).toBeNull();
  });

  it("opens a role in its own window from its row, and deletes the user's files in two clicks", async () => {
    const onDeleteCapability = vi.fn();
    const { user } = await openRolesPage({ workspaces: WORKSPACE_A, onDeleteCapability });

    // Named after its row, armed by the first click and fired by the second.
    await user.click(screen.getByRole("button", { name: "删除 reviewer" }));
    expect(onDeleteCapability).not.toHaveBeenCalled();
    await user.click(screen.getByRole("button", { name: "确认删除 reviewer" }));
    expect(onDeleteCapability).toHaveBeenLastCalledWith(
      "agents",
      expect.objectContaining({ id: "agent_user_reviewer" })
    );
    await user.click(screen.getByRole("button", { name: "删除 planner" }));
    await user.click(screen.getByRole("button", { name: "确认删除 planner" }));
    expect(onDeleteCapability).toHaveBeenLastCalledWith(
      "agents",
      expect.objectContaining({ id: "agent_ws_planner", workspaceKey: WORKSPACE_A[0].key })
    );
    expect(onDeleteCapability).toHaveBeenCalledTimes(2);

    // A file whose body could not be read has nothing to open: the row says why,
    // and the file can still be removed.
    const broken = roleRow("broken");
    expect(within(broken).queryByRole("button", { name: "设置角色 broken" })).toBeNull();
    expect(within(broken).getByText("不可用")).toBeInTheDocument();
    expect(broken).toHaveAttribute("title", expect.stringContaining("角色文件不是合法的 JSON"));
    expect(within(broken).getByRole("button", { name: "删除 broken" })).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "设置角色 reviewer" }));
    const dialog = screen.getByRole("dialog", { name: "reviewer" });
    expect(within(dialog).getByRole("textbox", { name: "角色名称" })).toHaveValue("reviewer");
  });

  it("draws the role window from the whole tool catalogue, not the shells this conversation's machines have", async () => {
    // A Windows conversation lists neither zsh nor sh. A role is no one
    // machine's: a role written from here must still be able to hold them for
    // the conversations that do have those shells.
    const onSaveAgentRole = savesRoleAs("agent_user_new");
    const { seed, user } = await openRolesPage({
      onSaveAgentRole,
      tools: (catalogue) => catalogue.filter((tool) => tool.name !== "zsh" && tool.name !== "sh")
    });

    await user.click(screen.getByRole("button", { name: "新建角色" }));
    const dialog = screen.getByRole("dialog", { name: "新建角色" });
    const rail = within(dialog).getByRole("navigation", { name: "角色设置分类" });
    await user.click(within(rail).getByRole("button", { name: /^工具/ }));
    const rows = Array.from(dialog.querySelectorAll<HTMLElement>("[data-tool-name]"))
      .map((row) => row.dataset.toolName);
    expect(rows).toEqual(expect.arrayContaining(["pwsh", "powershell", "bash", "zsh", "sh"]));
    // Ticked, as every row a new role starts with is.
    expect(within(dialog).getByRole("button", { name: "zsh已启用" })).toBeInTheDocument();

    await user.click(within(rail).getByRole("button", { name: /^角色设置/ }));
    await user.type(within(dialog).getByRole("textbox", { name: "角色名称" }), "portable");
    await user.click(within(dialog).getByRole("button", { name: "保存角色" }));

    // The default draft is every tool a role can hold — what the host's
    // `all_role_tool_names` is — whichever conversation opened the window.
    const everyRoleTool = seed.tools
      .filter((tool) => tool.category !== "orchestration" && tool.category !== "memory"
        && !isHostDerivedToolName(tool.name))
      .map((tool) => tool.name)
      .sort();
    await waitFor(() => expect(onSaveAgentRole).toHaveBeenCalledTimes(1));
    expect(onSaveAgentRole.mock.calls[0][1].tools).toEqual(everyRoleTool);
    expect(everyRoleTool).toEqual(expect.arrayContaining(["zsh", "sh"]));

    // The conversation's own tools page is still narrowed to what it can run.
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "新建角色" })).toBeNull());
    await openPage(user, TOOLS_PAGE);
    expect(document.querySelector('[data-tool-name="zsh"]')).toBeNull();
    expect(document.querySelector('[data-tool-name="powershell"]')).not.toBeNull();
    expect(document.querySelector('[data-tool-name="pwsh"]')).not.toBeNull();
  });

  it("writes a new role at the global level and selects it on the conversation", async () => {
    const onSaveAgentRole = savesRoleAs("agent_user_new");
    const { seed, user, onSettingsChange } = await openRolesPage({
      selected: ["agent_user_reviewer"],
      workspaces: WORKSPACE_A,
      onSaveAgentRole
    });

    await user.click(screen.getByRole("button", { name: "新建角色" }));
    const dialog = screen.getByRole("dialog", { name: "新建角色" });
    // The global level first, then one entry per workspace of the conversation.
    expect(within(within(dialog).getByRole("combobox", { name: "位置" })).getAllByRole("option")
      .map((option) => option.textContent)).toEqual(["全局 · ~/.mewrk/agents", "/work/a"]);
    await user.type(within(dialog).getByRole("textbox", { name: "角色名称" }), "security-reviewer");
    await user.click(within(dialog).getByRole("button", { name: "保存角色" }));

    // The file is written through the host, not onto the conversation...
    await waitFor(() => expect(onSaveAgentRole).toHaveBeenCalledWith(
      { workspaceKey: null },
      expect.objectContaining({ name: "security-reviewer", modelSelection: { kind: "inherit" } })
    ));
    // ...and the conversation then selects the id the file landed under, after
    // the one it already had.
    await waitFor(() => expect(onSettingsChange).toHaveBeenLastCalledWith(expect.objectContaining({
      agentIds: ["agent_user_reviewer", "agent_user_new"]
    })));
    expect(screen.queryByRole("dialog", { name: "新建角色" })).toBeNull();
    expect(seed.globalSettings.conversationPresets[0].settings.agentIds).toEqual([]);
  });

  it("writes a new role into one of the conversation's workspaces when asked to", async () => {
    const onSaveAgentRole = savesRoleAs("agent_ws_new");
    const { user, onSettingsChange } = await openRolesPage({ workspaces: WORKSPACE_A, onSaveAgentRole });

    await user.click(screen.getByRole("button", { name: "新建角色" }));
    const dialog = screen.getByRole("dialog", { name: "新建角色" });
    await user.type(within(dialog).getByRole("textbox", { name: "角色名称" }), "local-helper");
    await user.selectOptions(within(dialog).getByRole("combobox", { name: "位置" }), WORKSPACE_A[0].key);
    await user.click(within(dialog).getByRole("button", { name: "保存角色" }));

    await waitFor(() => expect(onSaveAgentRole).toHaveBeenCalledWith(
      { workspaceKey: "local|/work/a" },
      expect.objectContaining({ name: "local-helper" })
    ));
    await waitFor(() => expect(onSettingsChange).toHaveBeenLastCalledWith(expect.objectContaining({
      agentIds: ["agent_ws_new"]
    })));
  });

  it("says a role the model cannot see is merely hidden, not that every run fails", async () => {
    const { unmount } = await openRolesPage({
      selected: ["agent_user_broken", "agent_gone"],
      workspaces: WORKSPACE_A
    });

    // Neither a vanished role file nor an unreadable one stops a run: the host
    // just does not offer that role to the model.
    const dangling = screen.getByRole("button", { name: "agent_gone" }).closest(".catalog-row") as HTMLElement;
    expect(dangling).toHaveAttribute("title", expect.stringContaining("找不到这个角色的文件，模型看不到它；可以取消勾选。"));
    expect(dangling).not.toHaveAttribute("title", expect.stringContaining("每次运行都会失败"));
    expect(roleRow("broken")).toHaveAttribute(
      "title",
      expect.stringContaining("已选择，但文件无法使用，模型看不到它；修好文件或取消勾选。")
    );
    expect(roleRow("broken")).not.toHaveAttribute("title", expect.stringContaining("每次运行都会失败"));
    unmount();

    // The same two rows in English.
    configureI18n("en-US");
    await openRolesPage({
      selected: ["agent_user_broken", "agent_gone"],
      workspaces: WORKSPACE_A,
      navigation: /^Agent roles/
    });
    expect(screen.getByRole("button", { name: "agent_gone" }).closest(".catalog-row")).toHaveAttribute(
      "title",
      expect.stringContaining("This role's file is gone, so the model does not see it; untick it.")
    );
    expect(roleRow("broken")).toHaveAttribute(
      "title",
      expect.stringContaining("Selected, but the file cannot be used, so the model does not see it; fix the file or untick it.")
    );
  });

  it("keeps the window open with the reason when the file cannot be written", async () => {
    const onSaveAgentRole = vi.fn<(target: SaveAgentRoleTarget, role: AgentRole) => Promise<string>>()
      .mockRejectedValue(new Error("磁盘已满"));
    const { user, onSettingsChange } = await openRolesPage({ onSaveAgentRole });

    await user.click(screen.getByRole("button", { name: "新建角色" }));
    const dialog = screen.getByRole("dialog", { name: "新建角色" });
    await user.type(within(dialog).getByRole("textbox", { name: "角色名称" }), "unsaved");
    await user.click(within(dialog).getByRole("button", { name: "保存角色" }));

    expect(await within(dialog).findByRole("alert")).toHaveTextContent("磁盘已满");
    // Nothing was written, so nothing is selected on the conversation.
    expect(onSettingsChange).not.toHaveBeenCalled();
    expect(screen.getByRole("dialog", { name: "新建角色" })).toBeInTheDocument();
    // The draft is kept for the next try rather than saved half-way: a save that
    // went through would have closed the window and dropped it.
    expect(within(dialog).getByRole("textbox", { name: "角色名称" })).toHaveValue("unsaved");

    // Saving it somewhere that works is what ends the draft, so a later window
    // does not open on a name this test left behind.
    onSaveAgentRole.mockResolvedValue("agent_user_unsaved");
    await user.click(within(dialog).getByRole("button", { name: "保存角色" }));
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "新建角色" })).toBeNull());
  });

  it("draws the role-less switch even with no usable role, and writes it onto the conversation", async () => {
    // Selected, but its file could not be read: nothing the model can name.
    const unreadable = await openRolesPage({ selected: ["agent_user_broken"] });
    // With no usable role the host allows role-less execution regardless, but
    // the switch stays on screen so the policy is never invisible.
    expect(screen.getByRole("switch", { name: /角色必填|角色可选/ })).toBeInTheDocument();
    expect(screen.getByText(/当前没有可用角色/)).toBeInTheDocument();
    unreadable.unmount();

    // Nor does a role whose model no provider resolves count as usable.
    const unresolved = await openRolesPage({
      selected: ["agent_user_ghost"],
      agents: [
        ...listedRoles(),
        roleResource("agent_user_ghost", "ghost", {
          role: roleBody("ghost", {
            modelSelection: { kind: "explicit", providerId: "ghost_provider", modelId: "ghost-model" }
          })
        })
      ]
    });
    expect(screen.getByText(/当前没有可用角色/)).toBeInTheDocument();
    unresolved.unmount();

    const { user, onSettingsChange } = await openRolesPage({ selected: ["agent_user_reviewer"] });
    expect(screen.queryByText(/当前没有可用角色/)).toBeNull();
    await user.click(screen.getByRole("switch", { name: "角色必填" }));
    expect(onSettingsChange).toHaveBeenLastCalledWith(
      expect.objectContaining({ allowRolelessSubagents: true })
    );
    await user.click(screen.getByRole("switch", { name: "角色可选" }));
    expect(onSettingsChange).toHaveBeenLastCalledWith(
      expect.objectContaining({ allowRolelessSubagents: false })
    );
  });

  it("lists the whole role catalog in a preset window and saves the selection into the preset", async () => {
    const user = userEvent.setup();
    const seed = createSeedDocument();
    seed.globalSettings.conversationPresets[0].settings.agentIds = ["agent_user_reviewer"];
    const onSavePreset = vi.fn();
    render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={withRoles(seed.capabilities, [
          ...listedRoles(),
          roleResource("agent_c_auditor", "auditor", {
            source: "workspace",
            location: "/work/c/.mewrk/agents/auditor.json",
            workspaceKey: "local|/work/c"
          })
        ])}
        onSettingsChange={vi.fn()}
        onSavePreset={onSavePreset}
        workspaces={WORKSPACE_A}
      />
    );

    await openPage(user, /^对话预设/);
    await user.click(screen.getByRole("button", { name: "打开预设 默认" }));
    const dialog = screen.getByRole("dialog", { name: "默认" });
    const nestedNav = within(dialog).getByRole("navigation", { name: "对话设置分类" });
    // The preset's own selection, not the conversation's.
    expect(within(nestedNav).getByRole("button", { name: /^代理角色/ })).toHaveTextContent(/^代理角色1$/);
    await user.click(within(nestedNav).getByRole("button", { name: /^代理角色/ }));

    // A preset points at no workspace, so it sees every level the catalog has,
    // not the conversation's narrowing.
    expect(within(dialog).getByRole("button", { name: "reviewer" })).toHaveAttribute("aria-pressed", "true");
    expect(within(within(dialog).getByRole("region", { name: "/work/a" })).getByRole("button", { name: "planner" }))
      .toHaveAttribute("aria-pressed", "false");
    expect(within(within(dialog).getByRole("region", { name: "/work/c" })).getByRole("button", { name: "auditor" }))
      .toBeInTheDocument();

    await user.click(within(dialog).getByRole("button", { name: "planner" }));
    await user.click(within(nestedNav).getByRole("button", { name: "保存预设" }));
    expect(onSavePreset).toHaveBeenCalledWith("conversation_default", expect.objectContaining({
      agentIds: ["agent_user_reviewer", "agent_ws_planner"]
    }));
  });

  it("writes a role from a preset window at the global level alone, and selects it in the preset", async () => {
    const user = userEvent.setup();
    const seed = createSeedDocument();
    const onSavePreset = vi.fn();
    const onSaveAgentRole = savesRoleAs("agent_user_new");
    render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={withRoles(seed.capabilities)}
        onSettingsChange={vi.fn()}
        onSavePreset={onSavePreset}
        onSaveAgentRole={onSaveAgentRole}
        workspaces={WORKSPACE_A}
      />
    );

    await openPage(user, /^对话预设/);
    await user.click(screen.getByRole("button", { name: "打开预设 默认" }));
    const presetWindow = screen.getByRole("dialog", { name: "默认" });
    const nestedNav = within(presetWindow).getByRole("navigation", { name: "对话设置分类" });
    await user.click(within(nestedNav).getByRole("button", { name: /^代理角色/ }));
    await user.click(within(presetWindow).getByRole("button", { name: "新建角色" }));

    // A preset belongs to no workspace, so a role written from its window can
    // only be written where every conversation can reach it.
    const editor = screen.getByRole("dialog", { name: "新建角色" });
    const location = within(editor).getByRole("combobox", { name: "位置" });
    expect(within(location).getAllByRole("option").map((option) => option.textContent))
      .toEqual(["全局 · ~/.mewrk/agents"]);
    await user.type(within(editor).getByRole("textbox", { name: "角色名称" }), "preset-helper");
    await user.click(within(editor).getByRole("button", { name: "保存角色" }));
    await waitFor(() => expect(onSaveAgentRole).toHaveBeenCalledWith(
      { workspaceKey: null },
      expect.objectContaining({ name: "preset-helper" })
    ));
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "新建角色" })).toBeNull());

    // The created id is part of the preset body, which is what saving writes.
    await user.click(within(nestedNav).getByRole("button", { name: "保存预设" }));
    expect(onSavePreset).toHaveBeenCalledWith("conversation_default", expect.objectContaining({
      agentIds: ["agent_user_new"]
    }));
  });

  it("renames a saved preset in the row itself, committing when the field is left", async () => {
    const user = userEvent.setup();
    const seed = createSeedDocument();
    // A preset of the user's own: only those offer the rename and delete
    // actions, which the built-in draws disabled.
    seed.globalSettings.conversationPresets.push({
      ...seed.globalSettings.conversationPresets[0],
      id: "conversation_team",
      name: "团队预设"
    });
    const onRenamePreset = vi.fn();
    render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
        onRenamePreset={onRenamePreset}
      />
    );

    await openPage(user, /^对话预设/);
    const openRename = async (): Promise<HTMLElement> => {
      const row = screen.getByRole("button", { name: "打开预设 团队预设" })
        .closest(".catalog-row") as HTMLElement;
      await user.click(within(row).getByRole("button", { name: "重命名" }));
      return screen.getByRole("textbox", { name: "重命名预设 团队预设" });
    };

    // The row becomes the field, the way the sidebar renames a conversation:
    // leaving it is what commits, so there is no Save button to forget.
    const field = await openRename();
    expect(within(field.closest(".catalog-row") as HTMLElement)
      .queryByRole("button", { name: "保存" })).toBeNull();
    await user.clear(field);
    await user.type(field, "发布流程");
    await user.tab();
    expect(onRenamePreset).toHaveBeenCalledWith("conversation_team", "发布流程");
    expect(screen.queryByRole("textbox", { name: /^重命名预设/ })).toBeNull();

    // Enter is that same commit, reached from the keyboard.
    const viaEnter = await openRename();
    await user.clear(viaEnter);
    await user.type(viaEnter, "审查流程{Enter}");
    expect(onRenamePreset).toHaveBeenLastCalledWith("conversation_team", "审查流程");
    expect(onRenamePreset).toHaveBeenCalledTimes(2);

    // A blank name is not a rename, and neither is the name the preset already
    // had: both close the field without writing rather than storing a blank.
    const blank = await openRename();
    await user.clear(blank);
    await user.type(blank, "   ");
    await user.tab();
    expect(onRenamePreset).toHaveBeenCalledTimes(2);

    const unchanged = await openRename();
    expect(unchanged).toHaveValue("团队预设");
    await user.tab();
    expect(onRenamePreset).toHaveBeenCalledTimes(2);

    // Escape discards, so a name typed and then abandoned never reaches disk.
    const abandoned = await openRename();
    await user.clear(abandoned);
    await user.type(abandoned, "不要这个{Escape}");
    expect(onRenamePreset).toHaveBeenCalledTimes(2);
    expect(screen.queryByRole("textbox", { name: /^重命名预设/ })).toBeNull();
  });

  it("deletes a saved conversation preset from its own row", async () => {
    const user = userEvent.setup();
    const seed = createSeedDocument();
    seed.globalSettings.conversationPresets.push({
      ...seed.globalSettings.conversationPresets[0],
      id: "conversation_team",
      name: "团队预设"
    });
    const onDeletePreset = vi.fn();
    render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
        onDeletePreset={onDeletePreset}
      />
    );

    await openPage(user, /^对话预设/);
    // Deleting arms in place and needs a second click, the same two steps the
    // conversation list uses.
    const saved = screen.getByRole("button", { name: "打开预设 团队预设" })
      .closest(".catalog-row") as HTMLElement;
    await user.click(within(saved).getByRole("button", { name: "删除预设 团队预设" }));
    expect(onDeletePreset).not.toHaveBeenCalled();
    await user.click(within(saved).getByRole("button", { name: "确认删除预设 团队预设" }));
    expect(onDeletePreset).toHaveBeenCalledWith("conversation_team");
  });

  it("deletes a catalog entry from its own row, in two clicks, and says nothing about its scope", async () => {
    const user = userEvent.setup();
    const seed = createSeedDocument();
    const onDeleteCapability = vi.fn();
    render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
        onDeleteCapability={onDeleteCapability}
      />
    );

    await openPage(user, /^技能/);
    const row = screen.getByText("代码审查").closest(".catalog-row") as HTMLElement;
    // A row says what the entry is called. Where it came from was a badge that
    // only ever repeated what the path in the tooltip already says.
    expect(within(row).queryByText("用户")).not.toBeInTheDocument();
    expect(row.querySelector(".catalog-row__badge")).toBeNull();

    await user.click(within(row).getByRole("button", { name: "删除 代码审查" }));
    expect(onDeleteCapability).not.toHaveBeenCalled();
    await user.click(within(row).getByRole("button", { name: "确认删除 代码审查" }));
    expect(onDeleteCapability).toHaveBeenCalledWith(
      "skills",
      expect.objectContaining({ id: "skill_code_review" })
    );

    // The same row on the MCP page, so the shared page is not skills-only.
    await openPage(user, /^MCP/);
    const server = screen.getByText("Workspace Files").closest(".catalog-row") as HTMLElement;
    await user.click(within(server).getByRole("button", { name: "删除 Workspace Files" }));
    await user.click(within(server).getByRole("button", { name: "确认删除 Workspace Files" }));
    expect(onDeleteCapability).toHaveBeenLastCalledWith(
      "mcp",
      expect.objectContaining({ id: "mcp_workspace" })
    );
  });

  it("offers no delete on a catalog row when the owner cannot route one", async () => {
    const user = userEvent.setup();
    const seed = createSeedDocument();
    render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
      />
    );

    await openPage(user, /^技能/);
    const row = screen.getByText("代码审查").closest(".catalog-row") as HTMLElement;
    expect(within(row).queryByRole("button", { name: /^删除/ })).not.toBeInTheDocument();
  });

  it("lets the built-in Mewrk SDK skill be selected but never deleted", async () => {
    const user = userEvent.setup();
    const seed = createSeedDocument();
    // The host lists the compiled-in skill first, as it does the built-in profile.
    seed.capabilities.skills.unshift({
      id: "skill_builtin_mewrk_sdk",
      name: "Mewrk SDK",
      description: "内置，随 Mewrk 版本更新：指导模型配置 Mewrk 自身",
      location: "builtin:skills/mewrk-sdk/SKILL.md",
      source: "builtin",
      available: true
    });
    const onSettingsChange = vi.fn();
    render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={onSettingsChange}
        onDeleteCapability={vi.fn()}
      />
    );

    await openPage(user, /^技能/);
    const row = screen.getByText("Mewrk SDK").closest(".catalog-row") as HTMLElement;
    expect(within(row).queryByRole("button", { name: /^删除/ })).not.toBeInTheDocument();
    // A skill of the user's beside it keeps its delete button.
    const own = screen.getByText("代码审查").closest(".catalog-row") as HTMLElement;
    expect(within(own).getByRole("button", { name: "删除 代码审查" })).toBeInTheDocument();

    const toggle = screen.getByRole("button", { name: "Mewrk SDK" });
    expect(toggle).toHaveAttribute("aria-pressed", "false");
    await user.click(toggle);
    expect(onSettingsChange).toHaveBeenLastCalledWith(expect.objectContaining({
      skillIds: expect.arrayContaining(["skill_builtin_mewrk_sdk", "skill_code_review"])
    }));
  });

  it("opens a preset into the whole pane in preset mode and saves it back", async () => {
    const user = userEvent.setup();
    const seed = createSeedDocument();
    const onSavePreset = vi.fn();
    render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
        onSavePreset={onSavePreset}
      />
    );

    await openPage(user, /^对话预设/);
    await user.click(screen.getByRole("button", { name: "打开预设 默认" }));

    const dialog = screen.getByRole("dialog", { name: "默认" });
    const nestedNav = within(dialog).getByRole("navigation", { name: "对话设置分类" });
    // A window takes the global settings' layout: its name heads the page list.
    // The rail names the page, so the page carries its description and no
    // title, as it does in the side pane.
    expect(dialog.querySelector(".settings-layout > .settings-nav")).toBe(nestedNav);
    expect(within(nestedNav).getByRole("heading", { level: 2, name: "默认" })).toBeInTheDocument();
    expect(within(dialog).queryByRole("heading", { level: 3 })).toBeNull();
    const heading = dialog.querySelector(".settings-page-heading");
    expect(heading).toHaveTextContent(/交给模型的工具/);
    // The documentation link ends the description's line, as in the side pane.
    expect(within(dialog).getByRole("link", { name: "配置说明文档" }).parentElement).toBe(heading);
    expect(within(dialog).queryByText("启用工具")).toBeNull();
    // A preset body describes a reusable copy, so the one page about a live
    // conversation — its own presets — is withheld, the page only a preset has
    // appears, and the rail's foot saves in place rather than saving a copy.
    expect(within(nestedNav).getAllByRole("button")
      .map((button) => (button.textContent ?? "").replace(/\d+$/, "")))
      .toEqual(["工具", "高级工具", "技能", "MCP", "钩子", "工具描述", "代理角色", "对话模板", "保存预设"]);
    expect(within(nestedNav).queryByRole("button", { name: /对话预设/ })).toBeNull();
    await user.click(within(nestedNav).getByRole("button", { name: /^高级工具/ }));
    expect(within(dialog).getByRole("switch", { name: /^联网搜索已/ })).toBeInTheDocument();
    // The file write guards are part of a preset, on in a fresh one.
    expect(within(dialog).getByRole("switch", { name: "文件防误写保护已开启" })).toBeChecked();
    // Plan mode is not part of a preset.
    expect(within(dialog).queryByRole("switch", { name: /^计划模式已/ })).toBeNull();
    expect(within(dialog).getByRole("link", { name: "配置说明文档" }).parentElement)
      .toBe(dialog.querySelector(".settings-page-heading"));

    // Saving belongs to the window rather than to a page, so it is the rail's
    // last thing, as it is in the side pane.
    const save = within(nestedNav).getByRole("button", { name: "保存预设" });
    expect(save.parentElement).toBe(nestedNav.lastElementChild);
    expect(save.parentElement).toHaveClass("settings-nav__footer");
    await user.click(save);
    expect(onSavePreset).toHaveBeenCalledWith("conversation_default", expect.objectContaining({
      enabledTools: expect.any(Array),
      skillIds: expect.any(Array),
      mcpIds: expect.any(Array),
      hookIds: expect.any(Array)
    }));
    // Saving narrows the pane's whole body back down to exactly what a preset owns.
    expect(Object.keys(onSavePreset.mock.calls.at(-1)![1]).sort()).toEqual([
      "agentIds", "allowRolelessSubagents",
      "enabledTools", "fileWriteGuardsEnabled",
      "globalMemoryEnabled",
      "hookIds", "hostMessageContainer", "mcpIds", "mcpToolDiscoveryEnabled", "projectMemoryEnabled",
      "securityLevel", "skillIds",
      "skillToolEnabled", "toolDescriptionFileId", "webSearch", "webSearchEnabled"
    ]);
  });

  it("offers the tool-descriptions page in a preset window and saves the chosen file with the preset", async () => {
    const user = userEvent.setup();
    const seed = createSeedDocument();
    seed.capabilities.toolDescriptionFiles.push({
      id: "tooldesc_workspace_stray",
      name: "stray",
      description: "放在项目里的文件",
      location: "/work/c/.mewrk/tool-descriptions/stray.json",
      source: "workspace",
      available: true,
      workspaceKey: "local|/work/c"
    });
    const onSavePreset = vi.fn();
    render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
        onSavePreset={onSavePreset}
        workspaces={WORKSPACE_A}
      />
    );

    await openPage(user, /^对话预设/);
    await user.click(screen.getByRole("button", { name: "打开预设 默认" }));
    const dialog = screen.getByRole("dialog", { name: "默认" });
    const nestedNav = within(dialog).getByRole("navigation", { name: "对话设置分类" });
    const entry = () => within(nestedNav).getByRole("button", { name: TOOL_DESCRIPTIONS_PAGE });
    // The same place in the rail as in the side pane, and a zero for the preset's built-in.
    expect(within(nestedNav).getByRole("button", { name: /^钩子/ }).nextElementSibling).toBe(entry());
    expect(entry()).toHaveTextContent(/^工具描述0$/);

    await user.click(entry());
    // A preset is reusable and points at no workspace, which the other catalogs
    // answer by drawing every level; this one has no level but the global one.
    expect([...dialog.querySelectorAll(".capability-section")].map((section) => section.getAttribute("aria-label")))
      .toEqual(["全局"]);
    expect(within(dialog).queryByRole("button", { name: "stray" })).toBeNull();
    expect(within(dialog).queryByRole("button", { name: "Mewrk guided" })).toBeNull();
    expect(within(dialog).getByRole("button", { name: "Mewrk concise" })).toBeInTheDocument();
    // A preset has run nothing, so nothing here is toned.
    expect(dialog.querySelectorAll(".catalog-row--cache")).toHaveLength(0);

    await user.click(within(dialog).getByRole("button", { name: "main" }));
    expect(entry()).toHaveTextContent(/^工具描述1$/);
    expect(within(dialog).getByRole("button", { name: "main" })).toHaveAttribute("aria-pressed", "true");
    expect(within(dialog).queryByRole("dialog")).toBeNull();

    await user.click(within(nestedNav).getByRole("button", { name: "保存预设" }));
    expect(onSavePreset).toHaveBeenCalledWith("conversation_default", expect.objectContaining({
      toolDescriptionFileId: MAIN_PROFILE
    }));
  });

  it("opens the preset's own message queue on its page and writes the edited body back", async () => {
    const user = userEvent.setup();
    const seed = createSeedDocument();
    seed.globalSettings.conversationPresets[0].templateId = "template_preset";
    const onReadTemplate = vi.fn(async (): Promise<ContextItem[]> => ([{
      id: "ctx_seed",
      kind: "user",
      content: "先读一下 README",
      createdAt: "2026-01-02T03:04:05.000Z"
    }]));
    const onWriteTemplate = vi.fn(async (): Promise<string> => "template_preset");
    const onBindPresetTemplate = vi.fn();
    render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
        templates={[template("template_preset", "开局", 1)]}
        onReadTemplate={onReadTemplate}
        onWriteTemplate={onWriteTemplate}
        onBindPresetTemplate={onBindPresetTemplate}
      />
    );

    await openPage(user, /^对话预设/);
    await user.click(screen.getByRole("button", { name: "打开预设 默认" }));
    const dialog = screen.getByRole("dialog", { name: "默认" });
    // A body is big enough that reading it on the chance the page is opened
    // would make opening a preset slower for everyone who never looks.
    expect(onReadTemplate).not.toHaveBeenCalled();

    const nestedNav = within(dialog).getByRole("navigation", { name: "对话设置分类" });
    await user.click(within(nestedNav).getByRole("button", { name: /^对话模板/ }));
    expect(onReadTemplate).toHaveBeenCalledWith("template_preset");
    expect(await within(dialog).findByText("先读一下 README")).toBeInTheDocument();

    // The queue is edited in place, on the surface a timeline is, and the whole
    // body goes back to the host as the edit lands — the page carries no save of
    // its own, so there is nothing here left to press.
    await user.click(within(dialog).getAllByRole("button", { name: "编辑上下文" })[0]);
    const field = within(dialog).getByRole("textbox", { name: "用户输入" });
    await user.clear(field);
    await user.type(field, "先读一下 AGENTS.md");
    const card = dialog.querySelector(".inline-text-editor") as HTMLElement;
    await user.click(within(card).getByRole("button", { name: "保存" }));
    expect(within(dialog).queryByRole("button", { name: "保存模板" })).toBeNull();

    await waitFor(() => expect(onWriteTemplate).toHaveBeenCalledWith("template_preset", [
      expect.objectContaining({ id: "ctx_seed", content: "先读一下 AGENTS.md" })
    ]));
    // The id did not move, so the preset has nothing new to cite.
    expect(onBindPresetTemplate).not.toHaveBeenCalled();
  });

  it("never disables a field on account of a preset", async () => {
    const seed = createSeedDocument();
    const user = userEvent.setup();
    render(
      <SettingsHarness
        initialConversation={{
          ...seed.workspaces[0].conversations[0],
          presetId: "conversation_default",
          templateId: ""
        }}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
      />
    );
    await openPage(user, /^高级工具/);

    // A preset trace names where the values came from; it grants nobody authority
    // over the conversation, so every field stays editable.
    expect(backendTrigger("搜索提供商")).toBeEnabled();
    expect(screen.getByRole("switch", { name: "全局记忆已关闭" })).toBeEnabled();
  });

  it("divides a capability page into the global level and each of the conversation's workspaces", async () => {
    const seed = createSeedDocument();
    // The fixture skill carries no workspace key: it is global, and every
    // conversation may select it.
    seed.capabilities.skills.push(
      {
        id: "skill_here",
        name: "本工作区技能",
        description: "测试用工作区技能。",
        location: "/work/a/.mewrk/skills/here/SKILL.md",
        source: "workspace",
        available: true,
        workspaceKey: WORKSPACE_A[0].key
      },
      {
        id: "skill_second",
        name: "第二工作区技能",
        description: "同一对话第二个工作区的技能。",
        location: "/srv/b/.mewrk/skills/second/SKILL.md",
        source: "workspace",
        available: true,
        workspaceKey: "ssh:m1|/srv/b"
      },
      {
        id: "skill_elsewhere",
        name: "别处技能",
        description: "另一个项目的技能。",
        location: "/work/c/.mewrk/skills/else/SKILL.md",
        source: "workspace",
        available: true,
        workspaceKey: "local|/work/c"
      }
    );
    const user = userEvent.setup();
    const { unmount } = render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
        workspaces={[
          ...WORKSPACE_A,
          { number: 2, key: "ssh:m1|/srv/b", machine: { kind: "ssh", machineId: "m1" }, path: "/srv/b" }
        ]}
      />
    );

    await openPage(user, /^技能/);
    // The union of the global level and both workspaces, each in its own
    // section headed by the workspace's absolute path; a workspace the
    // conversation does not have is not offered, because the host would fail
    // that selection at run time.
    expect(within(screen.getByRole("region", { name: "全局" })).getByRole("button", { name: "代码审查" }))
      .toBeInTheDocument();
    expect(within(screen.getByRole("region", { name: "/work/a" })).getByRole("button", { name: "本工作区技能" }))
      .toBeInTheDocument();
    const second = screen.getByRole("region", { name: /^\/srv\/b/ });
    expect(within(second).getByRole("button", { name: "第二工作区技能" })).toBeInTheDocument();
    expect(within(second).getByText("/srv/b")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "别处技能" })).toBeNull();
    // The counter divides the population it is drawn over, so it counts the
    // filtered catalog rather than everything the scan found.
    expect(screen.getByText("1 / 3 个已选")).toBeInTheDocument();
    unmount();

    // A draft with no project yet sees the global level alone.
    render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
        workspaces={[]}
      />
    );
    await openPage(user, /^技能/);
    expect(screen.getByRole("button", { name: "代码审查" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "本工作区技能" })).toBeNull();
    expect(screen.queryByRole("button", { name: "别处技能" })).toBeNull();
  });

  it("shows the whole catalog in a preset window, which belongs to no workspace", async () => {
    const seed = createSeedDocument();
    seed.capabilities.skills.push({
      id: "skill_elsewhere",
      name: "别处技能",
      description: "另一工作区的技能。",
      location: "/work/c/.mewrk/skills/else/SKILL.md",
      source: "workspace",
      available: true,
      workspaceKey: "local|/work/c"
    });
    const user = userEvent.setup();
    render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
        workspaces={WORKSPACE_A}
      />
    );

    await openPage(user, /^对话预设/);
    await user.click(screen.getByRole("button", { name: "打开预设 默认" }));
    const dialog = screen.getByRole("dialog", { name: "默认" });
    const nestedNav = within(dialog).getByRole("navigation", { name: "对话设置分类" });
    await user.click(within(nestedNav).getByRole("button", { name: /^技能/ }));

    // A preset is reusable and points at no workspace, so it must see the whole
    // catalog rather than inherit the conversation's narrowing.
    expect(within(dialog).getByRole("button", { name: "别处技能" })).toBeInTheDocument();
  });

  it("rescans on mount and from the toolbar of every capability page", async () => {
    const seed = createSeedDocument();
    const onRescanCapabilities = vi.fn();
    const user = userEvent.setup();
    render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
        onRescanCapabilities={onRescanCapabilities}
      />
    );

    // Opening the pane is one of the moments discovery runs.
    expect(onRescanCapabilities).toHaveBeenCalledTimes(1);

    await openPage(user, /^技能/);
    await user.click(screen.getByRole("button", { name: "重新扫描" }));
    expect(onRescanCapabilities).toHaveBeenCalledTimes(2);

    await openPage(user, /^MCP/);
    await user.click(screen.getByRole("button", { name: "重新扫描" }));
    expect(onRescanCapabilities).toHaveBeenCalledTimes(3);

    await openPage(user, /^钩子/);
    await user.click(screen.getByRole("button", { name: "重新扫描" }));
    expect(onRescanCapabilities).toHaveBeenCalledTimes(4);
  });

  it("opens the global or this workspace's config folder from a capability page", async () => {
    const seed = createSeedDocument();
    const onRevealCapabilityLocation = vi.fn();
    const user = userEvent.setup();
    const { unmount } = render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
        workspaces={WORKSPACE_A}
        onRevealCapabilityLocation={onRevealCapabilityLocation}
      />
    );

    await openPage(user, /^技能/);
    await user.click(screen.getByRole("button", { name: "打开全局配置目录" }));
    expect(onRevealCapabilityLocation).toHaveBeenLastCalledWith("skills", null);
    await user.click(screen.getByRole("button", { name: "打开 /work/a 的配置目录" }));
    expect(onRevealCapabilityLocation).toHaveBeenLastCalledWith("skills", WORKSPACE_A[0].key);

    await openPage(user, /^MCP/);
    await user.click(screen.getByRole("button", { name: "打开 /work/a 的配置目录" }));
    // The kind travels with the click: the host opens the directory that kind lives in.
    expect(onRevealCapabilityLocation).toHaveBeenLastCalledWith("mcp", WORKSPACE_A[0].key);
    unmount();

    // A draft with no workspace has only the global level to open.
    render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
        workspaces={[]}
        onRevealCapabilityLocation={onRevealCapabilityLocation}
      />
    );
    await openPage(user, /^钩子/);
    expect(screen.getByRole("button", { name: "打开全局配置目录" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /的配置目录$/ })).toBeNull();
  });

  it("tests an MCP server's connection in the row and reports what came back", async () => {
    const seed = createSeedDocument();
    seed.capabilities.mcps.push({
      id: "mcp_dead",
      name: "Dead Server",
      description: "sse is not supported",
      location: "test://mcp/dead",
      source: "user",
      available: false
    });
    const passing: McpProbeReport = {
      ok: true,
      protocolVersion: "2025-06-18",
      serverName: "files",
      serverVersion: "1.2.3",
      tools: [{
        name: "read",
        title: "",
        description: "",
        requiresUserInteraction: false,
        inputSchema: null
      }],
      prompts: [],
      resources: [],
      logs: [],
      error: ""
    };
    let resolveProbe!: (report: McpProbeReport) => void;
    const onProbeMcpServer = vi.fn(() => new Promise<McpProbeReport>((resolve) => {
      resolveProbe = resolve;
    }));
    const user = userEvent.setup();
    const { unmount } = render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
        onProbeMcpServer={onProbeMcpServer}
      />
    );

    await openPage(user, /^MCP/);
    const row = screen.getByText("Workspace Files").closest(".catalog-row") as HTMLElement;
    await user.click(within(row).getByRole("button", { name: "测试连接 Workspace Files" }));
    expect(onProbeMcpServer).toHaveBeenCalledWith(expect.objectContaining({ id: "mcp_workspace" }));
    // While the test is out, the row says so and the button holds.
    expect(within(row).getByText("测试中…")).toBeInTheDocument();
    expect(within(row).getByRole("button", { name: "测试连接 Workspace Files" })).toBeDisabled();

    await act(async () => { resolveProbe(passing); });
    // The row is one line, so the count is the badge and who answered is the tooltip.
    expect(await within(row).findByText("1 个工具")).toBeInTheDocument();
    expect(within(row).queryByText("测试中…")).toBeNull();
    expect(row).toHaveAttribute("title", expect.stringContaining("files 1.2.3"));

    // An unavailable server is not probed: it already carries the reason it is inert.
    const dead = screen.getByText("Dead Server").closest(".catalog-row") as HTMLElement;
    expect(within(dead).queryByRole("button", { name: "测试连接 Dead Server" })).toBeNull();
    expect(within(dead).getByText("不可用")).toBeInTheDocument();
    unmount();

    const failing = vi.fn(async (): Promise<McpProbeReport> => ({
      ok: false,
      protocolVersion: "",
      serverName: "",
      serverVersion: "",
      tools: [],
      prompts: [],
      resources: [],
      logs: ["connecting", "spawn ENOENT"],
      error: "spawn failed"
    }));
    render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
        onProbeMcpServer={failing}
      />
    );

    await openPage(user, /^MCP/);
    const failedRow = screen.getByText("Workspace Files").closest(".catalog-row") as HTMLElement;
    await user.click(within(failedRow).getByRole("button", { name: "测试连接 Workspace Files" }));
    expect(await within(failedRow).findByText("连接失败")).toBeInTheDocument();
    // The reason, then the last lines the server wrote while it tried.
    expect(failedRow).toHaveAttribute("title", expect.stringContaining("spawn failed"));
    expect(failedRow).toHaveAttribute("title", expect.stringContaining("spawn ENOENT"));
  });

  it("says why a remote workspace's entries are missing when its machine could not be read", async () => {
    const seed = createSeedDocument();
    const user = userEvent.setup();
    render(
      <SettingsHarness
        initialConversation={seed.workspaces[0].conversations[0]}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={{
          ...seed.capabilities,
          unreadableLevels: [{ workspaceKey: WORKSPACE_A[0].key, message: "devbox 没有响应" }]
        }}
        onSettingsChange={vi.fn()}
        workspaces={WORKSPACE_A}
      />
    );
    await openPage(user, /^技能/);
    expect(screen.getByText("devbox 没有响应")).toBeInTheDocument();
    await openPage(user, /^钩子/);
    expect(screen.getByText("devbox 没有响应")).toBeInTheDocument();
  });

  it("says a dangling skill or hook fails every run", async () => {
    const seed = createSeedDocument();
    const conversation = seed.workspaces[0].conversations[0];
    const user = userEvent.setup();
    render(
      <SettingsHarness
        initialConversation={{
          ...conversation,
          settings: {
            ...conversation.settings,
            skillIds: ["skill_code_review", "skill_gone"],
            hookIds: ["hook_gone"]
          }
        }}
        globalSettings={seed.globalSettings}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={vi.fn()}
        workspaces={WORKSPACE_A}
      />
    );

    await openPage(user, /^技能/);
    const skillDangling = screen.getByRole("button", { name: "skill_gone" })
      .closest(".catalog-row") as HTMLElement;
    expect(skillDangling).toHaveAttribute("title", expect.stringContaining("每次运行都会失败"));

    await openPage(user, /^钩子/);
    const hookDangling = screen.getByRole("button", { name: "hook_gone" })
      .closest(".catalog-row") as HTMLElement;
    expect(hookDangling).toHaveAttribute("title", expect.stringContaining("每次运行都会失败"));
  });

  it("unticks a dangling MCP row on a model that cannot append tools, from the lock as well, without asking", async () => {
    const seed = createSeedDocument();
    const noAppend = withNonAppendModel(seed.globalSettings);
    const conversation = seed.workspaces[0].conversations[0];
    const onSettingsChange = vi.fn();
    const user = userEvent.setup();
    render(
      <SettingsHarness
        initialConversation={{
          ...conversation,
          settings: {
            ...conversation.settings,
            mcpIds: ["mcp_gone"],
            // Warm on a model whose whole surface is orange: a live server in
            // this spot would be toned and would ask.
            toolLock: lockFor(noAppend, { mcpIds: ["mcp_gone"] })
          }
        }}
        globalSettings={noAppend}
        tools={seed.tools}
        capabilities={seed.capabilities}
        onSettingsChange={onSettingsChange}
      />
    );

    await openPage(user, /^MCP/);
    const dangling = screen.getByRole("button", { name: "mcp_gone" });
    expect(dangling).toBeEnabled();
    expect(dangling).toHaveAttribute("aria-pressed", "true");
    // The entry it named has nothing left to declare, so no tone and no lock.
    expect(dangling.closest(".catalog-row")).not.toHaveClass("catalog-row--cache");
    expect(dangling.closest(".catalog-row")?.querySelector(".lock-mark")).toBeNull();
    await user.click(dangling);
    expect(screen.queryByRole("dialog")).toBeNull();
    const written = onSettingsChange.mock.lastCall?.[0] as ConversationSettingsType;
    expect(written.mcpIds).toEqual([]);
    expect(written.toolLock?.mcpIds).toEqual([]);
  });
});
