import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { configureI18n } from "../i18n";
import { defaultAgentRoleWebSearch } from "../lib/agentRoles";
import { emptyConversationPresetSettings } from "../lib/conversationPresets";
import { defaultConversationWebSearchSettings, type SaveAgentRoleTarget } from "../lib/runtime";
import type { CapabilityWorkspace } from "../lib/workspaces";
import { createTestDocument } from "../test/fixtures";
import type {
  AgentRole,
  AgentRoleResource,
  ApiProvider,
  CapabilityCatalog,
  ContextItem,
  ConversationPreset,
  ConversationSettings,
  ConversationTemplateSummary,
  GlobalSettings,
  ResourceDescriptor,
  ToolDescriptor,
  WebSearchAssets
} from "../types";
import { forgetAgentRoleDraft } from "./AgentRoleEditor";
import { AgentRolesPage } from "./ConversationSettingsPages";

const exactProviderId = "provider:/精确";
const exactModelId = "kimi/vision:v4-模型";
const WORKSPACE_KEY = "local|/work/a";
const WORKSPACE_A: CapabilityWorkspace[] = [{ number: 1, key: WORKSPACE_KEY, machine: null, path: "/work/a" }];

const providers: ApiProvider[] = [{
  id: exactProviderId,
  name: "Exact Provider",
  enabled: true,
  familySettings: {},
  notes: "",
  family: "openai_chat",
  baseUrl: "https://example.invalid/v1",
  activeModelId: exactModelId,
  models: [{
    id: exactModelId,
    name: "",
    group: "",
    capabilities: ["image_recognition"],
    reasoningContent: "plaintext",
    promptCache: true
  }, {
    id: "second-model",
    name: "",
    group: "",
    capabilities: [],
    reasoningContent: "plaintext",
    promptCache: true
  }]
}, {
  // A Messages provider: the one family that spells its native web tools with a
  // version, so a role bound to it is offered the versions.
  id: "messages-provider",
  name: "Messages Provider",
  enabled: true,
  familySettings: {},
  notes: "",
  family: "anthropic",
  baseUrl: "https://messages.invalid/v1",
  activeModelId: "messages-model",
  models: [{
    id: "messages-model",
    name: "",
    group: "",
    capabilities: [],
    reasoningContent: "plaintext",
    promptCache: true
  }]
}, {
  id: "disabled-provider",
  name: "Disabled Provider",
  enabled: false,
  familySettings: {},
  notes: "",
  family: "openai_chat",
  baseUrl: "https://disabled.invalid/v1",
  activeModelId: "hidden-model",
  models: [{
    id: "hidden-model",
    name: "",
    group: "",
    capabilities: [],
    reasoningContent: "plaintext",
    promptCache: true
  }]
}];

const tools: ToolDescriptor[] = [{
  name: "read_file",
  label: "读取文件",
  description: "",
  category: "filesystem",
  dangerous: false,
  parameters: []
}, {
  name: "run_command",
  label: "运行命令",
  description: "",
  category: "shell",
  dangerous: true,
  parameters: []
}, {
  // The editor must exclude every orchestration tool, not just `workflow`.
  // These names ensure the test detects category-based filtering.
  name: "agent_spawn",
  label: "子代理工具（不应出现）",
  description: "",
  category: "orchestration",
  dangerous: false,
  parameters: []
}, {
  name: "task_wait",
  label: "等待任务工具（不应出现）",
  description: "",
  category: "orchestration",
  dangerous: false,
  parameters: []
}, {
  name: "workflow",
  label: "工作流工具（不应出现）",
  description: "",
  category: "orchestration",
  dangerous: false,
  parameters: []
}, {
  name: "read_global_memory",
  label: "读取全局记忆（不应出现）",
  description: "",
  category: "memory",
  dangerous: false,
  parameters: []
}];

// A catalog row the role editor can actually offer: the pickers list only
// providers that are switched on.
const webSearchAssets: WebSearchAssets = {
  providers: [{
    kind: "tavily",
    enabled: true,
    searchApiHost: "",
    fetchApiHost: "",
    engines: [],
    basicAuthUsername: ""
  }, {
    // Tavily searches but cannot fetch, so the fetch picker needs a row of its
    // own to offer — the two legs list different catalogues.
    kind: "jina",
    enabled: true,
    searchApiHost: "",
    fetchApiHost: "",
    engines: [],
    basicAuthUsername: ""
  }]
};

/* A role template body never travels with the role: the host stores it and
   hands back the id it landed under. These stand in for that store. */
const readTemplate = vi.fn(async (_templateId: string): Promise<ContextItem[]> => []);
const writeTemplate = vi.fn(async (templateId: string): Promise<string> => templateId || "template_minted");

/** The host's `save_agent_role`, answering with the id the file landed under. */
function saveRoleMock(id: string) {
  return vi.fn(async (_target: SaveAgentRoleTarget, _role: AgentRole): Promise<string> => id);
}

function roleBody(overrides: Partial<AgentRole> = {}): AgentRole {
  return {
    name: "reviewer",
    description: "",
    modelSelection: { kind: "inherit" },
    effort: null,
    tools: ["run_command"],
    disallowedTools: [],
    skillIds: [],
    mcpIds: [],
    hookIds: [],
    webSearch: defaultAgentRoleWebSearch(),
    templateId: null,
    ...overrides
  };
}

function roleResource(
  id: string,
  role: AgentRole | null,
  overrides: Partial<AgentRoleResource> = {}
): AgentRoleResource {
  return {
    id,
    name: role?.name ?? id,
    description: role?.description ?? "无法读取这个文件。",
    location: `/home/me/.mewrk/agents/${id}.json`,
    source: "user",
    available: role !== null,
    role,
    ...overrides
  };
}

const builtinOpus = roleResource("agent_builtin_opus", roleBody({
  name: "Opus",
  description: "内置角色的说明。",
  modelSelection: { kind: "explicit", providerId: exactProviderId, modelId: exactModelId },
  effort: "medium",
  tools: ["read_file", "run_command"]
}), { source: "builtin", location: "builtin:agents/opus.json" });

const globalReviewer = roleResource("agent_user_reviewer", roleBody());

const workspaceRole = roleResource("agent_workspace_local", roleBody({ name: "本地审查" }), {
  source: "workspace",
  workspaceKey: WORKSPACE_KEY,
  location: "/work/a/.mewrk/agents/local.json"
});

/** Global and workspace entries of every kind, and the three kinds of role. */
function catalogWith(agents: AgentRoleResource[] = [builtinOpus, globalReviewer, workspaceRole]): CapabilityCatalog {
  const row = (id: string, name: string, workspaceKey?: string): ResourceDescriptor => ({
    id,
    name,
    description: "",
    location: workspaceKey ? `/work/a/.mewrk/${id}` : `/home/me/.mewrk/${id}`,
    source: workspaceKey ? "workspace" : "user",
    available: true,
    ...(workspaceKey ? { workspaceKey } : {})
  });
  return {
    skills: [row("skill_a", "技能甲"), row("skill_ws", "工作区技能", WORKSPACE_KEY)],
    mcps: [row("mcp_docs", "文档服务器"), row("mcp_ws", "工作区服务器", WORKSPACE_KEY)],
    hooks: [row("hook_lint", "Lint 钩子"), row("hook_ws", "工作区钩子", WORKSPACE_KEY)],
    toolDescriptionFiles: [],
    agents
  };
}

