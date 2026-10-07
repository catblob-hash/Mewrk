import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import App from "./App";
import { configureI18n } from "./i18n";
import { emptyConversationPresetSettings } from "./lib/conversationPresets";
import type { AppDocument } from "./types";
import {
  chooseComposerOption,
  composerOptionValue,
  documentWithModel,
  resetAppMocks,
  runtimeMocks
} from "./test/appMocks";

vi.mock("./lib/runtime", async (importOriginal) => {
  const { runtimeMocks } = await import("./test/appMockInstances");
  return { ...await importOriginal<typeof import("./lib/runtime")>(), ...runtimeMocks };
});
vi.mock("./lib/terminal", async () => (await import("./test/appMockInstances")).terminalMocks);
vi.mock("./lib/browser", async () => (await import("./test/appMockInstances")).browserMocks);
vi.mock("./lib/browserRendererMount", async () => {
  const { browserRendererMountMocks } = await import("./test/appMockInstances");
  return {
    startBrowserRendererMountHeartbeat: browserRendererMountMocks.startHeartbeat,
    stopBrowserRendererMountHeartbeat: browserRendererMountMocks.stopHeartbeat
  };
});
vi.mock("./lib/git", async (importOriginal) => {
  const { gitMocks } = await import("./test/appMockInstances");
  return { ...await importOriginal<typeof import("./lib/git")>(), ...gitMocks };
});
vi.mock("./components/TerminalPanel", async () => (await import("./test/appMockInstances")).terminalPanelModuleMock());

afterEach(() => configureI18n("zh-CN"));

/** A visible conversation leaves the workspace's unsent slot free for a genuinely new task. */
function documentWithHistory(): AppDocument {
  const document = documentWithModel();
  document.workspaces[0].conversations[0].contexts = [{
    id: "ctx-existing",
    kind: "user",
    content: "已有任务的上下文",
    createdAt: "2026-08-25T00:00:00Z"
  }];
  return document;
}

/**
 * Security level is the only new-conversation setting directly readable from the composer.
 * Each source therefore uses a distinct value to identify the selected precedence path.
 */
function documentWithPresetsAndMemory(): AppDocument {
  const document = documentWithHistory();
  document.globalSettings.conversationPresets = [
    {
      id: "preset-global",
      name: "全局默认",
      description: "",
      templateId: "",
      settings: { ...emptyConversationPresetSettings(), securityLevel: "allow_edits" }
    },
    {
      id: "preset-workspace",
      name: "本工作区",
      description: "",
      templateId: "",
      settings: { ...emptyConversationPresetSettings(), securityLevel: "request_approval" }
    }
  ];
  document.globalSettings.defaultConversationPresetId = "preset-global";
  document.workspaces[0].defaultConversationPresetId = "";
  document.workspaces[0].lastConversationSettings = {
    ...document.workspaces[0].conversations[0].settings,
    securityLevel: "full_access"
  };
  return document;
}

const securityLevel = () => composerOptionValue("安全层级");

const quietReply = {
  contexts: [],
  usage: {},
  model: "test-model",
  providerName: "",
  durationMs: 1
};

const composer = () => screen.getByLabelText("向 Agent 发送消息");

async function send(user: ReturnType<typeof userEvent.setup>, text: string) {
  await user.type(composer(), text);
  await user.click(screen.getByRole("button", { name: "发送" }));
}