/* The conversation the roles page is opened in. Everything tool-like here is
   deliberately different from every role's own answer: the editor must never
   draw any of it. */
function conversationSettings(overrides: Partial<ConversationSettings> = {}): ConversationSettings {
  const base = createTestDocument().workspaces[0].conversations[0].settings;
  return {
    ...base,
    enabledTools: ["read_file"],
    skillIds: ["skill_a"],
    mcpIds: ["mcp_docs"],
    hookIds: ["hook_lint"],
    webSearch: {
      ...defaultConversationWebSearchSettings(),
      provider: { kind: "explicit", providerKind: "tavily" },
      fetchProvider: { kind: "explicit", providerKind: "jina" },
      domainFilter: "include",
      maxResults: 9
    },
    agentIds: ["agent_builtin_opus", "agent_user_reviewer"],
    allowRolelessSubagents: false,
    ...overrides
  };
}

function globalSettings(): GlobalSettings {
  const seed = createTestDocument().globalSettings;
  return {
    ...seed,
    apiProviders: providers,
    activeProviderId: exactProviderId,
    webSearch: webSearchAssets
  };
}

interface HarnessProps {
  catalog: CapabilityCatalog;
  initialSettings?: ConversationSettings;
  onSettingsChange?: (patch: Partial<ConversationSettings>) => void;
  onSaveRole?: (target: SaveAgentRoleTarget, role: AgentRole) => Promise<string>;
  onDelete?: (resource: AgentRoleResource) => Promise<boolean> | undefined;
  workspaces?: CapabilityWorkspace[];
  presets?: ConversationPreset[];
  templates?: ConversationTemplateSummary[];
}

/** The roles page as the conversation pane draws it, holding its own settings. */
function RolesHarness({
  catalog,
  initialSettings = conversationSettings(),
  onSettingsChange = () => undefined,
  onSaveRole = async () => "agent_user_saved",
  onDelete,
  workspaces,
  presets = [],
  templates = []
}: HarnessProps) {
  const [settings, setSettings] = useState(initialSettings);
  return (
    <AgentRolesPage
      listId="roles-test"
      settings={settings}
      globalSettings={globalSettings()}
      roleTools={tools}
      catalog={catalog}
      templates={templates}
      presets={presets}
      onReadTemplate={readTemplate}
      onWriteTemplate={writeTemplate}
      workspaces={workspaces}
      onDelete={onDelete}
      onSaveRole={onSaveRole}
      onChange={(patch) => {
        onSettingsChange(patch);
        setSettings((current) => ({ ...current, ...patch }));
      }}
    />
  );
}

function renderRoles(props: Partial<HarnessProps> = {}) {
  const onSettingsChange = props.onSettingsChange ?? vi.fn();
  const onSaveRole = props.onSaveRole ?? saveRoleMock("agent_user_saved");
  const view = render(
    <RolesHarness
      catalog={props.catalog ?? catalogWith()}
      {...props}
      // Undefined is a preset's window, so it has to be asked for by name.
      workspaces={"workspaces" in props ? props.workspaces : WORKSPACE_A}
      onSettingsChange={onSettingsChange}
      onSaveRole={onSaveRole}
    />
  );
  return { ...view, onSettingsChange, onSaveRole };
}

/** Opens one of the role window's pages from its rail. */
async function openPage(
  user: ReturnType<typeof userEvent.setup>,
  dialog: HTMLElement,
  page: RegExp
) {
  const rail = within(dialog).getByRole("navigation", { name: "角色设置分类" });
  await user.click(within(rail).getByRole("button", { name: page }));
}

function railEntries(dialog: HTMLElement): Array<string | null> {
  const rail = within(dialog).getByRole("navigation", { name: "角色设置分类" });
  return within(rail).getAllByRole("button").map((button) => button.textContent);
}

async function openRole(
  user: ReturnType<typeof userEvent.setup>,
  name: string,
  builtin = false
): Promise<HTMLElement> {
  await user.click(screen.getByRole("button", { name: `${builtin ? "设置内置角色" : "设置角色"} ${name}` }));
  return screen.getByRole("dialog", { name });
}

async function openCreate(user: ReturnType<typeof userEvent.setup>): Promise<HTMLElement> {
  await user.click(screen.getByRole("button", { name: "新建角色" }));
  return screen.getByRole("dialog", { name: "新建角色" });
}

function templateSummary(id: string, messageCount: number): ConversationTemplateSummary {
  return { id, name: "", messageCount, createdAt: "", updatedAt: "" };
}

function userMessage(id: string, content: string): ContextItem {
  return { id, kind: "user", content, createdAt: "2026-01-01T00:00:00.000Z" };
}

afterEach(() => {
  cleanup();
  // The editor's drafts are module state on purpose; each case starts clean.
  for (const id of ["new:global", `new:${WORKSPACE_KEY}`, builtinOpus.id, globalReviewer.id, workspaceRole.id]) {
    forgetAgentRoleDraft(id);
  }
  readTemplate.mockReset();
  readTemplate.mockImplementation(async () => []);
  writeTemplate.mockClear();
  configureI18n("zh-CN");
});

/** The pick rows' names, in order: a catalog row is picked by a pressed button named after the entry. */
const toggleNames = (root: HTMLElement) =>
  [...root.querySelectorAll(".catalog-row__toggle")].map((row) => row.getAttribute("aria-label"));

describe("AgentRolesPage", () => {
  it("lists the roles as a catalog: built-ins and global files first, then each workspace", () => {
    // Listed workspace-first on purpose: the page groups by level, it does not
    // follow the catalog's order across levels.
    renderRoles({ onDelete: vi.fn(), catalog: catalogWith([workspaceRole, builtinOpus, globalReviewer]) });
    // The levels in reading order, the global one before the workspace.
    expect(screen.getAllByRole("region").map((region) => region.getAttribute("aria-label")))
      .toEqual(["全局", "/work/a"]);
    const global = screen.getByRole("region", { name: "全局" });
    // Within a level, the catalog's own order: the built-in, then the global file.
    expect(toggleNames(global))
      .toEqual(["Opus（内置）", "reviewer"]);
    expect(within(global).getByRole("button", { name: "Opus（内置）" })).toHaveAttribute("aria-pressed", "true");
    expect(within(global).getByRole("button", { name: "reviewer" })).toHaveAttribute("aria-pressed", "true");
    const workspace = screen.getByRole("region", { name: "/work/a" });
    expect(toggleNames(workspace))
      .toEqual(["本地审查"]);
    expect(within(workspace).getByRole("button", { name: "本地审查" })).toHaveAttribute("aria-pressed", "false");
    // The one built-in says so; the files the user wrote do not.
    expect(within(global).getAllByText("内置")).toHaveLength(1);
    expect(within(workspace).queryByText("内置")).toBeNull();
    // A built-in has no file of its own to remove; a user's file does.
    expect(screen.queryByRole("button", { name: "删除 Opus" })).toBeNull();
    expect(screen.getByRole("button", { name: "删除 reviewer" })).toBeInTheDocument();
  });

  it("tells a built-in role from a global copy of it by a badge and by every control's name", () => {
    // What the catalog lists once Opus has been saved as a global role.
    const copy = roleResource("agent_user_opus", roleBody({ name: "Opus" }));
    renderRoles({ catalog: catalogWith([builtinOpus, copy]), onDelete: vi.fn() });
    const builtinRow = screen.getByRole("button", { name: "Opus（内置）" }).closest(".catalog-row") as HTMLElement;
    const copyRow = screen.getByRole("button", { name: "Opus" }).closest(".catalog-row") as HTMLElement;
    expect(builtinRow).not.toBe(copyRow);
    expect(within(builtinRow).getByText("内置")).toBeInTheDocument();
    expect(within(copyRow).queryByText("内置")).toBeNull();
    // No two controls on the page share a name.
    expect(within(builtinRow).getByRole("button", { name: "设置内置角色 Opus" })).toBeInTheDocument();
    expect(within(copyRow).getByRole("button", { name: "设置角色 Opus" })).toBeInTheDocument();
    expect(within(builtinRow).queryByRole("button", { name: /^删除/ })).toBeNull();
    expect(within(copyRow).getByRole("button", { name: "删除 Opus" })).toBeInTheDocument();
    const names = toggleNames(document.body);
    expect(new Set(names).size).toBe(names.length);
  });

  it("selects a role by id, the way a skill is selected", async () => {
    const user = userEvent.setup();
    const { onSettingsChange } = renderRoles();
    await user.click(screen.getByRole("button", { name: "本地审查" }));
    expect(onSettingsChange).toHaveBeenLastCalledWith({
      agentIds: ["agent_builtin_opus", "agent_user_reviewer", "agent_workspace_local"]
    });
    await user.click(screen.getByRole("button", { name: "Opus（内置）" }));
    expect(onSettingsChange).toHaveBeenLastCalledWith({
      agentIds: ["agent_user_reviewer", "agent_workspace_local"]
    });
  });

  /* A role row is picked the way a tool row is: the row itself is the control —
     a pressed button named after the role, no switch — and what is done to the
     role itself sits apart from it and leaves the selection alone. */
  it("picks a role like a tool row, apart from the role's own actions", async () => {
    const user = userEvent.setup();
    const onSettingsChange = vi.fn();
    renderRoles({ onSettingsChange });
    expect(screen.queryByRole("switch", { name: "reviewer" })).toBeNull();
    const toggle = screen.getByRole("button", { name: "reviewer" });
    expect(toggle).toHaveAttribute("aria-pressed", "true");
    expect(toggle.closest(".catalog-row")).toHaveClass("catalog-row--pick", "catalog-row--on");

    await user.click(toggle);
    expect(onSettingsChange).toHaveBeenLastCalledWith({
      agentIds: ["agent_builtin_opus"]
    });
    expect(screen.getByRole("button", { name: "reviewer" })).toHaveAttribute("aria-pressed", "false");
    expect(screen.getByRole("button", { name: "reviewer" }).closest(".catalog-row")).not.toHaveClass("catalog-row--on");

    const calls = onSettingsChange.mock.calls.length;
    await user.click(screen.getByRole("button", { name: "设置角色 reviewer" }));
    expect(onSettingsChange).toHaveBeenCalledTimes(calls);
    expect(screen.getByRole("dialog", { name: "reviewer" })).toBeInTheDocument();
  });

  it("puts each role's model in its tooltip and marks one whose model is gone", () => {
    renderRoles({
      catalog: catalogWith([
        roleResource("agent_user_live", roleBody({
          name: "live-role",
          modelSelection: { kind: "explicit", providerId: exactProviderId, modelId: exactModelId }
        })),
        roleResource("agent_user_gone", roleBody({
          name: "gone-role",
          modelSelection: { kind: "explicit", providerId: exactProviderId, modelId: "removed-model" }
        })),
        roleResource("agent_user_orphan", roleBody({
          name: "orphan-role",
          modelSelection: { kind: "unavailable" }
        })),
        roleResource("agent_user_inherit", roleBody({ name: "inheriting" }))
      ])
    });
    const rowOf = (name: string) => screen.getByRole("button", { name }).closest(".catalog-row") as HTMLElement;
    expect(rowOf("live-role")).toHaveAttribute("title", expect.stringContaining(`Exact Provider · ${exactModelId}`));
    expect(rowOf("gone-role")).toHaveAttribute(
      "title",
      expect.stringContaining("Exact Provider · removed-model · 模型暂时取不到，模型看不到这个角色")
    );
    expect(rowOf("orphan-role")).toHaveAttribute("title", expect.stringContaining("没有可用的模型"));
    expect(rowOf("inheriting")).toHaveAttribute("title", expect.stringContaining("跟随对话模型"));
    // An unresolvable binding is uncallable, and the row has to say so or it
    // reads as a normal row.
    expect(within(rowOf("gone-role")).getByText("模型不可用")).toBeInTheDocument();
    expect(within(rowOf("orphan-role")).getByText("模型不可用")).toBeInTheDocument();
    expect(within(rowOf("live-role")).queryByText("模型不可用")).toBeNull();
  });

  it("offers no settings for a file that could not be read, only its reason", () => {
    renderRoles({ catalog: catalogWith([roleResource("agent_user_broken", null, { name: "broken" })]) });
    const row = screen.getByRole("button", { name: "broken" }).closest(".catalog-row") as HTMLElement;
    expect(within(row).getByText("不可用")).toBeInTheDocument();
    expect(row).toHaveAttribute("title", expect.stringContaining("无法读取这个文件。"));
    expect(screen.queryByRole("button", { name: "设置角色 broken" })).toBeNull();
  });

  it("deletes a role's file on the second click, and forgets its draft", async () => {
    const user = userEvent.setup();
    const onDelete = vi.fn();
    renderRoles({ onDelete });

    // A draft left on the role first.
    const dialog = await openRole(user, "reviewer");
    await user.type(within(dialog).getByRole("textbox", { name: "子代理描述" }), "草稿");
    await user.click(within(dialog).getByRole("button", { name: "关闭" }));

    await user.click(screen.getByRole("button", { name: "删除 reviewer" }));
    expect(onDelete).not.toHaveBeenCalled();
    await user.click(screen.getByRole("button", { name: "确认删除 reviewer" }));
    expect(onDelete).toHaveBeenCalledWith(globalReviewer);

    const reopened = await openRole(user, "reviewer");
    expect(within(reopened).getByRole("textbox", { name: "子代理描述" })).toHaveValue("");
  });

  it("keeps a role's draft when the delete did not go through, and forgets it when it did", async () => {
    const user = userEvent.setup();
    const onDelete = vi.fn(async (_resource: AgentRoleResource): Promise<boolean> => false);
    renderRoles({ onDelete });

    const dialog = await openRole(user, "reviewer");
    await user.type(within(dialog).getByRole("textbox", { name: "子代理描述" }), "草稿");
    await user.click(within(dialog).getByRole("button", { name: "关闭" }));

    // The host refused: the file is still there, and so is the half-written edit.
    await user.click(screen.getByRole("button", { name: "删除 reviewer" }));
    await user.click(screen.getByRole("button", { name: "确认删除 reviewer" }));
    expect(onDelete).toHaveBeenCalledTimes(1);
    await act(async () => { await Promise.resolve(); });
    let reopened = await openRole(user, "reviewer");
    expect(within(reopened).getByRole("textbox", { name: "子代理描述" })).toHaveValue("草稿");
    await user.click(within(reopened).getByRole("button", { name: "关闭" }));

    // Then it went through: nothing is left for the draft to be a draft of.
    onDelete.mockResolvedValue(true);
    await user.click(screen.getByRole("button", { name: "删除 reviewer" }));
    await user.click(screen.getByRole("button", { name: "确认删除 reviewer" }));
    expect(onDelete).toHaveBeenCalledTimes(2);
    await act(async () => { await Promise.resolve(); });
    reopened = await openRole(user, "reviewer");
    expect(within(reopened).getByRole("textbox", { name: "子代理描述" })).toHaveValue("");
  });

  it("explains the role-less switch by whether a selected role can be called", async () => {
    const user = userEvent.setup();
    const { onSettingsChange, unmount } = renderRoles({
      initialSettings: conversationSettings({ agentIds: ["agent_user_missing"] })
    });
    expect(screen.getByText(/当前没有可用角色/)).toBeInTheDocument();
    unmount();

    renderRoles({ onSettingsChange });
    expect(screen.queryByText(/当前没有可用角色/)).toBeNull();
    await user.click(screen.getByRole("switch", { name: "角色必填" }));
    expect(onSettingsChange).toHaveBeenLastCalledWith({ allowRolelessSubagents: true });
  });

  it("does not count a selected role of a workspace this conversation does not have as one the model can call", () => {
    const selectsWorkspaceRole = conversationSettings({ agentIds: ["agent_workspace_local"] });
    // The conversation has no /work/a, so no run of it can reach that role.
    const { unmount } = renderRoles({ initialSettings: selectsWorkspaceRole, workspaces: [] });
    expect(screen.getByText(/当前没有可用角色/)).toBeInTheDocument();
    unmount();

    // With the workspace attached it is callable, and the switch is live.
    renderRoles({ initialSettings: selectsWorkspaceRole, workspaces: WORKSPACE_A });
    expect(screen.queryByText(/当前没有可用角色/)).toBeNull();
  });
});