describe("new conversation settings source", () => {
  beforeEach(resetAppMocks);

  // A new task is its project's draft: it resolves its settings from that project, and the
  // conversation it materializes as keeps them. New task opens the last project's — here the
  // one whose conversation is on screen.
  it.each([
    ["workspace memory", "在 Mewrk 新建任务", false, "完全访问", ""],
    ["the last project's settings", "新建任务", false, "完全访问", ""],
    ["workspace preset", "在 Mewrk 新建任务", true, "手动", "preset-workspace"]
  ] as const)("resolves %s for a new task", async (
    _source, buttonName, selectWorkspacePreset, expectedSecurity, expectedPresetId
  ) => {
    const user = userEvent.setup();
    const document = documentWithPresetsAndMemory();
    const existingId = document.workspaces[0].conversations[0].id;
    runtimeMocks.loadDocument.mockResolvedValue(document);
    runtimeMocks.runModel.mockResolvedValue(quietReply);

    render(<App />);
    await screen.findByLabelText("向 Agent 发送消息");

    if (selectWorkspacePreset) {
      await user.click(screen.getByRole("button", { name: "Mewrk 的更多选项" }));
      await user.click(screen.getByRole("menuitem", { name: /默认对话预设/ }));
      await user.click(screen.getByRole("menuitemradio", { name: "本工作区" }));
    }
    await user.click(screen.getByRole("button", { name: buttonName }));
    await waitFor(() => expect(securityLevel()).toBe(expectedSecurity));
    await send(user, "开始");
    await waitFor(() => {
      const conversations = savedConversations("ws_mewrk");
      expect(conversations).toHaveLength(2);
      expect(conversations.find((conversation) => conversation.id !== existingId)?.presetId)
        .toBe(expectedPresetId);
    });
  });

  it("starts a project with neither a preset nor remembered settings from the global default", async () => {
    const user = userEvent.setup();
    const document = documentWithPresetsAndMemory();
    document.workspaces[0].lastConversationSettings = null;
    runtimeMocks.loadDocument.mockResolvedValue(document);
    runtimeMocks.runModel.mockResolvedValue(quietReply);

    render(<App />);
    await screen.findByLabelText("向 Agent 发送消息");
    await user.click(screen.getByRole("button", { name: "在 Mewrk 新建任务" }));
    await waitFor(() => expect(securityLevel()).toBe("允许编辑"));
    await send(user, "开始");
    await waitFor(() => expect(savedConversations("ws_mewrk")
      .find((conversation) => conversation.id !== document.workspaces[0].conversations[0].id)?.presetId)
      .toBe("preset-global"));
  });

  it("moves a project's untouched draft along with its default preset, Last used included", async () => {
    const user = userEvent.setup();
    runtimeMocks.loadDocument.mockResolvedValue(documentWithPresetsAndMemory());

    render(<App />);
    await screen.findByLabelText("向 Agent 发送消息");
    await user.click(screen.getByRole("button", { name: "在 Mewrk 新建任务" }));
    await waitFor(() => expect(securityLevel()).toBe("完全访问"));

    const pickDefault = async (name: string) => {
      await user.click(screen.getByRole("button", { name: "Mewrk 的更多选项" }));
      await user.click(screen.getByRole("menuitem", { name: /默认对话预设/ }));
      await user.click(screen.getByRole("menuitemradio", { name: new RegExp(name) }));
    };
    // The draft is still what it was opened with, so it starts over from the new default.
    await pickDefault("本工作区");
    await waitFor(() => expect(securityLevel()).toBe("手动"));
    // Last used goes back to the project's remembered settings.
    await pickDefault("上一次");
    await waitFor(() => expect(securityLevel()).toBe("完全访问"));
    await waitFor(() => expect(savedWorkspaces()
      .find((workspace) => workspace.id === "ws_mewrk")?.defaultConversationPresetId).toBe(""));

    // Settings the user chose are the draft's own: a later default leaves them be.
    await chooseComposerOption(user, "安全层级", "允许编辑");
    await waitFor(() => expect(securityLevel()).toBe("允许编辑"));
    await pickDefault("本工作区");
    await waitFor(() => expect(savedWorkspaces()
      .find((workspace) => workspace.id === "ws_mewrk")?.defaultConversationPresetId).toBe("preset-workspace"));
    expect(securityLevel()).toBe("允许编辑");
  });

  it("keeps following the workspace snapshot after its last conversation is deleted", async () => {
    const user = userEvent.setup();
    runtimeMocks.loadDocument.mockResolvedValue(documentWithPresetsAndMemory());

    render(<App />);
    await screen.findByLabelText("向 Agent 发送消息");

    const list = screen.getByRole("navigation", { name: "对话列表" });
    await user.click(list.querySelector<HTMLElement>(".conversation-row__main")!);
    await user.click(within(list).getByRole("button", { name: /^删除 / }));
    await user.click(within(list).getByRole("button", { name: /^确认删除 / }));

    // The replacement slot follows the workspace, not the global default ("允许编辑").
    await waitFor(() => expect(securityLevel()).toBe("完全访问"));
  });

  it("remembers a conversation-level change as the workspace's next starting point", async () => {
    const user = userEvent.setup();
    runtimeMocks.loadDocument.mockResolvedValue(documentWithPresetsAndMemory());

    render(<App />);
    await screen.findByLabelText("向 Agent 发送消息");

    await chooseComposerOption(user, "安全层级", "允许编辑");
    await waitFor(() => expect(securityLevel()).toBe("允许编辑"));

    await user.click(screen.getByRole("button", { name: "在 Mewrk 新建任务" }));
    await waitFor(() => expect(securityLevel()).toBe("允许编辑"));
    // The new task is the draft, not a second conversation.
    await waitFor(() => expect(conversationsIn("ws_mewrk")).toBe(1));
  });
});

/** Inspect `saveDocument`: conversation commands are disabled in these tests. */
function savedWorkspaces(): AppDocument["workspaces"] {
  const calls = runtimeMocks.saveDocument.mock.calls;
  const latest = calls.at(-1);
  if (!latest) throw new Error("文档还没有落过盘");
  return (latest[0] as AppDocument).workspaces;
}

function savedConversations(workspaceId: string) {
  return savedWorkspaces().find((workspace) => workspace.id === workspaceId)?.conversations ?? [];
}

function conversationsIn(workspaceId: string): number {
  return savedConversations(workspaceId).length;
}

function lastSavedDocument(): AppDocument | undefined {
  return runtimeMocks.saveDocument.mock.calls.at(-1)?.[0] as AppDocument | undefined;
}

describe("draft conversation", () => {
  beforeEach(resetAppMocks);

  it("keeps a new task in the renderer until it is sent, and runs it under the id it became", async () => {
    const user = userEvent.setup();
    const document = documentWithHistory();
    const existingId = document.workspaces[0].conversations[0].id;
    runtimeMocks.loadDocument.mockResolvedValue(document);
    runtimeMocks.runModel.mockResolvedValue({
      contexts: [{
        id: "ctx_reply",
        kind: "assistant",
        content: "好的",
        createdAt: "2026-08-26T00:00:00Z"
      }],
      usage: {},
      model: "test-model",
      providerName: "",
      durationMs: 1
    });

    render(<App />);
    await screen.findByLabelText("向 Agent 发送消息");

    await user.click(screen.getByRole("button", { name: "新建任务" }));
    const list = screen.getByRole("navigation", { name: "对话列表" });
    expect(list.querySelectorAll(".conversation-row__main")).toHaveLength(1);
    await user.type(composer(), "第一句话");
    // Nothing about the draft reaches the document: its project is still the user's to change.
    await waitFor(() => expect(savedConversations("ws_mewrk").map((conversation) => conversation.id))
      .toEqual([existingId]));
    expect(runtimeMocks.runModel).not.toHaveBeenCalled();

    await user.click(screen.getByRole("button", { name: "发送" }));

    await waitFor(() => expect(runtimeMocks.runModel).toHaveBeenCalledTimes(1));
    const slotId = await waitFor(() => {
      const conversations = savedConversations("ws_mewrk");
      expect(conversations).toHaveLength(2);
      const created = conversations.find((conversation) => conversation.id !== existingId)!;
      expect(created.id).not.toMatch(/^__draft__/);
      expect(created.contexts).toEqual(
        expect.arrayContaining([expect.objectContaining({ kind: "user", content: "第一句话" })])
      );
      expect(list.querySelectorAll(".conversation-row__main")).toHaveLength(2);
      return created.id;
    });
    expect(runtimeMocks.runModel).toHaveBeenCalledWith(
      expect.objectContaining({
        conversationId: slotId,
        contexts: expect.arrayContaining([
          expect.objectContaining({ kind: "user", content: "第一句话" })
        ])
      }),
      expect.any(Function),
      expect.any(String)
    );
  });

  it("gives every project a draft of its own, and picking a project goes to that one", async () => {
    const document = documentWithHistory();
    const existingId = document.workspaces[0].conversations[0].id;
    runtimeMocks.loadDocument.mockResolvedValue(document);
    runtimeMocks.runModel.mockResolvedValue(quietReply);
    const user = userEvent.setup();
    render(<App />);
    await screen.findByLabelText("向 Agent 发送消息");

    await user.click(screen.getByRole("button", { name: "在 Mewrk 新建任务" }));
    await user.type(composer(), "Mewrk 的草稿");
    await user.click(screen.getByRole("button", { name: "项目：Mewrk" }));
    await user.click(within(screen.getByRole("menu", { name: "选择项目" }))
      .getByRole("menuitemradio", { name: "临时项目" }));

    // The temporary project's own draft: Mewrk's text stays behind with Mewrk's.
    await screen.findByRole("button", { name: "项目：临时项目" });
    await waitFor(() => expect(composer()).toHaveValue(""));
    await user.type(composer(), "临时的草稿");
    await user.click(screen.getByRole("button", { name: "在 Mewrk 新建任务" }));
    await waitFor(() => expect(composer()).toHaveValue("Mewrk 的草稿"));
    await waitFor(() => {
      expect(savedConversations("ws_mewrk").map((conversation) => conversation.id)).toEqual([existingId]);
      expect(conversationsIn("__temporary__")).toBe(0);
    });

    await user.click(screen.getByRole("button", { name: "发送" }));
    await waitFor(() => expect(conversationsIn("ws_mewrk")).toBe(2));
    expect(conversationsIn("__temporary__")).toBe(0);
    // The temporary project's draft is still waiting, text and all.
    await user.click(screen.getByRole("button", { name: "在 临时项目 新建任务" }));
    await waitFor(() => expect(composer()).toHaveValue("临时的草稿"));
  });

  /**
   * New task goes to the last project's draft. With none chosen yet, the last project is the
   * temporary one; once another project's draft is on screen, that one is.
   */
  it("opens the last project's draft from New task, keeping each draft's text", async () => {
    const document = documentWithModel();
    document.workspaces.forEach((workspace) => { workspace.conversations = []; });
    runtimeMocks.loadDocument.mockResolvedValue(document);
    const user = userEvent.setup();
    const { container } = render(<App />);
    await screen.findByRole("button", { name: "项目：临时项目" });
    expect(container.querySelector(".composer-cat")).not.toBeNull();

    await user.type(composer(), "临时项目的草稿");
    await user.click(screen.getByRole("button", { name: "在 Mewrk 新建任务" }));
    await screen.findByRole("button", { name: "项目：Mewrk" });
    await waitFor(() => expect(composer()).toHaveValue(""));
    expect(container.querySelector(".composer-cat")).not.toBeNull();

    await user.click(screen.getByRole("button", { name: "新建任务" }));
    expect(screen.getByRole("button", { name: "项目：Mewrk" })).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "在 临时项目 新建任务" }));
    await waitFor(() => expect(composer()).toHaveValue("临时项目的草稿"));
    await waitFor(() => expect(savedWorkspaces().flatMap((workspace) => workspace.conversations)).toEqual([]));
  });

  /** A conversation that holds messages is not the empty desk the cat lies on. */
  it("sends the cat away once the conversation holds content, and back on a new task", async () => {
    const user = userEvent.setup();
    runtimeMocks.loadDocument.mockResolvedValue(documentWithHistory());
    const { container } = render(<App />);
    await screen.findByLabelText("向 Agent 发送消息");

    const list = screen.getByRole("navigation", { name: "对话列表" });
    await user.click(list.querySelector<HTMLElement>(".conversation-row__main")!);
    await waitFor(() => expect(container.querySelector(".composer-cat")).toBeNull());

    await user.click(screen.getByRole("button", { name: "新建任务" }));
    await waitFor(() => expect(container.querySelector(".composer-cat")).not.toBeNull());
  });

  it("keeps the draft and its composer text while another conversation is open", async () => {
    const user = userEvent.setup();
    runtimeMocks.loadDocument.mockResolvedValue(documentWithHistory());

    render(<App />);
    await screen.findByLabelText("向 Agent 发送消息");

    await user.click(screen.getByRole("button", { name: "新建任务" }));
    await user.type(screen.getByLabelText("向 Agent 发送消息"), "没发出去的半句话");

    const list = screen.getByRole("navigation", { name: "对话列表" });
    await user.click(list.querySelector<HTMLElement>(".conversation-row__main")!);
    await waitFor(() => expect(screen.getByLabelText("向 Agent 发送消息")).toHaveValue(""));

    await user.click(screen.getByRole("button", { name: "新建任务" }));
    await waitFor(() => expect(screen.getByLabelText("向 Agent 发送消息")).toHaveValue("没发出去的半句话"));
    await waitFor(() => expect(conversationsIn("ws_mewrk")).toBe(1));
  });

  it("lands on the temporary project's draft on a fresh start and sends into it", async () => {
    const document = documentWithModel();
    document.workspaces.forEach((workspace) => { workspace.conversations = []; });
    runtimeMocks.loadDocument.mockResolvedValue(document);
    runtimeMocks.runModel.mockResolvedValue({
      contexts: [],
      usage: {},
      model: "test-model",
      providerName: "",
      durationMs: 1
    });
    const user = userEvent.setup();

    render(<App />);
    await screen.findByRole("button", { name: "项目：临时项目" });
    await waitFor(() => expect(savedWorkspaces().flatMap((workspace) => workspace.conversations)).toEqual([]));

    await user.type(screen.getByLabelText("向 Agent 发送消息"), "先说再挑目录");
    await user.click(screen.getByRole("button", { name: "发送" }));

    // With no project picked, the task is the temporary project's.
    await waitFor(() => expect(conversationsIn("__temporary__")).toBe(1));
    expect(conversationsIn("ws_mewrk")).toBe(0);
    expect(savedConversations("__temporary__")[0].id).not.toMatch(/^__draft__/);
  });

  /** Inserts a message into the timeline through the right-click menu on an empty conversation. */
  async function insertUserMessage(
    user: ReturnType<typeof userEvent.setup>,
    container: HTMLElement,
    content: string
  ) {
    fireEvent.contextMenu(container.querySelector(".empty-state") ?? container.querySelector(".context-stream")!, {
      clientX: 40,
      clientY: 180
    });
    await user.click(screen.getByRole("menuitem", { name: "用户输入" }));
    // The editor is a portalled dialog, so it is outside the render container.
    await user.type(document.querySelector<HTMLTextAreaElement>(".context-text-editor textarea")!, content);
    await user.click(screen.getByRole("button", { name: "保存" }));
  }

  it("starts the draft a project pick goes to from that project's own preset", async () => {
    const document = documentWithPresetsAndMemory();
    document.workspaces.forEach((workspace) => { workspace.conversations = []; });
    document.workspaces[0].defaultConversationPresetId = "preset-workspace";
    runtimeMocks.loadDocument.mockResolvedValue(document);
    runtimeMocks.runModel.mockResolvedValue(quietReply);
    const user = userEvent.setup();
    const { container } = render(<App />);
    await screen.findByRole("button", { name: "项目：临时项目" });
    expect(securityLevel()).toBe("允许编辑");

    await user.type(screen.getByLabelText("向 Agent 发送消息"), "选工作区前的输入");
    await user.click(screen.getByRole("button", { name: "项目：临时项目" }));
    await user.click(await screen.findByRole("menuitemradio", { name: "Mewrk" }));

    // Mewrk's draft is made from Mewrk's preset; the temporary project's keeps its own text,
    // and nothing is persisted yet.
    await screen.findByRole("button", { name: "项目：Mewrk" });
    await waitFor(() => expect(securityLevel()).toBe("手动"));
    expect(screen.getByLabelText("向 Agent 发送消息")).toHaveValue("");
    await waitFor(() => expect(savedWorkspaces().flatMap((workspace) => workspace.conversations)).toEqual([]));
    expect(screen.getByRole("navigation", { name: "对话列表" }).querySelectorAll(".conversation-row__main"))
      .toHaveLength(0);
    expect(container.querySelector(".composer-cat")).not.toBeNull();

    await send(user, "开始");
    await waitFor(() => {
      const conversations = savedConversations("ws_mewrk");
      expect(conversations).toHaveLength(1);
      expect(conversations[0].id).not.toMatch(/^__draft__/);
      expect(conversations[0]).toEqual(expect.objectContaining({
        presetId: "preset-workspace",
        settings: expect.objectContaining({ securityLevel: "request_approval" }),
        contexts: expect.arrayContaining([
          expect.objectContaining({ kind: "user", content: "开始" })
        ])
      }));
    });
    expect(conversationsIn("__temporary__")).toBe(0);
  });

  it("brings an unsent task back after a restart with its own settings, not its preset's", async () => {
    const document = documentWithPresetsAndMemory();
    runtimeMocks.loadDocument.mockResolvedValue(document);
    const user = userEvent.setup();
    const first = render(<App />);
    await screen.findByLabelText("向 Agent 发送消息");
    await user.click(screen.getByRole("button", { name: "新建任务" }));
    await waitFor(() => expect(securityLevel()).toBe("完全访问"));
    await chooseComposerOption(user, "安全层级", "手动");

    // Kept with its project.
    const saved = await waitFor(() => {
      const latest = lastSavedDocument();
      expect(latest?.workspaces.find((workspace) => workspace.id === "ws_mewrk")
        ?.draftConversation?.settings.securityLevel).toBe("request_approval");
      return latest!;
    });

    // The next run of the app opens the document this one left; the preset still says otherwise.
    first.unmount();
    runtimeMocks.loadDocument.mockResolvedValue(saved);
    render(<App />);
    await screen.findByLabelText("向 Agent 发送消息");
    await user.click(screen.getByRole("button", { name: "新建任务" }));
    await waitFor(() => expect(securityLevel()).toBe("手动"));
  });

  it("keeps nothing for an untouched draft, so a restart rebuilds it from the project's default then", async () => {
    const document = documentWithPresetsAndMemory();
    runtimeMocks.loadDocument.mockResolvedValue(document);
    const user = userEvent.setup();
    const first = render(<App />);
    await screen.findByLabelText("向 Agent 发送消息");
    await user.click(screen.getByRole("button", { name: "在 Mewrk 新建任务" }));
    await waitFor(() => expect(securityLevel()).toBe("完全访问"));
    await user.type(composer(), "只写了字");

    const saved = await waitFor(() => {
      const latest = lastSavedDocument();
      expect(latest).toBeTruthy();
      return latest!;
    });
    const mewrk = saved.workspaces.find((workspace) => workspace.id === "ws_mewrk")!;
    expect(mewrk.draftConversation ?? null).toBeNull();

    // The project's default changed while the app was closed; the draft follows it.
    first.unmount();
    mewrk.defaultConversationPresetId = "preset-workspace";
    runtimeMocks.loadDocument.mockResolvedValue(saved);
    render(<App />);
    await screen.findByLabelText("向 Agent 发送消息");
    await user.click(screen.getByRole("button", { name: "在 Mewrk 新建任务" }));
    await waitFor(() => expect(securityLevel()).toBe("手动"));
  });

  it("stops keeping a task as a draft once it is sent", async () => {
    const document = documentWithPresetsAndMemory();
    runtimeMocks.loadDocument.mockResolvedValue(document);
    runtimeMocks.runModel.mockResolvedValue(quietReply);
    const user = userEvent.setup();
    render(<App />);
    await screen.findByLabelText("向 Agent 发送消息");
    await user.click(screen.getByRole("button", { name: "新建任务" }));
    await chooseComposerOption(user, "安全层级", "手动");
    const mewrk = () => lastSavedDocument()?.workspaces.find((workspace) => workspace.id === "ws_mewrk");
    await waitFor(() => expect(mewrk()?.draftConversation).toBeTruthy());

    await send(user, "开始");
    await waitFor(() => {
      expect(conversationsIn("ws_mewrk")).toBe(2);
      expect(mewrk()?.draftConversation ?? null).toBeNull();
    });
  });

  it("keeps hand-written content in the draft until it is sent, then leads the new conversation with it", async () => {
    const user = userEvent.setup();
    const document = documentWithModel();
    // An empty conversation left from before new tasks were drafts; the draft does not reuse it.
    const leftoverId = document.workspaces[0].conversations[0].id;
    runtimeMocks.loadDocument.mockResolvedValue(document);
    runtimeMocks.runModel.mockResolvedValue({
      contexts: [{ id: "ctx_reply", kind: "assistant", content: "好的", createdAt: "2026-08-26T00:00:00Z" }],
      usage: {},
      model: "test-model",
      providerName: "",
      durationMs: 1
    });

    const { container } = render(<App />);
    await screen.findByLabelText("向 Agent 发送消息");
    await user.click(screen.getByRole("button", { name: "新建任务" }));
    const list = screen.getByRole("navigation", { name: "对话列表" });
    expect(list.querySelectorAll(".conversation-row__main")).toHaveLength(0);

    await insertUserMessage(user, container, "手写的开场白");

    // A hand-written message involves no workspace, so the draft stays a draft around it.
    expect(await screen.findByText("手写的开场白")).toBeInTheDocument();
    await waitFor(() => expect(savedConversations("ws_mewrk")).toEqual([
      expect.objectContaining({ id: leftoverId, contexts: [] })
    ]));
    expect(list.querySelectorAll(".conversation-row__main")).toHaveLength(0);
    expect(screen.getByRole("button", { name: "项目：Mewrk" })).toBeInTheDocument();
    expect(runtimeMocks.runModel).not.toHaveBeenCalled();

    await send(user, "第一句话");

    // The hand-written message leads the new conversation, ahead of the sent message.
    await waitFor(() => {
      expect(conversationsIn("ws_mewrk")).toBe(2);
      const conversation = savedConversations("ws_mewrk").find((item) => item.id !== leftoverId);
      expect(conversation?.contexts.slice(0, 2).map(
        (item) => ("content" in item ? item.content : item.kind)
      )).toEqual(["手写的开场白", "第一句话"]);
      expect(list.querySelectorAll(".conversation-row__main")).toHaveLength(1);
    });
  });

  it("opens a new task with its preset's opening messages, still a draft", async () => {
    const user = userEvent.setup();
    const document = documentWithModel();
    document.globalSettings.conversationPresets = [{
      id: "preset-engineering",
      name: "工程",
      description: "",
      templateId: "template-engineering",
      settings: { ...emptyConversationPresetSettings(), securityLevel: "allow_edits" }
    }];
    document.globalSettings.defaultConversationPresetId = "preset-engineering";
    runtimeMocks.loadDocument.mockResolvedValue(document);
    runtimeMocks.previewConversationTemplate.mockResolvedValue([
      { id: "tpl-system", kind: "system", content: "遵循工程实践", createdAt: "2026-08-26T00:00:00Z" }
    ]);

    render(<App />);
    await screen.findByLabelText("向 Agent 发送消息");
    await user.click(screen.getByRole("button", { name: "新建任务" }));

    expect(runtimeMocks.previewConversationTemplate).toHaveBeenCalledWith("template-engineering");
    expect(await screen.findByText("遵循工程实践")).toBeInTheDocument();
    // It is the draft's own first card; nothing became a conversation yet.
    const list = screen.getByRole("navigation", { name: "对话列表" });
    expect(list.querySelectorAll(".conversation-row__main")).toHaveLength(0);
  });

  it("fixes the draft in its project once a tool runs in it", async () => {
    const user = userEvent.setup();
    const document = documentWithModel();
    const leftoverId = document.workspaces[0].conversations[0].id;
    runtimeMocks.loadDocument.mockResolvedValue(document);

    const { container } = render(<App />);
    await screen.findByLabelText("向 Agent 发送消息");
    await user.click(screen.getByRole("button", { name: "新建任务" }));
    await user.type(composer(), "工具之后再说");

    fireEvent.contextMenu(container.querySelector(".empty-state")!, { clientX: 40, clientY: 180 });
    await user.click(screen.getByRole("menuitem", { name: /工具调用/ }));
    await user.click(screen.getByRole("menuitem", { name: "Shell" }));
    await user.click(screen.getByRole("menuitem", { name: /^PowerShell$/ }));
    await user.type(screen.getByLabelText("命令 *"), "Get-ChildItem");
    await user.click(screen.getByRole("button", { name: "执行并添加" }));

    // The host runs the tool against a conversation it holds, never the draft's placeholder.
    await waitFor(() => expect(runtimeMocks.executeTool).toHaveBeenCalledTimes(1));
    const [request] = runtimeMocks.executeTool.mock.calls[0];
    expect(request.conversationId).not.toMatch(/^__draft__/);
    expect(request.conversationId).not.toBe(leftoverId);
    expect(runtimeMocks.requestToolApproval)
      .toHaveBeenCalledWith(expect.objectContaining({ conversationId: request.conversationId }));
    // Which is real by the time the host is asked, with the tool card in it.
    expect(runtimeMocks.saveDocument.mock.calls.some(([saved]) => (saved as AppDocument).workspaces
      .find((workspace) => workspace.id === "ws_mewrk")?.conversations
      .some((conversation) => conversation.id === request.conversationId))).toBe(true);
    await waitFor(() => expect(savedConversations("ws_mewrk")
      .find((conversation) => conversation.id === request.conversationId)?.contexts)
      .toEqual([expect.objectContaining({ kind: "tool", toolName: "powershell" })]));
    expect(runtimeMocks.saveDocument.mock.invocationCallOrder[0])
      .toBeLessThan(runtimeMocks.requestToolApproval.mock.invocationCallOrder[0]);
    // It now belongs to its project for good: the project chip is gone, the composer text stays.
    await waitFor(() => expect(screen.queryByRole("button", { name: /^项目：/ })).not.toBeInTheDocument());
    expect(composer()).toHaveValue("工具之后再说");
  });
});