describe("AgentRoleEditor", () => {
  it.each([
    ["zh-CN" as const, "新建角色", "角色名称", "保存角色", "角色名称不能为空。", "请先修正标记的字段。"],
    ["en-US" as const, "New role", "Role name", "Save role", "A role name is required.", "Fix the marked fields before saving."]
  ])("opens a create window that asks for the name only on save (%s)", async (
    language,
    createLabel,
    nameLabel,
    saveLabel,
    requiredHint,
    banner
  ) => {
    configureI18n(language);
    const user = userEvent.setup();
    const { onSaveRole } = renderRoles();

    await user.click(screen.getByRole("button", { name: createLabel }));
    const dialog = screen.getByRole("dialog", { name: createLabel });
    expect(within(dialog).getByRole("textbox", { name: nameLabel })).toHaveValue("");
    // A create window opens on a blank name, and complaining about a name the
    // user has not had a chance to type yet reads as a scold — so the complaint
    // is withheld until Save asks the question.
    expect(within(dialog).queryByText(requiredHint)).not.toBeInTheDocument();
    await user.click(within(dialog).getByRole("button", { name: saveLabel }));

    expect(within(dialog).getByText(requiredHint)).toBeInTheDocument();
    expect(within(dialog).getByText(banner)).toBeInTheDocument();
    expect(screen.getByRole("dialog", { name: createLabel })).toBeInTheDocument();
    expect(onSaveRole).not.toHaveBeenCalled();
  });

  it("lays the role out as a preset's window is: seven pages down a rail, the save at its foot", async () => {
    const user = userEvent.setup();
    renderRoles();
    const dialog = await openRole(user, "reviewer");
    const rail = within(dialog).getByRole("navigation", { name: "角色设置分类" });
    expect(dialog.querySelector(".settings-layout > .settings-nav")).toBe(rail);
    // The counts are the role's own lists — one tool, nothing else — not the
    // conversation's one tool, one skill, one server and one hook.
    expect(railEntries(dialog))
      .toEqual(["角色设置", "工具1", "高级工具", "技能0", "MCP0", "钩子0", "对话模板0", "保存角色"]);
    expect(within(rail).getByRole("button", { name: "保存角色" }).parentElement)
      .toBe(rail.lastElementChild);
    expect(within(dialog).getByRole("textbox", { name: "角色名称" })).toHaveValue("reviewer");
    expect(within(dialog).getByRole("combobox", { name: "执行模型" })).toHaveValue("inherit");
    // An existing role's file stays where it is: its location is read, not picked.
    expect(within(dialog).queryByRole("combobox", { name: "位置" })).toBeNull();
    expect(within(dialog).getByText(globalReviewer.location)).toBeInTheDocument();
  });

  it("creates a role at the level it is asked for and selects it in the conversation", async () => {
    const user = userEvent.setup();
    const onSaveRole = saveRoleMock("agent_workspace_new");
    const { onSettingsChange } = renderRoles({ onSaveRole });
    const dialog = await openCreate(user);

    const location = within(dialog).getByRole("combobox", { name: "位置" });
    // The global level, then each of the conversation's workspaces.
    expect(within(location).getAllByRole("option").map((option) => option.textContent))
      .toEqual(["全局 · ~/.mewrk/agents", "/work/a"]);
    await user.selectOptions(location, within(location).getByRole("option", { name: "/work/a" }));
    await user.type(within(dialog).getByRole("textbox", { name: "角色名称" }), "  security-reviewer ");
    await user.type(within(dialog).getByRole("textbox", { name: "子代理描述" }), "对抗式审查。");
    await user.selectOptions(
      within(dialog).getByRole("combobox", { name: "执行模型" }),
      within(dialog).getByRole("option", { name: `Exact Provider · ${exactModelId}` })
    );
    // Every model on an enabled provider is offered; only the disabled
    // provider's model stays out.
    expect(within(dialog).getByRole("option", { name: /second-model/ })).toBeInTheDocument();
    expect(within(dialog).queryByRole("option", { name: /hidden-model/ })).toBeNull();

    await user.click(within(dialog).getByRole("button", { name: "保存角色" }));

    // A new role's every tool-like answer is its own default, never the
    // conversation's: every tool the picker offers, no skills, servers or
    // hooks, and the built-in preset's web configuration.
    expect(onSaveRole).toHaveBeenCalledWith({ workspaceKey: WORKSPACE_KEY }, {
      name: "security-reviewer",
      description: "对抗式审查。",
      modelSelection: { kind: "explicit", providerId: exactProviderId, modelId: exactModelId },
      effort: null,
      tools: ["read_file", "run_command"],
      disallowedTools: [],
      skillIds: [],
      mcpIds: [],
      hookIds: [],
      webSearch: defaultAgentRoleWebSearch(),
      templateId: null
    });
    await waitFor(() => expect(onSettingsChange).toHaveBeenLastCalledWith({
      agentIds: ["agent_builtin_opus", "agent_user_reviewer", "agent_workspace_new"]
    }));
    expect(screen.queryByRole("dialog", { name: "新建角色" })).toBeNull();
  });

  it("offers a preset's window the global level alone", async () => {
    const user = userEvent.setup();
    renderRoles({ workspaces: undefined });
    const dialog = await openCreate(user);
    const location = within(dialog).getByRole("combobox", { name: "位置" });
    expect(within(location).getAllByRole("option").map((option) => option.textContent))
      .toEqual(["全局 · ~/.mewrk/agents"]);
  });

  it("overwrites an existing role's file in place and leaves the selection alone", async () => {
    const user = userEvent.setup();
    const onSaveRole = saveRoleMock("agent_user_reviewer");
    const { onSettingsChange } = renderRoles({ onSaveRole });
    const dialog = await openRole(user, "reviewer");

    await user.type(within(dialog).getByRole("textbox", { name: "子代理描述" }), "挑错。");
    await user.selectOptions(
      within(dialog).getByRole("combobox", { name: "思考程度" }),
      within(dialog).getByRole("option", { name: "high" })
    );
    await user.click(within(dialog).getByRole("button", { name: "保存角色" }));

    expect(onSaveRole).toHaveBeenCalledWith(
      { id: "agent_user_reviewer", workspaceKey: null },
      roleBody({ description: "挑错。", effort: "high" })
    );
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "reviewer" })).toBeNull());
    expect(onSettingsChange).not.toHaveBeenCalled();
  });

  it("says a rename keeps the file, and saves the new name into it", async () => {
    const user = userEvent.setup();
    const onSaveRole = saveRoleMock("agent_user_reviewer");
    renderRoles({ onSaveRole });
    const dialog = await openRole(user, "reviewer");
    const name = within(dialog).getByRole("textbox", { name: "角色名称" });
    await user.clear(name);
    await user.type(name, "critic");
    expect(within(dialog).getByText(/改名不会换文件/)).toBeInTheDocument();
    await user.click(within(dialog).getByRole("button", { name: "保存角色" }));
    expect(onSaveRole).toHaveBeenCalledWith(
      { id: "agent_user_reviewer", workspaceKey: null },
      expect.objectContaining({ name: "critic" })
    );
  });

  it("saves a built-in as a global copy that takes the built-in's place here", async () => {
    const user = userEvent.setup();
    const onSaveRole = saveRoleMock("agent_user_opus");
    const { onSettingsChange } = renderRoles({ onSaveRole });
    const dialog = await openRole(user, "Opus", true);

    expect(within(dialog).getByText(/内置角色随 Mewrk 版本更新，不能修改或删除/)).toBeInTheDocument();
    expect(within(dialog).getByText("内置角色，随 Mewrk 版本更新")).toBeInTheDocument();
    expect(within(dialog).queryByRole("button", { name: "保存角色" })).toBeNull();
    await user.selectOptions(
      within(dialog).getByRole("combobox", { name: "思考程度" }),
      within(dialog).getByRole("option", { name: "high" })
    );
    await user.click(within(dialog).getByRole("button", { name: "另存为全局角色" }));

    expect(onSaveRole).toHaveBeenCalledWith(
      { workspaceKey: null },
      { ...builtinOpus.role!, effort: "high" }
    );
    // The copy takes the built-in's slot rather than being appended beside it.
    await waitFor(() => expect(onSettingsChange).toHaveBeenLastCalledWith({
      agentIds: ["agent_user_opus", "agent_user_reviewer"]
    }));
  });

  it("refuses a name another role already has at the same level, and only there", async () => {
    const user = userEvent.setup();
    const onSaveRole = saveRoleMock("agent_workspace_reviewer");
    renderRoles({ onSaveRole });
    const dialog = await openCreate(user);
    await user.type(within(dialog).getByRole("textbox", { name: "角色名称" }), "reviewer");
    await user.click(within(dialog).getByRole("button", { name: "保存角色" }));
    expect(within(dialog).getByText("同一位置已有同名的角色。")).toBeInTheDocument();
    expect(onSaveRole).not.toHaveBeenCalled();

    // A workspace's own reviewer shadows the global one, which is allowed.
    const location = within(dialog).getByRole("combobox", { name: "位置" });
    await user.selectOptions(location, within(location).getByRole("option", { name: "/work/a" }));
    expect(within(dialog).queryByText("同一位置已有同名的角色。")).toBeNull();
    await user.click(within(dialog).getByRole("button", { name: "保存角色" }));
    expect(onSaveRole).toHaveBeenCalledWith({ workspaceKey: WORKSPACE_KEY }, expect.objectContaining({ name: "reviewer" }));
  });

  it("refuses to write over a file that changed since the window opened", async () => {
    const user = userEvent.setup();
    const onSaveRole = saveRoleMock("agent_user_reviewer");
    const onSettingsChange = vi.fn();
    const { rerender } = render(
      <RolesHarness
        catalog={catalogWith()}
        onSaveRole={onSaveRole}
        onSettingsChange={onSettingsChange}
        workspaces={WORKSPACE_A}
      />
    );
    const dialog = await openRole(user, "reviewer");
    await user.type(within(dialog).getByRole("textbox", { name: "子代理描述" }), "我的改动");

    // Someone else rewrites the file; the next rescan brings the new body in.
    const rewritten = roleResource("agent_user_reviewer", roleBody({ description: "别处的改动" }));
    rerender(
      <RolesHarness
        catalog={catalogWith([builtinOpus, rewritten, workspaceRole])}
        onSaveRole={onSaveRole}
        onSettingsChange={onSettingsChange}
        workspaces={WORKSPACE_A}
      />
    );
    await user.click(within(dialog).getByRole("button", { name: "保存角色" }));

    expect(within(dialog).getByRole("alert")).toHaveTextContent("此角色的文件已在别处变化。请关闭后重新打开再编辑。");
    expect(onSaveRole).not.toHaveBeenCalled();
  });

  it("shows the host's refusal and keeps the window open", async () => {
    const user = userEvent.setup();
    const onSaveRole = vi.fn(async (_target: SaveAgentRoleTarget, _role: AgentRole): Promise<string> => {
      throw new Error("角色名称不能为空");
    });
    renderRoles({ onSaveRole });
    const dialog = await openRole(user, "reviewer");
    await user.click(within(dialog).getByRole("button", { name: "保存角色" }));
    expect(await within(dialog).findByRole("alert")).toHaveTextContent("角色名称不能为空");
    expect(screen.getByRole("dialog", { name: "reviewer" })).toBeInTheDocument();
  });

  it("draws exactly the selectable tool catalogue over the role's own list, never the conversation's", async () => {
    const user = userEvent.setup();
    const onSaveRole = saveRoleMock("agent_user_reviewer");
    renderRoles({ onSaveRole });
    const dialog = await openRole(user, "reviewer");
    await openPage(user, dialog, /^工具/);

    // Orchestration and memory tools are absent rather than disabled: a child
    // template can hold neither, so a switch for one would promise nothing.
    expect(Array.from(dialog.querySelectorAll<HTMLElement>("[data-tool-name]"))
      .map((row) => row.dataset.toolName)).toEqual(["read_file", "run_command"]);
    expect(dialog.querySelector('[data-tool-category="orchestration"]')).toBeNull();
    // The role's own list: `run_command` on, and `read_file` off although the
    // conversation enables it.
    expect(within(dialog).getByRole("button", { name: "运行命令已启用" })).toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: "读取文件已关闭" })).toBeInTheDocument();
    expect(within(dialog).queryByText(/跟随对话/)).toBeNull();
    expect(dialog.querySelector(".settings-page-heading")).toHaveTextContent("这个角色能调用的工具。");

    await user.click(within(dialog).getByRole("button", { name: "读取文件已关闭" }));
    expect(railEntries(dialog)[1]).toBe("工具2");
    await user.click(within(dialog).getByRole("button", { name: "保存角色" }));
    expect(onSaveRole.mock.calls.at(-1)![1].tools).toEqual(["read_file", "run_command"]);
  });

  it("keeps names the picker cannot draw, without counting them", async () => {
    const user = userEvent.setup();
    const onSaveRole = saveRoleMock("agent_user_reviewer");
    renderRoles({
      onSaveRole,
      catalog: catalogWith([roleResource("agent_user_reviewer", roleBody({
        tools: ["agent_spawn", "read_file", "task_wait"]
      }))])
    });
    const dialog = await openRole(user, "reviewer");
    expect(railEntries(dialog)[1]).toBe("工具1");
    await openPage(user, dialog, /^工具/);
    await user.click(within(dialog).getByRole("button", { name: "运行命令已关闭" }));
    await user.click(within(dialog).getByRole("button", { name: "保存角色" }));
    expect(onSaveRole.mock.calls.at(-1)![1].tools)
      .toEqual(["agent_spawn", "read_file", "run_command", "task_wait"]);
  });

  it("draws the preset's web section for the role's own configuration, with no answer that follows the caller", async () => {
    const user = userEvent.setup();
    const onSaveRole = saveRoleMock("agent_user_reviewer");
    renderRoles({ onSaveRole });
    const dialog = await openRole(user, "reviewer");
    await openPage(user, dialog, /^高级工具/);

    // What belongs to the calling conversation is not on this page.
    expect(within(dialog).queryByRole("switch", { name: /联网搜索已/ })).toBeNull();
    expect(within(dialog).queryByRole("switch", { name: /记忆/ })).toBeNull();
    expect(within(dialog).queryByRole("combobox", { name: "工具描述" })).toBeNull();
    expect(within(dialog).queryByRole("radiogroup", { name: "宿主消息容器" })).toBeNull();
    // The role's own answers — native on both legs, its own shaping and no
    // filter — not the conversation's Tavily, Jina, 9 results and allowlist.
    expect(within(dialog).getByRole("button", { name: "搜索提供商：原生" })).toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: "抓取提供商：原生" })).toBeInTheDocument();
    // A native search has no count of its own to send, so no row asks for one.
    expect(within(dialog).queryByRole("spinbutton", { name: "结果数" })).toBeNull();
    const filter = within(dialog).getByRole("combobox", { name: "域名过滤" });
    expect(filter).toHaveValue("off");
    expect(within(filter).getAllByRole("option").map((option) => option.textContent))
      .toEqual(["启用黑名单", "启用白名单", "不启用"]);

    await user.click(within(dialog).getByRole("button", { name: /^搜索提供商：/ }));
    const search = screen.getByRole("menu", { name: "搜索提供商" });
    expect(within(search).getAllByRole("menuitemradio").map((row) => row.textContent))
      .toEqual(["原生", "Tavily", "Jina", "不启用"]);
    await user.click(within(search).getByRole("menuitemradio", { name: "Tavily" }));
    // Tavily takes a result count: the role's own, under the search selector.
    expect(within(dialog).getByRole("spinbutton", { name: "结果数" })).toHaveValue(5);
    await user.click(within(dialog).getByRole("button", { name: /^抓取提供商：/ }));
    const fetch = screen.getByRole("menu", { name: "抓取提供商" });
    expect(within(fetch).queryByRole("menuitemradio", { name: /跟随/ })).toBeNull();
    await user.click(within(fetch).getByRole("menuitemradio", { name: "Jina" }));
    // Jina Reader takes a per-page token cap, which is the role's own fetch cap.
    const fetchCompression = within(dialog).getByRole("spinbutton", { name: "抓取结果压缩" });
    expect(fetchCompression).toHaveValue(defaultAgentRoleWebSearch().fetchCompressionCutoff);
    await user.clear(fetchCompression);
    await user.type(fetchCompression, "750");
    await user.selectOptions(filter, within(filter).getByRole("option", { name: "启用白名单" }));
    await user.click(within(dialog).getByRole("button", { name: "保存角色" }));

    expect(onSaveRole.mock.calls.at(-1)![1].webSearch).toEqual({
      ...defaultAgentRoleWebSearch(),
      provider: { kind: "explicit", providerKind: "tavily" },
      fetchProvider: { kind: "explicit", providerKind: "jina" },
      fetchCompressionCutoff: 750,
      domainFilter: "include"
    });
  });

  it("speaks of the role's own model on the web page, not of the conversation's", async () => {
    const user = userEvent.setup();
    renderRoles();
    const dialog = await openRole(user, "reviewer");
    await openPage(user, dialog, /^高级工具/);

    expect(within(dialog).getByText(/^原生用这个角色所跑模型自带的搜索/)).toBeInTheDocument();
    expect(within(dialog).getByText(/^原生用这个角色所跑模型自带的抓取/)).toBeInTheDocument();
    // The conversation's wording is what a conversation's own page says.
    expect(within(dialog).queryByText(/^原生用模型自带的/)).toBeNull();
  });

  it("offers the native tool versions when the role's own model speaks Messages", async () => {
    const user = userEvent.setup();
    const onSaveRole = saveRoleMock("agent_user_reviewer");
    renderRoles({
      onSaveRole,
      catalog: catalogWith([roleResource("agent_user_reviewer", roleBody({
        modelSelection: { kind: "explicit", providerId: "messages-provider", modelId: "messages-model" },
        webSearch: { ...defaultAgentRoleWebSearch(), provider: { kind: "explicit", providerKind: "tavily" } }
      }))])
    });
    const dialog = await openRole(user, "reviewer");
    await openPage(user, dialog, /^高级工具/);

    await user.click(within(dialog).getByRole("button", { name: /^搜索提供商：/ }));
    const native = within(screen.getByRole("menu", { name: "搜索提供商" }))
      .getByRole("menuitemradio", { name: /^原生/ });
    expect(native).toHaveAttribute("aria-haspopup", "menu");
    await user.click(native);
    const versions = screen.getByRole("menu", { name: /^原生/ });
    await user.click(within(versions).getByRole("menuitemradio", { name: "web_search_20260209" }));
    await user.click(within(dialog).getByRole("button", { name: "保存角色" }));

    // Both writes of the one choice land: the backend and its version.
    expect(onSaveRole.mock.calls.at(-1)![1].webSearch).toMatchObject({
      provider: { kind: "native" },
      nativeSearchTool: "web_search_20260209"
    });
  });

  it("has no version to pick on a model of another protocol, even while the conversation's is Messages", async () => {
    const user = userEvent.setup();
    renderRoles({
      catalog: catalogWith([roleResource("agent_user_reviewer", roleBody({
        modelSelection: { kind: "explicit", providerId: exactProviderId, modelId: exactModelId }
      }))])
    });
    const dialog = await openRole(user, "reviewer");
    await openPage(user, dialog, /^高级工具/);
    await user.click(within(dialog).getByRole("button", { name: /^搜索提供商：/ }));
    expect(within(screen.getByRole("menu", { name: "搜索提供商" }))
      .getByRole("menuitemradio", { name: /^原生/ })).not.toHaveAttribute("aria-haspopup");
  });

  it("picks a global role's own skills, servers and hooks out of the global level alone", async () => {
    const user = userEvent.setup();
    const onSaveRole = saveRoleMock("agent_user_reviewer");
    renderRoles({ onSaveRole });
    const dialog = await openRole(user, "reviewer");
    await openPage(user, dialog, /^技能/);

    expect(dialog.querySelector(".settings-page-heading")).toHaveTextContent(/只能选全局/);
    // The role's own selection: nothing, although the conversation selects 技能甲.
    expect(within(dialog).getByRole("button", { name: "技能甲" })).toHaveAttribute("aria-pressed", "false");
    // A workspace's skill could not reach every conversation this role is
    // called from, so it is not offered at all.
    expect(within(dialog).queryByRole("button", { name: "工作区技能" })).toBeNull();
    expect(within(dialog).queryByRole("region", { name: "/work/a" })).toBeNull();
    const row = within(dialog).getByRole("button", { name: "技能甲" }).closest(".catalog-row");
    expect(row).not.toHaveClass("catalog-row--cache");
    await user.click(within(dialog).getByRole("button", { name: "技能甲" }));

    await openPage(user, dialog, /^钩子/);
    await user.click(within(dialog).getByRole("button", { name: "Lint 钩子" }));
    expect(railEntries(dialog).slice(3, 6)).toEqual(["技能1", "MCP0", "钩子1"]);
    await user.click(within(dialog).getByRole("button", { name: "保存角色" }));

    expect(onSaveRole.mock.calls.at(-1)![1]).toMatchObject({
      skillIds: ["skill_a"],
      mcpIds: [],
      hookIds: ["hook_lint"]
    });
  });

  it("lets a workspace role pick its own workspace's entries too", async () => {
    const user = userEvent.setup();
    renderRoles();
    const dialog = await openRole(user, "本地审查");
    expect(within(dialog).getByText(workspaceRole.location)).toBeInTheDocument();
    await openPage(user, dialog, /^技能/);
    expect(within(dialog).getByRole("button", { name: "技能甲" })).toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: "工作区技能" })).toBeInTheDocument();
  });

  it("drops a new role's workspace picks when it moves to the global level", async () => {
    const user = userEvent.setup();
    const onSaveRole = saveRoleMock("agent_user_new");
    renderRoles({ onSaveRole });
    const dialog = await openCreate(user);
    const location = within(dialog).getByRole("combobox", { name: "位置" });
    await user.selectOptions(location, within(location).getByRole("option", { name: "/work/a" }));
    await openPage(user, dialog, /^技能/);
    await user.click(within(dialog).getByRole("button", { name: "技能甲" }));
    await user.click(within(dialog).getByRole("button", { name: "工作区技能" }));
    await openPage(user, dialog, /^角色设置/);
    await user.selectOptions(
      within(dialog).getByRole("combobox", { name: "位置" }),
      within(dialog).getByRole("option", { name: "全局 · ~/.mewrk/agents" })
    );
    await user.type(within(dialog).getByRole("textbox", { name: "角色名称" }), "mover");
    await user.click(within(dialog).getByRole("button", { name: "保存角色" }));
    expect(onSaveRole).toHaveBeenCalledWith({ workspaceKey: null }, expect.objectContaining({ skillIds: ["skill_a"] }));
  });

  it("keeps a half-written role until it is saved, whichever conversation opens it", async () => {
    const user = userEvent.setup();
    renderRoles();
    let dialog = await openCreate(user);
    await user.type(within(dialog).getByRole("textbox", { name: "角色名称" }), "半成品");
    // Closing is not discarding: nothing has been written to a file either way.
    await user.click(within(dialog).getByRole("button", { name: "关闭" }));
    dialog = await openCreate(user);
    expect(within(dialog).getByRole("textbox", { name: "角色名称" })).toHaveValue("半成品");

    // A role belongs to no conversation, so another conversation's pane opens
    // the same draft.
    cleanup();
    renderRoles({ initialSettings: conversationSettings({ agentIds: [] }), workspaces: [] });
    dialog = await openCreate(user);
    expect(within(dialog).getByRole("textbox", { name: "角色名称" })).toHaveValue("半成品");
  });

  it("moves a half-written role with its level instead of leaving a copy at the old one", async () => {
    const user = userEvent.setup();
    renderRoles();
    let dialog = await openCreate(user);
    await user.type(within(dialog).getByRole("textbox", { name: "角色名称" }), "搬家");
    const location = within(dialog).getByRole("combobox", { name: "位置" });
    await user.selectOptions(location, within(location).getByRole("option", { name: "/work/a" }));
    await user.click(within(dialog).getByRole("button", { name: "关闭" }));

    // It comes back at the level it was left at, not at the one a window opens on.
    dialog = await openCreate(user);
    expect(within(dialog).getByRole("textbox", { name: "角色名称" })).toHaveValue("搬家");
    expect(within(dialog).getByRole("combobox", { name: "位置" })).toHaveValue(WORKSPACE_KEY);

    // Moved back and saved, nothing of it is left behind at either level.
    await user.selectOptions(
      within(dialog).getByRole("combobox", { name: "位置" }),
      within(dialog).getByRole("option", { name: "全局 · ~/.mewrk/agents" })
    );
    await user.click(within(dialog).getByRole("button", { name: "保存角色" }));
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "新建角色" })).toBeNull());
    dialog = await openCreate(user);
    expect(within(dialog).getByRole("textbox", { name: "角色名称" })).toHaveValue("");
    expect(within(dialog).getByRole("combobox", { name: "位置" })).toHaveValue("");
  });

  it("brings a half-written workspace role to a window without that workspace as a global one, without its picks", async () => {
    const user = userEvent.setup();
    renderRoles();
    let dialog = await openCreate(user);
    const location = within(dialog).getByRole("combobox", { name: "位置" });
    await user.selectOptions(location, within(location).getByRole("option", { name: "/work/a" }));
    await user.type(within(dialog).getByRole("textbox", { name: "角色名称" }), "半成品");
    // One global and one workspace entry of every kind.
    await openPage(user, dialog, /^技能/);
    await user.click(within(dialog).getByRole("button", { name: "技能甲" }));
    await user.click(within(dialog).getByRole("button", { name: "工作区技能" }));
    await openPage(user, dialog, /^MCP/);
    await user.click(within(dialog).getByRole("button", { name: "文档服务器" }));
    await user.click(within(dialog).getByRole("button", { name: "工作区服务器" }));
    await openPage(user, dialog, /^钩子/);
    await user.click(within(dialog).getByRole("button", { name: "Lint 钩子" }));
    await user.click(within(dialog).getByRole("button", { name: "工作区钩子" }));
    await user.click(within(dialog).getByRole("button", { name: "关闭" }));

    // The window it was begun in gets all of it back.
    dialog = await openCreate(user);
    expect(within(dialog).getByRole("combobox", { name: "位置" })).toHaveValue(WORKSPACE_KEY);
    expect(railEntries(dialog).slice(3, 6)).toEqual(["技能2", "MCP2", "钩子2"]);
    await user.click(within(dialog).getByRole("button", { name: "关闭" }));

    // A conversation with no workspace cannot write at that level, so the
    // draft falls back to the global one — and takes along only what every
    // conversation can reach.
    cleanup();
    const onSaveRole = saveRoleMock("agent_user_new");
    renderRoles({ workspaces: [], onSaveRole });
    dialog = await openCreate(user);
    expect(within(dialog).getByRole("textbox", { name: "角色名称" })).toHaveValue("半成品");
    expect(within(dialog).getByRole("combobox", { name: "位置" })).toHaveValue("");
    expect(railEntries(dialog).slice(3, 6)).toEqual(["技能1", "MCP1", "钩子1"]);
    await user.click(within(dialog).getByRole("button", { name: "保存角色" }));
    expect(onSaveRole).toHaveBeenCalledWith({ workspaceKey: null }, expect.objectContaining({
      name: "半成品",
      skillIds: ["skill_a"],
      mcpIds: ["mcp_docs"],
      hookIds: ["hook_lint"]
    }));
  });

  it("keeps an edit's draft until the file changes under it", async () => {
    const user = userEvent.setup();
    const { rerender } = render(<RolesHarness catalog={catalogWith()} />);
    let dialog = await openRole(user, "reviewer");
    await user.type(within(dialog).getByRole("textbox", { name: "子代理描述" }), "未保存");
    await user.click(within(dialog).getByRole("button", { name: "关闭" }));
    dialog = await openRole(user, "reviewer");
    expect(within(dialog).getByRole("textbox", { name: "子代理描述" })).toHaveValue("未保存");
    await user.click(within(dialog).getByRole("button", { name: "关闭" }));

    // The file moved on elsewhere: the stale draft is dropped, the file wins.
    rerender(
      <RolesHarness catalog={catalogWith([
        builtinOpus,
        roleResource("agent_user_reviewer", roleBody({ description: "新的正文" })),
        workspaceRole
      ])} />
    );
    dialog = await openRole(user, "reviewer");
    expect(within(dialog).getByRole("textbox", { name: "子代理描述" })).toHaveValue("新的正文");
  });

  it("drops the draft once it has become the file", async () => {
    const user = userEvent.setup();
    renderRoles();
    let dialog = await openCreate(user);
    await user.type(within(dialog).getByRole("textbox", { name: "角色名称" }), "已保存");
    await user.click(within(dialog).getByRole("button", { name: "保存角色" }));
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "新建角色" })).toBeNull());
    dialog = await openCreate(user);
    expect(within(dialog).getByRole("textbox", { name: "角色名称" })).toHaveValue("");
  });

  it("does not dismiss an in-progress window when its backdrop is clicked", async () => {
    const user = userEvent.setup();
    renderRoles();
    const dialog = await openCreate(user);
    const backdrop = dialog.parentElement;
    expect(backdrop).toHaveClass("modal-backdrop");
    fireEvent.mouseDown(backdrop!);
    expect(screen.getByRole("dialog", { name: "新建角色" })).toBeInTheDocument();
  });

  it("names a binding whose provider is only disabled, and saves it untouched", async () => {
    const user = userEvent.setup();
    const onSaveRole = saveRoleMock("agent_user_reviewer");
    const paused = { kind: "explicit" as const, providerId: "disabled-provider", modelId: "hidden-model" };
    renderRoles({
      onSaveRole,
      catalog: catalogWith([roleResource("agent_user_reviewer", roleBody({ modelSelection: paused }))])
    });
    const dialog = await openRole(user, "reviewer");
    const select = within(dialog).getByRole("combobox", { name: "执行模型" }) as HTMLSelectElement;
    expect(select).toBeInvalid();
    // The provider's display name, never its raw ID.
    expect(within(dialog).getByText("Disabled Provider · hidden-model（不可用）")).toBeInTheDocument();
    await user.click(within(dialog).getByRole("button", { name: "保存角色" }));
    // Not a save blocker: a provider the user cannot restore right now must not
    // hold every other edit hostage.
    expect(onSaveRole.mock.calls.at(-1)![1].modelSelection).toEqual(paused);
  });

  it("edits the role's template on its own page and saves the minted id into the file", async () => {
    const user = userEvent.setup();
    const onSaveRole = saveRoleMock("agent_user_reviewer");
    readTemplate.mockImplementation(async (templateId: string) => (
      templateId === "template_preset" ? [userMessage("ctx_a", "预设的第一句")] : []
    ));
    renderRoles({
      onSaveRole,
      presets: [{
        id: "conversation_default",
        name: "默认",
        description: "",
        templateId: "template_preset",
        settings: emptyConversationPresetSettings()
      }],
      templates: [templateSummary("template_preset", 1)]
    });
    const dialog = await openRole(user, "reviewer");
    await openPage(user, dialog, /^对话模板/);
    await user.click(within(dialog).getByRole("button", { name: "从预设覆盖" }));
    await user.click(within(screen.getByRole("menu", { name: "从预设覆盖" }))
      .getByRole("menuitem", { name: /^默认/ }));
    expect(writeTemplate).toHaveBeenCalledWith("", [userMessage("ctx_a", "预设的第一句")]);
    expect(await within(dialog).findByText("预设的第一句")).toBeInTheDocument();

    await user.click(within(dialog).getByRole("button", { name: "保存角色" }));
    expect(onSaveRole.mock.calls.at(-1)![1].templateId).toBe("template_minted");
  });
});