describe("the first task in a new project", () => {
  beforeEach(resetAppMocks);

  it("reaches the host only after the project it lives in", async () => {
    const user = userEvent.setup();
    const document = documentWithHistory();
    runtimeMocks.loadDocument.mockResolvedValue(document);
    runtimeMocks.runModel.mockResolvedValue(quietReply);
    // The host files a conversation only under a workspace its own document already holds, and it
    // learns of a workspace only from a document save.
    const hostWorkspaces = new Set(document.workspaces.map((workspace) => workspace.id));
    runtimeMocks.hasConversationCommands.mockReturnValue(true);
    runtimeMocks.saveDocument.mockImplementation(async (saved: AppDocument) => {
      hostWorkspaces.clear();
      for (const workspace of saved.workspaces) hostWorkspaces.add(workspace.id);
    });
    runtimeMocks.createConversationRemote.mockImplementation(async (workspaceId, next) => {
      if (!hostWorkspaces.has(workspaceId)) throw new Error(`工作区 ${workspaceId} 不存在`);
      return next;
    });

    render(<App />);
    await screen.findByLabelText("向 Agent 发送消息");
    await user.click(screen.getByRole("button", { name: "新建项目" }));
    const dialog = screen.getByRole("dialog", { name: "新建项目" });
    await user.type(within(dialog).getByRole("textbox", { name: "工作区 1 的绝对路径" }), "C:\\Temp\\first-task");
    await user.click(within(dialog).getByRole("button", { name: "创建项目" }));

    // The new project opens on the draft; sending it at once materializes the conversation while
    // the project itself is still waiting on the debounced document save.
    await screen.findByRole("button", { name: "项目：first-task" });
    expect(runtimeMocks.createConversationRemote).not.toHaveBeenCalled();
    await send(user, "第一句话");

    await waitFor(() => expect(runtimeMocks.createConversationRemote).toHaveBeenCalledTimes(1));
    const [workspaceId, created] = runtimeMocks.createConversationRemote.mock.calls[0];
    expect(workspaceId).not.toBe("ws_mewrk");
    expect(created.id).not.toMatch(/^__draft__/);
    // The host accepted it, so it is not a renderer-only ghost the run cannot find.
    await expect(runtimeMocks.createConversationRemote.mock.results[0].value).resolves.toBe(created);
    await waitFor(() => expect(runtimeMocks.runModel).toHaveBeenCalledWith(
      expect.objectContaining({ conversationId: created.id }),
      expect.any(Function),
      expect.any(String)
    ));
  });
});
